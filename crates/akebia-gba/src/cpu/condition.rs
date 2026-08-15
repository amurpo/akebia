//! The four bits at the top of every ARM instruction.
//!
//! # Why every instruction carries one
//!
//! Because a branch on this processor costs the whole pipeline. Three stages
//! are in flight at any moment, and taking a branch throws two of them away, so
//! the short jump that skips one instruction — the shape almost every `if` in
//! compiled code has — pays more to skip the work than doing the work would
//! have cost. So the architecture spends four bits of every encoding letting
//! any instruction decline to happen. `if (a > b) c = d;` is two instructions
//! and no branch at all.
//!
//! For the emulator this means the condition is checked *before* the decode
//! that follows, and an instruction that fails it costs its fetch and nothing
//! else: no operand is read, no flag is written, and — this is the part that is
//! easy to get wrong — no side effect of an addressing mode happens either. A
//! `LDR` with write-back that fails its condition does not write back.

use super::registers::Registers;

/// When an instruction happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Equal: the last comparison came out zero.
    Eq,
    Ne,
    /// Carry set. Also "unsigned higher or same", which is the same bit read
    /// for a different purpose.
    Cs,
    /// Carry clear, or "unsigned lower".
    Cc,
    /// Negative.
    Mi,
    /// Positive or zero.
    Pl,
    /// Signed overflow.
    Vs,
    Vc,
    /// Unsigned higher: carry set *and* not equal.
    Hi,
    /// Unsigned lower or same.
    Ls,
    /// Signed greater or equal, which is `N == V` and not either one alone.
    Ge,
    Lt,
    Gt,
    Le,
    /// Always. What an instruction with no condition written on it assembles to.
    Al,
    /// Never.
    ///
    /// On later ARMs this encoding was taken back and given to instructions
    /// that cannot be conditional at all. This is an ARMv4T, where it means
    /// what it says: the instruction does not happen. Games do not use it, but
    /// a decoder that treats it as `Al` runs whatever bit pattern falls in it,
    /// and rubbish executed is worse than rubbish skipped.
    Nv,
}

impl Condition {
    /// The condition those four bits name. Every one of the sixteen names one,
    /// so there is nothing to refuse.
    pub const fn from_bits(bits: u32) -> Self {
        match bits & 0xF {
            0x0 => Self::Eq,
            0x1 => Self::Ne,
            0x2 => Self::Cs,
            0x3 => Self::Cc,
            0x4 => Self::Mi,
            0x5 => Self::Pl,
            0x6 => Self::Vs,
            0x7 => Self::Vc,
            0x8 => Self::Hi,
            0x9 => Self::Ls,
            0xA => Self::Ge,
            0xB => Self::Lt,
            0xC => Self::Gt,
            0xD => Self::Le,
            0xE => Self::Al,
            _ => Self::Nv,
        }
    }

    /// Whether the flags as they stand let the instruction happen.
    pub fn passes(self, regs: &Registers) -> bool {
        match self {
            Self::Eq => regs.z(),
            Self::Ne => !regs.z(),
            Self::Cs => regs.c(),
            Self::Cc => !regs.c(),
            Self::Mi => regs.n(),
            Self::Pl => !regs.n(),
            Self::Vs => regs.v(),
            Self::Vc => !regs.v(),
            Self::Hi => regs.c() && !regs.z(),
            Self::Ls => !regs.c() || regs.z(),
            // The signed comparisons are about `N` and `V` agreeing, not about
            // `N` alone: a subtraction that overflowed has the sign of the
            // wrong answer, and `V` is what says so.
            Self::Ge => regs.n() == regs.v(),
            Self::Lt => regs.n() != regs.v(),
            Self::Gt => !regs.z() && regs.n() == regs.v(),
            Self::Le => regs.z() || regs.n() != regs.v(),
            Self::Al => true,
            Self::Nv => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::registers::{C, N, V, Z};

    /// Sets exactly the flags given and clears the rest.
    fn with(flags: u32) -> Registers {
        let mut regs = Registers::new();
        regs.set_n(flags & N != 0);
        regs.set_z(flags & Z != 0);
        regs.set_c(flags & C != 0);
        regs.set_v(flags & V != 0);
        regs
    }

    #[test]
    fn all_sixteen_encodings_name_a_condition() {
        const ORDER: [Condition; 16] = [
            Condition::Eq,
            Condition::Ne,
            Condition::Cs,
            Condition::Cc,
            Condition::Mi,
            Condition::Pl,
            Condition::Vs,
            Condition::Vc,
            Condition::Hi,
            Condition::Ls,
            Condition::Ge,
            Condition::Lt,
            Condition::Gt,
            Condition::Le,
            Condition::Al,
            Condition::Nv,
        ];
        for (bits, wanted) in ORDER.iter().enumerate() {
            assert_eq!(Condition::from_bits(bits as u32), *wanted, "0x{bits:X}");
        }
    }

    /// The eight that read one flag each, and their opposites. Every pair must
    /// disagree on every set of flags there is.
    #[test]
    fn the_single_flag_conditions_are_exact_opposites() {
        for flags in 0..16u32 {
            let regs = with(flags << 28);
            for (yes, no) in [
                (Condition::Eq, Condition::Ne),
                (Condition::Cs, Condition::Cc),
                (Condition::Mi, Condition::Pl),
                (Condition::Vs, Condition::Vc),
                (Condition::Hi, Condition::Ls),
                (Condition::Ge, Condition::Lt),
                (Condition::Gt, Condition::Le),
            ] {
                assert_ne!(
                    yes.passes(&regs),
                    no.passes(&regs),
                    "{yes:?} and {no:?} agreed with flags 0b{flags:04b}"
                );
            }
        }
    }

    /// Unsigned higher is carry set *and* not equal. Carry alone is not enough,
    /// which is the difference between `CS` and `HI`.
    #[test]
    fn unsigned_higher_wants_the_carry_and_the_absence_of_zero() {
        assert!(Condition::Hi.passes(&with(C)));
        assert!(!Condition::Hi.passes(&with(C | Z)), "equal is not higher");
        assert!(!Condition::Hi.passes(&with(0)));
        assert!(Condition::Ls.passes(&with(C | Z)));
        assert!(Condition::Ls.passes(&with(0)));
    }

    /// The signed comparisons are about `N` and `V` agreeing. A subtraction
    /// that overflowed carries the sign of the wrong answer, and `V` is what
    /// says so - so negative-with-overflow is *greater*, not less.
    #[test]
    fn the_signed_comparisons_read_n_against_v_and_not_n_alone() {
        assert!(Condition::Ge.passes(&with(0)), "positive, no overflow");
        assert!(Condition::Ge.passes(&with(N | V)), "negative but overflowed: still greater");
        assert!(Condition::Lt.passes(&with(N)), "negative, no overflow");
        assert!(Condition::Lt.passes(&with(V)), "positive but overflowed");

        // And the strict pair is the same with zero excluded.
        assert!(Condition::Gt.passes(&with(N | V)));
        assert!(!Condition::Gt.passes(&with(N | V | Z)), "equal is not greater");
        assert!(Condition::Le.passes(&with(N | V | Z)));
    }

    /// `AL` happens whatever the flags say and `NV` never does. Treating `NV`
    /// as `AL` would execute whatever bit pattern falls in it.
    #[test]
    fn always_and_never_do_not_look_at_the_flags() {
        for flags in 0..16u32 {
            let regs = with(flags << 28);
            assert!(Condition::Al.passes(&regs), "flags 0b{flags:04b}");
            assert!(!Condition::Nv.passes(&regs), "flags 0b{flags:04b}");
        }
    }
}
