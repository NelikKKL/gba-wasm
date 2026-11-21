//! Звук: 4 PSG-канала (2 прямоугольных, волновой, шум) и 2 Direct Sound FIFO.
//! Выход — стерео f32 на 32768 Гц (по сэмплу каждые 512 тактов CPU).

pub const SAMPLE_RATE: u32 = 32_768;
const CYCLES_PER_SAMPLE: u32 = 512; // 16 777 216 / 32 768
const SEQ_CYCLES: u32 = 32_768; // 512 Гц: частота кадрового секвенсора
const DUTY: [u8; 4] = [0b0000_0001, 0b1000_0001, 0b1000_0111, 0b0111_1110];
const MAX_QUEUED: usize = 131_072; // не копим больше ~2 секунд, если никто не забирает звук

fn io16(io: &[u8], off: usize) -> u16 {
    io[off] as u16 | (io[off + 1] as u16) << 8
}

#[derive(Clone, Copy, Default)]
struct Envelope {
    period: u8,
    timer: u8,
    volume: u8,
    inc: bool,
}

impl Envelope {
    fn from_reg(b: u8) -> Self {
        let period = b & 7;
        Self { period, timer: if period == 0 { 8 } else { period }, volume: b >> 4, inc: b & 8 != 0 }
    }

    fn clock(&mut self) {
        if self.period == 0 {
            return;
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer == 0 {
            self.timer = self.period;
            if self.inc {
                if self.volume < 15 {
                    self.volume += 1;
                }
            } else if self.volume > 0 {
                self.volume -= 1;
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Square {
    on: bool,
    freq: u16,
    timer: i32,
    pos: u8,
    length: u16,
    len_en: bool,
    env: Envelope,
    // sweep (только канал 1)
    sweep_period: u8,
    sweep_shift: u8,
    sweep_dec: bool,
    sweep_timer: u8,
    sweep_on: bool,
    shadow: u16,
}

impl Square {
    fn period(&self) -> i32 {
        (2048 - self.freq as i32).max(1) * 16
    }

    fn advance(&mut self, cycles: u32) {
        if !self.on {
            return;
        }
        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.pos = (self.pos + 1) & 7;
        }
    }

    fn output(&self, duty: u8) -> u8 {
        if self.on && (DUTY[(duty & 3) as usize] >> self.pos) & 1 != 0 {
            self.env.volume
        } else {
            0
        }
    }

    fn clock_length(&mut self) {
        if self.len_en && self.length > 0 {
            self.length -= 1;
            if self.length == 0 {
                self.on = false;
            }
        }
    }

    fn sweep_calc(&self) -> u16 {
        let d = self.shadow >> self.sweep_shift;
        if self.sweep_dec { self.shadow.wrapping_sub(d) } else { self.shadow + d }
    }

    fn clock_sweep(&mut self) {
        if self.sweep_timer > 0 {
            self.sweep_timer -= 1;
        }
        if self.sweep_timer == 0 {
            self.sweep_timer = if self.sweep_period == 0 { 8 } else { self.sweep_period };
            if self.sweep_on && self.sweep_period != 0 {
                let new = self.sweep_calc();
                if new > 2047 {
                    self.on = false;
                } else if self.sweep_shift != 0 {
                    self.shadow = new;
                    self.freq = new;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Wave {
    on: bool,
    freq: u16,
    timer: i32,
    pos: u8,
    length: u16,
    len_en: bool,
    ram: [[u8; 16]; 2],
}

impl Wave {
    fn advance(&mut self, cycles: u32, dim64: bool) {
        if !self.on {
            return;
        }
        let period = (2048 - self.freq as i32).max(1) * 8;
        let n: u8 = if dim64 { 64 } else { 32 };
        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += period;
            self.pos = (self.pos + 1) % n;
        }
    }

    fn output(&self, io: &[u8]) -> u8 {
        if !self.on {
            return 0;
        }
        let cnt_l = io[0x70];
        let bank = if cnt_l & 0x20 != 0 { (self.pos / 32) as usize } else { ((cnt_l >> 6) & 1) as usize };
        let idx = (self.pos % 32) as usize;
        let byte = self.ram[bank][idx / 2];
        let s = if idx % 2 == 0 { byte >> 4 } else { byte & 0xF };
        let vol = io[0x73];
        if vol & 0x80 != 0 {
            return s * 3 / 4; // принудительные 75%
        }
        match (vol >> 5) & 3 {
            0 => 0,
            1 => s,
            2 => s >> 1,
            _ => s >> 2,
        }
    }

    fn clock_length(&mut self) {
        if self.len_en && self.length > 0 {
            self.length -= 1;
            if self.length == 0 {
                self.on = false;
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Noise {
    on: bool,
    timer: i32,
    lfsr: u16,
    length: u16,
    len_en: bool,
    env: Envelope,
}

impl Noise {
    fn period(io: &[u8]) -> i32 {
        let b = io[0x7C];
        let r = (b & 7) as i32;
        let s = ((b >> 4) & 0xF) as u32;
        (if r == 0 { 16 } else { 32 * r }) << (s + 1)
    }

    fn advance(&mut self, cycles: u32, io: &[u8]) {
        if !self.on {
            return;
        }
        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += Self::period(io);
            let x = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
            self.lfsr = (self.lfsr >> 1) | (x << 14);
            if io[0x7C] & 8 != 0 {
                self.lfsr = (self.lfsr & !0x40) | (x << 6); // 7-битный режим
            }
        }
    }

    fn output(&self) -> u8 {
        if self.on && self.lfsr & 1 == 0 { self.env.volume } else { 0 }
    }

    fn clock_length(&mut self) {
        if self.len_en && self.length > 0 {
            self.length -= 1;
            if self.length == 0 {
                self.on = false;
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Fifo {
    buf: [i8; 32],
    read: usize,
    count: usize,
    out: i8,
}

impl Fifo {
    fn push(&mut self, v: i8) {
        if self.count < 32 {
            self.buf[(self.read + self.count) & 31] = v;
            self.count += 1;
        }
    }

    fn pop(&mut self) {
        if self.count > 0 {
            self.out = self.buf[self.read];
            self.read = (self.read + 1) & 31;
            self.count -= 1;
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

pub struct Apu {
    sq: [Square; 2],
    wave: Wave,
    noise: Noise,
    fifo: [Fifo; 2],
    seq_acc: u32,
    seq_step: u8,
    sample_acc: u32,
    prev_in: [f32; 2],
    prev_out: [f32; 2],
    samples: Vec<f32>,
}

impl Apu {
    pub fn new() -> Self {
        Self {
            sq: [Square::default(); 2],
            wave: Wave::default(),
            noise: Noise::default(),
            fifo: [Fifo::default(); 2],
            seq_acc: 0,
            seq_step: 0,
            sample_acc: 0,
            prev_in: [0.0; 2],
            prev_out: [0.0; 2],
            samples: Vec::new(),
        }
    }

    /// Забрать накопленные сэмплы (чередуются L, R).
    pub fn take_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.samples)
    }

    pub fn fifo_len(&self, i: usize) -> usize {
        self.fifo[i].count
    }

    /// Биты 0-3 регистра SOUNDCNT_X: какие каналы играют.
    pub fn status(&self) -> u8 {
        self.sq[0].on as u8 | (self.sq[1].on as u8) << 1 | (self.wave.on as u8) << 2 | (self.noise.on as u8) << 3
    }

    pub fn wave_read(&self, io: &[u8], off: usize) -> u8 {
        let bank = 1 - ((io[0x70] >> 6) & 1) as usize;
        self.wave.ram[bank][off - 0x90]
    }

    fn power_off(&mut self) {
        self.sq = [Square::default(); 2];
        self.noise = Noise::default();
        self.wave.on = false;
        self.wave.len_en = false;
    }

    /// Запись байта в регистры звука (0x60-0xA7). `io[off]` уже обновлён.
    pub fn write(&mut self, io: &[u8], off: usize, v: u8) {
        match off {
            0x62 => self.sq[0].length = 64 - ((v & 0x3F) as u16),
            0x68 => self.sq[1].length = 64 - ((v & 0x3F) as u16),
            0x72 => self.wave.length = 256 - v as u16,
            0x78 => self.noise.length = 64 - ((v & 0x3F) as u16),
            0x64 => self.sq[0].freq = (self.sq[0].freq & 0x700) | v as u16,
            0x65 => {
                self.sq[0].freq = (self.sq[0].freq & 0xFF) | (((v & 7) as u16) << 8);
                self.sq[0].len_en = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.trigger_square(0, io);
                }
            }
            0x6C => self.sq[1].freq = (self.sq[1].freq & 0x700) | v as u16,
            0x6D => {
                self.sq[1].freq = (self.sq[1].freq & 0xFF) | (((v & 7) as u16) << 8);
                self.sq[1].len_en = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.trigger_square(1, io);
                }
            }
            0x70 => {
                if v & 0x80 == 0 {
                    self.wave.on = false; // DAC выключен
                }
            }
            0x74 => self.wave.freq = (self.wave.freq & 0x700) | v as u16,
            0x75 => {
                self.wave.freq = (self.wave.freq & 0xFF) | (((v & 7) as u16) << 8);
                self.wave.len_en = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.trigger_wave(io);
                }
            }
            0x7D => {
                self.noise.len_en = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.trigger_noise(io);
                }
            }
            0x83 => {
                if v & 0x08 != 0 {
                    self.fifo[0].reset();
                }
                if v & 0x80 != 0 {
                    self.fifo[1].reset();
                }
            }
            0x84 => {
                if v & 0x80 == 0 {
                    self.power_off();
                }
            }
            0x90..=0x9F => {
                let bank = 1 - ((io[0x70] >> 6) & 1) as usize;
                self.wave.ram[bank][off - 0x90] = v;
            }
            0xA0..=0xA3 => self.fifo[0].push(v as i8),
            0xA4..=0xA7 => self.fifo[1].push(v as i8),
            _ => {}
        }
    }

    fn trigger_square(&mut self, i: usize, io: &[u8]) {
        let (h, x) = if i == 0 { (0x62, 0x64) } else { (0x68, 0x6C) };
        let env_reg = io[h + 1];
        let freq = io16(io, x) & 0x7FF;
        let ch = &mut self.sq[i];
        ch.on = env_reg & 0xF8 != 0; // DAC включён, если громкость или рост envelope ненулевые
        if ch.length == 0 {
            ch.length = 64;
        }
        ch.freq = freq;
        ch.timer = ch.period();
        ch.pos = 0;
        ch.env = Envelope::from_reg(env_reg);
        if i == 0 {
            let sw = io[0x60];
            ch.sweep_period = (sw >> 4) & 7;
            ch.sweep_shift = sw & 7;
            ch.sweep_dec = sw & 8 != 0;
            ch.sweep_timer = if ch.sweep_period == 0 { 8 } else { ch.sweep_period };
            ch.shadow = freq;
            ch.sweep_on = ch.sweep_period != 0 || ch.sweep_shift != 0;
            if ch.sweep_shift != 0 && ch.sweep_calc() > 2047 {
                ch.on = false;
            }
        }
    }

    fn trigger_wave(&mut self, io: &[u8]) {
        let w = &mut self.wave;
        w.on = io[0x70] & 0x80 != 0;
        if w.length == 0 {
            w.length = 256;
        }
        w.freq = io16(io, 0x74) & 0x7FF;
        w.pos = 0;
        w.timer = (2048 - w.freq as i32).max(1) * 8;
    }

    fn trigger_noise(&mut self, io: &[u8]) {
        let n = &mut self.noise;
        let env_reg = io[0x79];
        n.on = env_reg & 0xF8 != 0;
        if n.length == 0 {
            n.length = 64;
        }
        n.env = Envelope::from_reg(env_reg);
        n.lfsr = 0x7FFF;
        n.timer = Noise::period(io);
    }

    /// Переполнение таймера: FIFO, привязанные к нему, отдают следующий сэмпл.
    /// Возвращает маску FIFO (бит 0 — A, бит 1 — B), которым нужны новые данные по DMA.
    pub fn timer_overflow(&mut self, timer: usize, io: &[u8]) -> u8 {
        let cnt_h = io[0x83];
        let mut need = 0u8;
        for f in 0..2 {
            let sel = if f == 0 { (cnt_h >> 2) & 1 } else { (cnt_h >> 6) & 1 };
            if sel as usize == timer {
                self.fifo[f].pop();
                if self.fifo[f].count <= 16 {
                    need |= 1 << f;
                }
            }
        }
        need
    }

    pub fn advance(&mut self, cycles: u32, io: &[u8]) {
        if io[0x84] & 0x80 != 0 {
            self.sq[0].advance(cycles);
            self.sq[1].advance(cycles);
            self.wave.advance(cycles, io[0x70] & 0x20 != 0);
            self.noise.advance(cycles, io);
            self.seq_acc += cycles;
            while self.seq_acc >= SEQ_CYCLES {
                self.seq_acc -= SEQ_CYCLES;
                self.clock_sequencer();
            }
        }
        self.sample_acc += cycles;
        while self.sample_acc >= CYCLES_PER_SAMPLE {
            self.sample_acc -= CYCLES_PER_SAMPLE;
            self.mix(io);
        }
    }

    fn clock_sequencer(&mut self) {
        let step = self.seq_step;
        self.seq_step = (step + 1) & 7;
        if step % 2 == 0 {
            self.sq[0].clock_length();
            self.sq[1].clock_length();
            self.wave.clock_length();
            self.noise.clock_length();
        }
        if step == 2 || step == 6 {
            self.sq[0].clock_sweep();
        }
        if step == 7 {
            self.sq[0].env.clock();
            self.sq[1].env.clock();
            self.noise.env.clock();
        }
    }

    fn mix(&mut self, io: &[u8]) {
        let cnt_l = io16(io, 0x80);
        let cnt_h = io16(io, 0x82);
        let (mut l, mut r) = (0i32, 0i32);
        if io[0x84] & 0x80 != 0 {
            let outs = [
                self.sq[0].output(io[0x62] >> 6) as i32,
                self.sq[1].output(io[0x68] >> 6) as i32,
                self.wave.output(io) as i32,
                self.noise.output() as i32,
            ];
            let (mut pl, mut pr) = (0i32, 0i32);
            for (i, o) in outs.iter().enumerate() {
                if cnt_l & (0x100 << i) != 0 {
                    pr += o;
                }
                if cnt_l & (0x1000 << i) != 0 {
                    pl += o;
                }
            }
            pr *= ((cnt_l & 7) + 1) as i32;
            pl *= (((cnt_l >> 4) & 7) + 1) as i32;
            let shift = match cnt_h & 3 {
                0 => 2,
                1 => 1,
                _ => 0,
            };
            l = pl >> shift;
            r = pr >> shift;
            for f in 0..2 {
                let full = cnt_h & (4 << f) != 0;
                let mut v = self.fifo[f].out as i32 * 4;
                if !full {
                    v >>= 1;
                }
                let (re, le) = if f == 0 { (0x100, 0x200) } else { (0x1000, 0x2000) };
                if cnt_h & re != 0 {
                    r += v;
                }
                if cnt_h & le != 0 {
                    l += v;
                }
            }
        }
        let x = [l as f32, r as f32];
        for c in 0..2 {
            // фильтр постоянной составляющей, как разделительный конденсатор в железе
            let y = x[c] - self.prev_in[c] + 0.998 * self.prev_out[c];
            self.prev_in[c] = x[c];
            self.prev_out[c] = y;
            self.samples.push((y / 768.0).clamp(-1.0, 1.0));
        }
        if self.samples.len() > MAX_QUEUED {
            self.samples.drain(..MAX_QUEUED / 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(io: &mut [u8], apu: &mut Apu, off: usize, v: u8) {
        io[off] = v;
        apu.write(io, off, v);
    }

    #[test]
    fn square_channel_produces_alternating_signal() {
        let mut io = vec![0u8; 0x400];
        let mut apu = Apu::new();
        set(&mut io, &mut apu, 0x84, 0x80); // мастер-включение
        set(&mut io, &mut apu, 0x80, 0x77); // громкость L/R = 7
        set(&mut io, &mut apu, 0x81, 0x11); // канал 1 в L и R
        set(&mut io, &mut apu, 0x82, 0x02); // PSG 100%
        set(&mut io, &mut apu, 0x62, 0x80); // скважность 50%
        set(&mut io, &mut apu, 0x63, 0xF0); // громкость 15
        set(&mut io, &mut apu, 0x64, 0xD0); // частота 2000
        set(&mut io, &mut apu, 0x65, 0x87); // запуск
        assert_eq!(apu.status() & 1, 1);
        for _ in 0..400 {
            apu.advance(512, &io);
        }
        let s = apu.take_samples();
        assert_eq!(s.len(), 800);
        let max = s.iter().cloned().fold(f32::MIN, f32::max);
        let min = s.iter().cloned().fold(f32::MAX, f32::min);
        assert!(max > 0.03 && min < -0.03, "max={max} min={min}");
    }

    #[test]
    fn silent_when_master_disabled() {
        let io = vec![0u8; 0x400];
        let mut apu = Apu::new();
        for _ in 0..100 {
            apu.advance(512, &io);
        }
        assert!(apu.take_samples().iter().all(|&x| x == 0.0));
    }

    #[test]
    fn length_counter_stops_channel() {
        let mut io = vec![0u8; 0x400];
        let mut apu = Apu::new();
        set(&mut io, &mut apu, 0x84, 0x80);
        set(&mut io, &mut apu, 0x62, 0x3F); // длина = 1 (64 - 63)
        set(&mut io, &mut apu, 0x63, 0xF0);
        set(&mut io, &mut apu, 0x64, 0x00);
        set(&mut io, &mut apu, 0x65, 0xC0); // запуск + использовать длину
        assert_eq!(apu.status() & 1, 1);
        for _ in 0..70 {
            apu.advance(512, &io); // > 32768 тактов: один такт секвенсора
        }
        assert_eq!(apu.status() & 1, 0);
    }

    #[test]
    fn fifo_pops_on_timer_and_requests_refill() {
        let mut io = vec![0u8; 0x400];
        let mut apu = Apu::new();
        for i in 0..40u8 {
            set(&mut io, &mut apu, 0xA0, i); // только 32 влезут
        }
        assert_eq!(apu.fifo_len(0), 32);
        io[0x83] = 0x00; // FIFO A тактируется таймером 0
        for _ in 0..15 {
            assert_eq!(apu.timer_overflow(0, &io), 0); // 32 -> 17
        }
        assert_eq!(apu.timer_overflow(0, &io), 1); // 16: просим DMA
        assert_eq!(apu.timer_overflow(1, &io), 0); // другой таймер не трогает FIFO
    }

    #[test]
    fn noise_and_wave_channels_trigger() {
        let mut io = vec![0u8; 0x400];
        let mut apu = Apu::new();
        set(&mut io, &mut apu, 0x84, 0x80);
        set(&mut io, &mut apu, 0x79, 0xF0);
        set(&mut io, &mut apu, 0x7D, 0x80);
        set(&mut io, &mut apu, 0x70, 0x80); // ЦАП волнового канала
        set(&mut io, &mut apu, 0x75, 0x80);
        assert_eq!(apu.status() & 0b1100, 0b1100);
        set(&mut io, &mut apu, 0x84, 0x00); // выключение питания гасит каналы
        assert_eq!(apu.status(), 0);
    }
}
