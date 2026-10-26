//! The chip on the cartridge board that keeps the saved game.
//!
//! # Three chips, three different things
//!
//! The older machine had one answer to this — battery-backed static RAM behind
//! the mapper — and this machine has three, which are not variations on each
//! other:
//!
//! - **SRAM** is memory. A byte written is a byte there, and a battery keeps it.
//! - **Flash** is a device that has to be asked. It answers its own name, takes
//!   commands, erases in blocks, and refuses data it was not warned about.
//! - **EEPROM** is a serial line. It is not addressed at all: the game sends bits
//!   down it and gets bits back, driven by a memory mover.
//!
//! Nothing useful is shared between the three but the fact that a frontend
//! wants a `Vec<u8>` out of them at the end, which is what [`Save`] is.
//!
//! # How a cartridge says which one it has
//!
//! It does not, anywhere in its header. What it has instead is the save library
//! its developer linked in, and that library leaves its own version string in
//! the ROM — `FLASH1M_V103`, `EEPROM_V124`, `SRAM_F_V102`. Every emulator finds
//! the save type by looking for those strings, because there is nothing else to
//! look at.
//!
//! It is a fingerprint and not a declaration, so it can be missing: a homebrew
//! ROM that pokes SRAM directly links no library and leaves no string. That is
//! why the fallback is SRAM and not "no saved game" — SRAM is the one that needs
//! no driver, so it is the one a ROM with no driver is using.
//!
//! # What goes wrong without any of this
//!
//! Not silence. A game whose flash chip is missing does not fail to save; it
//! asks the chip its name during startup, hears zero, and puts a message on the
//! screen instead of a menu. The symptom looks nothing like "saving does not
//! work" — the reported one was *"The 1M sub-circuit board is not installed."*

pub mod eeprom;
pub mod flash;

use eeprom::Eeprom;
use flash::Flash;

/// Battery-backed static RAM: 32 KiB, whatever the 64 KiB of address space it
/// sits in might suggest.
pub const SRAM_LEN: usize = 32 * 1024;

const FLASH_64: usize = 64 * 1024;
const FLASH_128: usize = 128 * 1024;

/// The largest cartridge whose EEPROM has the whole of the last region to
/// itself. Above this the ROM needs the room and the chip is squeezed into the
/// last 256 bytes of the address space.
const CROWDED_ROM: usize = 16 * 1024 * 1024;

/// Which chip a cartridge carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sram,
    Flash64,
    Flash128,
    Eeprom,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Sram => "SRAM",
            Self::Flash64 => "64 KiB flash",
            Self::Flash128 => "128 KiB flash",
            Self::Eeprom => "EEPROM",
        };
        f.write_str(name)
    }
}

/// The strings a save library leaves behind, and what each one means.
///
/// Order matters for one pair only: `FLASH1M_V` would never be found by a search
/// for `FLASH_V`, but reading the table it is worth having the specific one
/// first so that nobody later "simplifies" it into a prefix match.
const SIGNATURES: [(&[u8], Kind); 5] = [
    (b"EEPROM_V", Kind::Eeprom),
    (b"FLASH1M_V", Kind::Flash128),
    (b"FLASH512_V", Kind::Flash64),
    (b"FLASH_V", Kind::Flash64),
    (b"SRAM_", Kind::Sram),
];

/// Finds the save library's fingerprint in a cartridge, if it left one.
///
/// The search steps four bytes at a time because these strings are constants in
/// a compiled program and a compiled program aligns its constants. That is not
/// only four times less work: it also throws out the accidental matches that a
/// byte-by-byte sweep of thirty-two megabytes will otherwise find.
pub fn detect(rom: &[u8]) -> Option<Kind> {
    let mut at = 0;
    while at < rom.len() {
        for (signature, kind) in SIGNATURES {
            if rom[at..].starts_with(signature) {
                return Some(kind);
            }
        }
        at += 4;
    }
    None
}

/// The cartridge's saved game, whichever of the three it is kept in.
pub enum Save {
    Sram(Box<[u8; SRAM_LEN]>),
    Flash(Flash),
    Eeprom(Eeprom),
}

impl Default for Save {
    fn default() -> Self {
        Self::new(Kind::Sram)
    }
}

impl Save {
    pub fn new(kind: Kind) -> Self {
        match kind {
            Kind::Sram => Self::Sram(Box::new([0; SRAM_LEN])),
            Kind::Flash64 => Self::Flash(Flash::new(FLASH_64)),
            Kind::Flash128 => Self::Flash(Flash::new(FLASH_128)),
            Kind::Eeprom => Self::Eeprom(Eeprom::new()),
        }
    }

    /// The chip a cartridge carries, or plain SRAM if it named none. See the
    /// module note for why that is the right guess and not a shrug.
    pub fn for_rom(rom: &[u8]) -> Self {
        Self::new(detect(rom).unwrap_or(Kind::Sram))
    }

    pub fn kind(&self) -> Kind {
        match self {
            Self::Sram(_) => Kind::Sram,
            Self::Flash(flash) if flash.size() > FLASH_64 => Kind::Flash128,
            Self::Flash(_) => Kind::Flash64,
            Self::Eeprom(_) => Kind::Eeprom,
        }
    }

    /// The bytes to write to a saved game file.
    pub fn data(&self) -> &[u8] {
        match self {
            Self::Sram(sram) => &sram[..],
            Self::Flash(flash) => flash.data(),
            Self::Eeprom(eeprom) => eeprom.data(),
        }
    }

    /// Restores one, refusing anything that is not the right length.
    ///
    /// Refusing is the whole point: a file that does not fit this cartridge is
    /// far more likely to be some other cartridge's saved game than a damaged
    /// one, and stretching it to fit would overwrite it on the next autosave.
    pub fn load(&mut self, data: &[u8]) -> bool {
        match self {
            Self::Sram(sram) => {
                if data.len() != SRAM_LEN {
                    return false;
                }
                sram.copy_from_slice(data);
                true
            }
            Self::Flash(flash) => flash.load(data),
            Self::Eeprom(eeprom) => eeprom.load(data),
        }
    }

    /// Whether an address in the cartridge's third window belongs to the chip
    /// rather than to the ROM.
    ///
    /// Only an EEPROM is there at all, and where it is depends on how big the
    /// cartridge is: a ROM of 16 MiB or less leaves the whole window free, and a
    /// larger one needs all but the last 256 bytes of it. Getting this wrong on
    /// a large cartridge is not a broken saved game — it is a hole punched
    /// through the middle of the game's own code.
    pub fn claims(&self, addr: u32, rom_len: usize) -> bool {
        if !matches!(self, Self::Eeprom(_)) || addr >> 24 != 0x0D {
            return false;
        }
        rom_len <= CROWDED_ROM || addr >= 0x0DFF_FF00
    }

    /// A read from the chip, at whatever width the processor asked for.
    ///
    /// Reading can change what the chip will say next — asking a flash chip its
    /// name is a read, and every bit out of an EEPROM moves it along — so this
    /// is not a `peek` and cannot be made into one.
    pub fn read(&mut self, addr: u32, len: usize) -> u32 {
        match self {
            Self::Eeprom(eeprom) => u32::from(eeprom.read_bit()),
            // The other two sit on an eight-bit bus, so a wider read does not
            // fetch more of them: it fetches the one byte and hands back copies.
            // A game reading its saved game a word at a time gets four of the
            // first byte, which is what the hardware gives and not what a plain
            // array would.
            Self::Sram(sram) => spread(sram[offset(addr, SRAM_LEN)], len),
            Self::Flash(flash) => spread(flash.read(addr & 0xFFFF), len),
        }
    }

    /// A write to the chip. Its width is not a parameter because no chip here
    /// has one: two of them take a byte and the third takes a single bit.
    pub fn write(&mut self, addr: u32, value: u32) {
        match self {
            // Only the bottom bit of the halfword is the signal; the rest of
            // what the mover carried is noise the chip never sees.
            Self::Eeprom(eeprom) => eeprom.write_bit(value as u8 & 1),
            // Eight bits wide going out as well: only the bottom byte lands,
            // wherever in the word it was written from.
            Self::Sram(sram) => sram[offset(addr, SRAM_LEN)] = value as u8,
            Self::Flash(flash) => flash.write(addr & 0xFFFF, value as u8),
        }
    }

    /// What is there, without disturbing anything. For looking rather than
    /// running: a debugger reading an EEPROM must not consume a bit of it.
    pub fn peek(&self, addr: u32, len: usize) -> u32 {
        match self {
            Self::Sram(sram) => spread(sram[offset(addr, SRAM_LEN)], len),
            // A flash read only depends on state that peeking does not change.
            Self::Flash(flash) => spread(flash.read(addr & 0xFFFF), len),
            Self::Eeprom(_) => 0,
        }
    }
}

/// One byte answered on an eight-bit bus, however wide the read was.
fn spread(byte: u8, len: usize) -> u32 {
    let byte = u32::from(byte);
    match len {
        1 => byte,
        2 => byte * 0x0101,
        _ => byte * 0x0101_0101,
    }
}

/// Where in a chip an address lands. The chip is smaller than the window it sits
/// in, and it repeats to fill it rather than leaving a hole.
fn offset(addr: u32, len: usize) -> usize {
    addr as usize & (len - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROM with a signature planted at an aligned offset, as a linker would
    /// leave it.
    fn rom_saying(signature: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x400];
        rom[0x200..0x200 + signature.len()].copy_from_slice(signature);
        rom
    }

    #[test]
    fn each_library_is_recognised_by_the_string_it_leaves() {
        let cases: [(&[u8], Kind); 6] = [
            (b"EEPROM_V124", Kind::Eeprom),
            (b"FLASH_V126", Kind::Flash64),
            (b"FLASH512_V130", Kind::Flash64),
            (b"FLASH1M_V103", Kind::Flash128),
            (b"SRAM_V113", Kind::Sram),
            (b"SRAM_F_V102", Kind::Sram),
        ];
        for (signature, expected) in cases {
            let found = detect(&rom_saying(signature));
            assert_eq!(found, Some(expected), "{}", String::from_utf8_lossy(signature));
        }
    }

    /// The large flash part must not be read as the small one. They are told
    /// apart by five characters in the middle of the string, and a chip of the
    /// wrong size answers with an identifier the game's driver rejects.
    #[test]
    fn the_two_flash_parts_are_not_confused_for_each_other() {
        assert_eq!(detect(&rom_saying(b"FLASH1M_V103")), Some(Kind::Flash128));
        assert_eq!(detect(&rom_saying(b"FLASH_V126")), Some(Kind::Flash64));
    }

    /// A cartridge that names nothing is not a cartridge that saves nothing:
    /// SRAM is the one that needs no library, so it is what a ROM with no
    /// library is using.
    #[test]
    fn a_cartridge_that_names_nothing_gets_plain_ram() {
        assert_eq!(detect(&vec![0u8; 0x1000]), None);
        assert_eq!(Save::for_rom(&vec![0u8; 0x1000]).kind(), Kind::Sram);
    }

    /// The search steps four bytes at a time, which is where a compiler puts a
    /// string constant. A run of letters that happens to appear off the grid is
    /// data and not a fingerprint.
    #[test]
    fn a_string_that_is_not_aligned_is_not_a_fingerprint() {
        let mut rom = vec![0u8; 0x400];
        rom[0x201..0x201 + 10].copy_from_slice(b"FLASH_V126");
        assert_eq!(detect(&rom), None);
    }

    #[test]
    fn each_chip_saves_a_file_of_its_own_size() {
        assert_eq!(Save::new(Kind::Sram).data().len(), 32 * 1024);
        assert_eq!(Save::new(Kind::Flash64).data().len(), 64 * 1024);
        assert_eq!(Save::new(Kind::Flash128).data().len(), 128 * 1024);
        assert_eq!(Save::new(Kind::Eeprom).data().len(), 8 * 1024);
    }

    #[test]
    fn a_saved_game_of_the_wrong_size_is_refused_by_every_chip() {
        for kind in [Kind::Sram, Kind::Flash64, Kind::Flash128, Kind::Eeprom] {
            let mut save = Save::new(kind);
            assert!(!save.load(&[0u8; 3]), "{kind}");
            let right = save.data().len();
            assert!(save.load(&vec![0x5A; right]), "{kind}");
        }
    }

    /// Save memory is on an eight-bit bus. A word read of it is one byte four
    /// times over, and a word write of it puts one byte down.
    #[test]
    fn the_bus_is_one_byte_wide_in_both_directions() {
        let mut save = Save::new(Kind::Sram);
        save.write(0x0E00_0000, 0xDEAD_BEEF);
        assert_eq!(save.read(0x0E00_0000, 1), 0xEF);
        assert_eq!(save.read(0x0E00_0000, 2), 0xEFEF);
        assert_eq!(save.read(0x0E00_0000, 4), 0xEFEF_EFEF);
        assert_eq!(save.read(0x0E00_0001, 1), 0, "one byte landed, not four");
    }

    /// 32 KiB of chip in 64 KiB of address space: the second half is the first
    /// half again, not somewhere else to put things.
    #[test]
    fn the_chip_repeats_to_fill_the_window() {
        let mut save = Save::new(Kind::Sram);
        save.write(0x0E00_0000, 0x42);
        assert_eq!(save.read(0x0E00_8000, 1), 0x42);
    }

    /// The EEPROM is the only chip that lives in the cartridge's third window,
    /// and on a large cartridge it is squeezed into the last 256 bytes because
    /// the game needs the rest for itself.
    #[test]
    fn where_the_eeprom_is_depends_on_how_big_the_cartridge_is() {
        let small = Save::new(Kind::Eeprom);
        assert!(small.claims(0x0D00_0000, 4 * 1024 * 1024));
        assert!(small.claims(0x0DFF_FFFF, 4 * 1024 * 1024));

        let large = 32 * 1024 * 1024;
        assert!(!small.claims(0x0D00_0000, large), "that is the game's own code");
        assert!(small.claims(0x0DFF_FF00, large));
    }

    #[test]
    fn no_other_chip_is_in_the_cartridge_window_at_all() {
        for kind in [Kind::Sram, Kind::Flash64, Kind::Flash128] {
            assert!(!Save::new(kind).claims(0x0D00_0000, 1024), "{kind}");
        }
        assert!(!Save::new(Kind::Eeprom).claims(0x0E00_0000, 1024), "nor anywhere else");
    }

    /// Peeking must not move an EEPROM along: a debugger looking at one would
    /// otherwise eat the bit the game was about to read.
    #[test]
    fn peeking_leaves_a_serial_chip_where_it_was() {
        let mut save = Save::new(Kind::Eeprom);
        let Save::Eeprom(chip) = &mut save else { unreachable!() };
        // A read request for block zero of a small chip.
        for bit in [1, 1, 0, 0, 0, 0, 0, 0, 0] {
            chip.write_bit(bit);
        }
        assert_eq!(save.peek(0x0D00_0000, 2), 0, "nothing to see and nothing consumed");

        let Save::Eeprom(chip) = &mut save else { unreachable!() };
        for _ in 0..4 {
            assert_eq!(chip.read_bit(), 0, "the answer still starts at its start");
        }
    }
}
