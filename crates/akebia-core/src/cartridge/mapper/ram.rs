//! The cartridge SRAM, shared by every mapper.
//!
//! The four controllers repeat exactly the same logic: an enable register,
//! 8 KiB banks, address masking and a dump to disk if there is a battery.
//! Keeping it in one place stops a new mapper from reintroducing a bug already
//! fixed in another.
//!
//! # Why the SRAM starts disabled
//!
//! It is not an emulator precaution: it is the hardware. The SRAM only responds
//! after writing `0x0A` in the range `0x0000..0x2000`, and games disable it
//! again as soon as they are done writing. That way a power cut during shutdown
//! does not find the memory exposed and the saved game survives.

use super::OPEN_BUS;
use crate::cartridge::RAM_BANK_SIZE;

/// Value that has to be written into the low nibble of the enable register.
/// Any other value leaves the memory disconnected from the bus.
pub const RAM_ENABLE_MAGIC: u8 = 0x0A;

pub struct CartRam {
    bytes: Vec<u8>,
    battery: bool,
    enabled: bool,
}

impl CartRam {
    /// `always_on` is for cartridges with no mapper, which have no enable
    /// register and expose their RAM permanently.
    pub fn new(size: usize, battery: bool, always_on: bool) -> Self {
        Self { bytes: vec![0; size], battery, enabled: always_on }
    }

    /// Interprets a write to the enable register.
    pub fn set_enable_register(&mut self, value: u8) {
        self.enabled = value & 0x0F == RAM_ENABLE_MAGIC;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn banks(&self) -> usize {
        self.bytes.len() / RAM_BANK_SIZE
    }

    /// Index within the vector, or `None` if the access cannot be completed.
    fn index(&self, bank: usize, addr: u16) -> Option<usize> {
        if !self.enabled || self.banks() == 0 {
            return None;
        }
        // The bank wraps: an 8 KiB cartridge with bank 3 selected still sees its
        // one and only bank.
        let bank = bank % self.banks();
        Some(bank * RAM_BANK_SIZE + (addr as usize - 0xA000) % RAM_BANK_SIZE)
    }

    pub fn read(&self, bank: usize, addr: u16) -> u8 {
        self.index(bank, addr).map_or(OPEN_BUS, |i| self.bytes[i])
    }

    pub fn write(&mut self, bank: usize, addr: u16, value: u8) {
        if let Some(i) = self.index(bank, addr) {
            self.bytes[i] = value;
        }
    }

    /// Contents to persist, or `None` if there is no battery backing them.
    pub fn save(&self) -> Option<&[u8]> {
        (self.battery && !self.bytes.is_empty()).then_some(&self.bytes)
    }

    /// Restores a saved game. `false` if the size does not match the cartridge.
    pub fn load(&mut self, data: &[u8]) -> bool {
        if self.battery && data.len() == self.bytes.len() && !data.is_empty() {
            self.bytes.copy_from_slice(data);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_starts_disabled_and_responds_to_the_register() {
        let mut ram = CartRam::new(8 * 1024, true, false);
        ram.write(0, 0xA000, 0x42);
        assert_eq!(ram.read(0, 0xA000), OPEN_BUS);

        ram.set_enable_register(RAM_ENABLE_MAGIC);
        ram.write(0, 0xA000, 0x42);
        assert_eq!(ram.read(0, 0xA000), 0x42);

        ram.set_enable_register(0x00);
        assert_eq!(ram.read(0, 0xA000), OPEN_BUS, "the data is still there, but out of sight");
    }

    #[test]
    fn the_banks_are_independent() {
        let mut ram = CartRam::new(32 * 1024, true, true);
        assert_eq!(ram.banks(), 4);
        ram.write(0, 0xA000, 0x11);
        ram.write(3, 0xA000, 0x33);
        assert_eq!(ram.read(0, 0xA000), 0x11);
        assert_eq!(ram.read(3, 0xA000), 0x33);
    }

    #[test]
    fn a_nonexistent_bank_wraps_around() {
        let mut ram = CartRam::new(8 * 1024, false, true);
        ram.write(0, 0xA000, 0x77);
        assert_eq!(ram.read(5, 0xA000), 0x77, "there is only one bank: they all point at it");
    }

    #[test]
    fn with_no_battery_there_is_no_saved_game() {
        let ram = CartRam::new(8 * 1024, false, true);
        assert!(ram.save().is_none());
    }

    #[test]
    fn loading_rejects_a_different_size() {
        let mut ram = CartRam::new(8 * 1024, true, true);
        assert!(!ram.load(&[0; 4096]), "a .sav from another cartridge must not get in");
        assert!(ram.load(&[0xAB; 8 * 1024]));
        assert_eq!(ram.read(0, 0xA000), 0xAB);
    }
}
