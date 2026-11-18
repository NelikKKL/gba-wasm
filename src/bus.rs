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
        };
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
            0x08..=0x0D => self
                .rom
                .get((addr & 0x01FF_FFFF) as usize)
                .copied()
                .unwrap_or(0xFF),
            _ => 0,
        }
    }

    pub fn write8(&mut self, addr: u32, v: u8) {
        let off = (addr & 0x00FF_FFFF) as usize;
        match addr >> 24 {
            0x02 => self.ewram[off & 0x3_FFFF] = v,
            0x03 => self.iwram[off & 0x7FFF] = v,
            0x04 if off < self.io.len() => match off {
                0x202 | 0x203 => self.io[off] &= !v, // IF: запись 1 сбрасывает бит
                0x301 => self.halted = true,         // HALTCNT
                _ => self.io[off] = v,
            },
            // TODO: 8-битная запись в PRAM/VRAM дублирует байт, в OAM игнорируется
            0x05 => self.pram[off & 0x3FF] = v,
            0x06 => self.vram[vram_index(off)] = v,
            0x07 => self.oam[off & 0x3FF] = v,
            _ => {}
        }
    }

    pub fn read16(&self, addr: u32) -> u16 {
        let a = addr & !1;
        self.read8(a) as u16 | (self.read8(a + 1) as u16) << 8
    }

    pub fn read32(&self, addr: u32) -> u32 {
        let a = addr & !3;
        self.read16(a) as u32 | (self.read16(a + 2) as u32) << 16
    }

    pub fn write16(&mut self, addr: u32, v: u16) {
        let a = addr & !1;
        self.write8(a, v as u8);
        self.write8(a + 1, (v >> 8) as u8);
    }

    pub fn write32(&mut self, addr: u32, v: u32) {
        let a = addr & !3;
        self.write16(a, v as u16);
        self.write16(a + 2, (v >> 16) as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyinput_is_active_low() {
        let mut b = Bus::new();
        assert_eq!(b.read16(0x0400_0130), 0x03FF);
        b.keys = 1; // A
        assert_eq!(b.read16(0x0400_0130), 0x03FE);
    }
}
