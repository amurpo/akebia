//! The barrel shifter: the second operand of almost every data instruction.
//!
//! # Why a shift is not an instruction here
//!
//! On most processors shifting is something you do and then use. On this one it
//! is part of the operand: `ADD r0, r1, r2, LSL #3` shifts and adds in one
//! instruction and one cycle, because the shifter sits in front of the ALU and
//! everything passes through it. Which is why an emulator cannot treat shifts
//! as a corner of the instruction set — this code runs on the majority of
//! instructions the machine executes, and every one of its edge cases is on a
//! path something takes.
//!
//! # The edge cases are the whole job
//!
//! There are two ways to say how far to shift and **they do not agree**, which
//! is the single most concentrated source of wrong answers in an ARM
//! interpreter.
//!
//! A shift by a *constant* has five bits to say it in, so it cannot say
//! thirty-two. Rather than waste the encoding, a zero is read as something else
//! entirely for three of the four kinds: `LSR #0` means `LSR #32`, `ASR #0`
//! means `ASR #32`, and `ROR #0` is not a rotate at all but `RRX`, a one-bit
//! rotate through the carry flag. Only `LSL #0` means what it says, which is to
//! do nothing.
//!
//! A shift by a *register* takes the bottom byte, so it can say up to 255. Here
//! zero does mean zero — value and carry both untouched, for every kind — and
//! the large amounts have their own answers: shifting a 32-bit value right by
//! 32 leaves nothing, and the carry comes from the last bit to fall off, which
//! is not the same as the carry from shifting by 33.
//!
//! So the same written amount can mean two different things depending on where
//! it came from, and that is why there are two functions below and not one with
//! a flag.
//!
//! # The carry is an output, not an afterthought
//!
//! Every shift produces a carry bit as well as a value, and it is the bit that
//! last fell off the end. A shift of zero has nothing fall off, so the carry
//! passes through — which is why the flag as it stands has to be handed in.

/// Which of the four shifts, as the two bits encode them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Left, filling with zeros.
    Lsl,
    /// Right, filling with zeros.
    Lsr,
    /// Right, filling with the sign bit, which is how a signed divide by a
    /// power of two is written.
    Asr,
    /// Right, filling with what fell off the bottom.
    Ror,
}

impl Kind {
    pub const fn from_bits(bits: u32) -> Self {
        match bits & 0b11 {
            0b00 => Self::Lsl,
            0b01 => Self::Lsr,
            0b10 => Self::Asr,
            _ => Self::Ror,
        }
    }
}

/// What came out of the shifter: the value, and the bit that fell off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shifted {
    pub value: u32,
    pub carry: bool,
}

/// A shift by an amount written into the instruction, where zero is a special
/// encoding for three of the four kinds.
///
/// `amount` is the five-bit field, so `0..=31`.
pub fn by_immediate(kind: Kind, amount: u32, value: u32, carry_in: bool) -> Shifted {
    if amount != 0 {
        return within_word(kind, amount, value);
    }
    match kind {
        // The only one that means what it says.
        Kind::Lsl => Shifted { value, carry: carry_in },
        // `LSR #32`: everything falls off, and the last bit to go is the top one.
        Kind::Lsr => Shifted { value: 0, carry: value & 0x8000_0000 != 0 },
        // `ASR #32`: the sign fills the whole word, so the answer is all ones or
        // all zeros and the carry is that same sign.
        Kind::Asr => {
            let sign = value & 0x8000_0000 != 0;
            Shifted { value: if sign { 0xFFFF_FFFF } else { 0 }, carry: sign }
        }
        // `RRX`: not a rotate by zero but a rotate by one *through* the carry,
        // making a 33-bit shift register out of the word and the flag. It is
        // the only shift whose result depends on a flag.
        Kind::Ror => {
            Shifted { value: (u32::from(carry_in) << 31) | (value >> 1), carry: value & 1 != 0 }
        }
    }
}

/// A shift by an amount held in a register, where zero means zero and the
/// amount can reach 255.
///
/// Only the bottom byte of the register is read; the other twenty-four bits are
/// ignored however large the number in there looks.
pub fn by_register(kind: Kind, amount: u32, value: u32, carry_in: bool) -> Shifted {
    let amount = amount & 0xFF;
    if amount == 0 {
        // For every kind, and this is where the two ways of writing a shift
        // differ most: nothing happens at all, not even to the carry.
        return Shifted { value, carry: carry_in };
    }
    if amount < 32 {
        return within_word(kind, amount, value);
    }

    match kind {
        // Shifted right out of the word entirely. At exactly 32 the last bit to
        // fall off is still one of the value's own; past that, only zeros have
        // been falling off for a while.
        Kind::Lsl if amount == 32 => Shifted { value: 0, carry: value & 1 != 0 },
        Kind::Lsl => Shifted { value: 0, carry: false },
        Kind::Lsr if amount == 32 => Shifted { value: 0, carry: value & 0x8000_0000 != 0 },
        Kind::Lsr => Shifted { value: 0, carry: false },
        // An arithmetic shift never runs out: past 31 the sign has filled the
        // word and shifting further changes nothing.
        Kind::Asr => {
            let sign = value & 0x8000_0000 != 0;
            Shifted { value: if sign { 0xFFFF_FFFF } else { 0 }, carry: sign }
        }
        // A rotate never runs out either, it comes round. Only the low five bits
        // matter, and a multiple of 32 leaves the value alone - but not the
        // carry, which still comes from the bit that would have gone round last.
        Kind::Ror => {
            let turn = amount & 31;
            if turn == 0 {
                Shifted { value, carry: value & 0x8000_0000 != 0 }
            } else {
                within_word(Kind::Ror, turn, value)
            }
        }
    }
}

/// The four shifts for an amount of `1..=31`, where all of them behave
/// ordinarily and neither encoding has anything special to say.
fn within_word(kind: Kind, amount: u32, value: u32) -> Shifted {
    match kind {
        // Counting from the top: shifting left by `n` pushes bit `32 - n` off.
        Kind::Lsl => {
            Shifted { value: value << amount, carry: value >> (32 - amount) & 1 != 0 }
        }
        Kind::Lsr => Shifted { value: value >> amount, carry: value >> (amount - 1) & 1 != 0 },
        Kind::Asr => Shifted {
            value: ((value as i32) >> amount) as u32,
            carry: value >> (amount - 1) & 1 != 0,
        },
        Kind::Ror => {
            Shifted { value: value.rotate_right(amount), carry: value >> (amount - 1) & 1 != 0 }
        }
    }
}

/// The immediate operand of a data instruction: eight bits rotated right by
/// twice a four-bit field.
///
/// # Why a constant is built this way
///
/// Because a 32-bit instruction cannot hold a 32-bit constant and still have
/// room to say what to do with it. Eight bits placed at any even position
/// covers what compiled code actually asks for — small numbers, bit masks,
/// addresses of aligned things — and what it does not cover the assembler
/// builds out of two instructions.
///
/// The rotate is *not* a shift by the field: it is a shift by twice it, so the
/// reachable positions are the even ones only. And the carry follows the same
/// rule the shifter's does, which means a constant can set the carry flag as a
/// side effect of how it was encoded — `MOVS r0, #0x8000_0000` clears the carry
/// on some encodings and not others, purely because of where the byte sits.
pub fn immediate(rotate: u32, byte: u32, carry_in: bool) -> Shifted {
    let amount = (rotate & 0xF) * 2;
    if amount == 0 {
        return Shifted { value: byte, carry: carry_in };
    }
    let value = byte.rotate_right(amount);
    Shifted { value, carry: value & 0x8000_0000 != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain cases, where both ways of writing an amount agree. Everything
    /// else here is a departure from these.
    #[test]
    fn the_four_shifts_do_the_ordinary_thing_in_between() {
        for amount in 1..32 {
            for (kind, wanted) in [
                (Kind::Lsl, 0x1234_5678u32 << amount),
                (Kind::Lsr, 0x1234_5678 >> amount),
                (Kind::Asr, (0x1234_5678i32 >> amount) as u32),
                (Kind::Ror, 0x1234_5678u32.rotate_right(amount)),
            ] {
                let by_field = by_immediate(kind, amount, 0x1234_5678, false);
                let by_reg = by_register(kind, amount, 0x1234_5678, false);
                assert_eq!(by_field.value, wanted, "{kind:?} by {amount}");
                assert_eq!(by_reg, by_field, "{kind:?} by {amount}: the two ways agree here");
            }
        }
    }

    /// An arithmetic shift fills with the sign, which is what makes it a signed
    /// divide and not a logical shift.
    #[test]
    fn an_arithmetic_shift_carries_the_sign_down_with_it() {
        assert_eq!(by_immediate(Kind::Asr, 4, 0x8000_0000, false).value, 0xF800_0000);
        assert_eq!(by_immediate(Kind::Lsr, 4, 0x8000_0000, false).value, 0x0800_0000);
    }

    /// A constant amount has five bits and cannot say thirty-two, so a zero is
    /// read as something else for three of the four kinds.
    #[test]
    fn a_written_zero_means_something_else_for_three_of_the_four() {
        // LSL #0 is the one that means what it says.
        assert_eq!(by_immediate(Kind::Lsl, 0, 0x1234, true), Shifted { value: 0x1234, carry: true });
        assert_eq!(
            by_immediate(Kind::Lsl, 0, 0x1234, false),
            Shifted { value: 0x1234, carry: false },
            "and it passes the carry through untouched"
        );

        // LSR #0 means LSR #32: everything goes, the top bit last.
        assert_eq!(
            by_immediate(Kind::Lsr, 0, 0x8000_0000, false),
            Shifted { value: 0, carry: true }
        );
        assert_eq!(
            by_immediate(Kind::Lsr, 0, 0x7FFF_FFFF, true),
            Shifted { value: 0, carry: false }
        );

        // ASR #0 means ASR #32: the sign fills the word.
        assert_eq!(
            by_immediate(Kind::Asr, 0, 0x8000_0000, false),
            Shifted { value: 0xFFFF_FFFF, carry: true }
        );
        assert_eq!(
            by_immediate(Kind::Asr, 0, 0x7FFF_FFFF, true),
            Shifted { value: 0, carry: false }
        );
    }

    /// `ROR #0` is not a rotate by zero. It is `RRX`: a one-bit rotate through
    /// the carry, making a 33-bit register out of the word and the flag, and it
    /// is the only shift whose *result* depends on a flag.
    #[test]
    fn a_written_zero_rotate_is_a_rotate_through_the_carry_instead() {
        assert_eq!(
            by_immediate(Kind::Ror, 0, 0x0000_0001, false),
            Shifted { value: 0, carry: true },
            "the bottom bit falls into the carry"
        );
        assert_eq!(
            by_immediate(Kind::Ror, 0, 0x0000_0000, true),
            Shifted { value: 0x8000_0000, carry: false },
            "and the carry comes in at the top"
        );
        // Round trip: thirty-three of them return the value and the flag.
        let (mut value, mut carry) = (0xDEAD_BEEFu32, true);
        for _ in 0..33 {
            let out = by_immediate(Kind::Ror, 0, value, carry);
            value = out.value;
            carry = out.carry;
        }
        assert_eq!((value, carry), (0xDEAD_BEEF, true), "thirty-three bits come round");
    }

    /// A register amount of zero means zero for every kind, carry included.
    /// This is where the two ways of writing a shift differ most: the same
    /// written zero means four different things above and one thing here.
    #[test]
    fn a_register_zero_means_zero_for_every_kind() {
        for kind in [Kind::Lsl, Kind::Lsr, Kind::Asr, Kind::Ror] {
            for carry in [true, false] {
                assert_eq!(
                    by_register(kind, 0, 0x8000_0001, carry),
                    Shifted { value: 0x8000_0001, carry },
                    "{kind:?} by a register holding zero"
                );
            }
        }
    }

    /// Only the bottom byte of the register is read, however large the number
    /// in the rest of it looks.
    #[test]
    fn only_the_bottom_byte_of_the_amount_is_read() {
        assert_eq!(
            by_register(Kind::Lsl, 0xFFFF_FF04, 1, false),
            by_register(Kind::Lsl, 4, 1, false),
            "the top three bytes are ignored"
        );
        assert_eq!(
            by_register(Kind::Lsl, 0x1234_5600, 0xABCD, true),
            Shifted { value: 0xABCD, carry: true },
            "and a bottom byte of zero is still zero"
        );
    }

    /// Shifting a word right out of itself. At exactly 32 the last bit to fall
    /// off is one of the value's own; past that only zeros have been falling.
    #[test]
    fn a_register_amount_of_exactly_thirty_two_keeps_one_last_bit() {
        assert_eq!(by_register(Kind::Lsl, 32, 1, false), Shifted { value: 0, carry: true });
        assert_eq!(by_register(Kind::Lsl, 33, 1, false), Shifted { value: 0, carry: false });
        assert_eq!(by_register(Kind::Lsl, 255, 0xFFFF_FFFF, true), Shifted {
            value: 0,
            carry: false
        });

        assert_eq!(
            by_register(Kind::Lsr, 32, 0x8000_0000, false),
            Shifted { value: 0, carry: true }
        );
        assert_eq!(
            by_register(Kind::Lsr, 33, 0x8000_0000, true),
            Shifted { value: 0, carry: false }
        );
    }

    /// An arithmetic shift never runs out: past 31 the sign has filled the word
    /// and shifting further changes nothing at all.
    #[test]
    fn an_arithmetic_shift_saturates_rather_than_emptying() {
        for amount in [32, 33, 100, 255] {
            assert_eq!(
                by_register(Kind::Asr, amount, 0x8000_0000, false),
                Shifted { value: 0xFFFF_FFFF, carry: true },
                "negative, by {amount}"
            );
            assert_eq!(
                by_register(Kind::Asr, amount, 0x7FFF_FFFF, true),
                Shifted { value: 0, carry: false },
                "positive, by {amount}"
            );
        }
    }

    /// A rotate comes round, so only the low five bits of the amount matter.
    /// A whole number of turns leaves the value alone but still sets the carry
    /// from the bit that would have gone round last.
    #[test]
    fn a_rotate_by_a_whole_number_of_turns_moves_only_the_carry() {
        for amount in [32u32, 64, 96, 128, 160, 192, 224] {
            assert_eq!(
                by_register(Kind::Ror, amount, 0x8000_0001, false),
                Shifted { value: 0x8000_0001, carry: true },
                "by {amount}, a whole number of turns"
            );
            assert_eq!(
                by_register(Kind::Ror, amount, 0x0000_0001, true),
                Shifted { value: 1, carry: false },
                "by {amount}, with the top bit clear"
            );
        }
        // And anything else is the rotate its low five bits name.
        for amount in 1..255u32 {
            if amount & 31 != 0 {
                assert_eq!(
                    by_register(Kind::Ror, amount, 0xDEAD_BEEF, false).value,
                    0xDEAD_BEEFu32.rotate_right(amount & 31),
                    "by {amount}"
                );
            }
        }
    }

    /// The carry out is the bit that last fell off the end, whichever direction
    /// it fell off in. Checked against a bit-by-bit shift rather than against a
    /// restatement of the code above.
    #[test]
    fn the_carry_is_the_last_bit_to_fall_off() {
        let value = 0b1010_1100_0011_0101_1111_0000_1001_0110u32;
        for amount in 1..32 {
            assert_eq!(
                by_immediate(Kind::Lsl, amount, value, false).carry,
                value & (1 << (32 - amount)) != 0,
                "left by {amount}"
            );
            for kind in [Kind::Lsr, Kind::Asr, Kind::Ror] {
                assert_eq!(
                    by_immediate(kind, amount, value, false).carry,
                    value & (1 << (amount - 1)) != 0,
                    "{kind:?} by {amount}"
                );
            }
        }
    }

    /// A shifter that reached for a 32-bit shift in Rust would panic in debug
    /// and give a wrong answer in release. Every amount a register can hold has
    /// to come out without either.
    #[test]
    fn no_amount_a_register_can_hold_overflows_the_shift() {
        for kind in [Kind::Lsl, Kind::Lsr, Kind::Asr, Kind::Ror] {
            for amount in 0..=0xFFu32 {
                let _ = by_register(kind, amount, 0xDEAD_BEEF, true);
                let _ = by_register(kind, amount | 0xFFFF_FF00, 0xDEAD_BEEF, false);
            }
            for amount in 0..32 {
                let _ = by_immediate(kind, amount, 0xDEAD_BEEF, true);
            }
        }
    }

    /// A constant is eight bits rotated by *twice* a four-bit field, so it can
    /// only land on even positions.
    #[test]
    fn a_constant_is_a_byte_rotated_by_twice_its_field() {
        assert_eq!(immediate(0, 0xFF, false).value, 0xFF, "no rotate");
        assert_eq!(immediate(1, 0xFF, false).value, 0xC000_003F, "by two, not by one");
        assert_eq!(immediate(4, 0xFF, false).value, 0xFF00_0000, "by eight");
        assert_eq!(immediate(6, 0xFF, false).value, 0x0FF0_0000, "by twelve");
        assert_eq!(immediate(8, 0x01, false).value, 0x0001_0000, "by sixteen");

        // The field is four bits, so the far end comes back round to the start.
        assert_eq!(immediate(15, 0xFF, false).value, 0x03FC, "by thirty");
    }

    /// A constant's carry follows the shifter's rule, so how a number was
    /// encoded can set the carry flag as a side effect.
    #[test]
    fn a_rotated_constant_sets_the_carry_from_its_top_bit() {
        // Not rotated: the carry passes through, whatever the byte is.
        assert!(immediate(0, 0xFF, true).carry);
        assert!(!immediate(0, 0xFF, false).carry);
        // Rotated: the carry is the top bit of what came out.
        assert!(immediate(1, 0xFF, false).carry, "0xC000003F has its top bit set");
        assert!(!immediate(6, 0xFF, true).carry, "0x0FF00000 does not");
    }
}
