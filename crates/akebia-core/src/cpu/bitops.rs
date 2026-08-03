//! Rotates, shifts and bit operations.
//!
//! They are almost the entire contents of the `0xCB` prefix, plus the four
//! variants on the accumulator that live in block 0 without a prefix.
//!
//! # Rotate versus shift
//!
//! ```text
//!   RLC  ┌──────────────┐        RL   ┌──────────────────┐
//!        │  ┌─┐ ┌──────┐│             │ ┌─┐   ┌────────┐ │
//!    C ◄─┴──┤7├─┤6..0  ├┴──┐      C ◄─┴─┤7├───┤6..0    ├─┴─◄ C
//!           └─┘ └──────┘   │            └─┘   └────────┘
//!           └──────────────┘         (the carry enters through bit 0)
//!
//!   SLA: a 0 enters through bit 0, bit 7 goes out to the carry.
//!   SRA: bit 7 receives **a copy of itself** (arithmetic shift: it keeps the
//!        sign). It is the only one that does that.
//!   SRL: a 0 enters through bit 7.
//! ```
//!
//! # The Z flag trap
//!
//! `RLC B` (prefixed) sets `Z` from the result. `RLCA` (unprefixed) **always
//! leaves `Z` at 0**, even if the accumulator ends up zero. The same goes for
//! `RRCA`, `RLA` and `RRA`. They are four opcodes distinct from their prefixed
//! equivalents precisely for that reason, and mixing them up breaks Blargg's
//! tests without giving any visible clue.

use super::{Cpu, Flags};

impl Cpu {
    /// `RLC`: rotate left; bit 7 goes to the carry **and** to bit 0.
    pub(super) fn rlc(&mut self, value: u8) -> u8 {
        let carry = value & 0x80 != 0;
        let result = value.rotate_left(1);
        self.regs.f.write(result == 0, false, false, carry);
        result
    }

    /// `RRC`: rotate right; bit 0 goes to the carry **and** to bit 7.
    pub(super) fn rrc(&mut self, value: u8) -> u8 {
        let carry = value & 0x01 != 0;
        let result = value.rotate_right(1);
        self.regs.f.write(result == 0, false, false, carry);
        result
    }

    /// `RL`: rotate left **through the carry**, which enters via bit 0.
    pub(super) fn rl(&mut self, value: u8) -> u8 {
        let carry = value & 0x80 != 0;
        let result = (value << 1) | u8::from(self.regs.f.c());
        self.regs.f.write(result == 0, false, false, carry);
        result
    }

    /// `RR`: rotate right through the carry, which enters via bit 7.
    pub(super) fn rr(&mut self, value: u8) -> u8 {
        let carry = value & 0x01 != 0;
        let result = (value >> 1) | (u8::from(self.regs.f.c()) << 7);
        self.regs.f.write(result == 0, false, false, carry);
        result
    }

    /// `SLA`: shift left; a 0 enters.
    pub(super) fn sla(&mut self, value: u8) -> u8 {
        let carry = value & 0x80 != 0;
        let result = value << 1;
        self.regs.f.write(result == 0, false, false, carry);
        result
    }

    /// `SRA`: **arithmetic** shift right; bit 7 is preserved.
    ///
    /// It is the only operation in the set that preserves the sign, and that is
    /// why it is the one used to halve a signed number.
    pub(super) fn sra(&mut self, value: u8) -> u8 {
        let carry = value & 0x01 != 0;
        let result = (value >> 1) | (value & 0x80);
        self.regs.f.write(result == 0, false, false, carry);
        result
    }

    /// `SWAP`: exchanges the two nibbles. The carry always ends up at 0.
    pub(super) fn swap(&mut self, value: u8) -> u8 {
        let result = value.rotate_left(4);
        self.regs.f.write(result == 0, false, false, false);
        result
    }

    /// `SRL`: logical shift right; a 0 enters through bit 7.
    pub(super) fn srl(&mut self, value: u8) -> u8 {
        let carry = value & 0x01 != 0;
        let result = value >> 1;
        self.regs.f.write(result == 0, false, false, carry);
        result
    }

    /// `BIT n,r`: tests a bit without modifying the operand.
    ///
    /// `Z` ends up at 1 if the bit is **off** (it is a negated test), and `C` is
    /// left alone, which allows chaining tests without losing the carry from a
    /// previous operation.
    pub(super) fn bit(&mut self, index: u8, value: u8) {
        self.regs.f.set(Flags::Z, value & (1 << index) == 0);
        self.regs.f.set(Flags::N, false);
        self.regs.f.set(Flags::H, true);
    }

    /// Applies a rotate to the accumulator and forces `Z` to 0.
    ///
    /// It wraps the four unprefixed variants (`RLCA`, `RRCA`, `RLA`, `RRA`) so
    /// they share the implementation with the prefixed ones without inheriting
    /// their handling of `Z`.
    pub(super) fn rotate_accumulator(&mut self, rotate: fn(&mut Self, u8) -> u8) {
        let a = self.regs.a;
        self.regs.a = rotate(self, a);
        self.regs.f.set(Flags::Z, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu() -> Cpu {
        let mut cpu = Cpu::new_dmg();
        cpu.regs.f = Flags::from_bits(0);
        cpu
    }

    #[test]
    fn rlc_returns_bit_7_into_bit_0_and_the_carry() {
        let mut c = cpu();
        assert_eq!(c.rlc(0b1000_0001), 0b0000_0011);
        assert!(c.regs.f.c());
        assert!(!c.regs.f.z());
    }

    #[test]
    fn rl_passes_the_carry_through_bit_0() {
        let mut c = cpu();
        c.regs.f.set(Flags::C, true);
        assert_eq!(c.rl(0b0000_0000), 0b0000_0001, "the incoming carry fills bit 0");
        assert!(!c.regs.f.c(), "the original bit 7 was 0");

        c.regs.f.set(Flags::C, false);
        assert_eq!(c.rl(0b1000_0000), 0b0000_0000);
        assert!(c.regs.f.c() && c.regs.f.z());
    }

    #[test]
    fn rr_passes_the_carry_through_bit_7() {
        let mut c = cpu();
        c.regs.f.set(Flags::C, true);
        assert_eq!(c.rr(0b0000_0001), 0b1000_0000);
        assert!(c.regs.f.c());
    }

    #[test]
    fn sra_keeps_the_sign_and_srl_does_not() {
        let mut c = cpu();
        assert_eq!(c.sra(0b1000_0010), 0b1100_0001, "SRA replicates bit 7");
        assert_eq!(c.srl(0b1000_0010), 0b0100_0001, "SRL shifts in a 0");
    }

    #[test]
    fn sla_shifts_in_a_zero_from_the_right() {
        let mut c = cpu();
        assert_eq!(c.sla(0b0100_0001), 0b1000_0010);
        assert!(!c.regs.f.c());
    }

    #[test]
    fn swap_exchanges_the_nibbles() {
        let mut c = cpu();
        assert_eq!(c.swap(0xAB), 0xBA);
        assert!(!c.regs.f.c(), "SWAP always leaves the carry at 0");
        assert_eq!(c.swap(0x00), 0x00);
        assert!(c.regs.f.z());
    }

    #[test]
    fn bit_sets_z_when_the_bit_is_off() {
        let mut c = cpu();
        c.regs.f.set(Flags::C, true);

        c.bit(3, 0b0000_1000);
        assert!(!c.regs.f.z(), "the bit is on");
        c.bit(3, 0b1111_0111);
        assert!(c.regs.f.z(), "the bit is off");

        assert!(c.regs.f.h(), "BIT leaves H at 1");
        assert!(c.regs.f.c(), "BIT does not touch the carry");
    }

    #[test]
    fn the_accumulator_rotates_never_set_z() {
        let mut c = cpu();
        c.regs.a = 0x00;
        c.rotate_accumulator(Cpu::rlc);
        assert_eq!(c.regs.a, 0x00);
        assert!(!c.regs.f.z(), "RLCA forces Z to 0 even when A ends up zero");

        // The prefixed variant on the same value does set Z.
        c.rlc(0x00);
        assert!(c.regs.f.z());
    }
}
