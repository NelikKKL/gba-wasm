//! PPU: построчный рендер. Фоны 0-3 (режимы 0-2 тайловые, 3-5 bitmap), спрайты,
//! окна, alpha-смешивание и яркость.
//! TODO: мозаика для спрайтов и аффинных фонов, тонкости OAM-тайминга.

use crate::bus::Bus;

pub const W: usize = 240;
pub const H: usize = 160;

/// Бит 15 в BGR555 не используется — им помечаем прозрачный пиксель фона.
const TRANSPARENT: u16 = 0x8000;

#[derive(Clone, Copy)]
struct ObjPx {
    color: u16,
    prio: u8, // 0xFF = пикселя нет
    semi: bool,
}
const OBJ_EMPTY: ObjPx = ObjPx { color: 0, prio: 0xFF, semi: false };

pub struct Ppu {
    /// Внутренние опорные точки аффинных фонов BG2/BG3 (формат 20.8).
    ref_x: [i32; 2],
    ref_y: [i32; 2],
    bg: [[u16; W]; 4],
    obj: [ObjPx; W],
    objwin: [bool; W],
}

// ---------- доступ к памяти и регистрам ----------

fn io16(bus: &Bus, off: usize) -> u16 {
    bus.io[off] as u16 | (bus.io[off + 1] as u16) << 8
}

fn io32(bus: &Bus, off: usize) -> u32 {
    io16(bus, off) as u32 | (io16(bus, off + 2) as u32) << 16
}

fn vram8(bus: &Bus, off: usize) -> u8 {
    let o = off & 0x1_FFFF;
    bus.vram[if o >= 0x18000 { o - 0x8000 } else { o }]
}

fn vram16(bus: &Bus, off: usize) -> u16 {
    vram8(bus, off) as u16 | (vram8(bus, off + 1) as u16) << 8
}

/// Цвет из палитры: 0-255 фоны, 256-511 спрайты.
fn pal(bus: &Bus, index: usize) -> u16 {
    (bus.pram[index * 2] as u16 | (bus.pram[index * 2 + 1] as u16) << 8) & 0x7FFF
}

fn oam16(bus: &Bus, off: usize) -> u16 {
    bus.oam[off] as u16 | (bus.oam[off + 1] as u16) << 8
}

fn bgr555(c: u16) -> [u8; 4] {
    let r = (c & 31) as u8;
    let g = ((c >> 5) & 31) as u8;
    let b = ((c >> 10) & 31) as u8;
    [(r << 3) | (r >> 2), (g << 3) | (g >> 2), (b << 3) | (b >> 2), 255]
}

// ---------- цветовые эффекты ----------

fn map_channels(c: u16, f: impl Fn(u32) -> u32) -> u16 {
    let mut out = 0u16;
    for sh in [0u32, 5, 10] {
        let v = f((c as u32 >> sh) & 31).min(31);
        out |= (v as u16) << sh;
    }
    out
}

fn alpha_blend(a: u16, b: u16, eva: u32, evb: u32) -> u16 {
    let mut out = 0u16;
    for sh in [0u32, 5, 10] {
        let ca = (a as u32 >> sh) & 31;
        let cb = (b as u32 >> sh) & 31;
        let v = ((ca * eva + cb * evb) >> 4).min(31);
        out |= (v as u16) << sh;
    }
    out
}

fn in_span(v: usize, a: usize, b: usize) -> bool {
    if a <= b { v >= a && v < b } else { v >= a || v < b }
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            ref_x: [0; 2],
            ref_y: [0; 2],
            bg: [[TRANSPARENT; W]; 4],
            obj: [OBJ_EMPTY; W],
            objwin: [false; W],
        }
    }

    /// Рисует одну видимую строку в `fb`. Вызывать для y = 0..159 по порядку.
    pub fn render_line(&mut self, bus: &mut Bus, y: usize, fb: &mut [u8]) {
        for i in 0..2 {
            if y == 0 || bus.affine_dirty[i] {
                let base = 0x28 + i * 0x10;
                self.ref_x[i] = (io32(bus, base) << 4) as i32 >> 4;
                self.ref_y[i] = (io32(bus, base + 4) << 4) as i32 >> 4;
                bus.affine_dirty[i] = false;
            }
        }

        let dispcnt = io16(bus, 0x00) as u32;
        let line = &mut fb[y * W * 4..(y + 1) * W * 4];
        if dispcnt & 0x80 != 0 {
            line.fill(255); // forced blank
        } else {
            self.draw(bus, y, dispcnt, line);
        }

        // после строки опорные точки сдвигаются на PB/PD
        for i in 0..2 {
            let base = 0x20 + i * 0x10;
            let pb = io16(bus, base + 2) as i16 as i32;
            let pd = io16(bus, base + 6) as i16 as i32;
            self.ref_x[i] = self.ref_x[i].wrapping_add(pb);
            self.ref_y[i] = self.ref_y[i].wrapping_add(pd);
        }
    }

    fn draw(&mut self, bus: &Bus, y: usize, dispcnt: u32, line: &mut [u8]) {
        let mode = dispcnt & 7;
        let backdrop = pal(bus, 0);

        let mut bg_on = [false; 4];
        for b in 0..4 {
            bg_on[b] = dispcnt & (0x100 << b) != 0;
        }
        match mode {
            0 => {}
            1 => bg_on[3] = false,
            2 => {
                bg_on[0] = false;
                bg_on[1] = false;
            }
            _ => {
                bg_on[0] = false;
                bg_on[1] = false;
                bg_on[3] = false;
            }
        }
        for b in 0..4 {
            self.bg[b] = [TRANSPARENT; W];
        }
        match mode {
            0 => {
                for b in 0..4 {
                    if bg_on[b] {
                        self.text_bg(bus, b, y);
                    }
                }
            }
            1 => {
                if bg_on[0] {
                    self.text_bg(bus, 0, y);
                }
                if bg_on[1] {
                    self.text_bg(bus, 1, y);
                }
                if bg_on[2] {
                    self.affine_bg(bus, 2, y);
                }
            }
            2 => {
                if bg_on[2] {
                    self.affine_bg(bus, 2, y);
                }
                if bg_on[3] {
                    self.affine_bg(bus, 3, y);
                }
            }
            3..=5 => {
                if bg_on[2] {
                    self.bitmap_bg(bus, mode, dispcnt, y);
                }
            }
            _ => {}
        }

        self.obj = [OBJ_EMPTY; W];
        self.objwin = [false; W];
        let obj_on = dispcnt & 0x1000 != 0;
        if obj_on {
            self.sprites(bus, y, mode, dispcnt & 0x40 != 0);
        }

        // окна
        let win0_on = dispcnt & 0x2000 != 0;
        let win1_on = dispcnt & 0x4000 != 0;
        let objwin_on = dispcnt & 0x8000 != 0;
        let any_win = win0_on || win1_on || objwin_on;
        let win_in = io16(bus, 0x48);
        let win_out = io16(bus, 0x4A);
        let span = |reg: u16, limit: usize| -> (usize, usize) {
            let a = (reg >> 8) as usize;
            let b = ((reg & 0xFF) as usize).min(limit);
            (a, b)
        };
        let (h0a, h0b) = span(io16(bus, 0x40), W);
        let (h1a, h1b) = span(io16(bus, 0x42), W);
        let (v0a, v0b) = span(io16(bus, 0x44), H);
        let (v1a, v1b) = span(io16(bus, 0x46), H);
        let y_in0 = win0_on && in_span(y, v0a, v0b);
        let y_in1 = win1_on && in_span(y, v1a, v1b);

        let prio = [
            (io16(bus, 0x08) & 3) as u8,
            (io16(bus, 0x0A) & 3) as u8,
            (io16(bus, 0x0C) & 3) as u8,
            (io16(bus, 0x0E) & 3) as u8,
        ];
        let bldcnt = io16(bus, 0x50);
        let effect = (bldcnt >> 6) & 3;
        let alpha = io16(bus, 0x52);
        let eva = ((alpha & 0x1F) as u32).min(16);
        let evb = (((alpha >> 8) & 0x1F) as u32).min(16);
        let evy = ((io16(bus, 0x54) & 0x1F) as u32).min(16);

        for x in 0..W {
            let flags: u16 = if !any_win {
                0x3F
            } else if y_in0 && in_span(x, h0a, h0b) {
                win_in & 0x3F
            } else if y_in1 && in_span(x, h1a, h1b) {
                (win_in >> 8) & 0x3F
            } else if objwin_on && self.objwin[x] {
                (win_out >> 8) & 0x3F
            } else {
                win_out & 0x3F
            };

            // два верхних слоя: (цвет, слой) где слой 0-3 BG, 4 OBJ, 5 backdrop
            let mut found = [(0u16, 0u8); 2];
            let mut n = 0usize;
            let mut top_semi = false;
            'scan: for p in 0..4u8 {
                if obj_on && flags & 0x10 != 0 {
                    let o = self.obj[x];
                    if o.prio == p {
                        found[n] = (o.color, 4);
                        if n == 0 {
                            top_semi = o.semi;
                        }
                        n += 1;
                        if n == 2 {
                            break 'scan;
                        }
                    }
                }
                for b in 0..4 {
                    if bg_on[b] && flags & (1 << b) != 0 && prio[b] == p {
                        let c = self.bg[b][x];
                        if c != TRANSPARENT {
                            found[n] = (c, b as u8);
                            n += 1;
                            if n == 2 {
                                break 'scan;
                            }
                        }
                    }
                }
            }
            if n < 2 {
                found[n] = (backdrop, 5);
                n += 1;
            }

            let (top_c, top_l) = found[0];
            let mut color = top_c;
            if flags & 0x20 != 0 {
                let first = (bldcnt >> top_l) & 1 != 0;
                let mut done = false;
                if n == 2 {
                    let (sec_c, sec_l) = found[1];
                    let second = (bldcnt >> (8 + sec_l)) & 1 != 0;
                    if top_l == 4 && top_semi && second {
                        color = alpha_blend(top_c, sec_c, eva, evb);
                        done = true;
                    } else if effect == 1 && first && second {
                        color = alpha_blend(top_c, sec_c, eva, evb);
                        done = true;
                    }
                }
                if !done && first {
                    match effect {
                        2 => color = map_channels(top_c, |c| c + (((31 - c) * evy) >> 4)),
                        3 => color = map_channels(top_c, |c| c - ((c * evy) >> 4)),
                        _ => {}
                    }
                }
            }
            line[x * 4..x * 4 + 4].copy_from_slice(&bgr555(color));
        }
    }

    /// Текстовый (тайловый) фон: режимы 0 (BG0-3) и 1 (BG0-1).
    fn text_bg(&mut self, bus: &Bus, b: usize, y: usize) {
        let cnt = io16(bus, 0x08 + b * 2) as usize;
        let char_base = ((cnt >> 2) & 3) * 0x4000;
        let mosaic = cnt & 0x40 != 0;
        let color256 = cnt & 0x80 != 0;
        let screen_base = ((cnt >> 8) & 0x1F) * 0x800;
        let (w, h) = match cnt >> 14 {
            0 => (256usize, 256usize),
            1 => (512, 256),
            2 => (256, 512),
            _ => (512, 512),
        };
        let hofs = io16(bus, 0x10 + b * 4) as usize & 0x1FF;
        let vofs = io16(bus, 0x12 + b * 4) as usize & 0x1FF;
        let mos = io16(bus, 0x4C);
        let mh = (mos & 0xF) as usize + 1;
        let mv = ((mos >> 4) & 0xF) as usize + 1;

        let yy = if mosaic { y - y % mv } else { y };
        let py = (yy + vofs) & (h - 1);
        let ty = py >> 3;
        for x in 0..W {
            let xx = if mosaic { x - x % mh } else { x };
            let px = (xx + hofs) & (w - 1);
            let tx = px >> 3;
            let sb = (tx >> 5) + (ty >> 5) * (w >> 8);
            let e = vram16(bus, screen_base + sb * 0x800 + ((ty & 31) * 32 + (tx & 31)) * 2) as usize;
            let tile = e & 0x3FF;
            let mut ix = px & 7;
            let mut iy = py & 7;
            if e & 0x400 != 0 {
                ix = 7 - ix;
            }
            if e & 0x800 != 0 {
                iy = 7 - iy;
            }
            let (idx, color_index) = if color256 {
                let i = vram8(bus, char_base + tile * 64 + iy * 8 + ix) as usize;
                (i, i)
            } else {
                let byte = vram8(bus, char_base + tile * 32 + iy * 4 + (ix >> 1));
                let nib = (if ix & 1 == 0 { byte & 15 } else { byte >> 4 }) as usize;
                (nib, (e >> 12) * 16 + nib)
            };
            if idx != 0 {
                self.bg[b][x] = pal(bus, color_index);
            }
        }
    }

    /// Аффинный тайловый фон (BG2/BG3 в режимах 1 и 2).
    fn affine_bg(&mut self, bus: &Bus, b: usize, y: usize) {
        let _ = y;
        let cnt = io16(bus, 0x08 + b * 2) as usize;
        let char_base = ((cnt >> 2) & 3) * 0x4000;
        let screen_base = ((cnt >> 8) & 0x1F) * 0x800;
        let wrap = cnt & 0x2000 != 0;
        let size = 128usize << (cnt >> 14);
        let tiles = size >> 3;
        let i = b - 2;
        let base = 0x20 + i * 0x10;
        let pa = io16(bus, base) as i16 as i32;
        let pc = io16(bus, base + 4) as i16 as i32;
        let (mut fx, mut fy) = (self.ref_x[i], self.ref_y[i]);
        for x in 0..W {
            let (mut tx, mut ty) = (fx >> 8, fy >> 8);
            fx = fx.wrapping_add(pa);
            fy = fy.wrapping_add(pc);
            if wrap {
                tx &= size as i32 - 1;
                ty &= size as i32 - 1;
            } else if tx < 0 || ty < 0 || tx >= size as i32 || ty >= size as i32 {
                continue;
            }
            let (tx, ty) = (tx as usize, ty as usize);
            let tile = vram8(bus, screen_base + (ty >> 3) * tiles + (tx >> 3)) as usize;
            let idx = vram8(bus, char_base + tile * 64 + (ty & 7) * 8 + (tx & 7)) as usize;
            if idx != 0 {
                self.bg[b][x] = pal(bus, idx);
            }
        }
    }

    /// Bitmap-режимы 3, 4, 5 (BG2 с аффинным преобразованием).
    fn bitmap_bg(&mut self, bus: &Bus, mode: u32, dispcnt: u32, y: usize) {
        let _ = y;
        let (bw, bh) = if mode == 5 { (160i32, 128i32) } else { (240, 160) };
        let page: usize = if mode != 3 && dispcnt & 0x10 != 0 { 0xA000 } else { 0 };
        let pa = io16(bus, 0x20) as i16 as i32;
        let pc = io16(bus, 0x24) as i16 as i32;
        let (mut fx, mut fy) = (self.ref_x[0], self.ref_y[0]);
        for x in 0..W {
            let (tx, ty) = (fx >> 8, fy >> 8);
            fx = fx.wrapping_add(pa);
            fy = fy.wrapping_add(pc);
            if tx < 0 || ty < 0 || tx >= bw || ty >= bh {
                continue;
            }
            let (tx, ty) = (tx as usize, ty as usize);
            match mode {
                3 => self.bg[2][x] = vram16(bus, (ty * 240 + tx) * 2) & 0x7FFF,
                4 => {
                    let idx = vram8(bus, page + ty * 240 + tx) as usize;
                    if idx != 0 {
                        self.bg[2][x] = pal(bus, idx);
                    }
                }
                _ => self.bg[2][x] = vram16(bus, page + (ty * 160 + tx) * 2) & 0x7FFF,
            }
        }
    }

    /// Спрайты на строке y. Меньший индекс OAM и меньший priority — выше.
    fn sprites(&mut self, bus: &Bus, y: usize, mode: u32, one_d: bool) {
        for i in 0..128usize {
            let a0 = oam16(bus, i * 8) as usize;
            let a1 = oam16(bus, i * 8 + 2) as usize;
            let a2 = oam16(bus, i * 8 + 4) as usize;
            let affine = a0 & 0x100 != 0;
            let double = affine && a0 & 0x200 != 0;
            if !affine && a0 & 0x200 != 0 {
                continue; // скрыт
            }
            let om = (a0 >> 10) & 3;
            if om == 3 {
                continue;
            }
            let (w, h) = match (a0 >> 14, a1 >> 14) {
                (0, 0) => (8usize, 8usize),
                (0, 1) => (16, 16),
                (0, 2) => (32, 32),
                (0, 3) => (64, 64),
                (1, 0) => (16, 8),
                (1, 1) => (32, 8),
                (1, 2) => (32, 16),
                (1, 3) => (64, 32),
                (2, 0) => (8, 16),
                (2, 1) => (8, 32),
                (2, 2) => (16, 32),
                (2, 3) => (32, 64),
                _ => continue,
            };
            let (bw, bh) = if double { (w * 2, h * 2) } else { (w, h) };
            let sy = y.wrapping_sub(a0 & 0xFF) & 0xFF;
            if sy >= bh {
                continue;
            }
            let mut ox = (a1 & 0x1FF) as i32;
            if ox >= 256 {
                ox -= 512;
            }
            let color256 = a0 & 0x2000 != 0;
            let tile = a2 & 0x3FF;
            let prio = ((a2 >> 10) & 3) as u8;
            let pal_bank = a2 >> 12;
            if mode >= 3 && tile < 512 {
                continue; // в bitmap-режимах эта часть VRAM занята кадром
            }
            let (pa, pb, pc, pd) = if affine {
                let g = ((a1 >> 9) & 31) * 32;
                (
                    oam16(bus, g + 6) as i16 as i32,
                    oam16(bus, g + 14) as i16 as i32,
                    oam16(bus, g + 22) as i16 as i32,
                    oam16(bus, g + 30) as i16 as i32,
                )
            } else {
                (0, 0, 0, 0)
            };

            for i_x in 0..bw {
                let screen_x = ox + i_x as i32;
                if screen_x < 0 || screen_x >= W as i32 {
                    continue;
                }
                let (tx, ty) = if affine {
                    let dx = i_x as i32 - (bw / 2) as i32;
                    let dy = sy as i32 - (bh / 2) as i32;
                    let tx = ((pa * dx + pb * dy) >> 8) + (w / 2) as i32;
                    let ty = ((pc * dx + pd * dy) >> 8) + (h / 2) as i32;
                    if tx < 0 || ty < 0 || tx >= w as i32 || ty >= h as i32 {
                        continue;
                    }
                    (tx as usize, ty as usize)
                } else {
                    let tx = if a1 & 0x1000 != 0 { w - 1 - i_x } else { i_x };
                    let ty = if a1 & 0x2000 != 0 { h - 1 - sy } else { sy };
                    (tx, ty)
                };

                let step = if color256 { 2 } else { 1 };
                let row = if one_d { (w >> 3) * step } else { 32 };
                let unit = tile + (ty >> 3) * row + (tx >> 3) * step;
                let (ix, iy) = (tx & 7, ty & 7);
                let off = unit * 32 + if color256 { iy * 8 + ix } else { iy * 4 + (ix >> 1) };
                let addr = 0x10000 + (off & 0x7FFF);
                let idx = if color256 {
                    vram8(bus, addr) as usize
                } else {
                    let byte = vram8(bus, addr);
                    (if ix & 1 == 0 { byte & 15 } else { byte >> 4 }) as usize
                };
                if idx == 0 {
                    continue;
                }
                let xu = screen_x as usize;
                if om == 2 {
                    self.objwin[xu] = true; // OBJ-окно: цвет не рисуется
                    continue;
                }
                if self.obj[xu].prio > prio {
                    let ci = 256 + if color256 { idx } else { pal_bank * 16 + idx };
                    self.obj[xu] = ObjPx { color: pal(bus, ci), prio, semi: om == 1 };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bus: &mut Bus) -> Vec<u8> {
        let mut ppu = Ppu::new();
        let mut fb = vec![0u8; W * H * 4];
        for y in 0..H {
            ppu.render_line(bus, y, &mut fb);
        }
        fb
    }

    fn px(fb: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * W + x) * 4;
        [fb[i], fb[i + 1], fb[i + 2], fb[i + 3]]
    }

    const RED: u16 = 0x001F;
    const GREEN: u16 = 0x03E0;
    const BLUE: u16 = 0x7C00;

    fn reg(bus: &mut Bus, off: u32, v: u16) {
        bus.write16(0x0400_0000 + off, v);
    }

    /// Плитка 4bpp, первая строка — индексы `idx` на всю строку.
    fn solid_tile_row0(bus: &mut Bus, char_base: u32, tile: u32, idx: u8) {
        for i in 0..4 {
            bus.write8(0x0600_0000 + char_base + tile * 32 + i, idx | idx << 4);
        }
    }

    #[test]
    fn mode3_pixel() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x0403);
        bus.write16(0x0600_0000 + (5 * 240 + 7) * 2, RED);
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 7, 5), [255, 0, 0, 255]);
        assert_eq!(px(&fb, 8, 5), [0, 0, 0, 255]);
    }

    #[test]
    fn text_bg_4bpp_tile_and_palette() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x0100); // режим 0, BG0
        reg(&mut bus, 0x08, 0x0100); // char base 0, screen base 1
        bus.write16(0x0500_0000 + 2, GREEN);
        bus.write16(0x0500_0000 + 4, BLUE);
        bus.write16(0x0600_0800, 1); // карта (0,0) -> плитка 1
        // плитка 1, строка 0: пиксель 0 = индекс 1, пиксель 1 = индекс 2
        bus.write8(0x0600_0000 + 32, 0x21);
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 0, 0), [0, 255, 0, 255]);
        assert_eq!(px(&fb, 1, 0), [0, 0, 255, 255]);
        assert_eq!(px(&fb, 0, 1), [0, 0, 0, 255]); // строка 1 плитки пустая
    }

    #[test]
    fn text_bg_hflip_and_scroll() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x0100);
        reg(&mut bus, 0x08, 0x0100);
        bus.write16(0x0500_0000 + 2, GREEN);
        bus.write16(0x0600_0800, 1 | 0x400); // hflip
        bus.write8(0x0600_0000 + 32, 0x01); // пиксель 0 плитки = индекс 1
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 7, 0), [0, 255, 0, 255]); // отразился вправо
        assert_eq!(px(&fb, 0, 0), [0, 0, 0, 255]);

        reg(&mut bus, 0x10, 3); // HOFS = 3: картинка уезжает влево
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 4, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn sprite_4bpp_1d() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x1040); // OBJ, 1D-раскладка
        bus.write16(0x0500_0000 + 0x200 + 2, RED);
        bus.write16(0x0601_0000, 0x0001); // плитка 0, пиксель (0,0) = индекс 1 (OBJ-VRAM: только 16/32 бит)
        bus.write16(0x0700_0000, 10); // y=10, квадрат
        bus.write16(0x0700_0002, 20); // x=20, 8x8
        bus.write16(0x0700_0004, 0); // плитка 0, приоритет 0, палитра 0
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 20, 10), [255, 0, 0, 255]);
        assert_eq!(px(&fb, 21, 10), [0, 0, 0, 255]);
    }

    #[test]
    fn alpha_blending_bg_over_backdrop() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x0100);
        reg(&mut bus, 0x08, 0x0100);
        bus.write16(0x0500_0000, BLUE); // backdrop
        bus.write16(0x0500_0000 + 2, RED);
        bus.write16(0x0600_0800, 1);
        solid_tile_row0(&mut bus, 0, 1, 1);
        reg(&mut bus, 0x50, 0x2041); // 1-я цель BG0, alpha, 2-я цель backdrop
        reg(&mut bus, 0x52, 0x0808); // 8/16 + 8/16
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 0, 0), [123, 0, 123, 255]);
    }

    #[test]
    fn window_masks_background() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x2100); // BG0 + окно 0
        reg(&mut bus, 0x08, 0x0100);
        bus.write16(0x0500_0000 + 2, GREEN);
        for tx in 0..2 {
            bus.write16(0x0600_0800 + tx * 2, 1);
        }
        for row in 0..8u32 {
            for i in 0..4 {
                bus.write8(0x0600_0000 + 32 + row * 4 + i, 0x11);
            }
        }
        reg(&mut bus, 0x40, 0x0008); // x: 0..8
        reg(&mut bus, 0x44, 0x0008); // y: 0..8
        reg(&mut bus, 0x48, 0x0001); // в окне 0 виден BG0
        reg(&mut bus, 0x4A, 0x0000); // снаружи ничего
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 3, 3), [0, 255, 0, 255]);
        assert_eq!(px(&fb, 10, 3), [0, 0, 0, 255]); // правее окна, но плитка есть
    }

    #[test]
    fn affine_bg_mode1() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x0401); // режим 1, BG2
        reg(&mut bus, 0x0C, 0x0100); // BG2CNT: char 0, screen 1, 128x128
        bus.write16(0x0500_0000 + 2, GREEN);
        bus.write16(0x0600_0800, 0x0001); // карта (0,0) -> плитка 1, (1,0) -> плитка 0
        for i in 0..64 {
            bus.write8(0x0600_0000 + 64 + i, 1); // плитка 1: 8bpp, всё индекс 1
        }
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 0, 0), [0, 255, 0, 255]);
        assert_eq!(px(&fb, 7, 7), [0, 255, 0, 255]);
        assert_eq!(px(&fb, 8, 0), [0, 0, 0, 255]); // плитка 0 пустая
    }

    #[test]
    fn forced_blank_is_white() {
        let mut bus = Bus::new();
        reg(&mut bus, 0, 0x0080);
        let fb = frame(&mut bus);
        assert_eq!(px(&fb, 100, 80), [255, 255, 255, 255]);
    }
}
