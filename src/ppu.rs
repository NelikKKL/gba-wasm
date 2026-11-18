use crate::bus::Bus;

pub const W: usize = 240;
pub const H: usize = 160;

fn bgr555(c: u16) -> [u8; 4] {
    let r = (c & 31) as u8;
    let g = ((c >> 5) & 31) as u8;
    let b = ((c >> 10) & 31) as u8;
    [(r << 3) | (r >> 2), (g << 3) | (g >> 2), (b << 3) | (b >> 2), 255]
}

/// Рисует кадр из VRAM. Пока только bitmap-режимы 3/4/5.
/// TODO: тайловые режимы 0-2, спрайты, окна, blending, построчный рендер.
pub fn render_frame(bus: &Bus, fb: &mut [u8]) {
    let dispcnt = bus.read16(0x0400_0000);
    let mode = dispcnt & 7;
    let backdrop = bus.read16(0x0500_0000);
    let page: u32 = if dispcnt & 0x10 != 0 { 0xA000 } else { 0 };

    if dispcnt & 0x80 != 0 {
        fb.fill(255); // forced blank
        return;
    }

    for y in 0..H {
        for x in 0..W {
            let i = y * W + x;
            let c = match mode {
                3 => bus.read16(0x0600_0000 + i as u32 * 2),
                4 => {
                    let idx = bus.read8(0x0600_0000 + page + i as u32) as u32;
                    bus.read16(0x0500_0000 + idx * 2)
                }
                5 if x < 160 && y < 128 => {
                    bus.read16(0x0600_0000 + page + (y * 160 + x) as u32 * 2)
                }
                _ => backdrop,
            };
            fb[i * 4..i * 4 + 4].copy_from_slice(&bgr555(c));
        }
    }
}
