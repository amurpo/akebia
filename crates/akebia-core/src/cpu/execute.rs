//! Instruction decoding and execution.
//!
//! # How to read this file
//!
//! The SM83 instruction set is **not** an arbitrary list of 256 entries: it is
//! a table with structure. Every opcode decomposes like this:
//!
//! ```text
//!   7 6   5 4 3   2 1 0
//!  ┌─────┬───────┬───────┐
//!  │block│   y   │   z   │
//!  └─────┴───────┴───────┘
//!          p   q  (y = p*2 + q)
//! ```
//!
//! - **block 1** (`0x40..0x80`): `LD y, z` — 64 opcodes with a single line of code.
//! - **block 2** (`0x80..0xC0`): ALU operation `y` on operand `z`.
//! - **blocks 0 and 3**: irregular, decoded case by case.
//!
//! Code `z = 6` (or `y = 6`) does not designate a register but `(HL)`, a memory
//! access that costs one extra M-cycle. That is why [`R8::from_code`] returns an
//! `Option`.
//!
//! Exploiting the structure avoids writing —and getting wrong— 500 `match` arms
//! by hand.
//!
//! # Coverage
//!
//! The instruction set is complete: all 245 legal unprefixed opcodes and the 256
//! of the `0xCB` prefix (see [`super::prefix_cb`]). The 11 opcodes the SM83 does
//! not define return [`Fault::Illegal`]; the `no_opcode_is_left_unimplemented`
//! test verifies that exhaustively.
//!
//! [`Fault::Unimplemented`] still exists as a safety net for the `_` arm of the
//! `match`, but it should never happen any more.

use super::{Bus, Cpu, Fault, Power, Registers, R16, R8};

/// Condition of a conditional jump, call or return (bits 4-3 of the opcode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cond {
    Nz,
    Z,
    Nc,
    C,
}

impl Cond {
    const fn from_code(code: u8) -> Self {
        match code & 0x03 {
            0 => Self::Nz,
            1 => Self::Z,
            2 => Self::Nc,
            _ => Self::C,
        }
    }

    fn holds(self, regs: &Registers) -> bool {
        match self {
            Self::Nz => !regs.f.z(),
            Self::Z => regs.f.z(),
            Self::Nc => !regs.f.c(),
            Self::C => regs.f.c(),
        }
    }
}

/// 16-bit pair encoded in bits 5-4, the `BC/DE/HL/SP` family.
const fn r16_sp(code: u8) -> R16 {
    match (code >> 4) & 0x03 {
        0 => R16::Bc,
        1 => R16::De,
        2 => R16::Hl,
        _ => R16::Sp,
    }
}

/// The same, except the `PUSH`/`POP` family uses `AF` where the other uses `SP`.
const fn r16_af(code: u8) -> R16 {
    match (code >> 4) & 0x03 {
        0 => R16::Bc,
        1 => R16::De,
        2 => R16::Hl,
        _ => R16::Af,
    }
}

/// The eleven opcodes the SM83 does not define. On real hardware they lock up
/// the CPU.
const ILLEGAL: [u8; 11] = [0xD3, 0xDB, 0xDD, 0xE3, 0xE4, 0xEB, 0xEC, 0xED, 0xF4, 0xFC, 0xFD];

impl Cpu {
    /// Reads the 8-bit operand designated by `code`: a register, or `(HL)`.
    pub(super) fn read_operand(&mut self, bus: &mut impl Bus, code: u8) -> u8 {
        match R8::from_code(code) {
            Some(r) => self.regs.read8(r),
            None => bus.read(self.regs.hl()),
        }
    }

    pub(super) fn write_operand(&mut self, bus: &mut impl Bus, code: u8, value: u8) {
        match R8::from_code(code) {
            Some(r) => self.regs.write8(r, value),
            None => bus.write(self.regs.hl(), value),
        }
    }

    /// Extra cost in M-cycles for using `(HL)` instead of a register.
    pub(super) const fn hl_penalty(code: u8) -> u32 {
        if R8::from_code(code).is_none() {
            1
        } else {
            0
        }
    }

    pub(super) fn execute(&mut self, bus: &mut impl Bus, op: u8) -> Result<u32, Fault> {
        if ILLEGAL.contains(&op) {
            return Err(Fault::Illegal { opcode: op, pc: self.regs.pc.wrapping_sub(1) });
        }

        let z = op & 0x07;
        let y = (op >> 3) & 0x07;

        match op {
            // ---- Block 0: irregular ----------------------------------------
            0x00 => Ok(1), // NOP

            // LD rr,nn
            0x01 | 0x11 | 0x21 | 0x31 => {
                let value = self.fetch16(bus);
                self.regs.write16(r16_sp(op), value);
                Ok(3)
            }

            // LD (BC),A / LD (DE),A / LD (HL+),A / LD (HL-),A
            0x02 | 0x12 | 0x22 | 0x32 => {
                let addr = self.indirect_addr(op);
                bus.write(addr, self.regs.a);
                Ok(2)
            }

            // LD A,(BC) / (DE) / (HL+) / (HL-)
            0x0A | 0x1A | 0x2A | 0x3A => {
                let addr = self.indirect_addr(op);
                self.regs.a = bus.read(addr);
                Ok(2)
            }

            // INC rr — the 16-bit increment does not affect any flag.
            0x03 | 0x13 | 0x23 | 0x33 => {
                let r = r16_sp(op);
                self.regs.write16(r, self.regs.read16(r).wrapping_add(1));
                bus.tick();
                Ok(2)
            }

            // DEC rr
            0x0B | 0x1B | 0x2B | 0x3B => {
                let r = r16_sp(op);
                self.regs.write16(r, self.regs.read16(r).wrapping_sub(1));
                bus.tick();
                Ok(2)
            }

            // ADD HL,rr
            0x09 | 0x19 | 0x29 | 0x39 => {
                let result = self.alu_add16(self.regs.hl(), self.regs.read16(r16_sp(op)));
                self.regs.set_hl(result);
                bus.tick();
                Ok(2)
            }

            // INC r / INC (HL)
            0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => {
                let value = self.read_operand(bus, y);
                let result = self.alu_inc(value);
                self.write_operand(bus, y, result);
                Ok(1 + 2 * Self::hl_penalty(y))
            }

            // DEC r / DEC (HL)
            0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
                let value = self.read_operand(bus, y);
                let result = self.alu_dec(value);
                self.write_operand(bus, y, result);
                Ok(1 + 2 * Self::hl_penalty(y))
            }

            // LD r,n / LD (HL),n
            0x06 | 0x0E | 0x16 | 0x1E | 0x26 | 0x2E | 0x36 | 0x3E => {
                let value = self.fetch8(bus);
                self.write_operand(bus, y, value);
                Ok(2 + Self::hl_penalty(y))
            }

            // Accumulator rotates. They exist separately from their prefixed
            // twins because these force Z to 0 and those do not.
            0x07 => {
                self.rotate_accumulator(Self::rlc);
                Ok(1)
            }
            0x0F => {
                self.rotate_accumulator(Self::rrc);
                Ok(1)
            }
            0x17 => {
                self.rotate_accumulator(Self::rl);
                Ok(1)
            }
            0x1F => {
                self.rotate_accumulator(Self::rr);
                Ok(1)
            }

            0x27 => {
                self.alu_daa();
                Ok(1)
            }
            0x2F => {
                self.alu_cpl();
                Ok(1)
            }
            0x37 => {
                self.alu_scf();
                Ok(1)
            }
            0x3F => {
                self.alu_ccf();
                Ok(1)
            }

            // LD (nn),SP — the only instruction that writes SP to memory.
            0x08 => {
                let addr = self.fetch16(bus);
                bus.write16(addr, self.regs.sp);
                Ok(5)
            }

            // STOP. In hardware it is a two-byte opcode: the second one is
            // discarded.
            //
            // On a CGB with a speed switch armed in `KEY1`, `STOP` does not put
            // the console to sleep: it toggles between 4 and 8 MHz and keeps
            // executing. It is the only way to change speed, and that is why
            // colour games run `STOP` right at boot.
            0x10 => {
                self.fetch8(bus);
                if !bus.perform_speed_switch() {
                    self.power = Power::Stopped;
                }
                Ok(2)
            }

            // JR e8 — the offset is relative to the *already advanced* PC.
            0x18 => {
                let offset = self.fetch8(bus) as i8;
                self.regs.pc = self.regs.pc.wrapping_add_signed(offset as i16);
                bus.tick();
                Ok(3)
            }

            // JR cc,e8 — the operand is always read, even when not jumping.
            0x20 | 0x28 | 0x30 | 0x38 => {
                let offset = self.fetch8(bus) as i8;
                if Cond::from_code(y).holds(&self.regs) {
                    self.regs.pc = self.regs.pc.wrapping_add_signed(offset as i16);
                    bus.tick();
                    Ok(3)
                } else {
                    Ok(2)
                }
            }

            // ---- Block 1: LD r,r' (0x40..0x80), with HALT in the 0x76 slot --
            0x76 => {
                // HALT bug: without IME but with a pending interrupt, the CPU
                // does not stop and the next byte is read twice.
                if !self.ime() && bus.interrupts().any_pending() {
                    self.halt_bug = true;
                } else {
                    self.power = Power::Halted;
                }
                Ok(1)
            }
            0x40..=0x7F => {
                let value = self.read_operand(bus, z);
                self.write_operand(bus, y, value);
                Ok(1 + Self::hl_penalty(z) + Self::hl_penalty(y))
            }

            // ---- Block 2: ALU A,r (0x80..0xC0) -----------------------------
            0x80..=0xBF => {
                let value = self.read_operand(bus, z);
                self.alu_dispatch(y, value);
                Ok(1 + Self::hl_penalty(z))
            }

            // ---- Block 3: irregular ----------------------------------------

            // Prefix: the real opcode is the next byte.
            0xCB => Ok(self.execute_cb(bus)),

            // ALU A,n
            0xC6 | 0xCE | 0xD6 | 0xDE | 0xE6 | 0xEE | 0xF6 | 0xFE => {
                let value = self.fetch8(bus);
                self.alu_dispatch(y, value);
                Ok(2)
            }

            // RET cc — one extra M-cycle to evaluate the condition.
            0xC0 | 0xC8 | 0xD0 | 0xD8 => {
                bus.tick();
                if Cond::from_code(y).holds(&self.regs) {
                    self.regs.pc = self.pop16(bus);
                    bus.tick();
                    Ok(5)
                } else {
                    Ok(2)
                }
            }

            // RET
            0xC9 => {
                self.regs.pc = self.pop16(bus);
                bus.tick();
                Ok(4)
            }

            // RETI: returns and re-enables interrupts immediately, without EI's
            // one-cycle delay.
            0xD9 => {
                self.regs.pc = self.pop16(bus);
                self.ime = true;
                bus.tick();
                Ok(4)
            }

            // POP rr
            0xC1 | 0xD1 | 0xE1 | 0xF1 => {
                let value = self.pop16(bus);
                self.regs.write16(r16_af(op), value);
                Ok(3)
            }

            // PUSH rr — the extra M-cycle happens before the first decrement.
            0xC5 | 0xD5 | 0xE5 | 0xF5 => {
                bus.tick();
                self.push16(bus, self.regs.read16(r16_af(op)));
                Ok(4)
            }

            // JP cc,nn
            0xC2 | 0xCA | 0xD2 | 0xDA => {
                let target = self.fetch16(bus);
                if Cond::from_code(y).holds(&self.regs) {
                    self.regs.pc = target;
                    bus.tick();
                    Ok(4)
                } else {
                    Ok(3)
                }
            }

            // JP nn
            0xC3 => {
                self.regs.pc = self.fetch16(bus);
                bus.tick();
                Ok(4)
            }

            // JP HL — the only jump with no internal cycle: it does not go
            // through the ALU.
            0xE9 => {
                self.regs.pc = self.regs.hl();
                Ok(1)
            }

            // CALL cc,nn
            0xC4 | 0xCC | 0xD4 | 0xDC => {
                let target = self.fetch16(bus);
                if Cond::from_code(y).holds(&self.regs) {
                    bus.tick();
                    self.push16(bus, self.regs.pc);
                    self.regs.pc = target;
                    Ok(6)
                } else {
                    Ok(3)
                }
            }

            // CALL nn
            0xCD => {
                let target = self.fetch16(bus);
                bus.tick();
                self.push16(bus, self.regs.pc);
                self.regs.pc = target;
                Ok(6)
            }

            // RST n — one-byte call to the eight fixed vectors.
            0xC7 | 0xCF | 0xD7 | 0xDF | 0xE7 | 0xEF | 0xF7 | 0xFF => {
                bus.tick();
                self.push16(bus, self.regs.pc);
                self.regs.pc = u16::from(y) * 8;
                Ok(4)
            }

            // LDH (n),A — fast access to 0xFF00+n, the I/O page.
            0xE0 => {
                let offset = self.fetch8(bus);
                bus.write(0xFF00 + u16::from(offset), self.regs.a);
                Ok(3)
            }
            0xF0 => {
                let offset = self.fetch8(bus);
                self.regs.a = bus.read(0xFF00 + u16::from(offset));
                Ok(3)
            }

            // LDH (C),A and its inverse.
            0xE2 => {
                bus.write(0xFF00 + u16::from(self.regs.c), self.regs.a);
                Ok(2)
            }
            0xF2 => {
                self.regs.a = bus.read(0xFF00 + u16::from(self.regs.c));
                Ok(2)
            }

            // LD (nn),A / LD A,(nn)
            0xEA => {
                let addr = self.fetch16(bus);
                bus.write(addr, self.regs.a);
                Ok(4)
            }
            0xFA => {
                let addr = self.fetch16(bus);
                self.regs.a = bus.read(addr);
                Ok(4)
            }

            // ADD SP,e8 — two internal M-cycles: the 8-bit ALU has to process
            // the two bytes of the stack pointer separately.
            0xE8 => {
                let offset = self.fetch8(bus) as i8;
                self.regs.sp = self.alu_add_sp(self.regs.sp, offset);
                bus.tick();
                bus.tick();
                Ok(4)
            }

            // LD HL,SP+e8 — the same, but the result goes to HL and it only
            // costs one internal cycle because it need not be written back to
            // SP.
            0xF8 => {
                let offset = self.fetch8(bus) as i8;
                let result = self.alu_add_sp(self.regs.sp, offset);
                self.regs.set_hl(result);
                bus.tick();
                Ok(3)
            }

            // LD SP,HL
            0xF9 => {
                self.regs.sp = self.regs.hl();
                bus.tick();
                Ok(2)
            }

            // DI cuts interrupts off on the spot...
            0xF3 => {
                self.ime = false;
                self.ime_pending = false;
                Ok(1)
            }
            // ...but EI does not take effect until after the next instruction.
            0xFB => {
                self.ime_pending = true;
                Ok(1)
            }

            _ => Err(Fault::Unimplemented {
                opcode: op,
                prefixed: false,
                pc: self.regs.pc.wrapping_sub(1),
            }),
        }
    }

    /// Effective address of the block 0 indirect loads, including the
    /// post-increment and post-decrement of `HL`.
    fn indirect_addr(&mut self, op: u8) -> u16 {
        match (op >> 4) & 0x03 {
            0 => self.regs.read16(R16::Bc),
            1 => self.regs.read16(R16::De),
            2 => {
                let hl = self.regs.hl();
                self.regs.set_hl(hl.wrapping_add(1));
                hl
            }
            _ => {
                let hl = self.regs.hl();
                self.regs.set_hl(hl.wrapping_sub(1));
                hl
            }
        }
    }

    /// Selects the ALU operation from the `y` field of block 2.
    fn alu_dispatch(&mut self, y: u8, value: u8) {
        match y {
            0 => self.alu_add(value),
            1 => self.alu_adc(value),
            2 => self.alu_sub(value),
            3 => self.alu_sbc(value),
            4 => self.alu_and(value),
            5 => self.alu_xor(value),
            6 => self.alu_or(value),
            _ => self.alu_cp(value),
        }
    }
}
