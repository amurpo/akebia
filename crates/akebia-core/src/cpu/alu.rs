//! ALU operations, isolated from the decoder.
//!
//! They live here and not inside the giant `match` for a practical reason: the
//! flags —above all the *half-carry*— are the number one source of silent bugs
//! in a Game Boy emulator, and here they can be tested one by one without
//! needing a bus.
//!
//! Half-carry rules, which are not uniform:
//!
//! | Operation      | H is computed on             |
//! |----------------|------------------------------|
//! | `ADD`/`ADC` 8  | carry from bit 3 into bit 4  |
//! | `SUB`/`SBC` 8  | borrow from bit 4 into bit 3 |
//! | `ADD HL,rr`    | carry from bit 11 into bit 12 |
//! | `ADD SP,e8`    | carry from bit 3 into bit 4 (8-bit!) |
//! | `INC`/`DEC` r  | like ADD/SUB, but without touching C |

use super::{Cpu, Flags};

impl Cpu {
    pub(super) fn alu_add(&mut self, value: u8) {
        let a = self.regs.a;
        let (result, carry) = a.overflowing_add(value);
        self.regs.f.write(result == 0, false, (a & 0x0F) + (value & 0x0F) > 0x0F, carry);
        self.regs.a = result;
    }

    pub(super) fn alu_adc(&mut self, value: u8) {
        let a = self.regs.a;
        let c = u8::from(self.regs.f.c());
        let result = a.wrapping_add(value).wrapping_add(c);
        // Done in 16 bits because a+value+c can exceed 0xFF twice over.
        let carry = u16::from(a) + u16::from(value) + u16::from(c) > 0xFF;
        let half = (a & 0x0F) + (value & 0x0F) + c > 0x0F;
        self.regs.f.write(result == 0, false, half, carry);
        self.regs.a = result;
    }

    pub(super) fn alu_sub(&mut self, value: u8) {
        self.regs.a = self.alu_cp_inner(value);
    }

    pub(super) fn alu_sbc(&mut self, value: u8) {
        let a = self.regs.a;
        let c = u8::from(self.regs.f.c());
        let result = a.wrapping_sub(value).wrapping_sub(c);
        let borrow = u16::from(a) < u16::from(value) + u16::from(c);
        let half = (a & 0x0F) < (value & 0x0F) + c;
        self.regs.f.write(result == 0, true, half, borrow);
        self.regs.a = result;
    }

    /// `CP` is a `SUB` that discards the result and only keeps the flags.
    pub(super) fn alu_cp(&mut self, value: u8) {
        self.alu_cp_inner(value);
    }

    fn alu_cp_inner(&mut self, value: u8) -> u8 {
        let a = self.regs.a;
        let result = a.wrapping_sub(value);
        self.regs.f.write(result == 0, true, (a & 0x0F) < (value & 0x0F), a < value);
        result
    }

    pub(super) fn alu_and(&mut self, value: u8) {
        self.regs.a &= value;
        // AND is the only logical op that leaves H at 1. There is no deep
        // reason: that is just how it is.
        self.regs.f.write(self.regs.a == 0, false, true, false);
    }

    pub(super) fn alu_or(&mut self, value: u8) {
        self.regs.a |= value;
        self.regs.f.write(self.regs.a == 0, false, false, false);
    }

    pub(super) fn alu_xor(&mut self, value: u8) {
        self.regs.a ^= value;
        self.regs.f.write(self.regs.a == 0, false, false, false);
    }

    /// `INC r`: does not touch the C flag.
    pub(super) fn alu_inc(&mut self, value: u8) -> u8 {
        let result = value.wrapping_add(1);
        self.regs.f.set(Flags::Z, result == 0);
        self.regs.f.set(Flags::N, false);
        self.regs.f.set(Flags::H, value & 0x0F == 0x0F);
        result
    }

    /// `DEC r`: does not touch C either.
    pub(super) fn alu_dec(&mut self, value: u8) -> u8 {
        let result = value.wrapping_sub(1);
        self.regs.f.set(Flags::Z, result == 0);
        self.regs.f.set(Flags::N, true);
        self.regs.f.set(Flags::H, value & 0x0F == 0);
        result
    }

    /// `ADD HL,rr`: leaves Z untouched and computes H on bit 11.
    pub(super) fn alu_add16(&mut self, a: u16, b: u16) -> u16 {
        let (result, carry) = a.overflowing_add(b);
        self.regs.f.set(Flags::N, false);
        self.regs.f.set(Flags::H, (a & 0x0FFF) + (b & 0x0FFF) > 0x0FFF);
        self.regs.f.set(Flags::C, carry);
        result
    }

    /// `DAA`: fixes up the accumulator after a **BCD** operation.
    ///
    /// It is the most misunderstood instruction in the set. It does not convert
    /// to BCD: it assumes the operands already were BCD and fixes the binary
    /// result. If you add `0x09 + 0x01` you get `0x0A`, which is not a valid
    /// decimal digit; `DAA` turns it into `0x10`.
    ///
    /// To know how to correct it, it needs to know whether the previous
    /// operation was an addition or a subtraction, and whether there was a
    /// nibble carry. That is the entire reason the `N` and `H` flags exist:
    /// **`DAA` is their only consumer**.
    ///
    /// The outgoing `C` flag does not reflect the binary result but the decimal
    /// one: it turns on if the value went past 99, so that multi-byte BCD
    /// additions can be chained.
    pub(super) fn alu_daa(&mut self) {
        let a = self.regs.a;
        let (n, h, c) = (self.regs.f.n(), self.regs.f.h(), self.regs.f.c());

        // Both corrections are decided **on the original `A`** and applied in a
        // single step. Chaining them —adding 0x06 and then checking whether the
        // result goes past 0x99— produces one carry too many: with `A = 0x99`
        // and `H = 1`, the correct answer is 0x9F with no carry, and chaining
        // yields 0xFF with carry. That is exactly what
        // `blargg/cpu_instrs/01-special` detects.
        let mut adjust = 0u8;
        let mut carry = c;

        if h || (!n && (a & 0x0F) > 0x09) {
            adjust |= 0x06;
        }
        // After a subtraction nothing can be deduced from the value: a borrow
        // already deformed it, so only the flags count.
        if c || (!n && a > 0x99) {
            adjust |= 0x60;
            carry = true;
        }

        let result = if n { a.wrapping_sub(adjust) } else { a.wrapping_add(adjust) };

        self.regs.a = result;
        self.regs.f.set(Flags::Z, result == 0);
        self.regs.f.set(Flags::H, false);
        self.regs.f.set(Flags::C, carry);
    }

    /// `CPL`: one's complement of the accumulator. Touches neither `Z` nor `C`.
    pub(super) fn alu_cpl(&mut self) {
        self.regs.a = !self.regs.a;
        self.regs.f.set(Flags::N, true);
        self.regs.f.set(Flags::H, true);
    }

    /// `SCF`: forces the carry to 1.
    pub(super) fn alu_scf(&mut self) {
        self.regs.f.set(Flags::N, false);
        self.regs.f.set(Flags::H, false);
        self.regs.f.set(Flags::C, true);
    }

    /// `CCF`: inverts the carry.
    pub(super) fn alu_ccf(&mut self) {
        let c = self.regs.f.c();
        self.regs.f.set(Flags::N, false);
        self.regs.f.set(Flags::H, false);
        self.regs.f.set(Flags::C, !c);
    }

    /// `ADD SP,e8` and `LD HL,SP+e8`.
    ///
    /// Famous quirk: even though the result is 16 bits, H and C are computed on
    /// the **low byte**, as if it were an 8-bit addition, and Z is forced to 0.
    /// That is the behaviour `blargg/cpu_instrs/09` verifies.
    pub(super) fn alu_add_sp(&mut self, sp: u16, offset: i8) -> u16 {
        let off = offset as u16; // sign extension
        let half = (sp & 0x0F) + (off & 0x0F) > 0x0F;
        let carry = (sp & 0xFF) + (off & 0xFF) > 0xFF;
        self.regs.f.write(false, false, half, carry);
        sp.wrapping_add(off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_with_a(a: u8) -> Cpu {
        let mut cpu = Cpu::new_dmg();
        cpu.regs.a = a;
        cpu.regs.f = Flags::from_bits(0);
        cpu
    }

    #[test]
    fn add_sets_the_half_carry_on_bit_3() {
        let mut cpu = cpu_with_a(0x0F);
        cpu.alu_add(0x01);
        assert_eq!(cpu.regs.a, 0x10);
        assert!(cpu.regs.f.h(), "0x0F + 0x01 overflows the low nibble");
        assert!(!cpu.regs.f.c());
    }

    #[test]
    fn add_sets_carry_and_zero_when_wrapping_around() {
        let mut cpu = cpu_with_a(0xFF);
        cpu.alu_add(0x01);
        assert_eq!(cpu.regs.a, 0x00);
        assert!(cpu.regs.f.z() && cpu.regs.f.h() && cpu.regs.f.c());
    }

    #[test]
    fn adc_adds_the_previous_carry() {
        let mut cpu = cpu_with_a(0x0F);
        cpu.regs.f.set(Flags::C, true);
        cpu.alu_adc(0x00);
        assert_eq!(cpu.regs.a, 0x10);
        assert!(cpu.regs.f.h(), "the incoming carry causes the half-carry");
    }

    #[test]
    fn sub_sets_the_borrow() {
        let mut cpu = cpu_with_a(0x10);
        cpu.alu_sub(0x01);
        assert_eq!(cpu.regs.a, 0x0F);
        assert!(cpu.regs.f.n() && cpu.regs.f.h() && !cpu.regs.f.c());
    }

    #[test]
    fn cp_does_not_modify_the_accumulator() {
        let mut cpu = cpu_with_a(0x42);
        cpu.alu_cp(0x42);
        assert_eq!(cpu.regs.a, 0x42);
        assert!(cpu.regs.f.z());
    }

    #[test]
    fn and_always_leaves_h_at_one() {
        let mut cpu = cpu_with_a(0xF0);
        cpu.alu_and(0x0F);
        assert_eq!(cpu.regs.a, 0x00);
        assert!(cpu.regs.f.z() && cpu.regs.f.h());
    }

    #[test]
    fn inc_and_dec_preserve_the_carry() {
        let mut cpu = cpu_with_a(0x00);
        cpu.regs.f.set(Flags::C, true);
        let r = cpu.alu_inc(0xFF);
        assert_eq!(r, 0x00);
        assert!(cpu.regs.f.c(), "INC must not touch C");
        assert!(cpu.regs.f.z() && cpu.regs.f.h());
    }

    #[test]
    fn add16_uses_bit_11_for_the_half_carry() {
        let mut cpu = cpu_with_a(0);
        cpu.regs.f.set(Flags::Z, true);
        let r = cpu.alu_add16(0x0FFF, 0x0001);
        assert_eq!(r, 0x1000);
        assert!(cpu.regs.f.h());
        assert!(cpu.regs.f.z(), "ADD HL,rr does not touch Z");
    }

    #[test]
    fn add_sp_computes_the_flags_on_the_low_byte() {
        let mut cpu = cpu_with_a(0);
        let r = cpu.alu_add_sp(0xFFF8, 0x08);
        assert_eq!(r, 0x0000);
        assert!(!cpu.regs.f.z(), "Z is always forced to 0");
        assert!(cpu.regs.f.h() && cpu.regs.f.c());
    }

    #[test]
    fn add_sp_with_a_negative_offset() {
        let mut cpu = cpu_with_a(0);
        assert_eq!(cpu.alu_add_sp(0x0100, -1), 0x00FF);
    }

    /// Adds two BCD values the way a program would: `ADD` and then `DAA`.
    fn bcd_add(x: u8, y: u8) -> (u8, bool) {
        let mut cpu = cpu_with_a(x);
        cpu.alu_add(y);
        cpu.alu_daa();
        (cpu.regs.a, cpu.regs.f.c())
    }

    #[test]
    fn daa_fixes_a_bcd_addition() {
        assert_eq!(bcd_add(0x09, 0x01), (0x10, false), "9 + 1 = 10 in decimal");
        assert_eq!(bcd_add(0x15, 0x27), (0x42, false));
        assert_eq!(bcd_add(0x99, 0x01), (0x00, true), "100 overflows and sets C");
        assert_eq!(bcd_add(0x50, 0x50), (0x00, true));
    }

    #[test]
    fn daa_fixes_a_bcd_subtraction() {
        let mut cpu = cpu_with_a(0x42);
        cpu.alu_sub(0x15);
        cpu.alu_daa();
        assert_eq!(cpu.regs.a, 0x27, "42 - 15 = 27 in decimal");
        assert!(cpu.regs.f.n(), "DAA keeps N");
    }

    #[test]
    fn daa_leaves_an_already_valid_result_alone() {
        assert_eq!(bcd_add(0x12, 0x34), (0x46, false));
    }

    /// Regression from `blargg/cpu_instrs/01-special`.
    ///
    /// Both corrections must be decided on the incoming `A`. If the second one
    /// looks at the value already adjusted by the first, this yields 0xFF with
    /// carry instead of 0x9F without it.
    #[test]
    fn daa_does_not_chain_the_two_corrections() {
        let mut cpu = cpu_with_a(0x99);
        cpu.regs.f.write(false, false, true, false); // N=0, H=1, C=0
        cpu.alu_daa();
        assert_eq!(cpu.regs.a, 0x9F);
        assert!(!cpu.regs.f.c(), "0x99 does not exceed 0x99: there is no decimal carry");
    }

    #[test]
    fn daa_keeps_the_carry_after_a_subtraction() {
        let mut cpu = cpu_with_a(0x00);
        cpu.regs.f.write(false, true, false, true); // N=1, H=0, C=1
        cpu.alu_daa();
        assert_eq!(cpu.regs.a, 0xA0);
        assert!(cpu.regs.f.c(), "after a subtraction the carry is left alone");
    }

    #[test]
    fn daa_sets_zero_and_clears_the_half_carry() {
        let mut cpu = cpu_with_a(0x00);
        cpu.regs.f.write(false, false, false, false);
        cpu.alu_daa();
        assert!(cpu.regs.f.z());
        assert!(!cpu.regs.f.h(), "DAA always clears H");
    }

    #[test]
    fn cpl_inverts_without_touching_z_or_c() {
        let mut cpu = cpu_with_a(0b1010_0101);
        cpu.regs.f.write(true, false, false, true);
        cpu.alu_cpl();
        assert_eq!(cpu.regs.a, 0b0101_1010);
        assert!(cpu.regs.f.n() && cpu.regs.f.h());
        assert!(cpu.regs.f.z() && cpu.regs.f.c(), "Z and C are preserved");
    }

    #[test]
    fn scf_and_ccf_manipulate_the_carry() {
        let mut cpu = cpu_with_a(0);
        cpu.alu_scf();
        assert!(cpu.regs.f.c());
        cpu.alu_ccf();
        assert!(!cpu.regs.f.c());
        cpu.alu_ccf();
        assert!(cpu.regs.f.c());
        assert!(!cpu.regs.f.n() && !cpu.regs.f.h());
    }
}
