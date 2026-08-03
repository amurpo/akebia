//! The console model being emulated.
//!
//! It is not a user preference: it changes the hardware. In CGB mode there are
//! registers a DMG does not have, VRAM has two banks instead of one, the colours
//! come from the cartridge rather than a fixed palette, and even the priority
//! rules between sprites are different.
//!
//! It is propagated by value to the PPU and the bus at construction, and neither
//! of them changes it afterwards: a console does not transform mid-game.

use crate::cartridge::CgbSupport;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    /// Original Game Boy. Four shades, one VRAM bank, a single block of WRAM.
    #[default]
    Dmg,
    /// Game Boy Color: 15-bit palettes, paged VRAM and WRAM, HDMA and double
    /// speed.
    Cgb,
}

impl Model {
    /// Picks the model to boot with according to what the cartridge declares.
    ///
    /// "Enhanced" cartridges (`0x80`) carry colour graphics but still boot on a
    /// DMG. They are emulated as CGB because that is what a player with the
    /// colour console in hand gets, and because it is the mode in which the game
    /// looks the way its authors designed it.
    pub fn detect(support: CgbSupport) -> Self {
        match support {
            CgbSupport::None => Self::Dmg,
            CgbSupport::Enhanced | CgbSupport::Only => Self::Cgb,
        }
    }

    pub const fn is_cgb(self) -> bool {
        matches!(self, Self::Cgb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cartridge_decides_the_model() {
        assert_eq!(Model::detect(CgbSupport::None), Model::Dmg);
        assert_eq!(Model::detect(CgbSupport::Enhanced), Model::Cgb);
        assert_eq!(Model::detect(CgbSupport::Only), Model::Cgb);
    }
}
