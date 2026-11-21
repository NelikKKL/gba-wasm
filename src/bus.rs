use crate::apu::Apu;
use crate::eeprom::Eeprom;
use crate::save::{Save, SaveKind};

/// Карта памяти GBA. Все доступы идут через эту шину.
pub struct Bus {
    pub bios: Vec<u8>, // 16 KB (позже: HLE-замена BIOS)
    pub ewram: Vec<u8>, // 256 KB
    pub iwram: Vec<u8>, // 32 KB
    pub pram: Vec<u8>,  // 1 KB палитры
    pub vram: Vec<u8>,  // 96 KB
    pub oam: Vec<u8>,   // 1 KB
    pub io: Vec<u8>,    // 1 KB регистров
    pub rom: Vec<u8>,
    /// Нажатые кнопки, бит = 1 если нажата.
    pub keys: u16,
    /// CPU спит до прерывания (запись в HALTCNT).
    pub halted: bool,
    /// Сейчас исполняется код внутри BIOS (иначе чтение BIOS защищено).
    pub in_bios: bool,
    /// Последний «выбранный» опкод BIOS: его возвращает чтение BIOS из кода игры.
    pub bios_latch: u32,
    /// Записаны BG2X/Y или BG3X/Y: PPU перезагрузит внутренние опорные точки.
    pub affine_dirty: [bool; 2],
    /// Сохранение картриджа (SRAM/Flash).
    pub save: Save,
    pub eeprom: Eeprom,
    pub apu: Apu,
    dma_src: [u32; 4],
    dma_dst: [u32; 4],
    dma_cnt: [u32; 4],
    /// Циклы, потраченные DMA с прошлого опроса (CPU на это время стоит).
    pub dma_cycles: u32,
    tm_counter: [u32; 4],
    tm_reload: [u16; 4],
    tm_acc: [u32; 4],
}

fn vram_index(off: usize) -> usize {
    let o = off & 0x1_FFFF;
    if o >= 0x18000 { o - 0x8000 } else { o }
}

impl Bus {
    pub fn new() -> Self {
        let mut bus = Self {
            bios: vec![0; 0x4000],
            ewram: vec![0; 0x4_0000],
            iwram: vec![0; 0x8000],
            pram: vec![0; 0x400],
            vram: vec![0; 0x1_8000],
            oam: vec![0; 0x400],
            io: vec![0; 0x400],
            rom: Vec::new(),
            keys: 0,
            halted: false,
            in_bios: false,
            bios_latch: 0xE129_F000,
            affine_dirty: [false; 2],
            save: Save::new(SaveKind::None),
            eeprom: Eeprom::new(false),
            apu: Apu::new(),
            dma_src: [0; 4],
            dma_dst: [0; 4],
            dma_cnt: [0; 4],
            dma_cycles: 0,
            tm_counter: [0; 4],
            tm_reload: [0; 4],
            tm_acc: [0; 4],
        };
        // значения регистров после BIOS: единичные матрицы BG2/BG3
        for off in [0x20usize, 0x26, 0x30, 0x36] {
            bus.io[off + 1] = 0x01;
        }
        bus.install_stub_bios();
        bus
    }

    /// Минимальный BIOS: только обработчик IRQ (как в настоящем BIOS:
    /// сохраняет регистры и вызывает пользовательский обработчик из 0x03007FFC).
    /// TODO: заменить полноценным HLE.
    fn install_stub_bios(&mut self) {
        let words: [(usize, u32); 8] = [
            (0x018, 0xEA00_0042), // b    0x128
            (0x128, 0xE92D_500F), // stmfd sp!, {r0-r3, r12, lr}
            (0x12C, 0xE3A0_0301), // mov  r0, #0x04000000
            (0x130, 0xE28F_E000), // add  lr, pc, #0
            (0x134, 0xE510_F004), // ldr  pc, [r0, #-4]
            (0x138, 0xE8BD_500F), // ldmfd sp!, {r0-r3, r12, lr}
            (0x13C, 0xE25E_F004), // subs pc, lr, #4
            (0x144, 0xE55E_C002), // ldrb r12, [lr, #-2] — значение, которое видит игра после возврата из IRQ
        ];
        for (a, w) in words {
            self.bios[a..a + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    /// IE & IF != 0 — прерывание запрошено (будит из Halt даже при IME=0).
    pub fn irq_asserted(&self) -> bool {
        self.read16(0x0400_0200) & self.read16(0x0400_0202) & 0x3FFF != 0
    }

    pub fn ime(&self) -> bool {
        self.io[0x208] & 1 != 0
    }

    /// Выставляет биты в IF (для PPU, таймеров, DMA).
    pub fn raise_irq(&mut self, mask: u16) {
        let v = self.read16(0x0400_0202) | mask;
        self.io[0x202] = v as u8;
        self.io[0x203] = (v >> 8) as u8;
    }

    fn read_io(&self, off: usize) -> u8 {
        match off {
            // KEYINPUT: active-low, 0 = нажата
            0x130 => (!self.keys) as u8,
            0x131 => ((!self.keys >> 8) & 0x03) as u8,
            0x84 => (self.io[0x84] & 0x80) | self.apu.status(),
            0x90..=0x9F => self.apu.wave_read(&self.io, off),
            // TMxCNT_L при чтении отдаёт текущее значение счётчика
            0x100..=0x10F if (off & 3) < 2 => (self.tm_counter[(off - 0x100) / 4] >> (8 * (off & 1))) as u8,
            _ if off < self.io.len() => self.io[off],
            _ => 0,
        }
    }

    pub fn read8(&self, addr: u32) -> u8 {
        let off = (addr & 0x00FF_FFFF) as usize;
        match addr >> 24 {
            0x00 if off >= 0x4000 => 0,
            0x00 if self.in_bios => self.bios[off],
            0x00 => (self.bios_latch >> ((off & 3) * 8)) as u8, // защита чтения BIOS
            0x02 => self.ewram[off & 0x3_FFFF],
            0x03 => self.iwram[off & 0x7FFF],
            0x04 => self.read_io(off),
            0x05 => self.pram[off & 0x3FF],
            0x06 => self.vram[vram_index(off)],
            0x07 => self.oam[off & 0x3FF],
            0x08..=0x0D => {
                let o = (addr & 0x01FF_FFFF) as usize;
                match self.rom.get(o) {
                    Some(&b) => b,
                    None => {
                        // за концом ROM шина отдаёт адрес/2
                        let h = (o >> 1) as u16;
                        if o & 1 == 0 { h as u8 } else { (h >> 8) as u8 }
                    }
                }
            }
            0x0E | 0x0F => self.save.read(addr),
            _ => 0,
        }
    }

    /// Однобайтовая запись из кода (STRB): в PRAM/VRAM дублируется в полуслово,
    /// в OAM и в OBJ-часть VRAM игнорируется.
    pub fn write8(&mut self, addr: u32, v: u8) {
        let off = (addr & 0x00FF_FFFF) as usize;
        match addr >> 24 {
            0x05 => {
                let o = off & 0x3FE;
                self.pram[o] = v;
                self.pram[o + 1] = v;
            }
            0x06 => {
                let o = vram_index(off);
                let limit = if self.io[0] & 7 >= 3 { 0x14000 } else { 0x10000 };
                if o < limit {
                    let o = o & !1;
                    self.vram[o] = v;
                    self.vram[o + 1] = v;
                }
            }
            0x07 => {}
            _ => self.write8_raw(addr, v),
        }
    }

    /// Запись байта без особенностей шины (используется 16/32-битными записями).
    fn write8_raw(&mut self, addr: u32, v: u8) {
        let off = (addr & 0x00FF_FFFF) as usize;
        match addr >> 24 {
            0x02 => self.ewram[off & 0x3_FFFF] = v,
            0x03 => self.iwram[off & 0x7FFF] = v,
            0x04 if off < self.io.len() => match off {
                0x202 | 0x203 => self.io[off] &= !v, // IF: запись 1 сбрасывает бит
                0x301 => self.halted = true,         // HALTCNT
                0x28..=0x2F => {
                    self.io[off] = v;
                    self.affine_dirty[0] = true;
                }
                0x38..=0x3F => {
                    self.io[off] = v;
                    self.affine_dirty[1] = true;
                }
                // регистры звука (PSG, FIFO, управление)
                0x60..=0xA7 => {
                    self.io[off] = v;
                    self.apu.write(&self.io, off, v);
                }
                // таймеры: запись в CNT_L задаёт перезагружаемое значение,
                // включение (бит 7 CNT_H) загружает его в счётчик
                0x100..=0x10F => {
                    let ch = (off - 0x100) / 4;
                    let was_enabled = self.io[0x102 + ch * 4] & 0x80 != 0;
                    self.io[off] = v;
                    match (off - 0x100) % 4 {
                        0 | 1 => self.tm_reload[ch] = self.io16_raw(0x100 + ch * 4),
                        2 => {
                            if v & 0x80 != 0 && !was_enabled {
                                self.tm_counter[ch] = self.tm_reload[ch] as u32;
                                self.tm_acc[ch] = 0;
                            }
                        }
                        _ => {}
                    }
                }
                // старший байт DMAxCNT_H: бит 7 — включение канала
                0xBB | 0xC7 | 0xD3 | 0xDF => {
                    let ch = (off - 0xB0) / 12;
                    let was_enabled = self.io[off] & 0x80 != 0;
                    self.io[off] = v;
                    if v & 0x80 != 0 && !was_enabled {
                        self.dma_enable(ch);
                    }
                }
                _ => self.io[off] = v,
            },
            0x05 => self.pram[off & 0x3FF] = v,
            0x06 => self.vram[vram_index(off)] = v,
            0x07 => self.oam[off & 0x3FF] = v,
            0x0E | 0x0F => self.save.write(addr, v),
            _ => {}
        }
    }

    /// Обращение попадает в окно EEPROM (0x0D...; для ROM > 16 МБ — только последние 256 байт).
    fn eeprom_hit(&self, addr: u32) -> bool {
        self.eeprom.present()
            && addr >> 24 == 0x0D
            && (self.rom.len() <= 0x0100_0000 || (addr & 0x01FF_FFFF) >= 0x01FF_FF00)
    }

    pub fn read16(&self, addr: u32) -> u16 {
        if self.eeprom_hit(addr) {
            return self.eeprom.read_bit();
        }
        if matches!(addr >> 24, 0x0E | 0x0F) {
            return self.read8(addr) as u16 * 0x0101; // 8-битная шина сохранений
        }
        let a = addr & !1;
        self.read8(a) as u16 | (self.read8(a.wrapping_add(1)) as u16) << 8
    }

    pub fn read32(&self, addr: u32) -> u32 {
        if matches!(addr >> 24, 0x0E | 0x0F) {
            return self.read8(addr) as u32 * 0x0101_0101;
        }
        let a = addr & !3;
        self.read16(a) as u32 | (self.read16(a.wrapping_add(2)) as u32) << 16
    }

    pub fn write16(&mut self, addr: u32, v: u16) {
        if self.eeprom_hit(addr) {
            self.eeprom.write_bit((v & 1) as u8);
            return;
        }
        if matches!(addr >> 24, 0x0E | 0x0F) {
            // на 8-битную шину уходит один байт, повёрнутый по младшим битам адреса
            self.write8_raw(addr, (v >> ((addr & 1) * 8)) as u8);
            return;
        }
        let a = addr & !1;
        self.write8_raw(a, v as u8);
        self.write8_raw(a.wrapping_add(1), (v >> 8) as u8);
    }

    pub fn write32(&mut self, addr: u32, v: u32) {
        if matches!(addr >> 24, 0x0E | 0x0F) {
            self.write8_raw(addr, (v >> ((addr & 3) * 8)) as u8);
            return;
        }
        let a = addr & !3;
        self.write16(a, v as u16);
        self.write16(a.wrapping_add(2), (v >> 16) as u16);
    }

    // ---------- DMA ----------

    fn io16_raw(&self, off: usize) -> u16 {
        self.io[off] as u16 | (self.io[off + 1] as u16) << 8
    }

    fn io32_raw(&self, off: usize) -> u32 {
        self.io16_raw(off) as u32 | (self.io16_raw(off + 2) as u32) << 16
    }

    fn dma_addr_mask(ch: usize) -> u32 {
        if ch == 3 { 0x0FFF_FFFF } else { 0x07FF_FFFF }
    }

    fn dma_count(&self, ch: usize) -> u32 {
        let max_mask = if ch == 3 { 0xFFFF } else { 0x3FFF };
        let n = self.io16_raw(0xB0 + ch * 12 + 8) as u32 & max_mask;
        if n == 0 {
            if ch == 3 { 0x1_0000 } else { 0x4000 }
        } else {
            n
        }
    }

    /// Канал включили: запоминаем адреса и счётчик; немедленный DMA стартует сразу.
    fn dma_enable(&mut self, ch: usize) {
        let base = 0xB0 + ch * 12;
        let mask = Self::dma_addr_mask(ch);
        self.dma_src[ch] = self.io32_raw(base) & mask;
        self.dma_dst[ch] = self.io32_raw(base + 4) & mask;
        self.dma_cnt[ch] = self.dma_count(ch);
        if (self.io16_raw(base + 10) >> 12) & 3 == 0 {
            self.dma_run(ch);
        }
    }

    /// timing: 1 = VBlank, 2 = HBlank.
    pub fn dma_trigger(&mut self, timing: u16) {
        for ch in 0..4 {
            let cnth = self.io16_raw(0xB0 + ch * 12 + 10);
            if cnth & 0x8000 != 0 && (cnth >> 12) & 3 == timing {
                self.dma_run(ch);
            }
        }
    }

    fn dma_run(&mut self, ch: usize) {
        let base = 0xB0 + ch * 12;
        let cnth = self.io16_raw(base + 10);
        let timing = (cnth >> 12) & 3;
        // DMA1/DMA2 в режиме «special» — подкачка звуковых FIFO: 4 слова, адрес приёмника фиксирован
        let fifo = timing == 3 && (ch == 1 || ch == 2);
        let word = fifo || cnth & 0x400 != 0;
        let step: u32 = if word { 4 } else { 2 };
        let dest_ctl = if fifo { 2 } else { (cnth >> 5) & 3 };
        let src_ctl = (cnth >> 7) & 3;
        let (mut src, mut dst) = (self.dma_src[ch], self.dma_dst[ch]);
        let dst_start = dst;
        let count = if fifo { 4 } else { self.dma_cnt[ch] };
        for _ in 0..count {
            if word {
                let v = self.read32(src);
                self.write32(dst, v);
            } else {
                let v = self.read16(src);
                self.write16(dst, v);
            }
            src = match src_ctl {
                1 => src.wrapping_sub(step),
                2 => src,
                _ => src.wrapping_add(step),
            };
            dst = match dest_ctl {
                1 => dst.wrapping_sub(step),
                2 => dst,
                _ => dst.wrapping_add(step),
            };
        }
        self.dma_cycles += 2 * count + 2;
        self.dma_src[ch] = src;
        self.dma_dst[ch] = dst;
        if self.eeprom_hit(dst_start) {
            self.eeprom.dma_end(count as usize); // передача в EEPROM закончилась
        }

        if cnth & 0x200 != 0 && timing != 0 {
            // повтор: счётчик (и адрес назначения при режиме 3) перезагружаются
            self.dma_cnt[ch] = self.dma_count(ch);
            if dest_ctl == 3 {
                self.dma_dst[ch] = self.io32_raw(base + 4) & Self::dma_addr_mask(ch);
            }
        } else {
            self.io[base + 11] &= 0x7F; // канал выключается
        }
        if cnth & 0x4000 != 0 {
            self.raise_irq(1u16 << (8 + ch));
        }
    }

    /// Звуковому FIFO нужны данные: запускаем DMA1/DMA2, который пишет в его регистр.
    fn dma_fifo(&mut self, fifo: usize) {
        let addr = 0x0400_00A0 + 4 * fifo as u32;
        for ch in 1..3 {
            let cnth = self.io16_raw(0xB0 + ch * 12 + 10);
            if cnth & 0x8000 != 0 && (cnth >> 12) & 3 == 3 && self.dma_dst[ch] & 0x0FFF_FFFC == addr {
                self.dma_run(ch);
            }
        }
    }

    // ---------- Таймеры и звук ----------

    /// Продвигает таймеры и APU на `cycles` тактов CPU.
    pub fn tick(&mut self, cycles: u32) {
        for ch in 0..4 {
            let ctrl = self.io[0x102 + ch * 4];
            if ctrl & 0x80 == 0 || (ctrl & 4 != 0 && ch > 0) {
                continue; // выключен или считает по переполнению соседа
            }
            let shift = [0u32, 6, 8, 10][(ctrl & 3) as usize];
            self.tm_acc[ch] += cycles;
            let ticks = self.tm_acc[ch] >> shift;
            self.tm_acc[ch] &= (1 << shift) - 1;
            if ticks > 0 {
                self.timer_advance(ch, ticks);
            }
        }
        self.apu.advance(cycles, &self.io);
    }

    fn timer_advance(&mut self, ch: usize, mut ticks: u32) {
        while ticks > 0 {
            let remaining = 0x1_0000 - self.tm_counter[ch];
            if ticks < remaining {
                self.tm_counter[ch] += ticks;
                break;
            }
            ticks -= remaining;
            self.tm_counter[ch] = self.tm_reload[ch] as u32;
            self.timer_overflow(ch);
        }
    }

    fn timer_overflow(&mut self, ch: usize) {
        let ctrl = self.io[0x102 + ch * 4];
        if ctrl & 0x40 != 0 {
            self.raise_irq(1u16 << (3 + ch));
        }
        if ch < 2 {
            let need = self.apu.timer_overflow(ch, &self.io);
            for f in 0..2 {
                if need & (1 << f) != 0 {
                    self.dma_fifo(f);
                }
            }
        }
        if ch < 3 {
            let next = self.io[0x102 + (ch + 1) * 4];
            if next & 0x80 != 0 && next & 4 != 0 {
                self.timer_advance(ch + 1, 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_store_quirks() {
        let mut b = Bus::new();
        b.write8(0x0500_0020, 1);
        assert_eq!(b.read16(0x0500_0020), 0x0101); // палитра: байт дублируется
        b.write8(0x0600_0010, 2);
        assert_eq!(b.read16(0x0600_0010), 0x0202); // VRAM (BG-часть)
        b.write8(0x0601_0010, 3);
        assert_eq!(b.read16(0x0601_0010), 0); // OBJ-часть в режиме 0 игнорируется
        b.write8(0x0700_0010, 1);
        assert_eq!(b.read16(0x0700_0010), 0); // OAM
        b.write16(0x0700_0010, 0x1234);
        assert_eq!(b.read16(0x0700_0010), 0x1234); // 16-битные записи работают
        b.write16(0x0400_0000, 3); // bitmap-режим: граница 0x14000
        b.write8(0x0601_4010, 5);
        assert_eq!(b.read16(0x0601_4010), 0);
        b.write8(0x0601_3010, 5);
        assert_eq!(b.read16(0x0601_3010), 0x0505);
    }

    #[test]
    fn sram_bus_is_8bit() {
        let mut b = Bus::new();
        b.save = Save::new(SaveKind::Sram);
        b.write8(0x0E00_0000, 1);
        assert_eq!(b.read16(0x0E00_0000), 0x0101);
        assert_eq!(b.read32(0x0E00_0000), 0x0101_0101);
        b.write16(0x0E00_0010, 0xAABB);
        assert_eq!((b.read8(0x0E00_0010), b.read8(0x0E00_0011)), (0xBB, 0xFF));
        b.write16(0x0E00_0021, 0xAABB);
        assert_eq!(b.read8(0x0E00_0021), 0xAA);
        b.write32(0x0E00_0032, 0xAABB_CCDD);
        assert_eq!(b.read8(0x0E00_0032), 0xBB);
    }

    #[test]
    fn rom_beyond_end_returns_address_pattern() {
        let mut b = Bus::new();
        b.rom = vec![0; 16];
        assert_eq!(b.read8(0x0800_0100), 0x80);
        assert_eq!(b.read8(0x0800_0101), 0x00);
    }

    #[test]
    fn dma_immediate_and_hblank_repeat() {
        let mut b = Bus::new();
        b.write32(0x0200_0000, 0x1122_3344);
        b.write32(0x0200_0004, 0x5566_7788);
        b.write32(0x0400_00B0, 0x0200_0000); // SAD
        b.write32(0x0400_00B4, 0x0200_0100); // DAD
        b.write32(0x0400_00B8, 2 | (0x8400 << 16)); // 2 слова, 32 бита, старт сразу
        assert_eq!(b.read32(0x0200_0100), 0x1122_3344);
        assert_eq!(b.read32(0x0200_0104), 0x5566_7788);
        assert_eq!(b.io[0xBB] & 0x80, 0); // канал выключился
        assert!(b.dma_cycles > 0);

        // HBlank + повтор: ждёт триггера и остаётся включённым
        b.write32(0x0400_00B0, 0x0200_0000);
        b.write32(0x0400_00B4, 0x0200_0200);
        b.write32(0x0400_00B8, 1 | (0xA600 << 16));
        assert_eq!(b.read32(0x0200_0200), 0);
        b.dma_trigger(2);
        assert_eq!(b.read32(0x0200_0200), 0x1122_3344);
        assert_ne!(b.io[0xBB] & 0x80, 0);
    }

    #[test]
    fn timer_counts_prescales_and_cascades() {
        let mut b = Bus::new();
        b.write16(0x0400_0100, 0xFFFE); // TM0: переполнение через 2 такта
        b.write16(0x0400_0102, 0x00C0); // включён, IRQ, делитель 1
        b.write16(0x0400_0104, 0xFFFF); // TM1: перезагрузка 0xFFFF
        b.write16(0x0400_0106, 0x00C4); // включён, IRQ, каскад от TM0
        b.tick(1);
        assert_eq!(b.read16(0x0400_0100), 0xFFFF);
        b.tick(1); // TM0 переполняется -> IRQ, TM1 получает +1 и тоже переполняется
        assert_eq!(b.read16(0x0400_0202) & 0x18, 0x18);
        assert_eq!(b.read16(0x0400_0100), 0xFFFE); // перезагрузился
        assert_eq!(b.read16(0x0400_0104), 0xFFFF);

        // делитель 64
        b.write16(0x0400_0108, 0x0000);
        b.write16(0x0400_010A, 0x0081);
        b.tick(63);
        assert_eq!(b.read16(0x0400_0108), 0);
        b.tick(1);
        assert_eq!(b.read16(0x0400_0108), 1);
    }

    #[test]
    fn sound_fifo_dma_refills_on_timer_overflow() {
        let mut b = Bus::new();
        for i in 0..16u32 {
            b.write32(0x0200_0000 + i * 4, 0x4040_4040);
        }
        b.write16(0x0400_0084, 0x0080); // звук включён
        b.write16(0x0400_0082, 0x0304); // FIFO A: 100%, L+R, таймер 0
        b.write16(0x0400_0100, 0xFFF0); // TM0: переполнение через 16 тактов
        b.write16(0x0400_0102, 0x0080);
        b.write32(0x0400_00BC, 0x0200_0000); // DMA1: источник
        b.write32(0x0400_00C0, 0x0400_00A0); // приёмник = FIFO A
        b.write32(0x0400_00C4, 4 | (0xB640 << 16)); // special, повтор, 32 бита, приёмник фиксирован
        assert_eq!(b.apu.fifo_len(0), 0);
        b.tick(16);
        assert_eq!(b.apu.fifo_len(0), 16); // DMA подкачал 4 слова
        assert_eq!(b.dma_src[1], 0x0200_0010);
        for _ in 0..3 {
            b.tick(16);
        }
        assert!(b.apu.fifo_len(0) > 16);
    }

    #[test]
    fn eeprom_window_and_rom_size_rule() {
        let mut b = Bus::new();
        b.eeprom = Eeprom::new(true);
        b.rom = vec![0; 0x100];
        assert!(b.eeprom_hit(0x0D00_0000)); // ROM <= 16 МБ: всё окно
        assert_eq!(b.read16(0x0D00_0000), 1); // готов
        b.rom = vec![0; 0x0200_0000];
        assert!(!b.eeprom_hit(0x0D00_0000)); // ROM 32 МБ: только последние 256 байт
        assert!(b.eeprom_hit(0x0DFF_FF00));
    }

    #[test]
    fn keyinput_is_active_low() {
        let mut b = Bus::new();
        assert_eq!(b.read16(0x0400_0130), 0x03FF);
        b.keys = 1; // A
        assert_eq!(b.read16(0x0400_0130), 0x03FE);
    }
}
