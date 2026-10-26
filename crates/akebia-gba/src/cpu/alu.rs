//! The sixteen data operations, and the flags each of them leaves behind.
//!
//! # Why there are two kinds and not one
//!
//! Eight of these are arithmetic and eight are logical, and the difference is
//! not what they compute but **what they do to the flags**. An addition has a
//! carry because a bit fell out of the top of the adder, and an overflow
//! because the signed answer did not fit. An `AND` has neither: nothing carried
//! and nothing overflowed. But `AND` still has to put *something* in the carry
//! flag, and what it puts there is whatever the barrel shifter produced while
//! working out the second operand — a carry that has nothing to do with this
//! instruction's arithmetic and everything to do with how its operand was
//! written.
//!
//! So a logical operation's carry comes from one place and an arithmetic one's
//! from another, and the overflow flag a logical operation leaves is the
//! overflow flag it found. That is why [`execute`] is handed two carries by
//! name rather than one: passing the wrong one is the single easiest mistake to
//! make here, and it is invisible until a game takes a conditional branch the
//! wrong way.
//!
//! # Why subtraction is addition
//!
//! Every arithmetic operation here goes through the same adder, with the
//! operands and the carry-in arranged to suit. `a - b` is `a + !b + 1`, which
//! is what the hardware does too — there is one adder on the chip and
//! subtraction is how it is used, not a second circuit.
//!
//! It matters beyond tidiness because of what it means for the carry flag. A
//! subtraction sets the carry when the adder carried out, which happens exactly
//! when the subtraction did **not** borrow. So carry-set means "no borrow",
//! which reads backwards from every other processor and is why the unsigned
//! comparisons are named the way they are: `CS` and "higher or same" are the
//! same bit.

/// Which of the sixteen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    And,
    Eor,
    Sub,
    /// Reverse subtract: the second operand less the first. It exists because
    /// the second operand is the one that can be shifted, and `1 - x` is
    /// otherwise two instructions.
    Rsb,
    Add,
    /// Add with carry, which is how a number wider than a word is added.
    Adc,
    /// Subtract with carry, which is the same for subtraction. The carry is a
    /// borrow held the other way up: set means none.
    Sbc,
    Rsc,
    /// `AND` for the flags alone.
    Tst,
    /// `EOR` for the flags alone. Also the usual way to ask whether two values
    /// have the same sign.
    Teq,
    /// `SUB` for the flags alone: the comparison.
    Cmp,
    /// `ADD` for the flags alone, which compares against a negated value.
    Cmn,
    Orr,
    /// The second operand, unchanged. It ignores the first entirely.
    Mov,
    /// Bit clear: the first with the second's bits taken out of it.
    Bic,
    /// The second operand, inverted. Also ignores the first.
    Mvn,
}

impl Op {
    pub const fn from_bits(bits: u32) -> Self {
        match bits & 0xF {
            0x0 => Self::And,
            0x1 => Self::Eor,
            0x2 => Self::Sub,
            0x3 => Self::Rsb,
            0x4 => Self::Add,
            0x5 => Self::Adc,
            0x6 => Self::Sbc,
            0x7 => Self::Rsc,
            0x8 => Self::Tst,
            0x9 => Self::Teq,
            0xA => Self::Cmp,
            0xB => Self::Cmn,
            0xC => Self::Orr,
            0xD => Self::Mov,
            0xE => Self::Bic,
            _ => Self::Mvn,
        }
    }

    /// Whether the result goes anywhere.
    ///
    /// The four comparisons compute an answer and throw it away, keeping only
    /// the flags. They are the same four that are only ever written with the
    /// `S` bit set, since without it they would do nothing whatsoever.
    pub const fn writes_result(self) -> bool {
        !matches!(self, Self::Tst | Self::Teq | Self::Cmp | Self::Cmn)
    }

    /// Whether the flags come from the adder or from the shifter.
    pub const fn is_arithmetic(self) -> bool {
        matches!(
            self,
            Self::Sub
                | Self::Rsb
                | Self::Add
                | Self::Adc
                | Self::Sbc
                | Self::Rsc
                | Self::Cmp
                | Self::Cmn
        )
    }

    /// Whether the first operand is read at all.
    ///
    /// `MOV` and `MVN` ignore it, which is why an assembler writes them with
    /// one register and not two, and why a decoder must not expect the field to
    /// hold anything meaningful.
    pub const fn reads_first(self) -> bool {
        !matches!(self, Self::Mov | Self::Mvn)
    }
}

/// What an operation produced.
pub struct Outcome {
    pub result: u32,
    pub carry: bool,
    /// The overflow flag, or nothing when the operation leaves it alone.
    ///
    /// A logical operation does not touch `V` at all, and this says so rather
    /// than handing back the old value and trusting the caller to notice: an
    /// `Option` cannot be written to the flag by accident.
    pub overflow: Option<bool>,
}

/// Runs one of the sixteen.
///
/// `flag_carry` is the carry flag as it stands, which only `ADC`, `SBC` and
/// `RSC` read. `shifter_carry` is what the barrel shifter produced working out
/// `b`, which is what the logical operations put in the carry flag. They are
/// separate arguments because they are different values and choosing the wrong
/// one is invisible until something branches the wrong way.
pub fn execute(op: Op, a: u32, b: u32, flag_carry: bool, shifter_carry: bool) -> Outcome {
    match op {
        Op::And | Op::Tst => logical(a & b, shifter_carry),
        Op::Eor | Op::Teq => logical(a ^ b, shifter_carry),
        Op::Orr => logical(a | b, shifter_carry),
        Op::Bic => logical(a & !b, shifter_carry),
        Op::Mov => logical(b, shifter_carry),
        Op::Mvn => logical(!b, shifter_carry),

        // `a - b` is `a + !b + 1`. The carry that comes out is "did not
        // borrow", which is the sense the whole architecture uses.
        Op::Sub | Op::Cmp => add(a, !b, true),
        Op::Sbc => add(a, !b, flag_carry),
        // Reversed: the same sum with the operands the other way round.
        Op::Rsb => add(b, !a, true),
        Op::Rsc => add(b, !a, flag_carry),

        Op::Add | Op::Cmn => add(a, b, false),
        Op::Adc => add(a, b, flag_carry),
    }
}

/// A result with no arithmetic behind it: the carry is the shifter's and the
/// overflow flag is not this instruction's business.
fn logical(result: u32, shifter_carry: bool) -> Outcome {
    Outcome { result, carry: shifter_carry, overflow: None }
}

/// The one adder every arithmetic operation goes through.
fn add(a: u32, b: u32, carry_in: bool) -> Outcome {
    // In 33 bits, so the carry out is a bit of the sum rather than something to
    // be reconstructed from the operands afterwards.
    let wide = u64::from(a) + u64::from(b) + u64::from(carry_in);
    let result = wide as u32;

    // Signed overflow is the two operands agreeing on a sign that the answer
    // then disagrees with. Two numbers of opposite sign can never overflow -
    // the answer lies between them - which is what the first term says.
    let overflow = (!(a ^ b) & (a ^ result)) & 0x8000_0000 != 0;

    Outcome { result, carry: wide > u64::from(u32::MAX), overflow: Some(overflow) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs an operation with both carries clear, which is the ordinary case.
    fn run(op: Op, a: u32, b: u32) -> Outcome {
        execute(op, a, b, false, false)
    }

    #[test]
    fn all_sixteen_encodings_name_an_operation() {
        const ORDER: [Op; 16] = [
            Op::And,
            Op::Eor,
            Op::Sub,
            Op::Rsb,
            Op::Add,
            Op::Adc,
            Op::Sbc,
            Op::Rsc,
            Op::Tst,
            Op::Teq,
            Op::Cmp,
            Op::Cmn,
            Op::Orr,
            Op::Mov,
            Op::Bic,
            Op::Mvn,
        ];
        for (bits, wanted) in ORDER.iter().enumerate() {
            assert_eq!(Op::from_bits(bits as u32), *wanted, "0x{bits:X}");
        }
    }

    #[test]
    fn the_logical_operations_compute_what_they_are_named_for() {
        assert_eq!(run(Op::And, 0xFF00_FF00, 0x0FF0_0FF0).result, 0x0F00_0F00);
        assert_eq!(run(Op::Eor, 0xFF00_FF00, 0x0FF0_0FF0).result, 0xF0F0_F0F0);
        assert_eq!(run(Op::Orr, 0xFF00_FF00, 0x0FF0_0FF0).result, 0xFFF0_FFF0);
        assert_eq!(run(Op::Bic, 0xFF00_FF00, 0x0FF0_0FF0).result, 0xF000_F000, "the second taken out");
        assert_eq!(run(Op::Mov, 0xDEAD_BEEF, 0x1234_5678).result, 0x1234_5678, "the first ignored");
        assert_eq!(run(Op::Mvn, 0xDEAD_BEEF, 0x1234_5678).result, 0xEDCB_A987);
    }

    /// The comparisons compute what their arithmetic twin does and keep only
    /// the flags. If these ever disagree with the twin, one of them is wrong.
    #[test]
    fn each_comparison_is_its_twin_with_the_result_thrown_away() {
        for (a, b) in [(0u32, 0u32), (1, 1), (5, 3), (3, 5), (0x8000_0000, 1), (u32::MAX, 1)] {
            for (test, twin) in
                [(Op::Tst, Op::And), (Op::Teq, Op::Eor), (Op::Cmp, Op::Sub), (Op::Cmn, Op::Add)]
            {
                let (one, other) = (run(test, a, b), run(twin, a, b));
                assert_eq!(one.result, other.result, "{test:?} against {twin:?} on {a:#X},{b:#X}");
                assert_eq!(one.carry, other.carry, "{test:?} carry");
                assert_eq!(one.overflow, other.overflow, "{test:?} overflow");
            }
        }
    }

    /// A logical operation puts the *shifter's* carry in the flag, not one of
    /// its own, and does not touch the overflow flag at all. This is the pair
    /// of rules that is easiest to get wrong and hardest to see wrong.
    #[test]
    fn a_logical_operation_passes_the_shifters_carry_and_leaves_overflow_alone() {
        for op in [Op::And, Op::Eor, Op::Orr, Op::Bic, Op::Mov, Op::Mvn, Op::Tst, Op::Teq] {
            for shifter in [true, false] {
                // The flag carry is deliberately the opposite, so that reading
                // the wrong one shows up.
                let out = execute(op, 0xF0F0_F0F0, 0x0F0F_0F0F, !shifter, shifter);
                assert_eq!(out.carry, shifter, "{op:?} took the wrong carry");
                assert_eq!(out.overflow, None, "{op:?} touched the overflow flag");
            }
        }
    }

    /// And an arithmetic one ignores the shifter entirely: its carry is the
    /// adder's.
    #[test]
    fn an_arithmetic_operation_ignores_the_shifters_carry() {
        for op in [Op::Add, Op::Sub, Op::Rsb, Op::Cmp, Op::Cmn] {
            let with = execute(op, 12, 5, false, true);
            let without = execute(op, 12, 5, false, false);
            assert_eq!(with.carry, without.carry, "{op:?}");
            assert_eq!(with.result, without.result, "{op:?}");
        }
    }

    #[test]
    fn the_arithmetic_operations_compute_what_they_are_named_for() {
        assert_eq!(run(Op::Add, 12, 5).result, 17);
        assert_eq!(run(Op::Sub, 12, 5).result, 7);
        assert_eq!(run(Op::Rsb, 12, 5).result, 0xFFFF_FFF9, "5 - 12, reversed");
        assert_eq!(run(Op::Rsb, 5, 12).result, 7);
    }

    /// Carry set means *no borrow*, which reads backwards from most processors
    /// and is why `CS` and "unsigned higher or same" are the same bit.
    #[test]
    fn a_subtraction_sets_the_carry_when_it_did_not_borrow() {
        assert!(run(Op::Sub, 12, 5).carry, "12 - 5 does not borrow");
        assert!(run(Op::Sub, 5, 5).carry, "nor does 5 - 5");
        assert!(!run(Op::Sub, 5, 12).carry, "5 - 12 does");
        assert!(run(Op::Sub, 0, 0).carry, "and 0 - 0 does not");
        assert!(!run(Op::Sub, 0, 1).carry);
    }

    #[test]
    fn an_addition_sets_the_carry_when_a_bit_leaves_the_top() {
        assert!(!run(Op::Add, 1, 1).carry);
        assert!(run(Op::Add, 0xFFFF_FFFF, 1).carry);
        assert_eq!(run(Op::Add, 0xFFFF_FFFF, 1).result, 0, "and it wraps");
        assert!(run(Op::Add, 0x8000_0000, 0x8000_0000).carry);
    }

    /// The carry in is what makes a number wider than a word addable. Adding
    /// two 64-bit values is `ADDS` on the low halves and `ADC` on the high.
    #[test]
    fn adding_with_carry_is_how_a_wider_number_is_added() {
        // 0x1_0000_0000 + 0x1_0000_0000, as two halves.
        let low = execute(Op::Add, 0, 0, false, false);
        assert!(!low.carry);
        let high = execute(Op::Adc, 1, 1, low.carry, false);
        assert_eq!(high.result, 2);

        // And with a carry out of the low half.
        let low = execute(Op::Add, 0x8000_0000, 0x8000_0000, false, false);
        assert!(low.carry);
        assert_eq!(low.result, 0);
        let high = execute(Op::Adc, 0, 0, low.carry, false);
        assert_eq!(high.result, 1, "the carry came up");
    }

    /// Subtract with carry reads the flag as a borrow held the other way up:
    /// set means none, so `SBC` with the carry set is a plain subtraction.
    #[test]
    fn subtracting_with_carry_reads_the_flag_as_the_absence_of_a_borrow() {
        assert_eq!(execute(Op::Sbc, 12, 5, true, false).result, 7, "carry set: no borrow");
        assert_eq!(execute(Op::Sbc, 12, 5, false, false).result, 6, "carry clear: one borrowed");
        assert_eq!(execute(Op::Rsc, 5, 12, true, false).result, 7, "and reversed");
        assert_eq!(execute(Op::Rsc, 5, 12, false, false).result, 6);
    }

    /// Signed overflow is the operands agreeing on a sign the answer then
    /// disagrees with. Two of opposite sign can never overflow, because the
    /// answer lies between them.
    #[test]
    fn overflow_is_the_signed_answer_not_fitting() {
        // Positive plus positive coming out negative.
        assert_eq!(run(Op::Add, 0x7FFF_FFFF, 1).overflow, Some(true));
        // Negative plus negative coming out positive.
        assert_eq!(run(Op::Add, 0x8000_0000, 0x8000_0000).overflow, Some(true));
        // Opposite signs: never.
        assert_eq!(run(Op::Add, 0x7FFF_FFFF, 0x8000_0000).overflow, Some(false));
        assert_eq!(run(Op::Add, 1, 0xFFFF_FFFF).overflow, Some(false));
        // Ordinary sums do not overflow however large the unsigned carry.
        assert_eq!(run(Op::Add, 0xFFFF_FFFF, 0xFFFF_FFFF).overflow, Some(false), "-1 + -1");
    }

    /// The same rule seen through a subtraction, where the operands' signs are
    /// compared after one of them is inverted.
    #[test]
    fn a_subtraction_overflows_when_the_signs_differ_and_the_answer_takes_the_wrong_one() {
        // Most negative minus one: no room below.
        assert_eq!(run(Op::Sub, 0x8000_0000, 1).overflow, Some(true));
        // Most positive minus minus-one.
        assert_eq!(run(Op::Sub, 0x7FFF_FFFF, 0xFFFF_FFFF).overflow, Some(true));
        // Same signs: never.
        assert_eq!(run(Op::Sub, 5, 3).overflow, Some(false));
        assert_eq!(run(Op::Sub, 3, 5).overflow, Some(false));
        assert_eq!(run(Op::Sub, 0x8000_0000, 0x8000_0000).overflow, Some(false));
    }

    /// Carry and overflow answer different questions and must not track each
    /// other. A sum can do either, both or neither.
    #[test]
    fn carry_and_overflow_are_independent() {
        let both = run(Op::Add, 0x8000_0000, 0x8000_0000);
        assert_eq!((both.carry, both.overflow), (true, Some(true)), "-2^31 + -2^31");

        let carry_only = run(Op::Add, 0xFFFF_FFFF, 1);
        assert_eq!((carry_only.carry, carry_only.overflow), (true, Some(false)), "-1 + 1");

        let overflow_only = run(Op::Add, 0x7FFF_FFFF, 1);
        assert_eq!((overflow_only.carry, overflow_only.overflow), (false, Some(true)));

        let neither = run(Op::Add, 1, 1);
        assert_eq!((neither.carry, neither.overflow), (false, Some(false)));
    }

    /// Which operations write a result, and which two ignore their first
    /// operand. A decoder that expects `MOV` to read a register field finds
    /// rubbish in it.
    #[test]
    fn the_comparisons_write_nothing_and_the_moves_read_nothing() {
        for op in [Op::Tst, Op::Teq, Op::Cmp, Op::Cmn] {
            assert!(!op.writes_result(), "{op:?}");
            assert!(op.is_arithmetic() == matches!(op, Op::Cmp | Op::Cmn), "{op:?}");
        }
        for op in [Op::And, Op::Add, Op::Mov, Op::Mvn, Op::Orr] {
            assert!(op.writes_result(), "{op:?}");
        }
        assert!(!Op::Mov.reads_first());
        assert!(!Op::Mvn.reads_first());
        assert!(Op::And.reads_first());
        assert!(Op::Sub.reads_first());
    }

    /// Every arithmetic operation goes through the one adder, so the reversed
    /// pair must agree with the ordinary pair on swapped operands - flags
    /// included, since that is the part a separate code path would get wrong.
    #[test]
    fn the_reversed_subtractions_match_the_ordinary_ones_swapped() {
        for a in [0u32, 1, 5, 0x7FFF_FFFF, 0x8000_0000, u32::MAX] {
            for b in [0u32, 1, 5, 0x7FFF_FFFF, 0x8000_0000, u32::MAX] {
                for carry in [true, false] {
                    let forward = execute(Op::Sub, a, b, carry, false);
                    let reversed = execute(Op::Rsb, b, a, carry, false);
                    assert_eq!(forward.result, reversed.result, "{a:#X} - {b:#X}");
                    assert_eq!(forward.carry, reversed.carry, "{a:#X} - {b:#X} carry");
                    assert_eq!(forward.overflow, reversed.overflow, "{a:#X} - {b:#X} overflow");

                    let forward = execute(Op::Sbc, a, b, carry, false);
                    let reversed = execute(Op::Rsc, b, a, carry, false);
                    assert_eq!(forward.result, reversed.result, "{a:#X} -c {b:#X}");
                    assert_eq!(forward.carry, reversed.carry, "{a:#X} -c {b:#X} carry");
                }
            }
        }
    }

    /// Nothing here may panic on any pair of operands, which in Rust means no
    /// bare arithmetic on the edges of the range.
    #[test]
    fn no_pair_of_operands_overflows_the_arithmetic() {
        const EDGES: [u32; 8] =
            [0, 1, 2, 0x7FFF_FFFE, 0x7FFF_FFFF, 0x8000_0000, 0x8000_0001, u32::MAX];
        for op in [Op::Add, Op::Adc, Op::Sub, Op::Sbc, Op::Rsb, Op::Rsc, Op::Cmp, Op::Cmn] {
            for a in EDGES {
                for b in EDGES {
                    for carry in [true, false] {
                        let _ = execute(op, a, b, carry, !carry);
                    }
                }
            }
        }
    }
}
