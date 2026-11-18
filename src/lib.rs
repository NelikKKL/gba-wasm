mod bus;
mod cpu;
mod gba;
mod ppu;

use wasm_bindgen::prelude::*;

/// Публичный API для JS. Кадр — RGBA 240x160, читается прямо из памяти wasm.
#[wasm_bindgen]
pub struct Emulator {
    gba: gba::Gba,
}

#[wasm_bindgen]
impl Emulator {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Emulator {
        Emulator { gba: gba::Gba::new() }
    }

    /// Загружает ROM, возвращает название игры из заголовка.
    pub fn load_rom(&mut self, rom: &[u8]) -> Result<String, JsValue> {
        self.gba.load_rom(rom).map_err(|e| JsValue::from_str(&e))
    }

    /// Маска нажатых кнопок: бит 1 = нажата.
    /// 0:A 1:B 2:Select 3:Start 4:Right 5:Left 6:Up 7:Down 8:R 9:L
    pub fn set_keys(&mut self, mask: u16) {
        self.gba.bus.keys = mask & 0x3FF;
    }

    pub fn run_frame(&mut self) {
        self.gba.run_frame();
    }

    pub fn frame_ptr(&self) -> *const u8 {
        self.gba.fb.as_ptr()
    }
}
