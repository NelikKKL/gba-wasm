//! HLE BIOS: функции вызываются через SWI и выполняются мгновенно на стороне эмулятора.
//! Звуковые функции (0x19-0x2A) пока не реализованы.

use crate::bus::Bus;

/// Div: (частное, остаток, |частное|). Деление на 0 на железе зависает — тут возвращаем ±1.
pub fn div(num: i32, den: i32) -> (i32, i32, u32) {
    if den == 0 {
        return (if num < 0 { -1 } else { 1 }, num, 1);
    }
    let q = num.wrapping_div(den);
    (q, num.wrapping_rem(den), q.unsigned_abs())
}

pub fn sqrt(x: u32) -> u32 {
    (x as f64).sqrt() as u32
}

/// ArcTan: вход — tan в формате 1.14 (s16), выход — угол, 0x2000 = π/4.
pub fn arctan(i: i32) -> i32 {
    let a = -((i * i) >> 14);
    let mut b = ((0xA9 * a) >> 14) + 0x390;
    b = ((b * a) >> 14) + 0x91C;
    b = ((b * a) >> 14) + 0xFB6;
    b = ((b * a) >> 14) + 0x16AA;
    b = ((b * a) >> 14) + 0x2081;
    b = ((b * a) >> 14) + 0x3651;
    b = ((b * a) >> 14) + 0xA2F9;
    (i * b) >> 16
}

/// ArcTan2: полный круг = 0x10000.
pub fn arctan2(x: i32, y: i32) -> u32 {
    let r = if y == 0 {
        if x >= 0 { 0 } else { 0x8000 }
    } else if x == 0 {
        if y >= 0 { 0x4000 } else { 0xC000 }
    } else if y >= 0 {
        if x >= 0 {
            if x >= y { arctan((y << 14) / x) } else { 0x4000 - arctan((x << 14) / y) }
        } else if -x >= y {
            arctan((y << 14) / x) + 0x8000
        } else {
            0x4000 - arctan((x << 14) / y)
        }
    } else if x <= 0 {
        if -x > -y { arctan((y << 14) / x) + 0x8000 } else { 0xC000 - arctan((x << 14) / y) }
    } else if x >= -y {
        arctan((y << 14) / x) + 0x10000
    } else {
        0xC000 - arctan((x << 14) / y)
    };
    (r & 0xFFFF) as u32
}

/// CpuSet: ctrl[20:0] — число элементов, bit24 — fill, bit26 — 32 бита (иначе 16).
pub fn cpu_set(bus: &mut Bus, src: u32, dst: u32, ctrl: u32) {
    let count = ctrl & 0x1F_FFFF;
    let fill = ctrl & (1 << 24) != 0;
    if ctrl & (1 << 26) != 0 {
        let (mut s, mut d) = (src & !3, dst & !3);
        let fill_val = bus.read32(s);
        for _ in 0..count {
            let w = if fill {
                fill_val
            } else {
                let t = bus.read32(s);
                s = s.wrapping_add(4);
                t
            };
            bus.write32(d, w);
            d = d.wrapping_add(4);
        }
    } else {
        let (mut s, mut d) = (src & !1, dst & !1);
        let fill_val = bus.read16(s);
        for _ in 0..count {
            let w = if fill {
                fill_val
            } else {
                let t = bus.read16(s);
                s = s.wrapping_add(2);
                t
            };
            bus.write16(d, w);
            d = d.wrapping_add(2);
        }
    }
}

/// CpuFastSet: всегда слова, число слов округляется вверх до кратного 8.
pub fn cpu_fast_set(bus: &mut Bus, src: u32, dst: u32, ctrl: u32) {
    let count = ((ctrl & 0x1F_FFFF) + 7) & !7;
    let fill = ctrl & (1 << 24) != 0;
    let (mut s, mut d) = (src & !3, dst & !3);
    let fill_val = bus.read32(s);
    for _ in 0..count {
        let w = if fill {
            fill_val
        } else {
            let t = bus.read32(s);
            s = s.wrapping_add(4);
            t
        };
        bus.write32(d, w);
        d = d.wrapping_add(4);
    }
}

/// Записывает распакованные данные: в VRAM-вариантах только 16-битными записями.
fn write_out(bus: &mut Bus, dst: u32, data: &[u8], halfwords: bool) {
    let mut d = dst;
    if halfwords {
        for ch in data.chunks(2) {
            let hi = ch.get(1).copied().unwrap_or(0) as u16;
            bus.write16(d, ch[0] as u16 | (hi << 8));
            d = d.wrapping_add(2);
        }
    } else {
        for &b in data {
            bus.write8(d, b);
            d = d.wrapping_add(1);
        }
    }
}

pub fn lz77(bus: &mut Bus, src: u32, dst: u32, vram: bool) {
    let size = (bus.read32(src) >> 8) as usize;
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut s = src.wrapping_add(4);
    while out.len() < size {
        let flags = bus.read8(s);
        s = s.wrapping_add(1);
        for bit in (0..8).rev() {
            if out.len() >= size {
                break;
            }
            if flags & (1 << bit) == 0 {
                out.push(bus.read8(s));
                s = s.wrapping_add(1);
            } else {
                let b0 = bus.read8(s) as usize;
                let b1 = bus.read8(s.wrapping_add(1)) as usize;
                s = s.wrapping_add(2);
                let len = (b0 >> 4) + 3;
                let disp = (((b0 & 0xF) << 8) | b1) + 1;
                for _ in 0..len {
                    if out.len() >= size {
                        break;
                    }
                    let v = if disp <= out.len() { out[out.len() - disp] } else { 0 };
                    out.push(v);
                }
            }
        }
    }
    write_out(bus, dst, &out, vram);
}

pub fn rl_uncomp(bus: &mut Bus, src: u32, dst: u32, vram: bool) {
    let size = (bus.read32(src) >> 8) as usize;
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut s = src.wrapping_add(4);
    while out.len() < size {
        let f = bus.read8(s);
        s = s.wrapping_add(1);
        if f & 0x80 != 0 {
            let len = (f & 0x7F) as usize + 3;
            let v = bus.read8(s);
            s = s.wrapping_add(1);
            for _ in 0..len {
                if out.len() < size {
                    out.push(v);
                }
            }
        } else {
            let len = (f & 0x7F) as usize + 1;
            for _ in 0..len {
                let v = bus.read8(s);
                s = s.wrapping_add(1);
                if out.len() < size {
                    out.push(v);
                }
            }
        }
    }
    write_out(bus, dst, &out, vram);
}

/// Diff8bitUnFilter (wide = false) и Diff16bitUnFilter (wide = true).
pub fn diff_unfilter(bus: &mut Bus, src: u32, dst: u32, wide: bool, vram: bool) {
    let size = (bus.read32(src) >> 8) as usize;
    let mut s = src.wrapping_add(4);
    let mut out: Vec<u8> = Vec::with_capacity(size);
    if wide {
        let mut acc = 0u16;
        for i in 0..size / 2 {
            let d = bus.read16(s);
            s = s.wrapping_add(2);
            acc = if i == 0 { d } else { acc.wrapping_add(d) };
            out.extend_from_slice(&acc.to_le_bytes());
        }
    } else {
        let mut acc = 0u8;
        for i in 0..size {
            let d = bus.read8(s);
            s = s.wrapping_add(1);
            acc = if i == 0 { d } else { acc.wrapping_add(d) };
            out.push(acc);
        }
    }
    write_out(bus, dst, &out, vram);
}

pub fn huff_uncomp(bus: &mut Bus, src: u32, dst: u32) {
    let hdr = bus.read32(src);
    let bits = hdr & 0xF;
    let size = (hdr >> 8) as usize;
    if bits == 0 || bits > 8 {
        return;
    }
    let tree_size = bus.read8(src.wrapping_add(4)) as u32;
    let tree = src.wrapping_add(5);
    let mut s = src.wrapping_add(4 + (tree_size + 1) * 2);
    let mut out: Vec<u8> = Vec::with_capacity(size + 4);
    let (mut word, mut word_bits) = (0u32, 0u32);
    let mut node = tree;
    let mut budget = size as u64 * 32 + 4096; // защита от битых деревьев
    'outer: while out.len() < size {
        let stream = bus.read32(s);
        s = s.wrapping_add(4);
        for i in (0..32).rev() {
            if budget == 0 {
                break 'outer;
            }
            budget -= 1;
            let bit = (stream >> i) & 1;
            let n = bus.read8(node) as u32;
            let next = (node & !1).wrapping_add((n & 0x3F) * 2 + 2 + bit);
            if n & (0x80 >> bit) != 0 {
                let data = bus.read8(next) as u32;
                word |= (data & ((1 << bits) - 1)) << word_bits;
                word_bits += bits;
                if word_bits >= 32 {
                    out.extend_from_slice(&word.to_le_bytes());
                    word = 0;
                    word_bits = 0;
                    if out.len() >= size {
                        break 'outer;
                    }
                }
                node = tree;
            } else {
                node = next;
            }
        }
    }
    out.truncate(size);
    write_out(bus, dst, &out, true);
}

/// BitUnPack: info — указатель на {u16 длина_в_байтах, u8 ширина_src, u8 ширина_dst, u32 offset|zero_flag<<31}.
pub fn bit_unpack(bus: &mut Bus, src: u32, dst: u32, info: u32) {
    let len = bus.read16(info) as u32;
    let sw = bus.read8(info.wrapping_add(2)) as u32;
    let dw = bus.read8(info.wrapping_add(3)) as u32;
    let off = bus.read32(info.wrapping_add(4));
    let zero_flag = off & 0x8000_0000 != 0;
    let offset = off & 0x7FFF_FFFF;
    if !matches!(sw, 1 | 2 | 4 | 8) || !matches!(dw, 1 | 2 | 4 | 8 | 16 | 32) {
        return;
    }
    let src_mask = (1u32 << sw) - 1;
    let dst_mask = if dw == 32 { u32::MAX } else { (1u32 << dw) - 1 };
    let (mut s, mut d) = (src, dst);
    let (mut acc, mut acc_bits) = (0u32, 0u32);
    for _ in 0..len {
        let b = bus.read8(s) as u32;
        s = s.wrapping_add(1);
        let mut pos = 0;
        while pos < 8 {
            let mut v = (b >> pos) & src_mask;
            if v != 0 || zero_flag {
                v = v.wrapping_add(offset);
            }
            acc |= (v & dst_mask) << acc_bits;
            acc_bits += dw;
            if acc_bits >= 32 {
                bus.write32(d, acc);
                d = d.wrapping_add(4);
                acc = 0;
                acc_bits = 0;
            }
            pos += sw;
        }
    }
}

/// (sin, cos) в формате 1.14; угол — старшие 8 бит из u16.
fn sin_cos(angle: u16) -> (i32, i32) {
    let t = (angle >> 8) as f64 * std::f64::consts::TAU / 256.0;
    ((t.sin() * 16384.0).round() as i32, (t.cos() * 16384.0).round() as i32)
}

pub fn bg_affine_set(bus: &mut Bus, mut src: u32, mut dst: u32, count: u32) {
    for _ in 0..count {
        let ox = bus.read32(src) as i32;
        let oy = bus.read32(src.wrapping_add(4)) as i32;
        let cx = bus.read16(src.wrapping_add(8)) as i16 as i32;
        let cy = bus.read16(src.wrapping_add(10)) as i16 as i32;
        let sx = bus.read16(src.wrapping_add(12)) as i16 as i32;
        let sy = bus.read16(src.wrapping_add(14)) as i16 as i32;
        let (sin, cos) = sin_cos(bus.read16(src.wrapping_add(16)));
        src = src.wrapping_add(20);

        let a = (sx * cos) >> 14;
        let b = (sx * -sin) >> 14;
        let c = (sy * sin) >> 14;
        let d = (sy * cos) >> 14;
        bus.write16(dst, a as u16);
        bus.write16(dst.wrapping_add(2), b as u16);
        bus.write16(dst.wrapping_add(4), c as u16);
        bus.write16(dst.wrapping_add(6), d as u16);
        let rx = ox.wrapping_sub(a.wrapping_mul(cx).wrapping_add(b.wrapping_mul(cy)));
        let ry = oy.wrapping_sub(c.wrapping_mul(cx).wrapping_add(d.wrapping_mul(cy)));
        bus.write32(dst.wrapping_add(8), rx as u32);
        bus.write32(dst.wrapping_add(12), ry as u32);
        dst = dst.wrapping_add(16);
    }
}

pub fn obj_affine_set(bus: &mut Bus, mut src: u32, mut dst: u32, count: u32, stride: u32) {
    for _ in 0..count {
        let sx = bus.read16(src) as i16 as i32;
        let sy = bus.read16(src.wrapping_add(2)) as i16 as i32;
        let (sin, cos) = sin_cos(bus.read16(src.wrapping_add(4)));
        src = src.wrapping_add(8);

        let vals = [(sx * cos) >> 14, (sx * -sin) >> 14, (sy * sin) >> 14, (sy * cos) >> 14];
        for (i, v) in vals.iter().enumerate() {
            bus.write16(dst.wrapping_add(stride.wrapping_mul(i as u32)), *v as u16);
        }
        dst = dst.wrapping_add(stride.wrapping_mul(4));
    }
}

/// RegisterRamReset: bit0 EWRAM, 1 IWRAM (без последних 0x200 байт), 2 палитра,
/// 3 VRAM, 4 OAM, 5 SIO, 6 звук, 7 остальные регистры.
pub fn register_ram_reset(bus: &mut Bus, flags: u32) {
    if flags & 0x01 != 0 {
        bus.ewram.fill(0);
    }
    if flags & 0x02 != 0 {
        let n = bus.iwram.len() - 0x200;
        bus.iwram[..n].fill(0);
    }
    if flags & 0x04 != 0 {
        bus.pram.fill(0);
    }
    if flags & 0x08 != 0 {
        bus.vram.fill(0);
    }
    if flags & 0x10 != 0 {
        bus.oam.fill(0);
    }
    if flags & 0x20 != 0 {
        bus.io[0x120..0x12C].fill(0);
        bus.io[0x134..0x15A].fill(0);
    }
    if flags & 0x40 != 0 {
        bus.io[0x060..0x0B0].fill(0);
    }
    if flags & 0x80 != 0 {
        bus.io[0x000..0x060].fill(0);
        bus.io[0x0B0..0x110].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: u32 = 0x0200_0000;

    fn put(bus: &mut Bus, addr: u32, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            bus.write8(addr + i as u32, *b);
        }
    }

    fn get(bus: &Bus, addr: u32, n: usize) -> Vec<u8> {
        (0..n).map(|i| bus.read8(addr + i as u32)).collect()
    }

    #[test]
    fn div_and_sqrt() {
        assert_eq!(div(7, 2), (3, 1, 3));
        assert_eq!(div(-7, 2), (-3, -1, 3));
        assert_eq!(div(i32::MIN, -1).0, i32::MIN);
        assert_eq!(div(5, 0), (1, 5, 1));
        assert_eq!((sqrt(0), sqrt(15), sqrt(16), sqrt(u32::MAX)), (0, 3, 4, 65535));
    }

    #[test]
    fn arctan_values() {
        assert_eq!(arctan(0x4000), 0x2000);
        assert_eq!(arctan2(100, 100), 0x2000);
        assert_eq!(arctan2(0, 5), 0x4000);
        assert_eq!(arctan2(-5, 0), 0x8000);
        assert_eq!(arctan2(0, -5), 0xC000);
        assert_eq!(arctan2(-3, -3), 0xA000);
    }

    #[test]
    fn cpu_set_copy_and_fill() {
        let mut bus = Bus::new();
        put(&mut bus, RAM, &[1, 2, 3, 4, 5, 6, 7, 8]);
        cpu_set(&mut bus, RAM, RAM + 0x100, 2 | (1 << 26));
        assert_eq!(get(&bus, RAM + 0x100, 8), [1, 2, 3, 4, 5, 6, 7, 8]);
        // fill halfword: 3 раза записать 0x0201
        cpu_set(&mut bus, RAM, RAM + 0x200, 3 | (1 << 24));
        assert_eq!(get(&bus, RAM + 0x200, 6), [1, 2, 1, 2, 1, 2]);
    }

    #[test]
    fn cpu_fast_set_rounds_up_to_8_words() {
        let mut bus = Bus::new();
        put(&mut bus, RAM, &[0xAA, 0xBB, 0xCC, 0xDD]);
        cpu_fast_set(&mut bus, RAM, RAM + 0x100, 1 | (1 << 24));
        assert_eq!(bus.read32(RAM + 0x100), 0xDDCC_BBAA);
        assert_eq!(bus.read32(RAM + 0x100 + 28), 0xDDCC_BBAA); // 8-е слово
        assert_eq!(bus.read32(RAM + 0x100 + 32), 0); // дальше не трогаем
    }

    #[test]
    fn lz77_literal_and_backref() {
        let mut bus = Bus::new();
        // "ABC" + копия 6 байт с дистанции 3 => "ABCABCABC"
        put(&mut bus, RAM, &[0x10, 0x09, 0x00, 0x00, 0x10, b'A', b'B', b'C', 0x30, 0x02]);
        lz77(&mut bus, RAM, RAM + 0x100, false);
        assert_eq!(get(&bus, RAM + 0x100, 9), b"ABCABCABC");
        lz77(&mut bus, RAM, RAM + 0x200, true);
        assert_eq!(get(&bus, RAM + 0x200, 9), b"ABCABCABC");
    }

    #[test]
    fn rl_and_diff() {
        let mut bus = Bus::new();
        put(&mut bus, RAM, &[0x30, 0x07, 0, 0, 0x82, b'X', 0x01, b'Y', b'Z']);
        rl_uncomp(&mut bus, RAM, RAM + 0x100, false);
        assert_eq!(get(&bus, RAM + 0x100, 7), b"XXXXXYZ");

        put(&mut bus, RAM + 0x200, &[0x81, 0x04, 0, 0, 10, 5, 5, 255]);
        diff_unfilter(&mut bus, RAM + 0x200, RAM + 0x300, false, false);
        assert_eq!(get(&bus, RAM + 0x300, 4), [10, 15, 20, 19]);
    }

    #[test]
    fn bit_unpack_1bpp_to_4bpp() {
        let mut bus = Bus::new();
        put(&mut bus, RAM, &[0xA5]);
        // len=1, src 1 бит, dst 4 бита, offset 0
        put(&mut bus, RAM + 0x10, &[1, 0, 1, 4, 0, 0, 0, 0]);
        bit_unpack(&mut bus, RAM, RAM + 0x100, RAM + 0x10);
        assert_eq!(bus.read32(RAM + 0x100), 0x1010_0101);
    }

    #[test]
    fn huffman_two_leaf_tree() {
        let mut bus = Bus::new();
        // заголовок: 8-битные данные, тип 2, размер 4; tree_size=1; корень=0xC0 (оба потомка — листья)
        put(&mut bus, RAM, &[0x28, 0x04, 0, 0, 0x01, 0xC0, 0x11, 0x22]);
        bus.write32(RAM + 8, 0x6000_0000); // биты 0,1,1,0
        huff_uncomp(&mut bus, RAM, RAM + 0x100);
        assert_eq!(bus.read32(RAM + 0x100), 0x1122_2211);
    }

    #[test]
    fn affine_identity() {
        let mut bus = Bus::new();
        // ObjAffineSet: sx=sy=0x100 (1.0), угол 0 => pa=pd=0x100, pb=pc=0
        put(&mut bus, RAM, &[0x00, 0x01, 0x00, 0x01, 0, 0, 0, 0]);
        obj_affine_set(&mut bus, RAM, RAM + 0x100, 1, 2);
        assert_eq!(get(&bus, RAM + 0x100, 8), [0x00, 0x01, 0, 0, 0, 0, 0x00, 0x01]);
    }
}
