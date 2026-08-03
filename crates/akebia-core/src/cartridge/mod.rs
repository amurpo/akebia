//! The cartridge: header + mapper.

pub mod header;
pub mod mapper;

pub use header::{CartridgeType, CgbSupport, Header, MapperKind};
pub use mapper::Mapper;

use crate::{Error, Result};

/// Size of a ROM bank.
pub const ROM_BANK_SIZE: usize = 16 * 1024;
/// Size of an SRAM bank.
pub const RAM_BANK_SIZE: usize = 8 * 1024;

/// A cartridge loaded and ready to be connected to the bus.
pub struct Cartridge {
    header: Header,
    mapper: Box<dyn Mapper>,
}

impl Cartridge {
    /// **Abstract Factory**: inspects the header and builds the right mapping
    /// strategy.
    ///
    /// Returns [`Error::UnsupportedMapper`] for the MBCs that are not
    /// implemented yet, instead of pretending they work.
    pub fn load(rom: Vec<u8>) -> Result<Self> {
        let header = Header::parse(&rom)?;
        let ct = header.cartridge_type;
        let ram_size = if ct.has_ram { header.ram_size } else { 0 };

        let mapper: Box<dyn Mapper> = match ct.kind {
            MapperKind::RomOnly => Box::new(mapper::RomOnly::new(rom, ram_size, ct.has_battery)),
            MapperKind::Mbc1 => Box::new(mapper::Mbc1::new(rom, ram_size, ct.has_battery)),
            MapperKind::Mbc3 => {
                Box::new(mapper::Mbc3::new(rom, ram_size, ct.has_battery, ct.has_rtc))
            }
            MapperKind::Mbc5 => {
                Box::new(mapper::Mbc5::new(rom, ram_size, ct.has_battery, ct.has_rumble))
            }
            other => {
                return Err(Error::UnsupportedMapper { code: ct.raw, name: other.name() });
            }
        };

        Ok(Self { header, mapper })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn mapper_name(&self) -> &'static str {
        self.mapper.name()
    }

    /// Contents of the battery-backed SRAM, to write it out to a `.sav`.
    pub fn save_ram(&self) -> Option<&[u8]> {
        self.mapper.save_ram()
    }

    /// Loads a saved game. Returns `false` if it does not fit.
    pub fn load_save_ram(&mut self, data: &[u8]) -> bool {
        self.mapper.load_save_ram(data)
    }

    /// Clock state to write after the SRAM. See [`mapper::RTC_SAVE_LEN`].
    pub fn rtc_save(&self, now_unix: u64) -> Option<[u8; mapper::RTC_SAVE_LEN]> {
        self.mapper.rtc_save(now_unix)
    }

    /// Restores the clock and advances it by the time it spent powered off.
    pub fn rtc_load(&mut self, data: &[u8], now_unix: u64) -> bool {
        self.mapper.rtc_load(data, now_unix)
    }
}

// Delegation to the mapper. The bus talks to the cartridge, not to the strategy.
impl Mapper for Cartridge {
    fn read_rom(&self, addr: u16) -> u8 {
        self.mapper.read_rom(addr)
    }
    fn write_rom(&mut self, addr: u16, value: u8) {
        self.mapper.write_rom(addr, value);
    }
    fn read_ram(&self, addr: u16) -> u8 {
        self.mapper.read_ram(addr)
    }
    fn write_ram(&mut self, addr: u16, value: u8) {
        self.mapper.write_ram(addr, value);
    }
    fn tick(&mut self, t_cycles: u32) {
        self.mapper.tick(t_cycles);
    }
    fn name(&self) -> &'static str {
        self.mapper.name()
    }
}
