//! SM83 register file and flags.
//!
//! The SM83 exposes eight 8-bit registers (`A B C D E H L` plus `F`, which is
//! not general purpose) paired into four 16-bit ones: `AF BC DE HL`. The [`R8`]
//! and [`R16`] enums exist so that the instruction decoder is a direct
//! translation of the opcode table instead of a sea of nested `match`es.

use core::fmt;

/// Flags of the `F` register. The low 4 bits are hardwired to zero: writing a 1
/// into them has no effect and they always read back as 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(u8);

impl Flags {
    /// Zero: the result was 0.
    pub const Z: u8 = 0b1000_0000;
    /// Subtract: the last ALU operation was a subtraction. Only `DAA` consumes it.
    pub const N: u8 = 0b0100_0000;
    /// Half-carry: carry from bit 3 into bit 4. Also for `DAA` only.
    pub const H: u8 = 0b0010_0000;
    /// Carry: carry out of bit 7 (or bit 15 in 16-bit operations).
    pub const C: u8 = 0b0001_0000;

    const MASK: u8 = 0b1111_0000;

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & Self::MASK)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    pub const fn z(self) -> bool {
        self.contains(Self::Z)
    }
    pub const fn n(self) -> bool {
        self.contains(Self::N)
    }
    pub const fn h(self) -> bool {
        self.contains(Self::H)
    }
    pub const fn c(self) -> bool {
        self.contains(Self::C)
    }

    /// Turns a specific flag on or off.
    pub fn set(&mut self, flag: u8, value: bool) {
        if value {
            self.0 |= flag & Self::MASK;
        } else {
            self.0 &= !(flag & Self::MASK);
        }
    }

    /// Writes all four flags at once. This is the usual way to finish an ALU
    /// operation.
    pub fn write(&mut self, z: bool, n: bool, h: bool, c: bool) {
        self.0 = (u8::from(z) * Self::Z)
            | (u8::from(n) * Self::N)
            | (u8::from(h) * Self::H)
            | (u8::from(c) * Self::C);
    }
}

impl fmt::Display for Flags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bit = |on: bool, c: char| if on { c } else { '-' };
        write!(
            f,
            "{}{}{}{}",
            bit(self.z(), 'Z'),
            bit(self.n(), 'N'),
            bit(self.h(), 'H'),
            bit(self.c(), 'C')
        )
    }
}

/// 8-bit register addressable by the 3-bit field of an opcode.
///
/// Code 110 is not a register but `(HL)`, a memory access; that is why
/// [`R8::from_code`] returns `None` in that case and the decoder must handle it
/// separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum R8 {
    A,
    B,
    C,
    D,
    E,
    H,
    L,
}

impl R8 {
    /// Translates the 3-bit field of an opcode. `None` means `(HL)`.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code & 0x07 {
            0 => Some(Self::B),
            1 => Some(Self::C),
            2 => Some(Self::D),
            3 => Some(Self::E),
            4 => Some(Self::H),
            5 => Some(Self::L),
            6 => None, // (HL)
            _ => Some(Self::A),
        }
    }
}

/// 16-bit register pair. `AF` and `SP` share the encoding `11` depending on the
/// instruction group, which is why both are here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum R16 {
    Af,
    Bc,
    De,
    Hl,
    Sp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registers {
    pub a: u8,
    pub f: Flags,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
}

impl Registers {
    /// State the DMG BootROM leaves the CPU in right before jumping to
    /// `0x0100`. It allows booting games without having the BootROM.
    pub const fn post_boot_dmg() -> Self {
        Self {
            a: 0x01,
            f: Flags(0xB0),
            b: 0x00,
            c: 0x13,
            d: 0x00,
            e: 0xD8,
            h: 0x01,
            l: 0x4D,
            sp: 0xFFFE,
            pc: 0x0100,
        }
    }

    /// The same for the CGB: `A = 0x11` is exactly what games check to find out
    /// which machine they are running on.
    pub const fn post_boot_cgb() -> Self {
        Self {
            a: 0x11,
            f: Flags(0x80),
            b: 0x00,
            c: 0x00,
            d: 0xFF,
            e: 0x56,
            h: 0x00,
            l: 0x0D,
            sp: 0xFFFE,
            pc: 0x0100,
        }
    }

    pub fn read8(&self, r: R8) -> u8 {
        match r {
            R8::A => self.a,
            R8::B => self.b,
            R8::C => self.c,
            R8::D => self.d,
            R8::E => self.e,
            R8::H => self.h,
            R8::L => self.l,
        }
    }

    pub fn write8(&mut self, r: R8, value: u8) {
        match r {
            R8::A => self.a = value,
            R8::B => self.b = value,
            R8::C => self.c = value,
            R8::D => self.d = value,
            R8::E => self.e = value,
            R8::H => self.h = value,
            R8::L => self.l = value,
        }
    }

    pub fn read16(&self, r: R16) -> u16 {
        let (hi, lo) = match r {
            R16::Af => (self.a, self.f.bits()),
            R16::Bc => (self.b, self.c),
            R16::De => (self.d, self.e),
            R16::Hl => (self.h, self.l),
            R16::Sp => return self.sp,
        };
        u16::from_be_bytes([hi, lo])
    }

    pub fn write16(&mut self, r: R16, value: u16) {
        let [hi, lo] = value.to_be_bytes();
        match r {
            // Writing to AF discards the low 4 bits: they are not real storage.
            R16::Af => (self.a, self.f) = (hi, Flags::from_bits(lo)),
            R16::Bc => (self.b, self.c) = (hi, lo),
            R16::De => (self.d, self.e) = (hi, lo),
            R16::Hl => (self.h, self.l) = (hi, lo),
            R16::Sp => self.sp = value,
        }
    }

    /// Shortcut for the heavy use of `HL` as a pointer.
    pub fn hl(&self) -> u16 {
        self.read16(R16::Hl)
    }

    pub fn set_hl(&mut self, value: u16) {
        self.write16(R16::Hl, value);
    }
}

impl Default for Registers {
    fn default() -> Self {
        Self::post_boot_dmg()
    }
}

impl fmt::Display for Registers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "A:{:02X} F:{} BC:{:04X} DE:{:04X} HL:{:04X} SP:{:04X} PC:{:04X}",
            self.a,
            self.f,
            self.read16(R16::Bc),
            self.read16(R16::De),
            self.read16(R16::Hl),
            self.sp,
            self.pc
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_low_bits_of_f_are_hardwired_to_zero() {
        let mut r = Registers::default();
        r.write16(R16::Af, 0x12FF);
        assert_eq!(r.read16(R16::Af), 0x12F0);
    }

    #[test]
    fn pairs_share_storage_with_their_halves() {
        let mut r = Registers::default();
        r.write16(R16::Bc, 0xBEEF);
        assert_eq!((r.b, r.c), (0xBE, 0xEF));
        r.write8(R8::C, 0x01);
        assert_eq!(r.read16(R16::Bc), 0xBE01);
    }

    #[test]
    fn register_encoding_of_the_opcodes() {
        assert_eq!(R8::from_code(0b000), Some(R8::B));
        assert_eq!(R8::from_code(0b110), None, "110 is (HL), not a register");
        assert_eq!(R8::from_code(0b111), Some(R8::A));
    }

    #[test]
    fn joint_flag_write() {
        let mut f = Flags::default();
        f.write(true, false, true, false);
        assert_eq!(f.bits(), Flags::Z | Flags::H);
        assert!(f.z() && f.h() && !f.n() && !f.c());
    }
}
