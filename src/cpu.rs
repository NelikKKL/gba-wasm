//! ARM7TDMI: наборы ARM и Thumb, режимы с банкингом регистров, IRQ.
//!
//! Модель PC: `r[15]` — адрес СЛЕДУЮЩЕЙ инструкции (после выборки).
//! Операнд PC для ARM = r[15] + 4 (то есть адрес инструкции + 8),
//! для Thumb = r[15] + 2 (адрес инструкции + 4).
//! Тайминги пока приблизительные (без waitstate'ов памяти).

use crate::{bios, bus::Bus};

const MODE_USR: u32 = 0x10;
const MODE_FIQ: u32 = 0x11;
const MODE_IRQ: u32 = 0x12;
const MODE_SVC: u32 = 0x13;
const MODE_ABT: u32 = 0x17;
const MODE_UND: u32 = 0x1B;
const MODE_SYS: u32 = 0x1F;

const FLAG_N: u32 = 1 << 31;
const FLAG_Z: u32 = 1 << 30;
const FLAG_C: u32 = 1 << 29;
const FLAG_V: u32 = 1 << 28;
const FLAG_I: u32 = 1 << 7;
const FLAG_T: u32 = 1 << 5;

/// Флаги прерываний для BIOS (их выставляет IRQ-обработчик игры).
const BIOS_IF: u32 = 0x0300_7FF8;

/// Индекс банка: 0 usr/sys, 1 fiq, 2 irq, 3 svc, 4 abt, 5 und.
fn bank(mode: u32) -> usize {
    match mode & 0x1F {
        MODE_FIQ => 1,
        MODE_IRQ => 2,
        MODE_SVC => 3,
        MODE_ABT => 4,
        MODE_UND => 5,
        _ => 0,
    }
}

/// a + b + c, возвращает (результат, carry, overflow).
fn add_c(a: u32, b: u32, c: u32) -> (u32, bool, bool) {
    let sum = a as u64 + b as u64 + c as u64;
    let res = sum as u32;
    (res, sum > 0xFFFF_FFFF, (!(a ^ b) & (a ^ res)) >> 31 != 0)
}

/// Barrel shifter. kind: 0 LSL, 1 LSR, 2 ASR, 3 ROR. Возвращает (значение, carry).
fn shift_op(kind: u32, v: u32, amt: u32, by_reg: bool, c: bool) -> (u32, bool) {
    match kind {
        0 => match amt {
            0 => (v, c),
            1..=31 => (v << amt, (v >> (32 - amt)) & 1 != 0),
            32 => (0, v & 1 != 0),
            _ => (0, false),
        },
        1 => {
            let amt = if amt == 0 && !by_reg { 32 } else { amt };
            match amt {
                0 => (v, c),
                1..=31 => (v >> amt, (v >> (amt - 1)) & 1 != 0),
                32 => (0, v >> 31 != 0),
                _ => (0, false),
            }
        }
        2 => {
            let amt = if amt == 0 && !by_reg { 32 } else { amt };
            match amt {
                0 => (v, c),
                1..=31 => (((v as i32) >> amt) as u32, (v >> (amt - 1)) & 1 != 0),
                _ => {
                    let neg = v >> 31 != 0;
                    (if neg { u32::MAX } else { 0 }, neg)
                }
            }
        }
        _ => {
            if amt == 0 {
                if by_reg {
                    (v, c)
                } else {
                    // RRX
                    ((v >> 1) | ((c as u32) << 31), v & 1 != 0)
                }
            } else {
                let a = amt & 31;
                if a == 0 {
                    (v, v >> 31 != 0)
                } else {
                    (v.rotate_right(a), (v >> (a - 1)) & 1 != 0)
                }
            }
        }
    }
}

fn ldrh(bus: &Bus, addr: u32) -> u32 {
    let v = bus.read16(addr) as u32;
    if addr & 1 != 0 { v.rotate_right(8) } else { v }
}

fn ldrsh(bus: &Bus, addr: u32) -> u32 {
    if addr & 1 != 0 {
        bus.read8(addr) as i8 as i32 as u32
    } else {
        bus.read16(addr) as i16 as i32 as u32
    }
}

pub struct Cpu {
    pub r: [u32; 16],
    pub cpsr: u32,
    spsr: [u32; 6],
    banked: [[u32; 2]; 6], // r13, r14 для каждого банка
    fiq_hi: [u32; 5],      // r8-r12 режима FIQ
    usr_hi: [u32; 5],      // r8-r12 остальных режимов, пока активен FIQ
    /// Ожидаемые биты для IntrWait/VBlankIntrWait (0 = не ждём).
    wait_mask: u16,
    /// Адрес инструкции после SWI: ожидание действует только на ней, а не в обработчике IRQ.
    wait_pc: u32,
    /// Конвейер выборки: опкоды инструкций по адресам r15 и r15 + размер.
    /// Запись в память не меняет уже выбранные опкоды (как на железе).
    pipe: [u32; 2],
    needs_refill: bool,
}

impl Cpu {
    pub fn new() -> Self {
        let mut c = Cpu {
            r: [0; 16],
            cpsr: MODE_SYS,
            spsr: [0; 6],
            banked: [[0; 2]; 6],
            fiq_hi: [0; 5],
            usr_hi: [0; 5],
            wait_mask: 0,
            wait_pc: 0,
            pipe: [0; 2],
            needs_refill: true,
        };
        c.reset();
        c
    }

    /// Состояние после BIOS: режим System, старт с начала ROM.
    pub fn reset(&mut self) {
        self.r = [0; 16];
        self.spsr = [0; 6];
        self.banked = [[0; 2]; 6];
        self.fiq_hi = [0; 5];
        self.usr_hi = [0; 5];
        self.wait_mask = 0;
        self.needs_refill = true;
        self.banked[bank(MODE_IRQ)][0] = 0x0300_7FA0;
        self.banked[bank(MODE_SVC)][0] = 0x0300_7FE0;
        self.r[13] = 0x0300_7F00;
        self.r[15] = 0x0800_0000;
        self.cpsr = MODE_SYS;
    }

    // ---------- Вспомогательное ----------

    #[inline]
    fn thumb(&self) -> bool {
        self.cpsr & FLAG_T != 0
    }

    #[inline]
    fn pcv(&self) -> u32 {
        self.r[15].wrapping_add(if self.thumb() { 2 } else { 4 })
    }

    #[inline]
    fn reg(&self, n: usize) -> u32 {
        if n == 15 { self.pcv() } else { self.r[n] }
    }

    fn set_pc(&mut self, v: u32) {
        self.r[15] = if self.thumb() { v & !1 } else { v & !3 };
    }

    #[inline]
    fn carry(&self) -> bool {
        self.cpsr & FLAG_C != 0
    }

    fn set_flag(&mut self, f: u32, on: bool) {
        if on { self.cpsr |= f } else { self.cpsr &= !f }
    }

    fn set_nz(&mut self, v: u32) {
        self.set_flag(FLAG_N, v >> 31 != 0);
        self.set_flag(FLAG_Z, v == 0);
    }

    fn set_nzcv(&mut self, v: u32, c: bool, o: bool) {
        self.set_nz(v);
        self.set_flag(FLAG_C, c);
        self.set_flag(FLAG_V, o);
    }

    fn cond(&self, c: u32) -> bool {
        let n = self.cpsr & FLAG_N != 0;
        let z = self.cpsr & FLAG_Z != 0;
        let cf = self.cpsr & FLAG_C != 0;
        let v = self.cpsr & FLAG_V != 0;
        match c {
            0 => z,
            1 => !z,
            2 => cf,
            3 => !cf,
            4 => n,
            5 => !n,
            6 => v,
            7 => !v,
            8 => cf && !z,
            9 => !cf || z,
            10 => n == v,
            11 => n != v,
            12 => !z && n == v,
            13 => z || n != v,
            14 => true,
            _ => false,
        }
    }

    // ---------- Режимы ----------

    fn switch_mode(&mut self, new: u32) {
        let old = self.cpsr & 0x1F;
        let new = new & 0x1F;
        let (ob, nb) = (bank(old), bank(new));
        if ob != nb {
            self.banked[ob] = [self.r[13], self.r[14]];
            if old == MODE_FIQ {
                self.fiq_hi.copy_from_slice(&self.r[8..13]);
                self.r[8..13].copy_from_slice(&self.usr_hi);
            }
            if new == MODE_FIQ {
                self.usr_hi.copy_from_slice(&self.r[8..13]);
                self.r[8..13].copy_from_slice(&self.fiq_hi);
            }
            self.r[13] = self.banked[nb][0];
            self.r[14] = self.banked[nb][1];
        }
        self.cpsr = (self.cpsr & !0x1F) | new;
    }

    fn set_cpsr(&mut self, v: u32) {
        self.switch_mode(v & 0x1F);
        self.cpsr = v;
    }

    fn get_spsr(&self) -> u32 {
        match bank(self.cpsr) {
            0 => self.cpsr,
            b => self.spsr[b],
        }
    }

    /// Регистр пользовательского банка (для LDM/STM с битом S).
    fn user_reg(&self, i: usize) -> u32 {
        let mode = self.cpsr & 0x1F;
        match i {
            8..=12 if mode == MODE_FIQ => self.usr_hi[i - 8],
            13 | 14 if bank(mode) != 0 => self.banked[0][i - 13],
            _ => self.r[i],
        }
    }

    fn set_user_reg(&mut self, i: usize, v: u32) {
        let mode = self.cpsr & 0x1F;
        match i {
            8..=12 if mode == MODE_FIQ => self.usr_hi[i - 8] = v,
            13 | 14 if bank(mode) != 0 => self.banked[0][i - 13] = v,
            _ => self.r[i] = v,
        }
    }

    fn enter_irq(&mut self) {
        let lr = self.r[15].wrapping_add(4);
        let old = self.cpsr;
        self.switch_mode(MODE_IRQ);
        self.spsr[bank(MODE_IRQ)] = old;
        self.r[14] = lr;
        self.cpsr = (self.cpsr & !FLAG_T) | FLAG_I;
        self.r[15] = 0x18;
        self.needs_refill = true;
    }

    /// Заново заполняет конвейер с адреса r15 (после перехода, смены режима ARM/Thumb, IRQ).
    fn refill(&mut self, bus: &mut Bus) {
        bus.in_bios = self.r[15] < 0x4000;
        let a = self.r[15];
        self.pipe = if self.thumb() {
            [bus.read16(a) as u32, bus.read16(a.wrapping_add(2)) as u32]
        } else {
            [bus.read32(a), bus.read32(a.wrapping_add(4))]
        };
        self.needs_refill = false;
    }

    // ---------- Главный шаг ----------

    /// Выполняет одну инструкцию (или вход в IRQ), возвращает число циклов.
    pub fn step(&mut self, bus: &mut Bus) -> u32 {
        if bus.halted {
            if bus.irq_asserted() {
                bus.halted = false;
            } else {
                return 4;
            }
        }
        if self.cpsr & FLAG_I == 0 && bus.ime() && bus.irq_asserted() {
            self.enter_irq();
            return 3;
        }
        if self.wait_mask != 0 && self.r[15] == self.wait_pc {
            // IntrWait: ждём, пока обработчик пользователя выставит флаги в [0x03007FF8]
            let flags = bus.read16(BIOS_IF);
            if flags & self.wait_mask != 0 {
                bus.write16(BIOS_IF, flags & !self.wait_mask);
                self.wait_mask = 0;
            } else {
                bus.halted = true;
                return 4;
            }
        }
        bus.in_bios = self.r[15] < 0x4000;
        if self.needs_refill {
            self.refill(bus);
        }
        let thumb = self.thumb();
        let size: u32 = if thumb { 2 } else { 4 };
        let addr = self.r[15];
        let op = self.pipe[0];
        self.pipe[0] = self.pipe[1];
        self.r[15] = addr.wrapping_add(size);
        // выборка через одну инструкцию вперёд происходит ДО выполнения текущей
        let fetch = addr.wrapping_add(2 * size);
        self.pipe[1] = if thumb { bus.read16(fetch) as u32 } else { bus.read32(fetch) };
        if bus.in_bios && !thumb {
            bus.bios_latch = self.pipe[1]; // последняя выборка из BIOS
        }
        let seq_pc = self.r[15];
        let cycles = if thumb { self.exec_thumb(bus, op) } else { self.exec_arm(bus, op) };
        if self.r[15] != seq_pc || self.thumb() != thumb {
            self.needs_refill = true; // был переход или смена режима
        }
        cycles
    }

    fn undefined(&mut self) -> u32 {
        // TODO: исключение UND (вектор 0x04). Пока — NOP.
        1
    }

    /// HLE BIOS. Звуковые функции (0x19-0x2A) пока не реализованы.
    fn swi(&mut self, bus: &mut Bus, n: u32) -> u32 {
        bus.bios_latch = 0xE3A0_2004; // что видит игра, читая BIOS после SWI
        let (r0, r1, r2, r3) = (self.r[0], self.r[1], self.r[2], self.r[3]);
        match n {
            0x00 => self.soft_reset(bus),
            0x01 => bios::register_ram_reset(bus, r0),
            0x02 | 0x03 => bus.halted = true, // Halt / Stop
            0x04 => self.intr_wait(bus, r0 != 0, r1 as u16),
            0x05 => self.intr_wait(bus, true, 1), // VBlankIntrWait
            0x06 | 0x07 => {
                let (num, den) = if n == 6 { (r0 as i32, r1 as i32) } else { (r1 as i32, r0 as i32) };
                let (q, m, abs) = bios::div(num, den);
                self.r[0] = q as u32;
                self.r[1] = m as u32;
                self.r[3] = abs;
            }
            0x08 => self.r[0] = bios::sqrt(r0),
            0x09 => self.r[0] = bios::arctan(r0 as i16 as i32) as u32,
            0x0A => self.r[0] = bios::arctan2(r0 as i16 as i32, r1 as i16 as i32),
            0x0B => bios::cpu_set(bus, r0, r1, r2),
            0x0C => bios::cpu_fast_set(bus, r0, r1, r2),
            0x0D => self.r[0] = 0xBAAE_187F, // GetBiosChecksum
            0x0E => bios::bg_affine_set(bus, r0, r1, r2),
            0x0F => bios::obj_affine_set(bus, r0, r1, r2, r3),
            0x10 => bios::bit_unpack(bus, r0, r1, r2),
            0x11 => bios::lz77(bus, r0, r1, false),
            0x12 => bios::lz77(bus, r0, r1, true),
            0x13 => bios::huff_uncomp(bus, r0, r1),
            0x14 => bios::rl_uncomp(bus, r0, r1, false),
            0x15 => bios::rl_uncomp(bus, r0, r1, true),
            0x16 => bios::diff_unfilter(bus, r0, r1, false, false),
            0x17 => bios::diff_unfilter(bus, r0, r1, false, true),
            0x18 => bios::diff_unfilter(bus, r0, r1, true, true),
            _ => {} // TODO: звук (SoundBias, SoundDriver*, MidiKey2Freq)
        }
        3
    }

    fn soft_reset(&mut self, bus: &mut Bus) {
        let to_ram = bus.read8(0x0300_7FFA) != 0;
        bus.iwram[0x7E00..].fill(0);
        self.reset();
        if to_ram {
            self.r[15] = 0x0200_0000;
        }
    }

    /// IntrWait: сбрасывает старые флаги (если нужно), включает IE/IME,
    /// а дальше step() усыпляет CPU, пока в [0x03007FF8] не появится нужный бит.
    fn intr_wait(&mut self, bus: &mut Bus, discard: bool, mask: u16) {
        if discard {
            let f = bus.read16(BIOS_IF);
            bus.write16(BIOS_IF, f & !mask);
        }
        let ie = bus.read16(0x0400_0200);
        bus.write16(0x0400_0200, ie | mask);
        bus.write16(0x0400_0208, 1);
        self.wait_mask = mask;
        self.wait_pc = self.r[15];
    }

    // ================= ARM =================

    fn exec_arm(&mut self, bus: &mut Bus, op: u32) -> u32 {
        if !self.cond(op >> 28) {
            return 1;
        }
        match (op >> 25) & 7 {
            0 => {
                if op & 0x0FFF_FFF0 == 0x012F_FF10 {
                    return self.arm_bx(op);
                }
                if op & 0x90 == 0x90 {
                    if op & 0x60 != 0 {
                        return self.arm_half(bus, op);
                    }
                    if op & 0x0FC0_00F0 == 0x0000_0090 {
                        return self.arm_mul(op);
                    }
                    if op & 0x0F80_00F0 == 0x0080_0090 {
                        return self.arm_mull(op);
                    }
                    if op & 0x0FB0_0FF0 == 0x0100_0090 {
                        return self.arm_swp(bus, op);
                    }
                    return self.undefined();
                }
                self.arm_dp(op)
            }
            1 => self.arm_dp(op),
            2 => self.arm_sdt(bus, op),
            3 => {
                if op & 0x10 != 0 { self.undefined() } else { self.arm_sdt(bus, op) }
            }
            4 => self.arm_block(bus, op),
            5 => {
                let off = (((op & 0x00FF_FFFF) << 8) as i32 >> 6) as u32;
                if op & (1 << 24) != 0 {
                    self.r[14] = self.r[15];
                }
                self.r[15] = self.r[15].wrapping_add(4).wrapping_add(off);
                3
            }
            6 => self.undefined(),
            _ => {
                if op & (1 << 24) != 0 {
                    self.swi(bus, (op >> 16) & 0xFF)
                } else {
                    self.undefined()
                }
            }
        }
    }

    fn arm_bx(&mut self, op: u32) -> u32 {
        let v = self.reg((op & 0xF) as usize);
        if v & 1 != 0 {
            self.cpsr |= FLAG_T;
            self.r[15] = v & !1;
        } else {
            self.cpsr &= !FLAG_T;
            self.r[15] = v & !3;
        }
        3
    }

    fn arm_dp(&mut self, op: u32) -> u32 {
        let opc = (op >> 21) & 0xF;
        let s = op & (1 << 20) != 0;
        if (8..=11).contains(&opc) && !s {
            return self.arm_psr(op);
        }
        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;
        let c_in = self.carry();
        let reg_shift = op & (1 << 25) == 0 && op & 0x10 != 0;

        let (b, sc) = if op & (1 << 25) != 0 {
            let rot = (op >> 8) & 0xF;
            let v = (op & 0xFF).rotate_right(rot * 2);
            (v, if rot != 0 { v >> 31 != 0 } else { c_in })
        } else {
            let rm = (op & 0xF) as usize;
            let kind = (op >> 5) & 3;
            if reg_shift {
                // при сдвиге по регистру PC читается как адрес + 12
                let amt = self.reg(((op >> 8) & 0xF) as usize) & 0xFF;
                let val = self.reg(rm).wrapping_add(if rm == 15 { 4 } else { 0 });
                shift_op(kind, val, amt, true, c_in)
            } else {
                shift_op(kind, self.reg(rm), (op >> 7) & 0x1F, false, c_in)
            }
        };
        let a = self.reg(rn).wrapping_add(if reg_shift && rn == 15 { 4 } else { 0 });
        let ov = self.cpsr & FLAG_V != 0;
        let ci = c_in as u32;

        let (res, cf, vf) = match opc {
            0 | 8 => (a & b, sc, ov),
            1 | 9 => (a ^ b, sc, ov),
            2 | 10 => add_c(a, !b, 1),
            3 => add_c(b, !a, 1),
            4 | 11 => add_c(a, b, 0),
            5 => add_c(a, b, ci),
            6 => add_c(a, !b, ci),
            7 => add_c(b, !a, ci),
            12 => (a | b, sc, ov),
            13 => (b, sc, ov),
            14 => (a & !b, sc, ov),
            _ => (!b, sc, ov),
        };

        let writes = !(8..=11).contains(&opc);
        if s {
            if rd == 15 {
                // у TSTP/TEQP/CMPP/CMNP результат не пишется, но SPSR -> CPSR
                let spsr = self.get_spsr();
                self.set_cpsr(spsr);
            } else {
                self.set_nzcv(res, cf, vf);
            }
        }
        if writes {
            if rd == 15 {
                self.set_pc(res);
            } else {
                self.r[rd] = res;
            }
        }
        if rd == 15 && writes { 3 } else if reg_shift { 2 } else { 1 }
    }

    fn arm_psr(&mut self, op: u32) -> u32 {
        let use_spsr = op & (1 << 22) != 0;
        if op & (1 << 21) == 0 {
            // MRS
            let rd = ((op >> 12) & 0xF) as usize;
            self.r[rd] = if use_spsr { self.get_spsr() } else { self.cpsr };
        } else {
            // MSR
            let val = if op & (1 << 25) != 0 {
                (op & 0xFF).rotate_right(((op >> 8) & 0xF) * 2)
            } else {
                self.reg((op & 0xF) as usize)
            };
            let mut mask = 0u32;
            if op & (1 << 19) != 0 { mask |= 0xFF00_0000; }
            if op & (1 << 18) != 0 { mask |= 0x00FF_0000; }
            if op & (1 << 17) != 0 { mask |= 0x0000_FF00; }
            if op & (1 << 16) != 0 { mask |= 0x0000_00FF; }
            if use_spsr {
                let b = bank(self.cpsr);
                if b != 0 {
                    self.spsr[b] = (self.spsr[b] & !mask) | (val & mask);
                }
            } else {
                if self.cpsr & 0x1F == MODE_USR {
                    mask &= 0xF000_0000;
                }
                let new = (self.cpsr & !mask) | (val & mask);
                self.set_cpsr(new);
            }
        }
        1
    }

    fn arm_mul(&mut self, op: u32) -> u32 {
        let rd = ((op >> 16) & 0xF) as usize;
        let rn = ((op >> 12) & 0xF) as usize;
        let rs = ((op >> 8) & 0xF) as usize;
        let rm = (op & 0xF) as usize;
        let mut res = self.reg(rm).wrapping_mul(self.reg(rs));
        if op & (1 << 21) != 0 {
            res = res.wrapping_add(self.reg(rn));
        }
        self.r[rd] = res;
        if op & (1 << 20) != 0 {
            self.set_nz(res);
        }
        3
    }

    fn arm_mull(&mut self, op: u32) -> u32 {
        let signed = op & (1 << 22) != 0;
        let acc = op & (1 << 21) != 0;
        let hi = ((op >> 16) & 0xF) as usize;
        let lo = ((op >> 12) & 0xF) as usize;
        let a = self.reg((op & 0xF) as usize);
        let b = self.reg(((op >> 8) & 0xF) as usize);
        let mut res: u64 = if signed {
            (a as i32 as i64).wrapping_mul(b as i32 as i64) as u64
        } else {
            (a as u64).wrapping_mul(b as u64)
        };
        if acc {
            res = res.wrapping_add(((self.r[hi] as u64) << 32) | self.r[lo] as u64);
        }
        self.r[lo] = res as u32;
        self.r[hi] = (res >> 32) as u32;
        if op & (1 << 20) != 0 {
            self.set_flag(FLAG_N, res >> 63 != 0);
            self.set_flag(FLAG_Z, res == 0);
        }
        4
    }

    fn arm_swp(&mut self, bus: &mut Bus, op: u32) -> u32 {
        let byte = op & (1 << 22) != 0;
        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;
        let src = self.reg((op & 0xF) as usize);
        let addr = self.reg(rn);
        if byte {
            let t = bus.read8(addr) as u32;
            bus.write8(addr, src as u8);
            self.r[rd] = t;
        } else {
            let t = bus.read32(addr).rotate_right((addr & 3) * 8);
            bus.write32(addr, src);
            self.r[rd] = t;
        }
        4
    }

    /// LDR/STR/LDRB/STRB
    fn arm_sdt(&mut self, bus: &mut Bus, op: u32) -> u32 {
        let reg_off = op & (1 << 25) != 0;
        let pre = op & (1 << 24) != 0;
        let up = op & (1 << 23) != 0;
        let byte = op & (1 << 22) != 0;
        let wb = op & (1 << 21) != 0;
        let load = op & (1 << 20) != 0;
        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;

        let off = if reg_off {
            let rm = (op & 0xF) as usize;
            shift_op((op >> 5) & 3, self.reg(rm), (op >> 7) & 0x1F, false, self.carry()).0
        } else {
            op & 0xFFF
        };
        let base = self.reg(rn);
        let applied = if up { base.wrapping_add(off) } else { base.wrapping_sub(off) };
        let addr = if pre { applied } else { base };
        let writeback = !pre || wb;

        if load {
            let v = if byte {
                bus.read8(addr) as u32
            } else {
                bus.read32(addr).rotate_right((addr & 3) * 8)
            };
            if writeback && rn != rd {
                self.r[rn] = applied;
            }
            if rd == 15 {
                self.set_pc(v);
                5
            } else {
                self.r[rd] = v;
                3
            }
        } else {
            let v = self.reg(rd).wrapping_add(if rd == 15 { 4 } else { 0 });
            if byte {
                bus.write8(addr, v as u8);
            } else {
                bus.write32(addr, v);
            }
            if writeback {
                self.r[rn] = applied;
            }
            2
        }
    }

    /// LDRH/STRH/LDRSB/LDRSH
    fn arm_half(&mut self, bus: &mut Bus, op: u32) -> u32 {
        let pre = op & (1 << 24) != 0;
        let up = op & (1 << 23) != 0;
        let imm = op & (1 << 22) != 0;
        let wb = op & (1 << 21) != 0;
        let load = op & (1 << 20) != 0;
        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;
        let sh = (op >> 5) & 3;

        let off = if imm {
            ((op >> 4) & 0xF0) | (op & 0xF)
        } else {
            self.reg((op & 0xF) as usize)
        };
        let base = self.reg(rn);
        let applied = if up { base.wrapping_add(off) } else { base.wrapping_sub(off) };
        let addr = if pre { applied } else { base };
        let writeback = !pre || wb;

        if load {
            let v = match sh {
                1 => ldrh(bus, addr),
                2 => bus.read8(addr) as i8 as i32 as u32,
                _ => ldrsh(bus, addr),
            };
            if writeback && rn != rd {
                self.r[rn] = applied;
            }
            if rd == 15 {
                self.set_pc(v);
                5
            } else {
                self.r[rd] = v;
                3
            }
        } else {
            if sh == 1 {
                let v = self.reg(rd).wrapping_add(if rd == 15 { 4 } else { 0 });
                bus.write16(addr, v as u16);
            }
            if writeback {
                self.r[rn] = applied;
            }
            2
        }
    }

    /// LDM/STM
    fn arm_block(&mut self, bus: &mut Bus, op: u32) -> u32 {
        let pre = op & (1 << 24) != 0;
        let up = op & (1 << 23) != 0;
        let s = op & (1 << 22) != 0;
        let wb = op & (1 << 21) != 0;
        let load = op & (1 << 20) != 0;
        let rn = ((op >> 16) & 0xF) as usize;
        let mut list = op & 0xFFFF;
        let mut n = list.count_ones();
        if list == 0 {
            // пустой список: передаётся только r15, база сдвигается на 0x40
            list = 0x8000;
            n = 16;
        }
        let base = self.reg(rn);
        let (start, final_base) = if up {
            (if pre { base.wrapping_add(4) } else { base }, base.wrapping_add(4 * n))
        } else {
            let lo = base.wrapping_sub(4 * n);
            (if pre { lo } else { lo.wrapping_add(4) }, lo)
        };
        let mut addr = start & !3;
        let user_bank = s && !(load && list & 0x8000 != 0);
        let first = list.trailing_zeros() as usize;

        if load && wb {
            self.r[rn] = final_base;
        }
        for i in 0..16usize {
            if list & (1 << i) == 0 {
                continue;
            }
            if load {
                let v = bus.read32(addr);
                if i == 15 {
                    if s {
                        let spsr = self.get_spsr();
                        self.set_cpsr(spsr);
                    }
                    self.set_pc(v);
                } else if user_bank {
                    self.set_user_reg(i, v);
                } else {
                    self.r[i] = v;
                }
            } else {
                let mut v = if user_bank {
                    self.user_reg(i)
                } else if i == 15 {
                    self.reg(15).wrapping_add(4)
                } else {
                    self.r[i]
                };
                if i == rn && wb && i != first {
                    v = final_base;
                }
                bus.write32(addr, v);
            }
            addr = addr.wrapping_add(4);
        }
        if !load && wb {
            self.r[rn] = final_base;
        }
        if load {
            n + 2 + if list & 0x8000 != 0 { 2 } else { 0 }
        } else {
            n + 1
        }
    }

    // ================= Thumb =================

    fn exec_thumb(&mut self, bus: &mut Bus, op: u32) -> u32 {
        match op >> 11 {
            // 1: сдвиг на константу
            0..=2 => {
                let rd = (op & 7) as usize;
                let rs = ((op >> 3) & 7) as usize;
                let (v, c) = shift_op((op >> 11) & 3, self.r[rs], (op >> 6) & 0x1F, false, self.carry());
                self.r[rd] = v;
                self.set_nz(v);
                self.set_flag(FLAG_C, c);
                1
            }
            // 2: ADD/SUB reg/imm3
            3 => {
                let rd = (op & 7) as usize;
                let a = self.r[((op >> 3) & 7) as usize];
                let f = (op >> 6) & 7;
                let b = if op & 0x400 != 0 { f } else { self.r[f as usize] };
                let (r, c, v) = if op & 0x200 != 0 { add_c(a, !b, 1) } else { add_c(a, b, 0) };
                self.r[rd] = r;
                self.set_nzcv(r, c, v);
                1
            }
            // 3: MOV/CMP/ADD/SUB imm8
            4..=7 => {
                let rd = ((op >> 8) & 7) as usize;
                let imm = op & 0xFF;
                let a = self.r[rd];
                match (op >> 11) & 3 {
                    0 => {
                        self.r[rd] = imm;
                        self.set_nz(imm);
                    }
                    1 => {
                        let (r, c, v) = add_c(a, !imm, 1);
                        self.set_nzcv(r, c, v);
                    }
                    2 => {
                        let (r, c, v) = add_c(a, imm, 0);
                        self.r[rd] = r;
                        self.set_nzcv(r, c, v);
                    }
                    _ => {
                        let (r, c, v) = add_c(a, !imm, 1);
                        self.r[rd] = r;
                        self.set_nzcv(r, c, v);
                    }
                }
                1
            }
            // 4/5: ALU и операции с высокими регистрами
            8 => {
                if op & 0x400 == 0 { self.thumb_alu(op) } else { self.thumb_hi(op) }
            }
            // 6: LDR rd, [pc, #imm]
            9 => {
                let rd = ((op >> 8) & 7) as usize;
                let addr = (self.pcv() & !2).wrapping_add((op & 0xFF) << 2);
                self.r[rd] = bus.read32(addr);
                3
            }
            // 7/8: load/store с регистровым смещением
            10 | 11 => {
                let rd = (op & 7) as usize;
                let rb = ((op >> 3) & 7) as usize;
                let ro = ((op >> 6) & 7) as usize;
                let addr = self.r[rb].wrapping_add(self.r[ro]);
                let load = if op & 0x200 == 0 { op & 0x800 != 0 } else { op & 0xC00 != 0 };
                if op & 0x200 == 0 {
                    match (op & 0x800 != 0, op & 0x400 != 0) {
                        (false, false) => bus.write32(addr, self.r[rd]),
                        (false, true) => bus.write8(addr, self.r[rd] as u8),
                        (true, false) => self.r[rd] = bus.read32(addr).rotate_right((addr & 3) * 8),
                        (true, true) => self.r[rd] = bus.read8(addr) as u32,
                    }
                } else {
                    // (S, H)
                    match (op & 0x400 != 0, op & 0x800 != 0) {
                        (false, false) => bus.write16(addr, self.r[rd] as u16),
                        (false, true) => self.r[rd] = ldrh(bus, addr),
                        (true, false) => self.r[rd] = bus.read8(addr) as i8 as i32 as u32,
                        (true, true) => self.r[rd] = ldrsh(bus, addr),
                    }
                }
                if load { 3 } else { 2 }
            }
            // 9: load/store с константным смещением (слово/байт)
            12..=15 => {
                let rd = (op & 7) as usize;
                let rb = ((op >> 3) & 7) as usize;
                let off = (op >> 6) & 0x1F;
                let byte = op & 0x1000 != 0;
                let load = op & 0x800 != 0;
                let addr = self.r[rb].wrapping_add(if byte { off } else { off << 2 });
                match (load, byte) {
                    (false, false) => bus.write32(addr, self.r[rd]),
                    (false, true) => bus.write8(addr, self.r[rd] as u8),
                    (true, false) => self.r[rd] = bus.read32(addr).rotate_right((addr & 3) * 8),
                    (true, true) => self.r[rd] = bus.read8(addr) as u32,
                }
                if load { 3 } else { 2 }
            }
            // 10: LDRH/STRH imm
            16 | 17 => {
                let rd = (op & 7) as usize;
                let rb = ((op >> 3) & 7) as usize;
                let addr = self.r[rb].wrapping_add(((op >> 6) & 0x1F) << 1);
                if op & 0x800 != 0 {
                    self.r[rd] = ldrh(bus, addr);
                    3
                } else {
                    bus.write16(addr, self.r[rd] as u16);
                    2
                }
            }
            // 11: SP-relative load/store
            18 | 19 => {
                let rd = ((op >> 8) & 7) as usize;
                let addr = self.r[13].wrapping_add((op & 0xFF) << 2);
                if op & 0x800 != 0 {
                    self.r[rd] = bus.read32(addr).rotate_right((addr & 3) * 8);
                    3
                } else {
                    bus.write32(addr, self.r[rd]);
                    2
                }
            }
            // 12: ADD rd, pc/sp, #imm
            20 | 21 => {
                let rd = ((op >> 8) & 7) as usize;
                let base = if op & 0x800 != 0 { self.r[13] } else { self.pcv() & !2 };
                self.r[rd] = base.wrapping_add((op & 0xFF) << 2);
                1
            }
            22 | 23 => {
                if op & 0x0F00 == 0 {
                    // 13: ADD sp, #imm
                    let off = (op & 0x7F) << 2;
                    self.r[13] = if op & 0x80 != 0 {
                        self.r[13].wrapping_sub(off)
                    } else {
                        self.r[13].wrapping_add(off)
                    };
                    1
                } else if op & 0x0600 == 0x0400 {
                    self.thumb_push_pop(bus, op)
                } else {
                    self.undefined()
                }
            }
            // 15: STMIA/LDMIA
            24 | 25 => self.thumb_block(bus, op),
            // 16/17: условный переход / SWI
            26 | 27 => {
                let c = (op >> 8) & 0xF;
                if c == 0xF {
                    self.swi(bus, op & 0xFF)
                } else if c == 0xE {
                    self.undefined()
                } else if self.cond(c) {
                    let off = ((op & 0xFF) as i8 as i32 * 2) as u32;
                    self.r[15] = self.pcv().wrapping_add(off);
                    3
                } else {
                    1
                }
            }
            // 18: безусловный переход
            28 => {
                let off = (((op & 0x7FF) << 21) as i32 >> 20) as u32;
                self.r[15] = self.pcv().wrapping_add(off);
                3
            }
            // 19: BL (две половины)
            30 => {
                let off = (((op & 0x7FF) << 21) as i32 >> 9) as u32;
                self.r[14] = self.pcv().wrapping_add(off);
                1
            }
            31 => {
                let target = self.r[14].wrapping_add((op & 0x7FF) << 1);
                self.r[14] = self.r[15] | 1;
                self.r[15] = target & !1;
                3
            }
            _ => self.undefined(),
        }
    }

    fn thumb_alu(&mut self, op: u32) -> u32 {
        let rd = (op & 7) as usize;
        let rs = ((op >> 3) & 7) as usize;
        let (a, b) = (self.r[rd], self.r[rs]);
        let cin = self.carry();
        let ci = cin as u32;
        let code = (op >> 6) & 0xF;
        let mut write = true;
        let res = match code {
            0 => { let r = a & b; self.set_nz(r); r }
            1 => { let r = a ^ b; self.set_nz(r); r }
            2 | 3 | 4 | 7 => {
                let kind = match code { 2 => 0, 3 => 1, 4 => 2, _ => 3 };
                let (r, c) = shift_op(kind, a, b & 0xFF, true, cin);
                self.set_nz(r);
                self.set_flag(FLAG_C, c);
                r
            }
            5 => { let (r, c, v) = add_c(a, b, ci); self.set_nzcv(r, c, v); r }
            6 => { let (r, c, v) = add_c(a, !b, ci); self.set_nzcv(r, c, v); r }
            8 => { write = false; self.set_nz(a & b); 0 }
            9 => { let (r, c, v) = add_c(0, !b, 1); self.set_nzcv(r, c, v); r }
            10 => { write = false; let (r, c, v) = add_c(a, !b, 1); self.set_nzcv(r, c, v); 0 }
            11 => { write = false; let (r, c, v) = add_c(a, b, 0); self.set_nzcv(r, c, v); 0 }
            12 => { let r = a | b; self.set_nz(r); r }
            13 => { let r = a.wrapping_mul(b); self.set_nz(r); r }
            14 => { let r = a & !b; self.set_nz(r); r }
            _ => { let r = !b; self.set_nz(r); r }
        };
        if write {
            self.r[rd] = res;
        }
        match code {
            2 | 3 | 4 | 7 => 2,
            13 => 3,
            _ => 1,
        }
    }

    fn thumb_hi(&mut self, op: u32) -> u32 {
        let rd = ((op & 7) | ((op >> 4) & 8)) as usize;
        let rs = ((op >> 3) & 0xF) as usize;
        match (op >> 8) & 3 {
            0 => {
                let v = self.reg(rd).wrapping_add(self.reg(rs));
                if rd == 15 { self.r[15] = v & !1; 3 } else { self.r[rd] = v; 1 }
            }
            1 => {
                let (r, c, v) = add_c(self.reg(rd), !self.reg(rs), 1);
                self.set_nzcv(r, c, v);
                1
            }
            2 => {
                let v = self.reg(rs);
                if rd == 15 { self.r[15] = v & !1; 3 } else { self.r[rd] = v; 1 }
            }
            _ => {
                let v = self.reg(rs);
                if v & 1 != 0 {
                    self.r[15] = v & !1;
                } else {
                    self.cpsr &= !FLAG_T;
                    self.r[15] = v & !3;
                }
                3
            }
        }
    }

    fn thumb_push_pop(&mut self, bus: &mut Bus, op: u32) -> u32 {
        let load = op & 0x800 != 0;
        let extra = op & 0x100 != 0;
        let list = op & 0xFF;
        let n = list.count_ones() + extra as u32;
        if !load {
            let mut addr = self.r[13].wrapping_sub(4 * n);
            self.r[13] = addr;
            for i in 0..8usize {
                if list & (1 << i) != 0 {
                    bus.write32(addr, self.r[i]);
                    addr = addr.wrapping_add(4);
                }
            }
            if extra {
                bus.write32(addr, self.r[14]);
            }
            n + 1
        } else {
            let mut addr = self.r[13];
            for i in 0..8usize {
                if list & (1 << i) != 0 {
                    self.r[i] = bus.read32(addr);
                    addr = addr.wrapping_add(4);
                }
            }
            if extra {
                let v = bus.read32(addr);
                addr = addr.wrapping_add(4);
                self.r[15] = v & !1;
            }
            self.r[13] = addr;
            n + 2 + extra as u32 * 2
        }
    }

    fn thumb_block(&mut self, bus: &mut Bus, op: u32) -> u32 {
        let rb = ((op >> 8) & 7) as usize;
        let list = op & 0xFF;
        let load = op & 0x800 != 0;
        let base = self.r[rb];
        if list == 0 {
            // пустой список: передаётся r15, база += 0x40
            if load {
                self.r[15] = bus.read32(base) & !1;
            } else {
                bus.write32(base, self.pcv().wrapping_add(2));
            }
            self.r[rb] = base.wrapping_add(0x40);
            return 3;
        }
        let n = list.count_ones();
        let final_base = base.wrapping_add(4 * n);
        let first = list.trailing_zeros() as usize;
        let mut addr = base;
        for i in 0..8usize {
            if list & (1 << i) == 0 {
                continue;
            }
            if load {
                self.r[i] = bus.read32(addr);
            } else {
                let v = if i == rb && i != first { final_base } else { self.r[i] };
                bus.write32(addr, v);
            }
            addr = addr.wrapping_add(4);
        }
        if !load || list & (1 << rb) == 0 {
            self.r[rb] = final_base;
        }
        if load { n + 2 } else { n + 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(words: &[u32]) -> (Cpu, Bus) {
        let mut bus = Bus::new();
        bus.rom = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        (Cpu::new(), bus)
    }

    fn run(cpu: &mut Cpu, bus: &mut Bus, n: usize) {
        for _ in 0..n {
            cpu.step(bus);
        }
    }

    #[test]
    fn arm_alu_and_flags() {
        let (mut cpu, mut bus) = setup(&[
            0xE3A0_0005, // mov  r0, #5
            0xE280_1003, // add  r1, r0, #3
            0xE251_2008, // subs r2, r1, #8
        ]);
        run(&mut cpu, &mut bus, 3);
        assert_eq!(cpu.r[0], 5);
        assert_eq!(cpu.r[1], 8);
        assert_eq!(cpu.r[2], 0);
        assert!(cpu.cpsr & FLAG_Z != 0 && cpu.cpsr & FLAG_C != 0);
    }

    #[test]
    fn arm_branch_link() {
        let (mut cpu, mut bus) = setup(&[0xEB00_0000]); // bl +0
        run(&mut cpu, &mut bus, 1);
        assert_eq!(cpu.r[15], 0x0800_0008);
        assert_eq!(cpu.r[14], 0x0800_0004);
    }

    #[test]
    fn arm_store_load() {
        let (mut cpu, mut bus) = setup(&[
            0xE3A0_1403, // mov r1, #0x03000000
            0xE3A0_0042, // mov r0, #0x42
            0xE581_0000, // str r0, [r1]
            0xE591_2000, // ldr r2, [r1]
        ]);
        run(&mut cpu, &mut bus, 4);
        assert_eq!(bus.read32(0x0300_0000), 0x42);
        assert_eq!(cpu.r[2], 0x42);
    }

    #[test]
    fn bx_to_thumb_and_run() {
        let (mut cpu, mut bus) = setup(&[
            0xE3A0_0302, // mov r0, #0x08000000
            0xE280_0011, // add r0, r0, #0x11
            0xE12F_FF10, // bx  r0
            0xE1A0_0000, // nop
            0x3103_2107, // movs r1, #7 ; adds r1, #3
        ]);
        run(&mut cpu, &mut bus, 5);
        assert!(cpu.thumb());
        assert_eq!(cpu.r[1], 10);
    }

    #[test]
    fn swi_div_and_cpuset() {
        let (mut cpu, mut bus) = setup(&[0xEF06_0000, 0xEF0B_0000]); // swi 6; swi 0x0B
        cpu.r[0] = (-7i32) as u32;
        cpu.r[1] = 2;
        run(&mut cpu, &mut bus, 1);
        assert_eq!((cpu.r[0] as i32, cpu.r[1] as i32, cpu.r[3]), (-3, -1, 3));

        bus.write32(0x0200_0000, 0xDEAD_BEEF);
        cpu.r[0] = 0x0200_0000;
        cpu.r[1] = 0x0200_0100;
        cpu.r[2] = 1 | (1 << 26); // 1 слово
        run(&mut cpu, &mut bus, 1);
        assert_eq!(bus.read32(0x0200_0100), 0xDEAD_BEEF);
    }

    #[test]
    fn vblank_intr_wait_sleeps_until_bios_flag() {
        let (mut cpu, mut bus) = setup(&[0xEF05_0000, 0xE3A0_1001]); // swi 5; mov r1, #1
        run(&mut cpu, &mut bus, 2); // swi, затем засыпаем
        assert!(bus.halted);
        assert_eq!(cpu.r[1], 0);
        cpu.cpsr |= FLAG_I; // обработчик не вызываем, проверяем только IntrWait
        bus.raise_irq(1);
        bus.write16(BIOS_IF, 1); // как делает IRQ-обработчик игры
        run(&mut cpu, &mut bus, 1);
        assert_eq!(cpu.r[1], 1);
        assert_eq!(bus.read16(BIOS_IF), 0);
    }

    #[test]
    fn irq_handler_runs_while_waiting_in_intr_wait() {
        let (mut cpu, mut bus) = setup(&[0xEF05_0000, 0xE3A0_1001]); // swi 5; mov r1, #1
        run(&mut cpu, &mut bus, 2); // swi, затем засыпаем в ожидании
        assert!(bus.halted);
        bus.raise_irq(1); // VBlank; IME и IE уже включены вызовом IntrWait
        run(&mut cpu, &mut bus, 1); // вход в IRQ
        assert_eq!(cpu.r[15], 0x18);
        run(&mut cpu, &mut bus, 1); // обязан выполниться вектор (b 0x128), а не заснуть снова
        assert_eq!(cpu.r[15], 0x128);
        assert_eq!(cpu.r[1], 0);
    }

    #[test]
    fn irq_entry() {
        let (mut cpu, mut bus) = setup(&[0xE1A0_0000]);
        bus.write16(0x0400_0208, 1); // IME
        bus.write16(0x0400_0200, 1); // IE: VBlank
        bus.raise_irq(1);
        run(&mut cpu, &mut bus, 1);
        assert_eq!(cpu.r[15], 0x18);
        assert_eq!(cpu.cpsr & 0x1F, MODE_IRQ);
        assert_eq!(cpu.r[14], 0x0800_0004);
    }
}
