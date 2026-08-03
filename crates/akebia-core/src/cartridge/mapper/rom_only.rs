//! Cartridge with no controller: 32 KiB of ROM mapped directly, with an optional
//! 8 KiB SRAM (types 0x00, 0x08 and 0x09).

use super::{ram::CartRam, Mapper, OPEN_BUS};
use crate::cartridge::ROM_BANK_SIZE;

pub struct RomOnly {
    rom: Vec<u8>,
    ram: CartRam,
}

impl RomOnly {
    pub fn new(mut rom: Vec<u8>, ram_size: usize, battery: bool) -> Self {
        // Some homebrew ROMs come up short; we pad them to two banks so that
        // indexing never runs out of range.
        rom.resize(rom.len().max(2 * ROM_BANK_SIZE), OPEN_BUS);
        // With no mapper there is no enable register: the RAM is always
        // connected.
        Self { rom, ram: CartRam::new(ram_size, battery, true) }
    }
}

impl Mapper for RomOnly {
    fn read_rom(&self, addr: u16) -> u8 {
        self.rom.get(addr as usize).copied().unwrap_or(OPEN_BUS)
    }

    fn write_rom(&mut self, _addr: u16, _value: u8) {
        // No control registers: the write is silently discarded.
    }

    fn read_ram(&self, addr: u16) -> u8 {
        self.ram.read(0, addr)
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        self.ram.write(0, addr, value);
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.ram.save()
    }

    fn load_save_ram(&mut self, data: &[u8]) -> bool {
        self.ram.load(data)
    }

    fn name(&self) -> &'static str {
        "ROM ONLY"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reads_the_two_fixed_banks() {
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0000] = 0xAA;
        rom[0x7FFF] = 0xBB;
        let m = RomOnly::new(rom, 0, false);
        assert_eq!(m.read_rom(0x0000), 0xAA);
        assert_eq!(m.read_rom(0x7FFF), 0xBB);
    }

    #[test]
    fn writing_to_rom_is_ignored() {
        let mut m = RomOnly::new(vec![0x42; 32 * 1024], 0, false);
        m.write_rom(0x2000, 0x05);
        assert_eq!(m.read_rom(0x2000), 0x42);
    }

    #[test]
    fn with_no_ram_it_returns_open_bus() {
        let mut m = RomOnly::new(vec![0; 32 * 1024], 0, false);
        m.write_ram(0xA000, 0x12);
        assert_eq!(m.read_ram(0xA000), OPEN_BUS);
    }
}
