//! MBC1: up to 2 MiB of ROM and 32 KiB of SRAM.
//!
//! It is the most common mapper and also the one with the least intuitive
//! behaviour. It has two bank registers and a mode bit that changes the meaning
//! of both:
//!
//! | Write               | Effect                                            |
//! |---------------------|---------------------------------------------------|
//! | `0x0000..0x2000`    | enables the SRAM if the low nibble is `0xA`       |
//! | `0x2000..0x4000`    | `bank1`: low 5 bits of the ROM bank               |
//! | `0x4000..0x6000`    | `bank2`: 2 bits, RAM bank **or** high ROM bits    |
//! | `0x6000..0x8000`    | mode: 0 = simple, 1 = advanced                    |
//!
//! In **simple mode** the region `0x0000..0x4000` always sees bank 0 and only
//! SRAM bank 0 exists. In **advanced mode**, `bank2` also applies to the low ROM
//! region and selects the SRAM bank. That asymmetry is what breaks naive
//! implementations.
//!
//! The quantisation of `bank1` is the other trap: the value 0 becomes 1 *before*
//! being combined with `bank2`, so banks 0x00, 0x20, 0x40 and 0x60 are
//! unreachable in the high region.

use super::{ram::CartRam, Mapper, OPEN_BUS};
use crate::cartridge::ROM_BANK_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BankingMode {
    /// `bank2` only affects `0x4000..0x8000`.
    Simple,
    /// `bank2` also affects `0x0000..0x4000` and the SRAM.
    Advanced,
}

#[derive(Clone)]
pub struct Mbc1 {
    rom: Vec<u8>,
    ram: CartRam,

    /// Mask derived from the real number of banks, to wrap the accesses.
    rom_bank_mask: u8,

    bank1: u8,
    bank2: u8,
    mode: BankingMode,
}

impl Mbc1 {
    pub fn new(mut rom: Vec<u8>, ram_size: usize, battery: bool) -> Self {
        rom.resize(rom.len().max(2 * ROM_BANK_SIZE), OPEN_BUS);
        let banks = rom.len() / ROM_BANK_SIZE;
        Self {
            rom,
            ram: CartRam::new(ram_size, battery, false),
            // The banks are a power of two, so the mask is banks-1.
            rom_bank_mask: (banks.next_power_of_two().max(2) - 1) as u8,
            bank1: 1,
            bank2: 0,
            mode: BankingMode::Simple,
        }
    }

    /// Bank visible at `0x0000..0x4000`.
    fn low_bank(&self) -> u8 {
        match self.mode {
            BankingMode::Simple => 0,
            BankingMode::Advanced => (self.bank2 << 5) & self.rom_bank_mask,
        }
    }

    /// Bank visible at `0x4000..0x8000`. `bank1` is never 0 here.
    fn high_bank(&self) -> u8 {
        ((self.bank2 << 5) | self.bank1) & self.rom_bank_mask
    }

    /// Active SRAM bank.
    fn ram_bank(&self) -> usize {
        match self.mode {
            BankingMode::Simple => 0,
            BankingMode::Advanced => self.bank2 as usize,
        }
    }

    fn rom_byte(&self, bank: u8, offset: usize) -> u8 {
        let index = bank as usize * ROM_BANK_SIZE + offset;
        self.rom.get(index).copied().unwrap_or(OPEN_BUS)
    }
}

impl Mapper for Mbc1 {
    fn read_rom(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.rom_byte(self.low_bank(), addr as usize),
            0x4000..=0x7FFF => self.rom_byte(self.high_bank(), addr as usize - 0x4000),
            _ => OPEN_BUS,
        }
    }

    fn write_rom(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram.set_enable_register(value),
            // The value 0 is promoted to 1: bank 0 is not addressable here.
            0x2000..=0x3FFF => self.bank1 = (value & 0x1F).max(1),
            0x4000..=0x5FFF => self.bank2 = value & 0x03,
            0x6000..=0x7FFF => {
                self.mode =
                    if value & 0x01 == 0 { BankingMode::Simple } else { BankingMode::Advanced };
            }
            _ => {}
        }
    }

    fn read_ram(&self, addr: u16) -> u8 {
        self.ram.read(self.ram_bank(), addr)
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        self.ram.write(self.ram_bank(), addr, value);
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.ram.save()
    }

    fn load_save_ram(&mut self, data: &[u8]) -> bool {
        self.ram.load(data)
    }

    fn duplicate(&self) -> Box<dyn Mapper> {
        Box::new(self.clone())
    }

    fn name(&self) -> &'static str {
        "MBC1"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ROM of `banks` banks where the first byte of each bank is its index.
    fn marked_rom(banks: usize) -> Vec<u8> {
        let mut rom = vec![0u8; banks * ROM_BANK_SIZE];
        for b in 0..banks {
            rom[b * ROM_BANK_SIZE] = b as u8;
        }
        rom
    }

    #[test]
    fn bank_zero_is_promoted_to_one() {
        let mut m = Mbc1::new(marked_rom(4), 0, false);
        m.write_rom(0x2000, 0x00);
        assert_eq!(m.read_rom(0x4000), 1, "writing 0 must select bank 1");
    }

    #[test]
    fn it_changes_the_high_bank() {
        let mut m = Mbc1::new(marked_rom(4), 0, false);
        m.write_rom(0x2000, 0x03);
        assert_eq!(m.read_rom(0x4000), 3);
        assert_eq!(m.read_rom(0x0000), 0, "the low region does not move in simple mode");
    }

    #[test]
    fn advanced_mode_moves_the_low_region() {
        // 64 banks = 1 MiB: bank2 contributes bits 5 and 6.
        let mut m = Mbc1::new(marked_rom(64), 0, false);
        m.write_rom(0x4000, 0x01); // bank2 = 1
        m.write_rom(0x6000, 0x01); // advanced mode
        assert_eq!(m.read_rom(0x0000), 32, "0x0000 must see bank bank2<<5");
        m.write_rom(0x2000, 0x05);
        assert_eq!(m.read_rom(0x4000), 37, "0x4000 sees (bank2<<5)|bank1");
    }

    #[test]
    fn the_sram_requires_an_explicit_enable() {
        let mut m = Mbc1::new(marked_rom(4), 8 * 1024, true);
        m.write_ram(0xA000, 0x42);
        assert_eq!(m.read_ram(0xA000), OPEN_BUS, "the SRAM starts disabled");

        m.write_rom(0x0000, 0x0A);
        m.write_ram(0xA000, 0x42);
        assert_eq!(m.read_ram(0xA000), 0x42);

        m.write_rom(0x0000, 0x00);
        assert_eq!(m.read_ram(0xA000), OPEN_BUS);
    }

    #[test]
    fn the_sram_banks_only_exist_in_advanced_mode() {
        let mut m = Mbc1::new(marked_rom(4), 32 * 1024, true);
        m.write_rom(0x0000, 0x0A); // enable RAM
        m.write_rom(0x6000, 0x01); // advanced mode

        m.write_rom(0x4000, 0x00);
        m.write_ram(0xA000, 0x11);
        m.write_rom(0x4000, 0x01);
        m.write_ram(0xA000, 0x22);

        assert_eq!(m.read_ram(0xA000), 0x22);
        m.write_rom(0x4000, 0x00);
        assert_eq!(m.read_ram(0xA000), 0x11);
    }
}
