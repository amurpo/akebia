//! Colour: the RGB555 format and the CGB palette RAM.
//!
//! # Why the framebuffer carries colour and not indices
//!
//! A DMG produces four shades and the frontend decides what colour they are
//! —original green, grey—. A CGB produces **real colour**: the game writes the
//! exact values into palette RAM and no frontend has any right to reinterpret
//! them.
//!
//! To avoid splitting the PPU into two paths, the framebuffer always carries
//! [`Rgb555`]. On CGB it comes straight from the game's palette; on DMG, the PPU
//! translates shade 0..3 through four colours **the frontend gives it** (see
//! [`Ppu::set_dmg_shades`]). That way the core still does not decide "which
//! green", which was the original goal, and at the same time there is a single
//! pixel format.
//!
//! [`Ppu::set_dmg_shades`]: super::Ppu::set_dmg_shades

/// A 15-bit colour in the Game Boy Color's native format.
///
/// ```text
///   15 14        10 9         5 4         0
///  ┌──┬────────────┬───────────┬───────────┐
///  │ -│    blue    │   green   │    red    │
///  └──┴────────────┴───────────┴───────────┘
/// ```
///
/// Note the order: red occupies the low bits, the opposite of most modern
/// formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb555(u16);

impl Rgb555 {
    pub const BLACK: Self = Self(0x0000);
    pub const WHITE: Self = Self(0x7FFF);

    pub const fn from_bits(bits: u16) -> Self {
        Self(bits & 0x7FFF)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Builds one from three 8-bit components, keeping the 5 most significant
    /// bits of each.
    pub const fn from_rgb888(r: u8, g: u8, b: u8) -> Self {
        Self(((b as u16 >> 3) << 10) | ((g as u16 >> 3) << 5) | (r as u16 >> 3))
    }

    /// Expands to 8 bits per component.
    ///
    /// The expansion replicates the high bits (`c << 3 | c >> 2`) instead of
    /// shifting and padding with zeros: that way maximum white (31) gives 255
    /// and not 248, and the full ramp reaches both ends.
    pub const fn to_rgb888(self) -> [u8; 3] {
        [expand5(self.0 & 0x1F), expand5((self.0 >> 5) & 0x1F), expand5((self.0 >> 10) & 0x1F)]
    }
}

/// Takes a 5-bit component up to 8 by replicating the high bits.
const fn expand5(c: u16) -> u8 {
    ((c << 3) | (c >> 2)) as u8
}

/// Number of palettes per bank: 8 for the background and 8 for sprites.
pub const PALETTE_COUNT: usize = 8;
/// Colours per palette.
pub const COLORS_PER_PALETTE: usize = 4;
/// Bytes of palette RAM: 8 palettes × 4 colours × 2 bytes.
pub const PALETTE_RAM_SIZE: usize = PALETTE_COUNT * COLORS_PER_PALETTE * 2;

/// The CGB palette RAM and its access register.
///
/// It is not memory-mapped: it is reached through a window of two registers,
/// one that sets the index and another that reads or writes the byte pointed
/// at. Bit 7 of the index register enables **auto-increment**, which is what
/// allows dumping a whole palette with a repeated `LD (HL),A` without touching
/// the index.
#[derive(Clone)]
pub struct PaletteRam {
    bytes: [u8; PALETTE_RAM_SIZE],
    index: u8,
    auto_increment: bool,
}

impl PaletteRam {
    pub const fn new() -> Self {
        // The hardware starts with undefined values; white is the most benign
        // choice, because a game that forgets to initialise a palette shows
        // something visible instead of a black rectangle.
        Self { bytes: [0xFF; PALETTE_RAM_SIZE], index: 0, auto_increment: false }
    }

    /// Writes the index register (`BCPS`/`OCPS`).
    pub fn write_index(&mut self, value: u8) {
        self.index = value & 0x3F;
        self.auto_increment = value & 0x80 != 0;
    }

    pub fn read_index(&self) -> u8 {
        // Bit 6 does not exist and reads as 1.
        self.index | (u8::from(self.auto_increment) << 7) | 0x40
    }

    /// Reads the byte pointed at (`BCPD`/`OCPD`). Reading does **not**
    /// auto-increment.
    pub fn read_data(&self) -> u8 {
        self.bytes[self.index as usize]
    }

    pub fn write_data(&mut self, value: u8) {
        self.bytes[self.index as usize] = value;
        if self.auto_increment {
            self.index = (self.index + 1) & 0x3F;
        }
    }

    /// Colour `color` (0..3) of palette `palette` (0..7).
    pub fn color(&self, palette: u8, color: u8) -> Rgb555 {
        let offset = (palette as usize % PALETTE_COUNT) * COLORS_PER_PALETTE * 2
            + (color as usize % COLORS_PER_PALETTE) * 2;
        Rgb555::from_bits(u16::from_le_bytes([self.bytes[offset], self.bytes[offset + 1]]))
    }
}

impl Default for PaletteRam {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn red_occupies_the_low_bits() {
        let red = Rgb555::from_bits(0x001F);
        assert_eq!(red.to_rgb888(), [255, 0, 0]);
        let blue = Rgb555::from_bits(0x7C00);
        assert_eq!(blue.to_rgb888(), [0, 0, 255]);
    }

    #[test]
    fn the_expansion_reaches_both_ends() {
        assert_eq!(Rgb555::WHITE.to_rgb888(), [255, 255, 255]);
        assert_eq!(Rgb555::BLACK.to_rgb888(), [0, 0, 0]);
    }

    #[test]
    fn round_trip_from_rgb888() {
        let c = Rgb555::from_rgb888(0xFF, 0x80, 0x00);
        let [r, g, b] = c.to_rgb888();
        assert_eq!(r, 255);
        assert!(g.abs_diff(0x80) < 8, "the low 3 bits are lost");
        assert_eq!(b, 0);
    }

    #[test]
    fn auto_increment_allows_dumping_a_palette_in_one_go() {
        let mut ram = PaletteRam::new();
        ram.write_index(0x80); // index 0, with auto-increment
        for byte in [0x1F, 0x00, 0xE0, 0x03] {
            ram.write_data(byte);
        }
        assert_eq!(ram.color(0, 0), Rgb555::from_bits(0x001F));
        assert_eq!(ram.color(0, 1), Rgb555::from_bits(0x03E0));
    }

    #[test]
    fn without_auto_increment_the_index_does_not_move() {
        let mut ram = PaletteRam::new();
        ram.write_index(0x00);
        ram.write_data(0x11);
        ram.write_data(0x22);
        assert_eq!(ram.read_data(), 0x22, "both writes go to the same byte");
    }

    #[test]
    fn the_palettes_are_independent() {
        let mut ram = PaletteRam::new();
        // Palette 3, colour 2 → offset 3*8 + 2*2 = 28.
        ram.write_index(28);
        ram.write_data(0x34);
        ram.write_index(29);
        ram.write_data(0x12);
        assert_eq!(ram.color(3, 2), Rgb555::from_bits(0x1234));
        assert_ne!(ram.color(2, 2), Rgb555::from_bits(0x1234));
    }
}
