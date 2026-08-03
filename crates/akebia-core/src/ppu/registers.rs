//! `LCDC` (0xFF40) and `STAT` (0xFF41), wrapped in named types.
//!
//! They could be two bare `u8`s, but then the rendering code would be full of
//! `if lcdc & 0x10 != 0`. Wrapping them turns every bit into a readable question
//! and avoids confusing bit 3 with bit 4, which is exactly the mistake that
//! makes the tiles come out swapped.

use super::Mode;

/// LCD Control (0xFF40). Each bit enables or configures a part of the PPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lcdc(u8);

impl Lcdc {
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Bit 7: with the LCD off the PPU stops completely.
    pub const fn lcd_enabled(self) -> bool {
        self.0 & 0x80 != 0
    }

    /// Bit 6: window tile map.
    pub const fn window_tilemap(self) -> u16 {
        if self.0 & 0x40 != 0 {
            0x9C00
        } else {
            0x9800
        }
    }

    /// Bit 5: window visible.
    pub const fn window_enabled(self) -> bool {
        self.0 & 0x20 != 0
    }

    /// Bit 4: tile data area for the background and the window.
    ///
    /// With the bit at 0 the base is 0x9000 and **the tile index is read as
    /// signed**, so the range 128..255 falls in 0x8800..0x9000. It is the quirk
    /// that gets implemented wrong most often.
    pub const fn bg_tile_data_signed(self) -> bool {
        self.0 & 0x10 == 0
    }

    /// Bit 3: background tile map.
    pub const fn bg_tilemap(self) -> u16 {
        if self.0 & 0x08 != 0 {
            0x9C00
        } else {
            0x9800
        }
    }

    /// Bit 2: sprite height, 8 or 16 pixels.
    pub const fn sprite_height(self) -> u8 {
        if self.0 & 0x04 != 0 {
            16
        } else {
            8
        }
    }

    /// Bit 1: sprites visible.
    pub const fn sprites_enabled(self) -> bool {
        self.0 & 0x02 != 0
    }

    /// Bit 0: on DMG it turns off background and window. On CGB it means
    /// something else (it removes the sprites' priority), so it has to be
    /// handled separately when adding colour mode.
    pub const fn bg_enabled(self) -> bool {
        self.0 & 0x01 != 0
    }
}

/// LCD Status (0xFF41).
///
/// Only bits 6-3 are writable: they select which conditions feed the STAT
/// interrupt. Bits 2-0 are produced by the PPU and bit 7 does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat(u8);

impl Stat {
    const WRITABLE: u8 = 0b0111_1000;

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & Self::WRITABLE)
    }

    /// Value as the CPU reads it: the configured sources, plus the current mode
    /// and coincidence flag, plus bit 7 hardwired to 1.
    pub const fn bits_with_mode(self, mode: Mode, coincidence: bool) -> u8 {
        0x80 | self.0 | ((coincidence as u8) << 2) | (mode as u8)
    }

    pub fn write(&mut self, value: u8) {
        self.0 = value & Self::WRITABLE;
    }

    /// Bit 6: interrupt when `LY == LYC`.
    pub const fn lyc_interrupt(self) -> bool {
        self.0 & 0x40 != 0
    }

    /// Bit 5: interrupt when entering mode 2.
    pub const fn mode2_interrupt(self) -> bool {
        self.0 & 0x20 != 0
    }

    /// Bit 4: interrupt when entering VBlank (mode 1).
    pub const fn mode1_interrupt(self) -> bool {
        self.0 & 0x10 != 0
    }

    /// Bit 3: interrupt when entering HBlank (mode 0).
    pub const fn mode0_interrupt(self) -> bool {
        self.0 & 0x08 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcdc_decodes_the_tile_areas() {
        let l = Lcdc::from_bits(0x91); // value after the BootROM
        assert!(l.lcd_enabled());
        assert!(l.bg_enabled());
        assert!(!l.bg_tile_data_signed(), "bit 4 set means base 0x8000");
        assert_eq!(l.bg_tilemap(), 0x9800);
        assert_eq!(l.sprite_height(), 8);
    }

    #[test]
    fn stat_ignores_writes_to_the_read_only_bits() {
        let mut s = Stat::from_bits(0);
        s.write(0xFF);
        assert_eq!(s.bits_with_mode(Mode::HBlank, false), 0x80 | Stat::WRITABLE);
    }

    #[test]
    fn stat_exposes_the_mode_and_the_coincidence() {
        let s = Stat::from_bits(0);
        assert_eq!(s.bits_with_mode(Mode::Drawing, true), 0x80 | 0x04 | 3);
    }
}
