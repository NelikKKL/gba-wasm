//! EEPROM 512 Б / 8 КБ. Обмен идёт по одному биту за обращение (обычно через DMA3):
//! запрос чтения — `11` + адрес + `0`, запись — `10` + адрес + 64 бита данных + `0`.
//! Размер определяется по длине первой DMA-передачи.

use std::cell::Cell;

const READ_IDLE: usize = 68;

pub struct Eeprom {
    present: bool,
    mem: Vec<u8>,
    size: usize, // 0 = ещё не определён
    bits: Vec<u8>,
    read_data: Cell<u64>,
    read_pos: Cell<usize>, // 0..67 — идёт чтение; READ_IDLE — нет
    pub dirty: bool,
}

impl Eeprom {
    pub fn new(present: bool) -> Self {
        Self {
            present,
            mem: vec![0xFF; 8192],
            size: 0,
            bits: Vec::new(),
            read_data: Cell::new(0),
            read_pos: Cell::new(READ_IDLE),
            dirty: false,
        }
    }

    pub fn present(&self) -> bool {
        self.present
    }

    pub fn data(&self) -> &[u8] {
        &self.mem[..self.size]
    }

    pub fn load(&mut self, data: &[u8]) {
        let size = if data.len() >= 8192 {
            8192
        } else if data.len() >= 512 {
            512
        } else {
            return;
        };
        self.mem[..size].copy_from_slice(&data[..size]);
        self.size = size;
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn write_bit(&mut self, bit: u8) {
        if self.bits.len() < 128 {
            self.bits.push(bit & 1);
        }
    }

    /// Очередной бит чтения: 4 нулевых бита, затем 64 бита данных (старший первым).
    /// Вне чтения возвращает 1 («готов»).
    pub fn read_bit(&self) -> u16 {
        let pos = self.read_pos.get();
        if pos >= READ_IDLE {
            return 1;
        }
        self.read_pos.set(pos + 1);
        if pos < 4 {
            0
        } else {
            ((self.read_data.get() >> (63 - (pos - 4))) & 1) as u16
        }
    }

    /// Конец DMA-передачи в EEPROM из `count` бит: разбираем накопленную команду.
    pub fn dma_end(&mut self, count: usize) {
        let bits = std::mem::take(&mut self.bits);
        self.read_pos.set(READ_IDLE);
        if bits.len() < 3 || bits[0] != 1 {
            return;
        }
        let abits = match count {
            9 | 73 => 6,
            17 | 81 => 14,
            _ => return,
        };
        let reading = bits[1] == 1;
        let expected = if reading { 3 + abits } else { 3 + abits + 64 };
        if bits.len() != expected {
            return;
        }
        let size = if abits == 6 { 512 } else { 8192 };
        if self.size == 0 {
            self.size = size;
        } else if self.size != size {
            return;
        }
        let addr = bits[2..2 + abits].iter().fold(0usize, |a, &b| (a << 1) | b as usize);
        let off = addr * 8;
        if off + 8 > self.size {
            return;
        }
        if reading {
            let mut v = 0u64;
            for i in 0..8 {
                v = (v << 8) | self.mem[off + i] as u64;
            }
            self.read_data.set(v);
            self.read_pos.set(0);
        } else {
            for i in 0..8 {
                let mut byte = 0u8;
                for j in 0..8 {
                    byte = (byte << 1) | bits[2 + abits + i * 8 + j];
                }
                self.mem[off + i] = byte;
            }
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(e: &mut Eeprom, bits: &[u8]) {
        for &b in bits {
            e.write_bit(b);
        }
        e.dma_end(bits.len());
    }

    fn addr_bits(addr: usize, n: usize) -> Vec<u8> {
        (0..n).rev().map(|i| ((addr >> i) & 1) as u8).collect()
    }

    #[test]
    fn write_then_read_512_bytes() {
        let mut e = Eeprom::new(true);
        assert_eq!(e.data().len(), 0); // размер ещё неизвестен
        assert_eq!(e.read_bit(), 1); // готов

        let data: u64 = 0x0123_4567_89AB_CDEF;
        let mut w = vec![1, 0];
        w.extend(addr_bits(5, 6));
        w.extend((0..64).rev().map(|i| ((data >> i) & 1) as u8));
        w.push(0);
        send(&mut e, &w);
        assert_eq!(e.data().len(), 512);
        assert!(e.take_dirty());

        let mut r = vec![1, 1];
        r.extend(addr_bits(5, 6));
        r.push(0);
        send(&mut e, &r);
        let bits: Vec<u16> = (0..68).map(|_| e.read_bit()).collect();
        assert_eq!(&bits[..4], &[0, 0, 0, 0]);
        let got = bits[4..].iter().fold(0u64, |a, &b| (a << 1) | b as u64);
        assert_eq!(got, data);
        assert_eq!(e.read_bit(), 1); // после чтения снова «готов»
    }

    #[test]
    fn eight_kb_size_detected_by_length() {
        let mut e = Eeprom::new(true);
        let mut r = vec![1, 1];
        r.extend(addr_bits(300, 14));
        r.push(0);
        send(&mut e, &r); // 17 бит
        assert_eq!(e.data().len(), 8192);
        let bits: Vec<u16> = (0..68).map(|_| e.read_bit()).collect();
        assert!(bits[4..].iter().all(|&b| b == 1)); // чистая память = 0xFF
    }

    #[test]
    fn load_sets_size() {
        let mut e = Eeprom::new(true);
        e.load(&[0xAB; 512]);
        assert_eq!(e.data().len(), 512);
        assert_eq!(e.data()[7], 0xAB);
    }
}
