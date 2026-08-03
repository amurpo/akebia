//! MBC5: up to 8 MiB of ROM and 128 KiB of SRAM.
//!
//! The last of the family and the simplest of all. Nintendo designed it after
//! the MBC1 and the MBC3 gave developers trouble, and it shows: no modes, no
//! bits split across registers, no promotions.
//!
//! | Write            | Effect                                        |
//! |------------------|-----------------------------------------------|
//! | `0x0000..0x2000` | enables the SRAM with `0x0A`                   |
//! | `0x2000..0x3000` | low 8 bits of the ROM bank                     |
//! | `0x3000..0x4000` | bit 8 of the ROM bank                          |
//! | `0x4000..0x6000` | SRAM bank (4 bits) and, if present, the rumble motor |
//!
//! # The two differences that matter
//!
//! 1. **Bank 0 is selectable.** On the MBC1 and the MBC3, writing 0 into the
//!    bank register selects bank 1. Not here: writing 0 maps bank 0 into
//!    `0x4000..0x8000`, so it shows up twice. It sounds useless, but it lets
//!    relocatable boot code work without special cases, and a mapper that
//!    "fixes" the 0 breaks the games that rely on it.
//! 2. **The bank is 9 bits** split across two contiguous registers: the low 8 in
//!    `0x2000..0x3000` and the ninth in `0x3000..0x4000`. That is 512 banks, the
//!    8 MiB ceiling of the platform.

use super::{ram::CartRam, Mapper, OPEN_BUS};
use crate::cartridge::ROM_BANK_SIZE;

/// Bit 3 of the SRAM bank register on cartridges with rumble.
const RUMBLE_BIT: u8 = 0x08;

pub struct Mbc5 {
    rom: Vec<u8>,
    ram: CartRam,

    rom_bank_mask: u16,
    /// 9-bit ROM bank. **It may be 0.**
    rom_bank: u16,
    ram_bank: u8,

    /// `true` if the cartridge carries a rumble motor.
    has_rumble: bool,
    /// Motor state. The core cannot make anything vibrate: it exposes this so a
    /// frontend with a gamepad can forward it to the hardware if it wants to.
    rumble: bool,
}

impl Mbc5 {
    pub fn new(mut rom: Vec<u8>, ram_size: usize, battery: bool, rumble: bool) -> Self {
        rom.resize(rom.len().max(2 * ROM_BANK_SIZE), OPEN_BUS);
        let banks = rom.len() / ROM_BANK_SIZE;
        Self {
            rom,
            ram: CartRam::new(ram_size, battery, false),
            rom_bank_mask: (banks.next_power_of_two().max(2) - 1) as u16,
            // After boot the bank visible at 0x4000 is bank 1.
            rom_bank: 1,
            ram_bank: 0,
            has_rumble: rumble,
            rumble: false,
        }
    }

    /// `true` if the rumble motor is on right now.
    pub fn rumble(&self) -> bool {
        self.rumble
    }

    fn rom_byte(&self, bank: u16, offset: usize) -> u8 {
        let index = bank as usize * ROM_BANK_SIZE + offset;
        self.rom.get(index).copied().unwrap_or(OPEN_BUS)
    }
}

impl Mapper for Mbc5 {
    fn read_rom(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.rom_byte(0, addr as usize),
            0x4000..=0x7FFF => {
                self.rom_byte(self.rom_bank & self.rom_bank_mask, addr as usize - 0x4000)
            }
            _ => OPEN_BUS,
        }
    }

    fn write_rom(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram.set_enable_register(value),
            // The low 8 bits, without promoting the 0.
            0x2000..=0x2FFF => self.rom_bank = (self.rom_bank & 0x100) | u16::from(value),
            // The ninth bit, in a register of its own.
            0x3000..=0x3FFF => {
                self.rom_bank = (self.rom_bank & 0x0FF) | (u16::from(value & 0x01) << 8);
            }
            0x4000..=0x5FFF => {
                if self.has_rumble {
                    // On these cartridges bit 3 does not address memory: it
                    // turns the motor on. That is why they only have 8 SRAM
                    // banks.
                    self.rumble = value & RUMBLE_BIT != 0;
                    self.ram_bank = value & 0x07;
                } else {
                    self.ram_bank = value & 0x0F;
                }
            }
            _ => {}
        }
    }

    fn read_ram(&self, addr: u16) -> u8 {
        self.ram.read(self.ram_bank as usize, addr)
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        self.ram.write(self.ram_bank as usize, addr, value);
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.ram.save()
    }

    fn load_save_ram(&mut self, data: &[u8]) -> bool {
        self.ram.load(data)
    }

    fn name(&self) -> &'static str {
        if self.has_rumble {
            "MBC5+RUMBLE"
        } else {
            "MBC5"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::mapper::ram::RAM_ENABLE_MAGIC;

    /// ROM of `banks` banks where the first two bytes of each bank are its index
    /// in little-endian, so that more than 256 banks can be told apart.
    fn marked_rom(banks: usize) -> Vec<u8> {
        let mut rom = vec![0u8; banks * ROM_BANK_SIZE];
        for b in 0..banks {
            let [lo, hi] = (b as u16).to_le_bytes();
            rom[b * ROM_BANK_SIZE] = lo;
            rom[b * ROM_BANK_SIZE + 1] = hi;
        }
        rom
    }

    fn visible_bank(m: &Mbc5) -> u16 {
        u16::from_le_bytes([m.read_rom(0x4000), m.read_rom(0x4001)])
    }

    fn mbc5(banks: usize, rumble: bool) -> Mbc5 {
        let mut m = Mbc5::new(marked_rom(banks), 128 * 1024, true, rumble);
        m.write_rom(0x0000, RAM_ENABLE_MAGIC);
        m
    }

    #[test]
    fn bank_zero_is_selectable() {
        let mut m = mbc5(16, false);
        m.write_rom(0x2000, 0x00);
        assert_eq!(visible_bank(&m), 0, "the MBC5 does not promote 0 to 1");
    }

    #[test]
    fn the_ninth_bit_lives_in_its_own_register() {
        let mut m = mbc5(512, false);
        m.write_rom(0x2000, 0x34); // low bits
        assert_eq!(visible_bank(&m), 0x034);

        m.write_rom(0x3000, 0x01); // bit 8
        assert_eq!(visible_bank(&m), 0x134);

        m.write_rom(0x3000, 0x00);
        assert_eq!(visible_bank(&m), 0x034, "clearing bit 8 does not touch the low ones");
    }

    #[test]
    fn it_addresses_all_512_banks() {
        let mut m = mbc5(512, false);
        m.write_rom(0x2000, 0xFF);
        m.write_rom(0x3000, 0x01);
        assert_eq!(visible_bank(&m), 511, "the full 8 MiB");
    }

    #[test]
    fn the_low_region_does_not_move() {
        let mut m = mbc5(16, false);
        m.write_rom(0x2000, 0x05);
        assert_eq!(u16::from_le_bytes([m.read_rom(0), m.read_rom(1)]), 0);
    }

    #[test]
    fn it_has_sixteen_sram_banks() {
        let mut m = mbc5(16, false);
        for bank in 0..16u8 {
            m.write_rom(0x4000, bank);
            m.write_ram(0xA000, bank);
        }
        for bank in 0..16u8 {
            m.write_rom(0x4000, bank);
            assert_eq!(m.read_ram(0xA000), bank);
        }
    }

    #[test]
    fn on_rumble_cartridges_bit_3_drives_the_motor() {
        let mut m = mbc5(16, true);
        assert!(!m.rumble());

        m.write_rom(0x4000, RUMBLE_BIT | 0x02);
        assert!(m.rumble(), "bit 3 turns the motor on");

        // And it must not contaminate the bank: only 3 addressing bits are left.
        m.write_ram(0xA000, 0x55);
        m.write_rom(0x4000, 0x02); // same bank, motor off
        assert!(!m.rumble());
        assert_eq!(m.read_ram(0xA000), 0x55, "bit 3 does not address memory");
    }

    #[test]
    fn the_sram_starts_disabled() {
        let mut m = Mbc5::new(marked_rom(16), 8 * 1024, true, false);
        m.write_ram(0xA000, 0x42);
        assert_eq!(m.read_ram(0xA000), OPEN_BUS);
    }
}
