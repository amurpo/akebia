//! The instruction set prefixed with `0xCB`.
//!
//! When the CPU reads a `0xCB` it executes nothing: it consumes a second byte
//! and decodes it against a different table. That is 256 more opcodes, but
//! unlike the main set this table is **perfectly regular**, so it fits in a
//! single function.
//!
//! ```text
//!   7 6   5 4 3   2 1 0
//!  ┌─────┬───────┬───────┐
//!  │  x  │   y   │   z   │
//!  └─────┴───────┴───────┘
//!
//!   x = 0 → rotate or shift `y` on operand `z`
//!   x = 1 → BIT y,z      (tests bit y)
//!   x = 2 → RES y,z      (clears bit y)
//!   x = 3 → SET y,z      (sets bit y)
//!
//!   z follows the usual encoding: 6 is `(HL)`, not a register.
//! ```
//!
//! # Cost
//!
//! | Form              | M-cycles | Why |
//! |-------------------|----------|-----|
//! | on a register     | 2        | two fetches |
//! | on `(HL)`         | 4        | two fetches + read + write |
//! | `BIT n,(HL)`      | 3        | it does not write: it only tests |
//!
//! That asymmetry in `BIT` is easy to overlook and throws off the timing of any
//! wait loop that uses it.

use super::{Bus, Cpu};

/// The eight rotates and shifts, indexed by the `y` field.
///
/// Keeping them as a table of function pointers, rather than a `match`, makes
/// it explicit that the order **is** the opcode encoding.
const SHIFTS: [fn(&mut Cpu, u8) -> u8; 8] =
    [Cpu::rlc, Cpu::rrc, Cpu::rl, Cpu::rr, Cpu::sla, Cpu::sra, Cpu::swap, Cpu::srl];

impl Cpu {
    /// Decodes and executes the byte following `0xCB`.
    ///
    /// Returns the instruction's total M-cycles, including the fetch of the
    /// `0xCB` itself.
    pub(super) fn execute_cb(&mut self, bus: &mut impl Bus) -> u32 {
        let op = self.fetch8(bus);
        let z = op & 0x07;
        let y = (op >> 3) & 0x07;
        let penalty = Self::hl_penalty(z);

        match op >> 6 {
            // x = 0: rotates and shifts.
            0 => {
                let value = self.read_operand(bus, z);
                let result = SHIFTS[y as usize](self, value);
                self.write_operand(bus, z, result);
                2 + 2 * penalty
            }
            // x = 1: BIT. The only form that does not write the operand back.
            1 => {
                let value = self.read_operand(bus, z);
                self.bit(y, value);
                2 + penalty
            }
            // x = 2: RES. Does not alter any flag.
            2 => {
                let value = self.read_operand(bus, z);
                self.write_operand(bus, z, value & !(1 << y));
                2 + 2 * penalty
            }
            // x = 3: SET. Does not touch the flags either.
            _ => {
                let value = self.read_operand(bus, z);
                self.write_operand(bus, z, value | (1 << y));
                2 + 2 * penalty
            }
        }
    }
}
