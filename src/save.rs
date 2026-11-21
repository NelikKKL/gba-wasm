//! Сохранения картриджа: SRAM (32 КБ), Flash 64 КБ и 128 КБ.
//! EEPROM пока не поддерживается (читается как «нет сохранения»).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaveKind {
    None,
    Sram,
    Flash64,
    Flash128,
    /// EEPROM живёт в отдельном модуле (`eeprom.rs`); здесь только для детекта.
    Eeprom,
}

pub struct Save {
    kind: SaveKind,
    mem: Vec<u8>,
    /// 0 ждём 0xAA, 1 ждём 0x55, 2 ждём команду, 3 запись байта, 4 выбор банка
    state: u8,
    id_mode: bool,
    erase_armed: bool,
    bank: usize,
    pub dirty: bool,
}

impl Save {
    pub fn new(kind: SaveKind) -> Self {
        let size = match kind {
            SaveKind::None | SaveKind::Eeprom => 0,
            SaveKind::Sram => 0x8000,
            SaveKind::Flash64 => 0x1_0000,
            SaveKind::Flash128 => 0x2_0000,
        };
        Self {
            kind,
            mem: vec![0xFF; size],
            state: 0,
            id_mode: false,
            erase_armed: false,
            bank: 0,
            dirty: false,
        }
    }

    /// Ищет строку библиотеки сохранений в ROM (они лежат по адресам, кратным 4).
    pub fn detect(rom: &[u8]) -> SaveKind {
        let sigs: [(&[u8], SaveKind); 6] = [
            (&b"FLASH1M_V"[..], SaveKind::Flash128),
            (&b"FLASH512_V"[..], SaveKind::Flash64),
            (&b"FLASH_V"[..], SaveKind::Flash64),
            (&b"SRAM_F_V"[..], SaveKind::Sram),
            (&b"SRAM_V"[..], SaveKind::Sram),
            (&b"EEPROM_V"[..], SaveKind::Eeprom),
        ];
        for (sig, kind) in sigs {
            let mut i = 0;
            while i + sig.len() <= rom.len() {
                if &rom[i..i + sig.len()] == sig {
                    return kind;
                }
                i += 4;
            }
        }
        SaveKind::None
    }

    pub fn kind(&self) -> SaveKind {
        self.kind
    }

    pub fn data(&self) -> &[u8] {
        &self.mem
    }

    pub fn load(&mut self, data: &[u8]) {
        let n = data.len().min(self.mem.len());
        self.mem[..n].copy_from_slice(&data[..n]);
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn read(&self, addr: u32) -> u8 {
        match self.kind {
            SaveKind::None | SaveKind::Eeprom => 0xFF,
            SaveKind::Sram => self.mem[addr as usize & 0x7FFF],
            _ => {
                let o = addr as usize & 0xFFFF;
                if self.id_mode && o < 2 {
                    let id = if self.kind == SaveKind::Flash128 { [0x62, 0x13] } else { [0x32, 0x1B] };
                    return id[o];
                }
                self.mem[self.bank * 0x10000 + o]
            }
        }
    }

    pub fn write(&mut self, addr: u32, v: u8) {
        match self.kind {
            SaveKind::None | SaveKind::Eeprom => {}
            SaveKind::Sram => {
                self.mem[addr as usize & 0x7FFF] = v;
                self.dirty = true;
            }
            _ => self.flash_write(addr as usize & 0xFFFF, v),
        }
    }

    fn flash_write(&mut self, o: usize, v: u8) {
        let base = self.bank * 0x10000;
        match self.state {
            3 => {
                // программирование: флеш может только сбрасывать биты
                self.mem[base + o] &= v;
                self.state = 0;
                self.dirty = true;
            }
            4 => {
                if o == 0 && self.kind == SaveKind::Flash128 {
                    self.bank = (v & 1) as usize;
                }
                self.state = 0;
            }
            0 => {
                if o == 0x5555 && v == 0xAA {
                    self.state = 1;
                }
            }
            1 => {
                self.state = if o == 0x2AAA && v == 0x55 { 2 } else { 0 };
            }
            _ => {
                self.state = 0;
                if o == 0x5555 {
                    match v {
                        0x10 if self.erase_armed => {
                            self.mem.fill(0xFF);
                            self.erase_armed = false;
                            self.dirty = true;
                        }
                        0x90 => self.id_mode = true,
                        0xF0 => self.id_mode = false,
                        0x80 => self.erase_armed = true,
                        0xA0 => self.state = 3,
                        0xB0 if self.kind == SaveKind::Flash128 => self.state = 4,
                        _ => {}
                    }
                } else if self.erase_armed && v == 0x30 {
                    let a = base + (o & 0xF000);
                    self.mem[a..a + 0x1000].fill(0xFF);
                    self.erase_armed = false;
                    self.dirty = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &mut Save, c: u8) {
        s.write(0x0E00_5555, 0xAA);
        s.write(0x0E00_2AAA, 0x55);
        s.write(0x0E00_5555, c);
    }

    #[test]
    fn detect_save_types() {
        let mut rom = vec![0u8; 64];
        assert_eq!(Save::detect(&rom), SaveKind::None);
        rom[16..25].copy_from_slice(b"FLASH1M_V");
        assert_eq!(Save::detect(&rom), SaveKind::Flash128);
        let mut rom = vec![0u8; 64];
        rom[8..14].copy_from_slice(b"SRAM_V");
        assert_eq!(Save::detect(&rom), SaveKind::Sram);
        let mut rom = vec![0u8; 64];
        rom[4..12].copy_from_slice(b"EEPROM_V");
        assert_eq!(Save::detect(&rom), SaveKind::Eeprom);
    }

    #[test]
    fn sram_is_ff_and_mirrored() {
        let mut s = Save::new(SaveKind::Sram);
        assert_eq!(s.read(0x0E00_0000), 0xFF);
        s.write(0x0E00_0010, 7);
        assert_eq!(s.read(0x0E01_0010), 7); // зеркало 64 КБ
        assert_eq!(s.read(0x0F00_0010), 7); // зеркало через 16 МБ
        assert!(s.take_dirty());
    }

    #[test]
    fn flash_program_and_erase() {
        let mut s = Save::new(SaveKind::Flash64);
        assert_eq!(s.read(0x0E00_0010), 0xFF);
        cmd(&mut s, 0xA0);
        s.write(0x0E00_0010, 0x12);
        assert_eq!(s.read(0x0E00_0010), 0x12);
        cmd(&mut s, 0xA0);
        s.write(0x0E00_0010, 0x0F);
        assert_eq!(s.read(0x0E00_0010), 0x02); // 0x12 & 0x0F

        // сектор: стираем только первые 4 КБ
        cmd(&mut s, 0xA0);
        s.write(0x0E00_1000, 0x55);
        cmd(&mut s, 0x80);
        s.write(0x0E00_5555, 0xAA);
        s.write(0x0E00_2AAA, 0x55);
        s.write(0x0E00_0000, 0x30);
        assert_eq!(s.read(0x0E00_0010), 0xFF);
        assert_eq!(s.read(0x0E00_1000), 0x55);

        // весь чип
        cmd(&mut s, 0x80);
        cmd(&mut s, 0x10);
        assert_eq!(s.read(0x0E00_1000), 0xFF);
    }

    #[test]
    fn flash_id_and_banks() {
        let mut s = Save::new(SaveKind::Flash128);
        cmd(&mut s, 0x90);
        assert_eq!((s.read(0x0E00_0000), s.read(0x0E00_0001)), (0x62, 0x13));
        cmd(&mut s, 0xF0);
        assert_eq!(s.read(0x0E00_0000), 0xFF);

        cmd(&mut s, 0xA0);
        s.write(0x0E00_0020, 1);
        cmd(&mut s, 0xB0);
        s.write(0x0E00_0000, 1); // банк 1
        assert_eq!(s.read(0x0E00_0020), 0xFF);
        cmd(&mut s, 0xB0);
        s.write(0x0E00_0000, 0); // банк 0
        assert_eq!(s.read(0x0E00_0020), 1);
    }
}
