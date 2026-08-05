//! Memory bank controllers (MBC).
//!
//! **Strategy pattern.** The bus does not know which mapper is inside the
//! cartridge: it only knows the [`Mapper`] trait. Adding support for a new MBC
//! means writing one more implementation and registering it in the
//! [`super::Cartridge::load`] factory.

mod mbc1;
mod mbc3;
mod mbc5;
mod ram;
mod rom_only;

pub use mbc1::Mbc1;
pub use mbc3::Mbc3;
pub use mbc5::Mbc5;
pub use rom_only::RomOnly;

/// Interface every cartridge controller must satisfy.
///
/// The addresses arriving here are the Game Boy memory map ones, unnormalised:
/// it is each mapper's responsibility to translate them into an offset within
/// its ROM or its SRAM.
///
/// | Range            | Method         |
/// |------------------|----------------|
/// | `0x0000..0x8000` | `read_rom` / `write_rom` |
/// | `0xA000..0xC000` | `read_ram` / `write_ram` |
///
/// Writing to the ROM range does not modify the ROM: those are the mapper's own
/// control registers (bank select, RAM enable, and so on).
pub trait Mapper {
    fn read_rom(&self, addr: u16) -> u8;

    /// A second cartridge in the state this one is in, banks and SRAM included.
    ///
    /// It is what lets a whole console be duplicated —see [`GameBoy::clone`]—
    /// and it has to be a method because the cartridge holds its mapper as a
    /// trait object: `Clone` is not object safe, and only the mapper itself
    /// knows what it is made of.
    ///
    /// [`GameBoy::clone`]: crate::GameBoy
    fn duplicate(&self) -> Box<dyn Mapper>;

    /// Write over the ROM range: configures the mapper's registers.
    fn write_rom(&mut self, addr: u16, value: u8);

    fn read_ram(&self, addr: u16) -> u8;

    fn write_ram(&mut self, addr: u16, value: u8);

    /// Advances the cartridge's internal peripherals (the MBC3's RTC). The
    /// default implementation does nothing.
    fn tick(&mut self, _t_cycles: u32) {}

    /// Contents of the battery-backed SRAM, to persist it to disk. Returns
    /// `None` if the cartridge has no battery.
    fn save_ram(&self) -> Option<&[u8]> {
        None
    }

    /// Restores a saved game. Returns `false` if the size does not fit or the
    /// cartridge does not support saving.
    fn load_save_ram(&mut self, _data: &[u8]) -> bool {
        false
    }

    /// Clock state to write after the SRAM in the `.sav`.
    ///
    /// `now_unix` comes in as a parameter and is not queried here because **the
    /// core does no I/O**, and the system clock is I/O: the frontend supplies
    /// it, and it is the one already deciding when a save happens.
    ///
    /// `None` if the cartridge carries no clock, which is nearly all of them.
    fn rtc_save(&self, _now_unix: u64) -> Option<[u8; RTC_SAVE_LEN]> {
        None
    }

    /// Restores the clock and advances it by the time the console spent powered
    /// off.
    ///
    /// Returns `false` if the cartridge carries no clock or the data does not
    /// fit.
    fn rtc_load(&mut self, _data: &[u8], _now_unix: u64) -> bool {
        false
    }

    /// Readable name, for diagnostics.
    fn name(&self) -> &'static str;
}

/// Bytes the clock state takes up at the end of a `.sav`.
///
/// It is the format BGB and VBA write, and therefore the one found in the
/// `.sav`s that circulate: **ten 32-bit little-endian integers** —the five live
/// registers and the five latched ones— followed by a **64-bit Unix timestamp**
/// with the instant the save was made. The timestamp is what allows the clock to
/// keep running while the emulator is closed.
pub const RTC_SAVE_LEN: usize = 10 * 4 + 8;

/// Value returned when reading a disabled or nonexistent region.
///
/// The Game Boy bus is *open bus*: with nobody driving the lines, every bit
/// reads as 1.
pub const OPEN_BUS: u8 = 0xFF;
