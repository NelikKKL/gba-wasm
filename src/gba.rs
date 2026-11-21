use crate::{
    bus::Bus,
    cpu::Cpu,
    eeprom::Eeprom,
    ppu::{self, Ppu},
    save::{Save, SaveKind},
};

pub const CYCLES_PER_FRAME: u32 = 280_896;
const CYCLES_PER_LINE: u32 = 1232;
const HBLANK_START: u32 = 960;

pub struct Gba {
    pub cpu: Cpu,
    pub bus: Bus,
    pub fb: Vec<u8>,
    ppu: Ppu,
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
            ppu: Ppu::new(),
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
        let kind = Save::detect(rom);
        self.bus.eeprom = Eeprom::new(kind == SaveKind::Eeprom);
        self.bus.save = Save::new(if kind == SaveKind::Eeprom { SaveKind::None } else { kind });
        Ok(if title.is_empty() { "(без названия)".into() } else { title })
    }

    pub fn save_data(&self) -> &[u8] {
        if self.bus.eeprom.present() { self.bus.eeprom.data() } else { self.bus.save.data() }
    }

    pub fn load_save(&mut self, data: &[u8]) {
        if self.bus.eeprom.present() {
            self.bus.eeprom.load(data);
        } else {
            self.bus.save.load(data);
        }
    }

    /// true, если с прошлого вызова сохранение изменилось.
    pub fn take_save_dirty(&mut self) -> bool {
        if self.bus.eeprom.present() { self.bus.eeprom.take_dirty() } else { self.bus.save.take_dirty() }
    }

    /// Звук, накопленный с прошлого вызова: стерео f32, чередование L, R, 32768 Гц.
    pub fn take_audio(&mut self) -> Vec<f32> {
        self.bus.apu.take_samples()
    }

    pub fn run_frame(&mut self) {
        let mut cycles = 0;
        while cycles < CYCLES_PER_FRAME {
            // DMA останавливает CPU на время передачи
            let c = self.cpu.step(&mut self.bus) + std::mem::take(&mut self.bus.dma_cycles);
            cycles += c;
            self.bus.tick(c); // таймеры и звук
            self.advance_video(c);
        }
    }

    /// Минимальный видео-таймер: VCOUNT, DISPSTAT и IRQ VBlank/HBlank/VCount.
    /// Нужен, чтобы игры не зависали в ожидании кадра.
    fn advance_video(&mut self, cycles: u32) {
        self.line_cycles += cycles;
        let dispstat = self.bus.io[4];

        if !self.hblank && self.line_cycles >= HBLANK_START {
            self.hblank = true;
            if self.vcount < 160 {
                self.ppu.render_line(&mut self.bus, self.vcount as usize, &mut self.fb);
                self.bus.dma_trigger(2); // HBlank-DMA
                if dispstat & 0x10 != 0 {
                    self.bus.raise_irq(1 << 1);
                }
            }
        }
        if self.line_cycles >= CYCLES_PER_LINE {
            self.line_cycles -= CYCLES_PER_LINE;
            self.hblank = false;
            self.vcount = (self.vcount + 1) % 228;
            if self.vcount == 160 {
                self.bus.dma_trigger(1); // VBlank-DMA
                if dispstat & 0x08 != 0 {
                    self.bus.raise_irq(1 << 0);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Экран "All tests passed" содержит ровно 270 тёмных пикселей
    /// (эталон получен из референсной модели; экран с "Failed test N" даёт другое число).
    const PASS_DARK_PIXELS: usize = 270;

    fn run_rom(rom: &[u8], frames: usize) -> Gba {
        let mut g = Gba::new();
        g.load_rom(rom).unwrap();
        for _ in 0..frames {
            g.run_frame();
        }
        g
    }

    fn dark_pixels(g: &Gba) -> usize {
        g.fb.chunks(4).filter(|p| p[0] < 128).count()
    }

    fn px(g: &Gba, x: usize, y: usize) -> [u8; 3] {
        let i = (y * ppu::W + x) * 4;
        [g.fb[i], g.fb[i + 1], g.fb[i + 2]]
    }

    macro_rules! passes {
        ($name:ident, $rom:expr, $frames:expr) => {
            #[test]
            fn $name() {
                let g = run_rom(include_bytes!($rom), $frames);
                assert_eq!(
                    dark_pixels(&g),
                    PASS_DARK_PIXELS,
                    "{}: r12 = {}",
                    $rom,
                    g.cpu.r[12]
                );
            }
        };
    }

    passes!(jsmolka_arm_tests_pass, "../tests/roms/arm.gba", 60);
    passes!(jsmolka_thumb_tests_pass, "../tests/roms/thumb.gba", 60);
    passes!(jsmolka_bios_tests_pass, "../tests/roms/bios.gba", 60);
    passes!(jsmolka_memory_tests_pass, "../tests/roms/memory.gba", 60);
    passes!(jsmolka_nes_tests_pass, "../tests/roms/nes.gba", 60);
    passes!(jsmolka_unsafe_tests_pass, "../tests/roms/unsafe.gba", 60);
    passes!(jsmolka_save_none_pass, "../tests/roms/save_none.gba", 60);
    passes!(jsmolka_save_sram_pass, "../tests/roms/save_sram.gba", 60);
    passes!(jsmolka_save_flash64_pass, "../tests/roms/save_flash64.gba", 150);
    passes!(jsmolka_save_flash128_pass, "../tests/roms/save_flash128.gba", 150);

    #[test]
    fn ppu_stripes_alternate_tiles() {
        let g = run_rom(include_bytes!("../tests/roms/ppu_stripes.gba"), 12);
        assert_eq!(px(&g, 0, 0), [90, 132, 173]);
        assert_eq!(px(&g, 8, 0), [132, 165, 198]);
        assert_eq!(px(&g, 16, 0), [90, 132, 173]);
        assert_eq!(px(&g, 239, 159), [132, 165, 198]);
    }

    #[test]
    fn ppu_shades_gradient() {
        let g = run_rom(include_bytes!("../tests/roms/ppu_shades.gba"), 12);
        assert_eq!(px(&g, 0, 0), [0, 0, 0]);
        assert_eq!(px(&g, 16, 0), [0, 0, 16]);
        assert_eq!(px(&g, 100, 0), [0, 0, 99]);
        assert_eq!(px(&g, 232, 159), [0, 0, 231]);
    }

    #[test]
    fn ppu_hello_world_text() {
        let g = run_rom(include_bytes!("../tests/roms/ppu_hello.gba"), 12);
        let white = g.fb.chunks(4).filter(|p| p[0] == 255 && p[1] == 255 && p[2] == 255).count();
        assert_eq!(white, 209);
        assert_eq!(px(&g, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn psg_square_rom_makes_sound_and_silent_rom_does_not() {
        let mut g = run_rom(include_bytes!("../tests/roms/psg_square.gba"), 10);
        let a = g.take_audio();
        assert!(a.len() > 2000, "len = {}", a.len());
        let peak = a.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(peak > 0.05, "peak = {peak}");

        let mut quiet = run_rom(include_bytes!("../tests/roms/arm.gba"), 10);
        assert!(quiet.take_audio().iter().all(|&x| x == 0.0));
    }

    #[test]
    fn eeprom_rom_detected() {
        // сигнатура EEPROM_V в ROM включает окно EEPROM и пустое сохранение до первого обращения
        let mut rom = vec![0u8; 0x400];
        rom[0x100..0x108].copy_from_slice(b"EEPROM_V");
        let mut g = Gba::new();
        g.load_rom(&rom).unwrap();
        assert!(g.bus.eeprom.present());
        assert_eq!(g.save_data().len(), 0);
        g.load_save(&[0x12; 512]);
        assert_eq!(g.save_data().len(), 512);
    }

    #[test]
    fn save_type_detected_and_exported() {
        let mut g = run_rom(include_bytes!("../tests/roms/save_sram.gba"), 60);
        assert_eq!(g.save_data().len(), 0x8000);
        assert!(g.take_save_dirty());
        assert!(!g.take_save_dirty());
        let none = run_rom(include_bytes!("../tests/roms/save_none.gba"), 1);
        assert_eq!(none.save_data().len(), 0);
    }
}
