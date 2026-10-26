//! The EEPROM, which is not memory at all: it is a serial line with two wires,
//! and the address space is only how a game gets at it.
//!
//! # One bit per transfer
//!
//! Everything here happens a bit at a time. The game sends a command by writing
//! a run of halfwords into the cartridge's last region, and only the bottom bit
//! of each one is real; the chip answers the same way, one bit per halfword
//! read. Where in the region those halfwords land does not matter and is never
//! looked at — there is no address decoding on this chip, because there is
//! nothing to decode against.
//!
//! Which is why this is driven by a memory mover and not by ordinary code: a
//! read of eight bytes is 68 separate bus accesses that must arrive without a
//! gap, and the machine has four channels whose whole purpose is running a fixed
//! number of transfers back to back. A game talking to its EEPROM is a game
//! programming channel 3 twice.
//!
//! # Four commands' worth of grammar
//!
//! - A read request is `11`, an address, and a `0`.
//! - A write is `10`, an address, sixty-four bits of data, and a `0`.
//! - The answer to a read request is sixty-eight bits: four to be thrown away
//!   and then the eight bytes, highest bit first.
//! - A read at any other time answers `1`, meaning *ready*. That is how a game
//!   waits out a write, and on hardware it takes milliseconds.
//!
//! # How the size is discovered rather than declared
//!
//! There are two of these chips, 512 bytes and 8 KiB, and **the cartridge does
//! not say which it carries**: both spell themselves `EEPROM_V` in the ROM. The
//! only thing that differs is how wide an address is — six bits or fourteen —
//! and so how long a command is.
//!
//! Those lengths happen to be all different: 9, 17, 73, 81. So the chip does not
//! need to be told its own size; it can read it off the first command that
//! arrives. That is what happens here, and it is the reason a command is acted
//! on when the game stops sending rather than after a fixed count: until the
//! sending stops there is no way to know whether a ninth bit is the end of a
//! small read or the middle of a large one.

/// The two chips there are.
const SMALL: usize = 512;
const LARGE: usize = 8 * 1024;

/// Eight bytes at a time, always: the chip has no smaller unit.
const BLOCK: usize = 8;

/// The longest command: `10`, fourteen address bits, sixty-four of data, and the
/// trailing bit. Nothing a game sends is longer, so reaching it means the
/// command is complete whatever its size turned out to be.
const LONGEST: u32 = 2 + 14 + 64 + 1;

/// The answer to a read request: four bits of nothing, then the block.
const ANSWER: u32 = 4 + 64;

/// A read request that has been answered but not yet fully collected.
struct Answer {
    bits: u64,
    sent: u32,
}

/// The serial EEPROM on the cartridge board.
pub struct Eeprom {
    /// Room for the larger of the two chips. How much of it is real is
    /// [`Eeprom::size`], which is not known until a game says something.
    cells: Vec<u8>,
    /// Discovered from the length of the first command, and never guessed
    /// before that: writing a saved game of the wrong size out to disk would
    /// make it unreadable by the machine that wrote it.
    size: Option<usize>,
    /// The bits sent so far, newest at the bottom.
    buffer: u128,
    len: u32,
    answer: Option<Answer>,
}

impl Default for Eeprom {
    fn default() -> Self {
        Self::new()
    }
}

impl Eeprom {
    pub fn new() -> Self {
        Self { cells: vec![0xFF; LARGE], size: None, buffer: 0, len: 0, answer: None }
    }

    /// How big this chip turned out to be.
    ///
    /// Before any command has arrived there is nothing to go on, and the larger
    /// of the two is the answer that loses nothing: a saved game written at 8
    /// KiB from a chip that was never spoken to is 8 KiB of untouched cells.
    pub fn size(&self) -> usize {
        self.size.unwrap_or(LARGE)
    }

    pub fn data(&self) -> &[u8] {
        &self.cells[..self.size()]
    }

    /// Restores a saved game, and takes the chip's size from its length.
    ///
    /// The file is the only place that size survives between sessions: a game
    /// loaded and quit without saving would otherwise come back not knowing
    /// which chip it had.
    pub fn load(&mut self, data: &[u8]) -> bool {
        let size = match data.len() {
            SMALL => SMALL,
            LARGE => LARGE,
            _ => return false,
        };
        self.cells[..size].copy_from_slice(data);
        self.size = Some(size);
        true
    }

    /// One bit from the game.
    pub fn write_bit(&mut self, bit: u8) {
        // Sending anything abandons an answer half collected. A game does not do
        // this, but a chip that kept the old answer around would hand out stale
        // bits for the rest of the session if one ever did.
        self.answer = None;
        self.buffer = (self.buffer << 1) | u128::from(bit & 1);
        self.len += 1;
        // With the size already known, a command's length is known too and it
        // can be acted on the moment it is complete. Otherwise the only certain
        // end is the longest command there is.
        if Some(self.len) == self.expected() || self.len >= LONGEST {
            self.commit();
        }
    }

    /// One bit back to the game.
    ///
    /// Reading is also what ends a command whose length was still ambiguous:
    /// the game has stopped sending, so whatever it sent was all of it.
    pub fn read_bit(&mut self) -> u8 {
        if self.answer.is_none() {
            self.commit();
        }
        let Some(answer) = self.answer.as_mut() else {
            // Nothing was asked for, so this is a game waiting out a write. On
            // hardware it would be told to wait; here the write already
            // happened, so it is done.
            return 1;
        };
        let sent = answer.sent;
        let bits = answer.bits;
        answer.sent += 1;
        if sent >= ANSWER {
            self.answer = None;
            return 1;
        }
        // The first four are thrown away by the game, so what they are does not
        // matter; the rest come out highest bit first.
        if sent < 4 { 0 } else { ((bits >> (63 - (sent - 4))) & 1) as u8 }
    }

    /// How long the command being sent will be, if that is already known.
    fn expected(&self) -> Option<u32> {
        let address_bits = match self.size? {
            SMALL => 6,
            _ => 14,
        };
        if self.len < 2 {
            return None;
        }
        match (self.buffer >> (self.len - 2)) & 0b11 {
            0b11 => Some(2 + address_bits + 1),
            0b10 => Some(2 + address_bits + 64 + 1),
            _ => None,
        }
    }

    /// Acts on what has been sent, and forgets it either way.
    fn commit(&mut self) {
        let (buffer, len) = (self.buffer, self.len);
        self.buffer = 0;
        self.len = 0;

        // The four lengths are all different, which is what makes the chip able
        // to size itself. Anything else is not a command.
        let address_bits: u32 = match len {
            9 | 73 => 6,
            17 | 81 => 14,
            _ => return,
        };
        self.size.get_or_insert(if address_bits == 6 { SMALL } else { LARGE });

        let address = (buffer >> (len - 2 - address_bits)) & ((1 << address_bits) - 1);
        // Fourteen bits is more address than the chip has cells for; the top
        // four are simply not wired to anything.
        let block = (address as usize % (self.size() / BLOCK)) * BLOCK;

        match (buffer >> (len - 2)) & 0b11 {
            0b11 => {
                let mut bits = 0u64;
                for byte in &self.cells[block..block + BLOCK] {
                    bits = (bits << 8) | u64::from(*byte);
                }
                self.answer = Some(Answer { bits, sent: 0 });
            }
            0b10 => {
                // The data sits between the address and the trailing bit.
                let data = ((buffer >> 1) & u128::from(u64::MAX)) as u64;
                for (index, cell) in self.cells[block..block + BLOCK].iter_mut().enumerate() {
                    *cell = (data >> (56 - 8 * index)) as u8;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends a run of bits, highest first, as a memory mover would.
    fn send(chip: &mut Eeprom, value: u128, bits: u32) {
        for index in (0..bits).rev() {
            chip.write_bit(((value >> index) & 1) as u8);
        }
    }

    /// Asks for a block and collects the sixty-eight bits that come back.
    fn read_block(chip: &mut Eeprom, address: u32, address_bits: u32) -> u64 {
        let request = (0b11u128 << address_bits << 1) | (u128::from(address) << 1);
        send(chip, request, 2 + address_bits + 1);
        let mut value = 0u64;
        for index in 0..ANSWER {
            let bit = u64::from(chip.read_bit());
            if index >= 4 {
                value = (value << 1) | bit;
            }
        }
        value
    }

    fn write_block(chip: &mut Eeprom, address: u32, address_bits: u32, data: u64) {
        let mut request = 0b10u128;
        request = (request << address_bits) | u128::from(address);
        request = (request << 64) | u128::from(data);
        request <<= 1;
        send(chip, request, 2 + address_bits + 64 + 1);
        // A game polls for readiness after a write, and that is also what tells
        // a chip of unknown size that the command has ended.
        assert_eq!(chip.read_bit(), 1, "the write is done");
    }

    #[test]
    fn a_block_written_is_a_block_read_back() {
        let mut chip = Eeprom::new();
        write_block(&mut chip, 3, 6, 0x0123_4567_89AB_CDEF);
        assert_eq!(read_block(&mut chip, 3, 6), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn blocks_do_not_bleed_into_each_other() {
        let mut chip = Eeprom::new();
        write_block(&mut chip, 0, 6, u64::MAX);
        write_block(&mut chip, 1, 6, 0);
        assert_eq!(read_block(&mut chip, 0, 6), u64::MAX, "the neighbour left it alone");
    }

    /// The chip has no way of being told which of the two it is, so it works it
    /// out from how long the first command was. A small one is 512 bytes.
    #[test]
    fn a_short_command_makes_it_the_small_chip() {
        let mut chip = Eeprom::new();
        assert_eq!(chip.size(), LARGE, "assumed until something says otherwise");
        read_block(&mut chip, 0, 6);
        assert_eq!(chip.size(), SMALL);
        assert_eq!(chip.data().len(), SMALL, "and that is the size of its saved game");
    }

    #[test]
    fn a_long_command_makes_it_the_large_chip() {
        let mut chip = Eeprom::new();
        read_block(&mut chip, 0, 14);
        assert_eq!(chip.size(), LARGE);
        assert_eq!(chip.data().len(), LARGE);
    }

    /// Fourteen address bits reach four thousand blocks and the chip has one
    /// thousand. The top bits are not wired to anything, so they fold.
    #[test]
    fn an_address_wider_than_the_chip_folds_onto_it() {
        let mut chip = Eeprom::new();
        write_block(&mut chip, 5, 14, 0xDEAD_BEEF_0000_0000);
        assert_eq!(read_block(&mut chip, 5 + 1024, 14), 0xDEAD_BEEF_0000_0000);
    }

    /// The four bits before the data are the game's to throw away, but they must
    /// be *there*: without them every byte read would be shifted.
    #[test]
    fn the_answer_begins_with_four_bits_of_nothing() {
        let mut chip = Eeprom::new();
        write_block(&mut chip, 0, 6, u64::MAX);
        let request = 0b11u128 << 6 << 1;
        send(&mut chip, request, 9);
        for _ in 0..4 {
            assert_eq!(chip.read_bit(), 0, "padding");
        }
        assert_eq!(chip.read_bit(), 1, "and then the data");
    }

    /// Reading with nothing outstanding means "are you done writing yet", and
    /// the answer has to be yes or the game waits forever.
    #[test]
    fn a_read_with_nothing_asked_for_says_ready() {
        let mut chip = Eeprom::new();
        assert_eq!(chip.read_bit(), 1);
        assert_eq!(chip.read_bit(), 1);
    }

    /// Past the end of an answer it goes back to saying ready rather than
    /// dealing out whatever comes next in memory.
    #[test]
    fn the_answer_runs_out_after_its_sixty_eight_bits() {
        let mut chip = Eeprom::new();
        read_block(&mut chip, 0, 6);
        assert_eq!(chip.read_bit(), 1);
    }

    /// Starting a new command throws away an answer only half collected.
    ///
    /// No game does this — it would be dropping bytes of its own saved game on
    /// the floor — but a chip that kept the old answer around would go on
    /// dealing out its stale bits, and every read for the rest of the session
    /// would be one command behind.
    /// It has to be an *incomplete* command that interrupts it. A complete one
    /// replaces the answer on its own, by acting on itself, so it would prove
    /// nothing: what is being pinned here is that the line goes quiet the moment
    /// the game starts saying something else, whether or not it finishes.
    #[test]
    fn sending_something_new_abandons_the_answer_half_collected() {
        let mut chip = Eeprom::new();
        // A block of zeros, so that a stale bit is telling apart from "ready".
        write_block(&mut chip, 0, 6, 0);

        send(&mut chip, 0b11u128 << 6 << 1, 9);
        for _ in 0..4 {
            assert_eq!(chip.read_bit(), 0, "padding");
        }
        assert_eq!(chip.read_bit(), 0, "and then the block, which is zeros");

        // Three bits of something else: not a command, but the answer is over.
        for _ in 0..3 {
            chip.write_bit(1);
        }
        assert_eq!(chip.read_bit(), 1, "ready — not the sixth bit of an abandoned answer");
    }

    /// The size is settled once — by the saved game if there is one, by the
    /// first command otherwise — and nothing moves it afterwards.
    ///
    /// It is where the file's length comes from, so a chip that changed its
    /// mind half way through a session would write out a saved game a different
    /// size from the one it read in, and the next session would refuse it.
    #[test]
    fn the_size_a_saved_game_came_with_is_not_re_decided() {
        let mut chip = Eeprom::new();
        assert!(chip.load(&[0x11; LARGE]));

        // A short command is what the small chip's driver sends. Hearing one
        // must not shrink a chip that a saved game already identified.
        read_block(&mut chip, 0, 6);
        assert_eq!(chip.size(), LARGE, "the file said which chip this is");
        assert_eq!(chip.data().len(), LARGE, "and the next file is the same size");
    }

    #[test]
    fn a_saved_game_carries_the_size_with_it() {
        let mut chip = Eeprom::new();
        assert!(chip.load(&[0x5A; SMALL]));
        assert_eq!(chip.size(), SMALL);
        assert_eq!(read_block(&mut chip, 0, 6), 0x5A5A_5A5A_5A5A_5A5A);

        assert!(!Eeprom::new().load(&[0; 1234]), "neither chip is that size");
    }

    /// With the size known a command is acted on as soon as it is complete,
    /// without waiting for the game to read. Nothing depends on that today, and
    /// it is what keeps a run of writes with no polling between them from
    /// running into each other.
    #[test]
    fn a_second_write_does_not_need_the_poll_between_them() {
        let mut chip = Eeprom::new();
        write_block(&mut chip, 0, 6, 0x1111_1111_1111_1111);

        let mut request = 0b10u128;
        request = (request << 6) | 1;
        request = (request << 64) | 0x2222_2222_2222_2222;
        request <<= 1;
        send(&mut chip, request, 73);

        assert_eq!(read_block(&mut chip, 1, 6), 0x2222_2222_2222_2222);
    }
}
