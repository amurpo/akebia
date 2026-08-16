//! The flash chip, which is the one most cartridges carry and the only one
//! that answers back.
//!
//! # Why a save chip needs a protocol at all
//!
//! Static RAM is memory: a game writes a byte to an address and the byte is
//! there. Flash is a *device* sitting where memory would be, and it will not
//! take a byte until it has been told, in so many words, that a byte is coming.
//! The telling is done by writing magic values to two magic addresses — `0x5555`
//! and `0x2AAA` — which are not storage and never were. That is the unlock
//! sequence, and every command starts with it.
//!
//! The reason it exists is physical. Flash cells are erased in blocks and
//! programmed one byte at a time, and a stray write from a crashing program
//! would otherwise be indistinguishable from a deliberate one. Three specific
//! writes in a specific order are not something a crash produces.
//!
//! # Why the identifier is the thing that matters most
//!
//! Before a game writes anything it asks the chip who it is: command `0x90`,
//! then read two bytes. A cartridge with no flash on it answers zero, and the
//! driver concludes there is no chip and says so on screen — which is exactly
//! what this emulator did until this module existed, in as many words: *"The 1M
//! sub-circuit board is not installed."*
//!
//! So the identifier is not decoration. It is the whole handshake, and it has to
//! be the right one: a driver built for the 64 KiB part does not recognise the
//! 128 KiB part's answer and vice versa. The two we give are the two those
//! drivers were written against.
//!
//! # Bits go down and never up
//!
//! Programming a flash cell can clear a bit and cannot set one; only an erase
//! sets bits, and it sets them a whole sector at a time. So a write here is an
//! `AND` and not an assignment. Every driver erases before it writes, so for
//! correct software the two are the same thing — and for software that forgets,
//! this gives the answer the hardware would give instead of quietly covering for
//! it.

/// The block an erase works on. Nothing smaller can be erased.
const SECTOR: usize = 4 * 1024;

/// The window the chip is seen through, which is smaller than the 128 KiB part.
/// That is what the bank is for.
const WINDOW: usize = 64 * 1024;

/// Where the unlock sequence is written, and where a command follows it.
const UNLOCK_A: u32 = 0x5555;
const UNLOCK_B: u32 = 0x2AAA;

/// How far the unlock sequence has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Ready,
    /// `0xAA` has been written to `0x5555`.
    First,
    /// `0x55` has followed it at `0x2AAA`, so the next write is a command.
    Second,
}

/// A command that consumes the write after it rather than acting at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// `0xA0`: the next write anywhere is one byte of data.
    Byte,
    /// `0xB0`: the next write to `0x0000` chooses which half of a 128 KiB chip
    /// the window shows.
    Bank,
}

/// The flash chip on the cartridge board.
pub struct Flash {
    /// The storage, erased to `0xFF` as flash is when it leaves the factory.
    cells: Vec<u8>,
    /// Manufacturer then device, in the order they are read at `0x0000` and
    /// `0x0001`.
    id: [u8; 2],
    stage: Stage,
    pending: Pending,
    /// Set by `0x80` and spent by the erase that follows it. An erase is two
    /// unlock sequences and not one, so that nothing accidental can reach it.
    arming_erase: bool,
    /// Answering the identifier instead of the storage, until `0xF0`.
    identifying: bool,
    /// Which 64 KiB half of a 128 KiB chip the window shows.
    bank: usize,
}

impl Flash {
    /// A chip of the given size, erased.
    ///
    /// The identifier follows from the size because it has to: a driver written
    /// for one part refuses the other's answer, and the two parts are told apart
    /// by nothing else.
    pub fn new(len: usize) -> Self {
        // Panasonic MN63F805MNP for the small part, Sanyo LE26FV10N1TS for the
        // large one. These are the two the cartridge drivers of the era were
        // built against, so they are the two that get recognised.
        let id = if len > WINDOW { [0x62, 0x13] } else { [0x32, 0x1B] };
        Self {
            cells: vec![0xFF; len],
            id,
            stage: Stage::Ready,
            pending: Pending::None,
            arming_erase: false,
            identifying: false,
            bank: 0,
        }
    }

    /// How much storage the chip has, which is also how big its saved game is.
    pub fn size(&self) -> usize {
        self.cells.len()
    }

    pub fn data(&self) -> &[u8] {
        &self.cells
    }

    /// Restores a saved game. A file of the wrong length is refused rather than
    /// padded: half a saved game is worse than none, because it looks like one.
    pub fn load(&mut self, data: &[u8]) -> bool {
        if data.len() != self.cells.len() {
            return false;
        }
        self.cells.copy_from_slice(data);
        true
    }

    /// What the chip answers at an offset into its 64 KiB window.
    ///
    /// Only two offsets are ever special, and only while the identifier has been
    /// asked for. Everything else is storage, from whichever bank is selected.
    pub fn read(&self, offset: u32) -> u8 {
        let offset = offset as usize % WINDOW;
        if self.identifying && offset < 2 {
            return self.id[offset];
        }
        self.cells[self.at(offset)]
    }

    /// A write, which is nearly always a command and only sometimes data.
    pub fn write(&mut self, offset: u32, value: u8) {
        let offset = offset % WINDOW as u32;

        // The two commands that consume the write after them are handled before
        // the unlock sequence, because their operand may look like anything —
        // including like the start of a sequence.
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Byte => {
                // Programming clears bits and cannot set them; see the module
                // note. A driver that erased first sees no difference.
                let at = self.at(offset as usize);
                self.cells[at] &= value;
                return;
            }
            Pending::Bank => {
                // Only the bottom bit, and only on a chip that has two halves.
                if offset == 0 && self.cells.len() > WINDOW {
                    self.bank = usize::from(value & 1);
                }
                return;
            }
            Pending::None => {}
        }

        match self.stage {
            Stage::Ready if offset == UNLOCK_A && value == 0xAA => self.stage = Stage::First,
            Stage::First if offset == UNLOCK_B && value == 0x55 => self.stage = Stage::Second,
            Stage::Second => {
                self.stage = Stage::Ready;
                self.command(offset, value);
            }
            // Anything else abandons a half-finished sequence. A command that
            // was not spelled out exactly is not a command.
            _ => self.stage = Stage::Ready,
        }
    }

    /// The third write of an unlock sequence, which says what to do.
    ///
    /// Most commands are written to `0x5555` like the two before them. The
    /// sector erase is the exception, and deliberately so: it needs to say
    /// *which* sector, and the address it is written to is how it says it.
    fn command(&mut self, offset: u32, value: u8) {
        match value {
            // Erase this sector — but only if an erase was armed. Without that
            // check a `0x30` written to a data address would wipe 4 KiB.
            0x30 if self.arming_erase => {
                self.arming_erase = false;
                let start = self.at(offset as usize) & !(SECTOR - 1);
                self.cells[start..start + SECTOR].fill(0xFF);
            }
            _ if offset != UNLOCK_A => {}
            // Erase everything, both banks of a chip that has two.
            0x10 if self.arming_erase => {
                self.arming_erase = false;
                self.cells.fill(0xFF);
            }
            // Arm an erase. The erase itself is a second sequence after this
            // one, which is what keeps it out of reach of an accident.
            0x80 => self.arming_erase = true,
            0x90 => self.identifying = true,
            0xF0 => self.identifying = false,
            0xA0 => self.pending = Pending::Byte,
            0xB0 => self.pending = Pending::Bank,
            _ => {}
        }
    }

    /// Where in the storage an offset into the window lands.
    fn at(&self, offset: usize) -> usize {
        (self.bank * WINDOW + offset) % self.cells.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the three-write unlock sequence and its command.
    fn command(flash: &mut Flash, value: u8) {
        flash.write(UNLOCK_A, 0xAA);
        flash.write(UNLOCK_B, 0x55);
        flash.write(UNLOCK_A, value);
    }

    /// Erases a sector: arm, then a second sequence ending at its address.
    fn erase_sector(flash: &mut Flash, at: u32) {
        command(flash, 0x80);
        flash.write(UNLOCK_A, 0xAA);
        flash.write(UNLOCK_B, 0x55);
        flash.write(at, 0x30);
    }

    fn program(flash: &mut Flash, at: u32, value: u8) {
        command(flash, 0xA0);
        flash.write(at, value);
    }

    fn small() -> Flash {
        Flash::new(64 * 1024)
    }

    fn large() -> Flash {
        Flash::new(128 * 1024)
    }

    /// The handshake the whole feature exists for. A driver that reads zero here
    /// tells the player the board is not installed.
    #[test]
    fn the_chip_gives_its_name_when_asked_for_it() {
        let mut flash = small();
        assert_ne!(flash.read(0), 0x32, "storage, not an identifier, until asked");

        command(&mut flash, 0x90);
        assert_eq!(flash.read(0), 0x32, "Panasonic");
        assert_eq!(flash.read(1), 0x1B, "MN63F805MNP");

        command(&mut flash, 0xF0);
        assert_eq!(flash.read(0), 0xFF, "and back to being storage");
    }

    /// A different part answers differently, and a driver built for one refuses
    /// the other. This is the only thing that tells the two sizes apart.
    #[test]
    fn the_two_sizes_are_two_different_chips() {
        let mut flash = large();
        command(&mut flash, 0x90);
        assert_eq!([flash.read(0), flash.read(1)], [0x62, 0x13], "Sanyo LE26FV10N1TS");
    }

    #[test]
    fn a_fresh_chip_is_erased_rather_than_blank() {
        assert!(small().data().iter().all(|&b| b == 0xFF), "flash leaves the factory erased");
    }

    #[test]
    fn a_byte_is_written_only_after_the_command_that_announces_it() {
        let mut flash = small();
        // Without the command it is not data, it is a failed unlock sequence.
        flash.write(0x1234, 0x42);
        assert_eq!(flash.read(0x1234), 0xFF, "nothing was announced");

        program(&mut flash, 0x1234, 0x42);
        assert_eq!(flash.read(0x1234), 0x42);
    }

    /// The command is spent on one byte. A driver writing a run of them issues
    /// it once per byte, and a chip that stayed in write mode would take the
    /// next unlock sequence as data.
    #[test]
    fn the_write_command_covers_exactly_one_byte() {
        let mut flash = small();
        program(&mut flash, 0x10, 0x11);
        flash.write(0x20, 0x22);
        assert_eq!(flash.read(0x20), 0xFF, "the second write was not announced");
    }

    /// Programming clears bits and cannot set them. Every driver erases first,
    /// so this only shows for one that does not — and then it shows the truth
    /// rather than covering for it.
    #[test]
    fn programming_over_a_byte_can_only_clear_bits() {
        let mut flash = small();
        program(&mut flash, 0, 0b1111_0000);
        program(&mut flash, 0, 0b1010_1111);
        assert_eq!(flash.read(0), 0b1010_0000, "the AND of the two, not the second");
    }

    /// An erase takes its own 4 KiB and neither neighbour.
    ///
    /// The sector erased is deliberately not the first one: an erase that
    /// rounded the address to the wrong boundary would still look right if
    /// everything were always erased from zero.
    #[test]
    fn erasing_a_sector_leaves_both_its_neighbours_alone() {
        let mut flash = small();
        program(&mut flash, 0x0100, 0x11);
        program(&mut flash, 0x1100, 0x22);
        program(&mut flash, 0x2100, 0x33);

        erase_sector(&mut flash, 0x1000);
        assert_eq!(flash.read(0x1100), 0xFF, "this sector went");
        assert_eq!(flash.read(0x0100), 0x11, "the one before it stayed");
        assert_eq!(flash.read(0x2100), 0x33, "and so did the one after");
    }

    /// A `0x30` that nothing armed is not an erase.
    ///
    /// It has to be aimed at the sector being checked, or the test proves
    /// nothing: an unarmed erase that went ahead would wipe whatever sector the
    /// address named, and if that is some other sector the data survives for
    /// the wrong reason.
    #[test]
    fn an_erase_needs_arming_first() {
        let mut flash = small();
        program(&mut flash, 0x0100, 0x11);
        flash.write(UNLOCK_A, 0xAA);
        flash.write(UNLOCK_B, 0x55);
        flash.write(0x0000, 0x30);
        assert_eq!(flash.read(0x0100), 0x11, "nothing armed it");
    }

    #[test]
    fn erasing_the_chip_takes_both_banks_of_a_large_one() {
        let mut flash = large();
        program(&mut flash, 0, 0x11);
        command(&mut flash, 0xB0);
        flash.write(0, 1);
        program(&mut flash, 0, 0x22);

        command(&mut flash, 0x80);
        command(&mut flash, 0x10);

        assert!(flash.data().iter().all(|&b| b == 0xFF), "all 128 KiB");
    }

    /// The window is 64 KiB and the chip is 128, so half of it is only reachable
    /// through the bank. Without this the second half is storage nothing can
    /// ever address.
    #[test]
    fn the_bank_moves_the_window_over_the_second_half() {
        let mut flash = large();
        program(&mut flash, 0x20, 0x11);

        command(&mut flash, 0xB0);
        flash.write(0, 1);
        assert_eq!(flash.read(0x20), 0xFF, "the other half, untouched");
        program(&mut flash, 0x20, 0x22);

        command(&mut flash, 0xB0);
        flash.write(0, 0);
        assert_eq!(flash.read(0x20), 0x11, "and back to the first");

        assert_eq!(flash.data()[0x20], 0x11);
        assert_eq!(flash.data()[WINDOW + 0x20], 0x22, "the second bank is the second half");
    }

    /// A chip with one bank ignores the command. Answering it would fold every
    /// address onto the same 64 KiB and look like a chip half the size.
    #[test]
    fn a_small_chip_has_no_second_bank_to_switch_to() {
        let mut flash = small();
        program(&mut flash, 0x20, 0x11);
        command(&mut flash, 0xB0);
        flash.write(0, 1);
        assert_eq!(flash.read(0x20), 0x11, "still the only half there is");
    }

    /// An interrupted sequence is not a command. A game that writes `0xAA` to
    /// the magic address as data and then carries on must not have the next
    /// thing it writes taken as a command.
    ///
    /// The wrong write has to be followed by the *right* rest of a sequence, or
    /// the test proves nothing: a chip that never abandoned anything would sit
    /// half-unlocked, and it is the write after that which turns into a command
    /// it should never have been.
    #[test]
    fn a_broken_sequence_starts_over() {
        let mut flash = small();
        flash.write(UNLOCK_A, 0xAA);
        flash.write(0x1234, 0x55); // wrong address: the sequence is abandoned
        flash.write(UNLOCK_B, 0x55); // and this does not resume it
        flash.write(UNLOCK_A, 0x90);
        assert_eq!(flash.read(0), 0xFF, "no identifier: that was never a command");
    }

    #[test]
    fn a_saved_game_of_the_wrong_length_is_refused() {
        let mut flash = small();
        assert!(!flash.load(&[0x5A; 1024]), "not this chip's saved game");
        assert!(flash.load(&[0x5A; 64 * 1024]));
        assert_eq!(flash.read(0), 0x5A);
    }
}
