use crate::{bus::Bus, cpu::Cpu, ppu};

pub const CYCLES_PER_FRAME: u32 = 280_896;
const CYCLES_PER_LINE: u32 = 1232;
const HBLANK_START: u32 = 960;

pub struct Gba {
    pub cpu: Cpu,
    pub bus: Bus,
    pub fb: Vec<u8>,
    line_cycles: u32,
    vcount: u32,
    hblank: bool,
}

impl Gba {
    pub fn new() -> Self {
        Self {
            cpu: Cpu::new(),
            bus: Bus::new(),
            fb: vec![0; ppu::W * ppu::H * 4],
            line_cycles: 0,
            vcount: 0,
            hblank: false,
        }
    }

    pub fn load_rom(&mut self, rom: &[u8]) -> Result<String, String> {
        if rom.len() < 0xC0 {
            return Err("Файл слишком маленький для GBA ROM".into());
        }
        let title = String::from_utf8_lossy(&rom[0xA0..0xAC])
            .trim_matches('\0')
            .trim()
            .to_string();
        *self = Gba::new();
        self.bus.rom = rom.to_vec();
        Ok(if title.is_empty() { "(без названия)".into() } else { title })
    }

    pub fn run_frame(&mut self) {
        let mut cycles = 0;
        while cycles < CYCLES_PER_FRAME {
            let c = self.cpu.step(&mut self.bus);
            cycles += c;
            self.advance_video(c);
        }
        ppu::render_frame(&self.bus, &mut self.fb);
    }

    /// Минимальный видео-таймер: VCOUNT, DISPSTAT и IRQ VBlank/HBlank/VCount.
    /// Нужен, чтобы игры не зависали в ожидании кадра.
    fn advance_video(&mut self, cycles: u32) {
        self.line_cycles += cycles;
        let dispstat = self.bus.io[4];

        if !self.hblank && self.line_cycles >= HBLANK_START {
            self.hblank = true;
            if self.vcount < 160 && dispstat & 0x10 != 0 {
                self.bus.raise_irq(1 << 1);
            }
        }
        if self.line_cycles >= CYCLES_PER_LINE {
            self.line_cycles -= CYCLES_PER_LINE;
            self.hblank = false;
            self.vcount = (self.vcount + 1) % 228;
            if self.vcount == 160 && dispstat & 0x08 != 0 {
                self.bus.raise_irq(1 << 0);
            }
            if self.vcount == self.bus.io[5] as u32 && dispstat & 0x20 != 0 {
                self.bus.raise_irq(1 << 2);
            }
        }

        let vblank = (160..227).contains(&self.vcount);
        let lyc = self.vcount == self.bus.io[5] as u32;
        self.bus.io[4] = (dispstat & 0x38) | vblank as u8 | (self.hblank as u8) << 1 | (lyc as u8) << 2;
        self.bus.io[6] = self.vcount as u8;
    }
}
