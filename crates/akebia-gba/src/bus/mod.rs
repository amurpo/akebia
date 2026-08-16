//! The memory map: which addresses are which memory, and where they repeat.
//!
//! # Why an address is not an index
//!
//! Every region on this machine is smaller than the block of address space it
//! sits in, and each one **repeats to fill its block** rather than leaving a
//! hole. Internal RAM is 32 KiB inside sixteen mebibytes, so `0x0300_0000` and
//! `0x0300_8000` are the same byte. This is not a curiosity to be tidied away:
//! a program that walks off the end of a structure lands back at the start of
//! it and keeps running, and one that reads a mirror deliberately — which the
//! BIOS does — needs it to be there.
//!
//! So every access folds the address into its region first. The masks are the
//! sizes, which is why they are all powers of two.
//!
//! # The one region that does not fold cleanly
//!
//! Video memory is 96 KiB, which is not a power of two, sitting in blocks of
//! 128 KiB. The first 64 KiB appear once and the last 32 KiB appear twice: the
//! quarter at `0x1_8000` is the quarter at `0x1_0000` again. It is the only
//! fold here that has to be written as an `if`, and getting it wrong is
//! invisible until a game stores something in the second half of a sprite sheet.
//!
//! # What is not here yet
//!
//! Most of the I/O registers. The picture unit's, the memory movers', the
//! interrupt controller's three and the buttons' two are answered; everything
//! else in that region reads back zero, and the real answer is not zero — an
//! unmapped address on this machine returns whatever the processor last
//! fetched, which some games read on purpose. That wants a pipeline to ask, and
//! there is not one yet.
//!
//! Answering zero is not the harmless default it looks like, and the buttons
//! are the proof: `KEYINPUT` reports a held button as a zero, so for as long as
//! it was missing this map was telling every game that all ten were held down.
//! One sat on its title screen because of it. A register whose resting value is
//! not zero cannot be left to the fallback.

use crate::cpu::Bus;
use crate::dma::{self, Dma};
use crate::interrupts::Interrupts;
use crate::keypad::{self, Keypad};
use crate::ppu::{self, Ppu};
use crate::sound::{self, Sound};
use crate::timers::{self, Timers};

pub const BIOS_LEN: usize = 16 * 1024;
pub const EWRAM_LEN: usize = 256 * 1024;
pub const IWRAM_LEN: usize = 32 * 1024;
/// The three video memories belong to the picture unit and are re-exported
/// here because they are still regions of this map like any other.
pub use crate::ppu::{OAM_LEN, PRAM_LEN, VRAM_LEN};
/// The most cartridge ROM the address space has room for.
pub const ROM_MAX: usize = 32 * 1024 * 1024;
/// Battery-backed save memory, on an 8-bit bus.
pub const SRAM_LEN: usize = 64 * 1024;

/// The sound level register, which is the only part of the sound hardware
/// that is here. See [`Memory::sound_bias`].
const SOUND_BIAS: u32 = 0x0400_0088;
const SOUND_BIAS_AT_RESET: u16 = 0x0200;

/// The blocks video memory repeats in: 128 KiB, of which it fills 96.
const VRAM_BLOCK: u32 = 0x2_0000;

/// Everything the processor can address.
pub struct Memory {
    /// Read-only, and empty until something puts a BIOS in it.
    bios: Box<[u8; BIOS_LEN]>,
    /// The big, slow one: 256 KiB on a 16-bit bus, off-chip.
    ewram: Box<[u8; EWRAM_LEN]>,
    /// The small, fast one: 32 KiB on a 32-bit bus, on the chip itself. Where
    /// anything that matters for speed goes.
    iwram: Box<[u8; IWRAM_LEN]>,
    /// The cartridge, however much of it there is. Kept at its real length
    /// rather than padded to the 32 MiB the address space allows, because
    /// reading past the end is not reading zeros — see [`Memory::read_bytes`].
    rom: Vec<u8>,
    /// The cartridge's save memory.
    sram: Box<[u8; SRAM_LEN]>,
    /// The three registers that decide whether the processor is interrupted.
    irq: Interrupts,
    /// The picture unit, which lives here because that is where a game reaches
    /// it: its registers are four addresses in this map like any other.
    ppu: Ppu,
    /// The four memory movers. They keep their settings here and the bytes move
    /// in [`Memory::run_transfers`], because moving them means reaching the
    /// whole map — which is this and not them.
    dma: Dma,
    /// The buttons. They live here for the same reason the picture unit does:
    /// a game reaches them at an address in this map.
    keypad: Keypad,
    /// The four counters, which are the only clock a game has that is not the
    /// beam.
    timers: Timers,
    /// The two queues digital sound is played out of. There is no sound; the
    /// queues are here because the memory movers are triggered by them.
    sound: Sound,
    /// The one sound register that exists, and the reason it does.
    ///
    /// There is no sound here at all, and this changes that not one bit: it
    /// holds what is written and hands it back. It is here because the BIOS's
    /// routine for changing the level does so a step at a time, reading the
    /// register back after each step and stopping when it has reached the
    /// target — so a register that always read zero was a target never reached,
    /// and the BIOS span in that loop for ever. It cost one of the test suites
    /// its entire run.
    ///
    /// The rest of the sound registers are still absent, and a register that
    /// merely remembers is not an implementation of anything. This one is here
    /// because *reading it back* is the whole of what the BIOS needs.
    sound_bias: u16,
    cycles: u64,
    bios_loaded: bool,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory {
    pub fn new() -> Self {
        Self {
            bios: Box::new([0; BIOS_LEN]),
            ewram: Box::new([0; EWRAM_LEN]),
            iwram: Box::new([0; IWRAM_LEN]),
            rom: Vec::new(),
            sram: Box::new([0; SRAM_LEN]),
            irq: Interrupts::new(),
            ppu: Ppu::new(),
            dma: Dma::new(),
            keypad: Keypad::new(),
            timers: Timers::new(),
            sound: Sound::new(),
            // What the hardware holds after a reset: the level sitting at the
            // midpoint of its range.
            sound_bias: SOUND_BIAS_AT_RESET,
            cycles: 0,
            bios_loaded: false,
        }
    }

    /// Puts a BIOS image in. Anything shorter than 16 KiB fills from the start
    /// and leaves the rest zero.
    pub fn load_bios(&mut self, image: &[u8]) {
        let len = image.len().min(BIOS_LEN);
        self.bios[..len].copy_from_slice(&image[..len]);
        self.bios_loaded = len > 0;
    }

    /// Whether a BIOS was handed in. An empty one reads as zeros, which decode
    /// to something, so nothing else can tell the difference.
    pub fn has_bios(&self) -> bool {
        self.bios_loaded
    }

    /// One byte, without the borrow a bus access needs. For looking at memory
    /// rather than running against it.
    pub fn peek8(&self, addr: u32) -> u8 {
        self.read_bytes(addr, 1) as u8
    }

    /// Puts a cartridge in. Anything past the 32 MiB the address space allows
    /// is not reachable and is dropped.
    pub fn load_rom(&mut self, image: &[u8]) {
        self.rom.clear();
        self.rom.extend_from_slice(&image[..image.len().min(ROM_MAX)]);
    }

    pub fn rom_len(&self) -> usize {
        self.rom.len()
    }

    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    pub fn ppu(&self) -> &Ppu {
        &self.ppu
    }

    pub fn ppu_mut(&mut self) -> &mut Ppu {
        &mut self.ppu
    }

    /// The sound queues. Nothing plays them yet; this is how a test — and one
    /// day a mixer — reads what has come out of them.
    pub fn sound(&self) -> &Sound {
        &self.sound
    }

    pub fn keypad(&self) -> &Keypad {
        &self.keypad
    }

    /// How a frontend presses a button: the only input this machine has.
    pub fn keypad_mut(&mut self) -> &mut Keypad {
        &mut self.keypad
    }

    pub fn interrupts(&self) -> &Interrupts {
        &self.irq
    }

    pub fn interrupts_mut(&mut self) -> &mut Interrupts {
        &mut self.irq
    }

    /// One byte of the I/O registers.
    ///
    /// Everything wider is composed from this rather than handled separately,
    /// because the registers are not all the same width and several are read as
    /// pairs: `IE` and `IF` sit next to each other and a game reads both in one
    /// go. Going a byte at a time means the alignment cases need no thought.
    fn read_io8(&self, addr: u32) -> u8 {
        let half = |value: u16| (value >> ((addr & 1) * 8)) as u8;
        match addr & !1 {
            ppu::DISPCNT..=ppu::LAST => self.ppu.read8(addr),
            dma::BASE..=dma::LAST => self.dma.read8(addr),
            SOUND_BIAS => half(self.sound_bias),
            0x0400_0200 => half(self.irq.enabled()),
            0x0400_0202 => half(self.irq.requested()),
            0x0400_0208 => half(u16::from(self.irq.master())),
            keypad::KEYINPUT..=keypad::LAST => self.keypad.read8(addr),
            timers::BASE..=timers::LAST => self.timers.read8(addr),
            sound::CONTROL | sound::FIFO_A..=sound::FIFO_B => self.sound.read8(addr),
            _ => 0,
        }
    }

    fn write_io8(&mut self, addr: u32, value: u8) {
        // A halfword register written a byte at a time: the byte goes into its
        // half and the other half is kept.
        let widened = |existing: u16| -> u16 {
            let shift = (addr & 1) * 8;
            (existing & !(0xFFu16 << shift)) | (u16::from(value) << shift)
        };
        match addr & !1 {
            ppu::DISPCNT..=ppu::LAST => self.ppu.write8(addr, value),
            dma::BASE..=dma::LAST => self.dma.write8(addr, value),
            keypad::KEYINPUT..=keypad::LAST => self.keypad.write8(addr, value),
            timers::BASE..=timers::LAST => self.timers.write8(addr, value),
            sound::CONTROL | sound::FIFO_A..=sound::FIFO_B => self.sound.write8(addr, value),
            SOUND_BIAS => self.sound_bias = widened(self.sound_bias),
            0x0400_0200 => {
                let updated = widened(self.irq.enabled());
                self.irq.set_enabled(updated);
            }
            // Not an ordinary register: the ones written retire requests and
            // the zeros leave them alone. Composing it through `widened` would
            // be wrong, because the half not being written must not be cleared.
            0x0400_0202 => {
                let shift = (addr & 1) * 8;
                self.irq.acknowledge(u16::from(value) << shift);
            }
            0x0400_0208 => {
                if addr & 1 == 0 {
                    self.irq.set_master(value & 1 != 0);
                }
            }
            // Telling the processor to stop until something arrives. The other
            // bit of this register asks for a deeper sleep that stops the clocks
            // as well; nothing here has clocks to stop yet, so it is taken as an
            // ordinary halt and the difference is written down rather than
            // pretended away.
            0x0400_0300 if addr & 1 == 1 => self.irq.halt(),
            _ => {}
        }
    }

    /// Runs whatever the memory movers have been asked to move.
    ///
    /// # Why the bytes move here and not in [`crate::dma`]
    ///
    /// Because a transfer reaches the whole address space, and the whole
    /// address space is this. The channels keep their settings and decide when
    /// and how far each address steps; what they hand over is a plain value
    /// describing the work, so that nothing borrows a channel while the copy
    /// runs through a map the channels are part of.
    ///
    /// A transfer goes through the same reads and writes the processor uses,
    /// which is not a shortcut: it is what makes a channel writing to palette
    /// memory behave like palette memory, and one reading past the end of a
    /// cartridge read back the floating pattern rather than zeros.
    fn run_transfers(&mut self) {
        while let Some(index) = self.dma.next_ready() {
            let job = self.dma.job(index);
            let mut source = job.source;
            let mut dest = job.dest;
            for _ in 0..job.units {
                let value = self.read_bytes(source & !(job.width - 1), job.width as usize);
                self.write_bytes(dest & !(job.width - 1), value, job.width as usize);
                source = source.wrapping_add(job.source_step);
                dest = dest.wrapping_add(job.dest_step);
            }
            self.dma.finished(index, source, dest, &mut self.irq);
        }
    }

    /// The region an address falls in, and where in that region.
    ///
    /// Returning the offset already folded is what keeps the mirroring in one
    /// place: every caller below gets an index it can use without thinking
    /// about it.
    fn locate(&self, addr: u32) -> Where<'_> {
        match addr >> 24 {
            // The BIOS does not mirror: everything above it and below external
            // RAM is simply not there.
            0x00 if (addr as usize) < BIOS_LEN => Where::Rom(&self.bios[..], addr as usize),
            // The registers. Everything the machine has that is not memory is
            // reached through this window.
            0x04 => Where::Registers,
            0x02 => Where::Ram(Bank::Ewram, addr as usize & (EWRAM_LEN - 1)),
            0x03 => Where::Ram(Bank::Iwram, addr as usize & (IWRAM_LEN - 1)),
            0x05 => Where::Ram(Bank::Pram, addr as usize & (PRAM_LEN - 1)),
            0x06 => Where::Ram(Bank::Vram, vram_offset(addr)),
            0x07 => Where::Ram(Bank::Oam, addr as usize & (OAM_LEN - 1)),
            // The cartridge appears three times over. It is one chip and one
            // set of contents; what differs between the three windows is how
            // long an access takes, which a game chooses by reading its code
            // through one and its data through another. Nothing here can tell
            // them apart yet because nothing here counts cycles.
            0x08..=0x0D => {
                let offset = (addr as usize) & (0x0200_0000 - 1);
                if offset < self.rom.len() {
                    Where::Rom(&self.rom[..], offset)
                } else {
                    // Past the end of the cartridge — or with none in at all —
                    // the bus is left floating and settles into a pattern made
                    // of the address itself. Some games read it on purpose and
                    // more read it by accident, so zeros here would be a
                    // different machine.
                    Where::Floating
                }
            }
            0x0E | 0x0F => Where::Ram(Bank::Sram, addr as usize & (SRAM_LEN - 1)),
            _ => Where::Nowhere,
        }
    }

    fn bank(&self, bank: Bank) -> &[u8] {
        match bank {
            Bank::Ewram => &self.ewram[..],
            Bank::Iwram => &self.iwram[..],
            Bank::Pram => self.ppu.pram(),
            Bank::Vram => self.ppu.vram(),
            Bank::Oam => self.ppu.oam(),
            Bank::Sram => &self.sram[..],
        }
    }

    fn bank_mut(&mut self, bank: Bank) -> &mut [u8] {
        match bank {
            Bank::Ewram => &mut self.ewram[..],
            Bank::Iwram => &mut self.iwram[..],
            Bank::Pram => self.ppu.pram_mut(),
            Bank::Vram => self.ppu.vram_mut(),
            Bank::Oam => self.ppu.oam_mut(),
            Bank::Sram => &mut self.sram[..],
        }
    }

    /// The bytes an access covers, or nothing if the address is not backed by
    /// memory that can be written.
    fn writable(&mut self, addr: u32) -> Option<(Bank, usize)> {
        match self.locate(addr) {
            Where::Ram(bank, offset) => Some((bank, offset)),
            // The BIOS and the cartridge are read-only, and unmapped space is
            // not there at all.
            Where::Rom(..) | Where::Floating | Where::Registers | Where::Nowhere => None,
        }
    }

    fn read_bytes(&self, addr: u32, len: usize) -> u32 {
        let (bytes, offset) = match self.locate(addr) {
            Where::Rom(bytes, offset) => (bytes, offset),
            Where::Ram(Bank::Sram, offset) => {
                // Save memory sits on an eight-bit bus, so a wider read does
                // not fetch more: it fetches the one byte and hands back copies
                // of it. A game that reads its save file a word at a time gets
                // four of the first byte, which is what the hardware gives and
                // not what a plain array would.
                let byte = u32::from(self.sram[offset]);
                return match len {
                    1 => byte,
                    2 => byte * 0x0101,
                    _ => byte * 0x0101_0101,
                };
            }
            Where::Ram(bank, offset) => (self.bank(bank), offset),
            Where::Floating => return floating(addr, len),
            Where::Registers => {
                let mut value = 0u32;
                for index in 0..len {
                    value |= u32::from(self.read_io8(addr + index as u32)) << (index * 8);
                }
                return value;
            }
            Where::Nowhere => return 0,
        };
        let mut value = 0u32;
        for index in 0..len {
            value |= u32::from(bytes[offset + index]) << (index * 8);
        }
        value
    }

    fn write_bytes(&mut self, addr: u32, value: u32, len: usize) {
        if matches!(self.locate(addr), Where::Registers) {
            for index in 0..len {
                self.write_io8(addr + index as u32, (value >> (index * 8)) as u8);
            }
            return;
        }
        let Some((bank, offset)) = self.writable(addr) else {
            return;
        };
        if bank == Bank::Sram {
            // Eight bits wide going out as well: only the bottom byte lands,
            // wherever in the word it was written from.
            self.sram[offset] = value as u8;
            return;
        }
        let bytes = self.bank_mut(bank);
        for index in 0..len {
            bytes[offset + index] = (value >> (index * 8)) as u8;
        }
    }
}

/// Which of the writable memories an offset is in. The BIOS is not among them,
/// which is the point of the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bank {
    Ewram,
    Iwram,
    Pram,
    Vram,
    Oam,
    Sram,
}

enum Where<'a> {
    /// Readable and not writable.
    Rom(&'a [u8], usize),
    Ram(Bank, usize),
    /// Cartridge space with no cartridge behind it, which reads as a pattern
    /// made of the address rather than as nothing.
    Floating,
    /// The I/O registers, which are not backed by an array: reading one can
    /// have an effect and writing one usually does.
    Registers,
    /// Not backed by anything.
    Nowhere,
}

/// What cartridge space reads as when there is no cartridge behind it.
///
/// The bus is left floating and settles into a pattern made of the address: each
/// halfword reads back as its own index. It is not a fiction — it is what the
/// hardware measurably does, and a machine that answered zero here would be a
/// different one.
fn floating(addr: u32, len: usize) -> u32 {
    let half = |at: u32| (at >> 1) & 0xFFFF;
    match len {
        1 => (half(addr) >> ((addr & 1) * 8)) & 0xFF,
        2 => half(addr),
        _ => half(addr) | (half(addr.wrapping_add(2)) << 16),
    }
}

/// Video memory's fold: 96 KiB of storage in blocks of 128, the last quarter of
/// each block being the third quarter over again.
fn vram_offset(addr: u32) -> usize {
    let offset = addr % VRAM_BLOCK;
    let offset = if offset >= VRAM_LEN as u32 { offset - 0x8000 } else { offset };
    offset as usize
}

impl Bus for Memory {
    fn read8(&mut self, addr: u32) -> u8 {
        self.read_bytes(addr, 1) as u8
    }

    fn read16(&mut self, addr: u32) -> u16 {
        self.read_bytes(addr & !1, 2) as u16
    }

    fn read32(&mut self, addr: u32) -> u32 {
        self.read_bytes(addr & !3, 4)
    }

    /// A write of one byte, which three of these memories will not do.
    ///
    /// Palette, video and sprite memory sit on a 16-bit bus and have no way to
    /// write half of one. What they do instead is not the same in each, and it
    /// is not a detail: a game that clears a palette a byte at a time gets
    /// **both** bytes of every entry set, and one that pokes a single byte of
    /// sprite memory gets nothing at all. Emulating this as a plain byte write
    /// leaves the first case half-black and the second silently working, and
    /// neither shows up until something looks wrong on screen.
    ///
    /// - Palette memory writes the byte to both halves of its halfword.
    /// - Video memory does the same, but only below where sprites begin. Above
    ///   it the write is dropped.
    /// - Sprite memory drops it always.
    fn write8(&mut self, addr: u32, value: u8) {
        if matches!(self.locate(addr), Where::Registers) {
            self.write_io8(addr, value);
            return;
        }
        let Some((bank, offset)) = self.writable(addr) else {
            return;
        };
        match bank {
            // Save memory is the one region a byte is the *natural* width for:
            // its bus is eight bits wide and a byte is all it can ever take.
            Bank::Ewram | Bank::Iwram | Bank::Sram => self.bank_mut(bank)[offset] = value,
            Bank::Oam => {}
            Bank::Pram => {
                let pair = offset & !1;
                self.ppu.pram_mut()[pair] = value;
                self.ppu.pram_mut()[pair + 1] = value;
            }
            Bank::Vram => {
                if (offset as u32) < self.ppu.obj_base() {
                    let pair = offset & !1;
                    self.ppu.vram_mut()[pair] = value;
                    self.ppu.vram_mut()[pair + 1] = value;
                }
            }
        }
    }

    fn write16(&mut self, addr: u32, value: u16) {
        self.write_bytes(addr & !1, u32::from(value), 2);
    }

    fn write32(&mut self, addr: u32, value: u32) {
        self.write_bytes(addr & !3, value, 4);
    }

    /// Moves the clock, and with it the beam.
    ///
    /// The picture unit is the only thing here that has anywhere to go, and it
    /// is driven from this one place rather than from the emulator's loop so
    /// that it cannot be forgotten. A frontend that runs the processor and
    /// never advances the picture would produce a machine that halts on the
    /// first thing it waits for, and would look like a bug in the game.
    fn tick(&mut self, cycles: u32) {
        self.cycles += u64::from(cycles);
        let crossed = self.ppu.tick(cycles, &mut self.irq);
        // The timers report coming round whether or not they interrupt, because
        // an overflow is also a sound queue's sample clock.
        let overflowed = self.timers.tick(cycles, &mut self.irq);
        self.dma.at_blanking(crossed);
        // Draining comes before asking, so that a queue emptied by this tick is
        // refilled by it rather than a tick later. What is asked is the queue's
        // state and not what this tick did to it: a queue nothing has played
        // from yet is empty, and has to be filled before a first sample can
        // come out of it.
        self.sound.at_timers(overflowed);
        self.dma.at_fifo(self.sound.hungry());
        self.run_transfers();
        // The buttons are compared against what the game asked to watch here
        // rather than where a button is pressed, because the hardware compares
        // them continuously: it is a level and not an edge, and a game that
        // starts watching a button already held expects to hear about it.
        self.keypad.poll(&mut self.irq);
    }

    fn interrupts(&self) -> &Interrupts {
        &self.irq
    }

    fn interrupts_mut(&mut self) -> &mut Interrupts {
        &mut self.irq
    }

    fn peek32(&self, addr: u32) -> u32 {
        self.read_bytes(addr & !3, 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keypad::Button;

    const EWRAM: u32 = 0x0200_0000;
    const IWRAM: u32 = 0x0300_0000;
    const PRAM: u32 = 0x0500_0000;
    const VRAM: u32 = 0x0600_0000;
    const OAM: u32 = 0x0700_0000;
    /// Where sprites begin in video memory in the tiled modes, which is where
    /// the picture unit starts out.
    const TILED_OBJ: u32 = 0x1_0000;

    /// Little-endian, and every width agrees with every other. If this is wrong
    /// nothing else here means anything.
    #[test]
    fn the_widths_see_the_same_bytes_in_the_same_order() {
        let mut mem = Memory::new();
        mem.write32(IWRAM, 0x1234_5678);

        assert_eq!(mem.read32(IWRAM), 0x1234_5678);
        assert_eq!(mem.read16(IWRAM), 0x5678, "the low halfword is first");
        assert_eq!(mem.read16(IWRAM + 2), 0x1234);
        assert_eq!(mem.read8(IWRAM), 0x78, "and the low byte first of all");
        assert_eq!(mem.read8(IWRAM + 1), 0x56);
        assert_eq!(mem.read8(IWRAM + 2), 0x34);
        assert_eq!(mem.read8(IWRAM + 3), 0x12);
    }

    /// The memory system has no way to fetch a word from an odd address and
    /// does not try. The rotation the processor does on top of this is the
    /// processor's, and is not here.
    #[test]
    fn an_unaligned_access_reads_the_aligned_one_underneath_it() {
        let mut mem = Memory::new();
        mem.write32(IWRAM, 0xAABB_CCDD);

        for skew in 0..4 {
            assert_eq!(mem.read32(IWRAM + skew), 0xAABB_CCDD, "word, skewed by {skew}");
        }
        assert_eq!(mem.read16(IWRAM + 1), 0xCCDD, "halfword, skewed by one");
        assert_eq!(mem.read16(IWRAM + 3), 0xAABB);

        // And a store to an odd address lands on the even one, rather than
        // straddling two.
        mem.write32(IWRAM + 2, 0x1111_2222);
        assert_eq!(mem.read32(IWRAM), 0x1111_2222);
    }

    /// Each memory repeats to fill its block instead of leaving a hole. A
    /// program that walks off the end of one lands back at the start of it.
    #[test]
    fn every_memory_repeats_to_fill_its_block() {
        let mut mem = Memory::new();
        for (name, base, len) in [
            ("external RAM", EWRAM, EWRAM_LEN),
            ("internal RAM", IWRAM, IWRAM_LEN),
            ("palette", PRAM, PRAM_LEN),
            ("sprites", OAM, OAM_LEN),
        ] {
            let len = len as u32;
            mem.write32(base, 0xFACE_0FF0);
            assert_eq!(mem.read32(base + len), 0xFACE_0FF0, "{name} at one length on");
            assert_eq!(mem.read32(base + len * 3), 0xFACE_0FF0, "{name} at three");

            // And the mirror is the same storage, not a copy of it.
            mem.write32(base + len, 0x0BAD_F00D);
            assert_eq!(mem.read32(base), 0x0BAD_F00D, "{name} writes through its mirror");
        }
    }

    /// Video memory is 96 KiB in blocks of 128: the first 64 appear once and
    /// the last 32 appear twice. It is the only fold that is not a mask, and
    /// getting it wrong stays invisible until something is stored in the second
    /// half of a sprite sheet.
    #[test]
    fn video_memory_folds_its_last_quarter_onto_its_third() {
        let mut mem = Memory::new();

        mem.write32(VRAM + 0x1_0000, 0x1111_1111);
        assert_eq!(mem.read32(VRAM + 0x1_8000), 0x1111_1111, "the last quarter is the third");

        mem.write32(VRAM + 0x1_8000, 0x2222_2222);
        assert_eq!(mem.read32(VRAM + 0x1_0000), 0x2222_2222, "and writes through it");

        // The first 64 KiB are their own, and must not be caught by the fold.
        mem.write32(VRAM, 0x3333_3333);
        assert_eq!(mem.read32(VRAM + 0x1_0000), 0x2222_2222, "the front is untouched");
        assert_eq!(mem.read32(VRAM), 0x3333_3333);

        // The whole thing repeats every 128 KiB.
        assert_eq!(mem.read32(VRAM + VRAM_BLOCK), 0x3333_3333, "and the block repeats");
        assert_eq!(mem.read32(VRAM + VRAM_BLOCK * 5), 0x3333_3333);
    }

    /// Every offset a fold can produce has to be inside the memory it folded
    /// into. This is the test that says the arithmetic cannot panic, which
    /// matters more than any single address being right.
    #[test]
    fn no_fold_lands_outside_the_memory_it_folds_into() {
        let mut mem = Memory::new();
        for block in 0..4u32 {
            for offset in (0..VRAM_BLOCK).step_by(4) {
                mem.write32(VRAM + block * VRAM_BLOCK + offset, offset);
            }
        }
        for base in [EWRAM, IWRAM, PRAM, OAM] {
            for offset in (0..0x2_0000).step_by(4) {
                mem.write32(base + offset, offset);
                let _ = mem.read32(base + offset);
            }
        }
    }

    /// Palette, video and sprite memory sit on a 16-bit bus and cannot write
    /// half of one. What each does instead is different, and none of them is a
    /// plain byte write.
    #[test]
    fn the_three_sixteen_bit_memories_refuse_a_byte_each_in_their_own_way() {
        let mut mem = Memory::new();

        // Palette doubles the byte into both halves of its halfword.
        mem.write8(PRAM + 4, 0x3C);
        assert_eq!(mem.read16(PRAM + 4), 0x3C3C, "a palette byte lands twice");
        mem.write8(PRAM + 5, 0x1F);
        assert_eq!(mem.read16(PRAM + 4), 0x1F1F, "and the odd byte does the same");

        // Sprites drop it entirely.
        mem.write16(OAM, 0xBEEF);
        mem.write8(OAM, 0x00);
        assert_eq!(mem.read16(OAM), 0xBEEF, "a byte written to sprite memory is dropped");

        // Video doubles it below where sprites begin and drops it above.
        mem.write8(VRAM, 0x7E);
        assert_eq!(mem.read16(VRAM), 0x7E7E, "a background byte lands twice");
        mem.write16(VRAM + TILED_OBJ, 0xCAFE);
        mem.write8(VRAM + TILED_OBJ, 0x00);
        assert_eq!(mem.read16(VRAM + TILED_OBJ), 0xCAFE, "a sprite byte is dropped");
    }

    /// Where sprites begin moves with the video mode, so the same address is
    /// background in one mode and sprites in another — and a byte written to it
    /// lands or is dropped accordingly. The memory map has to ask the picture
    /// unit, which is the only thing that knows.
    #[test]
    fn the_boundary_a_byte_write_respects_moves_with_the_video_mode() {
        let mut mem = Memory::new();

        mem.write16(VRAM + TILED_OBJ, 0xCAFE);
        mem.write8(VRAM + TILED_OBJ, 0x55);
        assert_eq!(mem.read16(VRAM + TILED_OBJ), 0xCAFE, "dropped, in a tiled mode");

        // Mode 3, where the picture itself reaches past that address.
        mem.write16(0x0400_0000, 3);
        mem.write8(VRAM + TILED_OBJ, 0x55);
        assert_eq!(mem.read16(VRAM + TILED_OBJ), 0x5555, "and lands once it is background");
    }

    /// The two 32-bit RAMs take a byte as a byte, which is the ordinary case
    /// and the one the special cases above have to be measured against.
    #[test]
    fn the_two_thirty_two_bit_memories_take_a_byte_as_a_byte() {
        let mut mem = Memory::new();
        for base in [EWRAM, IWRAM] {
            mem.write32(base, 0xFFFF_FFFF);
            mem.write8(base + 1, 0x00);
            assert_eq!(mem.read32(base), 0xFFFF_00FF, "one byte and no other");
        }
    }

    #[test]
    fn the_bios_can_be_read_and_not_written() {
        let mut mem = Memory::new();
        mem.load_bios(&[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(mem.read32(0), 0x4433_2211);

        mem.write32(0, 0xFFFF_FFFF);
        assert_eq!(mem.read32(0), 0x4433_2211, "a write to the BIOS is dropped");

        // It does not mirror: past its end is nothing, not the start again.
        assert_eq!(mem.read32(BIOS_LEN as u32), 0);
    }

    #[test]
    fn a_bios_shorter_than_the_space_leaves_the_rest_zero() {
        let mut mem = Memory::new();
        mem.load_bios(&[0xAB; 8]);
        assert_eq!(mem.read32(0), 0xABAB_ABAB);
        assert_eq!(mem.read32(8), 0, "and stops where the image did");
    }

    /// Reads of what is not there come back zero, which is *not* what the
    /// hardware does - it gives back whatever was last fetched. Pinned here so
    /// that when a pipeline exists to ask, this test is what changes.
    ///
    /// The cartridge used to be on this list, and so did the picture unit's
    /// first register, the buttons, and the timers. Things coming off it is the
    /// list working as intended — it is a list of what is missing, and the only
    /// way to keep it honest is to make it fail when something arrives.
    #[test]
    fn what_is_not_mapped_yet_reads_as_zero() {
        let mut mem = Memory::new();
        // A sound channel, a serial register, and the space above the map.
        for addr in [0x0400_0060, 0x0400_0120, 0x1000_0000, BIOS_LEN as u32] {
            assert_eq!(mem.read32(addr), 0, "0x{addr:08X}");
            mem.write32(addr, 0xFFFF_FFFF);
            assert_eq!(mem.read32(addr), 0, "0x{addr:08X} after a write");
        }
    }

    /// The buttons are reachable at their address, and a machine with nobody
    /// touching it answers with every bit set.
    ///
    /// This is the register whose absence was worth a test of its own: unmapped
    /// I/O answers zero, and zero here is not "no buttons" but *every button
    /// held down*, which is what left a cartridge unable to get off its title
    /// screen. The list above is what must never be allowed to swallow it.
    #[test]
    fn the_buttons_are_reachable_and_rest_with_every_bit_set() {
        let mut mem = Memory::new();
        assert_eq!(mem.read16(keypad::KEYINPUT), 0x03FF, "nobody is touching it");

        mem.keypad_mut().set(Button::Start, true);
        assert_eq!(mem.read16(keypad::KEYINPUT), 0x03FF & !0x0008, "Start is bit 3");

        // And a game cannot press its own buttons through the map either.
        mem.write16(keypad::KEYINPUT, 0);
        assert_eq!(mem.read16(keypad::KEYINPUT), 0x03FF & !0x0008, "unchanged by the write");

        // The control register beside it does take a write, so this is a rule
        // about the one address and not about the pair.
        mem.write16(keypad::KEYCNT, 0x4001);
        assert_eq!(mem.read16(keypad::KEYCNT), 0x4001);
    }

    /// A timer counts the same clock the beam does, and interrupts through the
    /// same controller. Wiring it to a clock of its own would be two machines.
    #[test]
    fn a_timer_counts_the_clock_and_interrupts_through_it() {
        let mut mem = Memory::new();
        // Four cycles short of coming round, asking for the interrupt.
        mem.write16(timers::BASE, 0xFFFC);
        mem.write16(timers::BASE + 2, 0x00C0);
        assert_eq!(mem.read16(timers::BASE), 0xFFFC, "switched on, so loaded");

        mem.tick(3);
        assert_eq!(mem.read16(timers::BASE), 0xFFFF);
        assert_eq!(mem.interrupts().requested(), 0, "one short");

        mem.tick(1);
        assert_eq!(mem.read16(timers::BASE), 0xFFFC, "back to the reload");
        assert_ne!(mem.interrupts().requested(), 0, "and it said so");
    }

    /// The whole of playing a tune, with nothing listening at the end of it.
    ///
    /// Three separate pieces have to meet for this and none of them is enough
    /// alone: a timer coming round at the sample rate, a queue that empties one
    /// sample at a time and asks when it is half gone, and a memory mover in
    /// its special mode that answers. This is the test that says they are wired
    /// to each other and not merely all present.
    #[test]
    fn a_timer_drains_a_queue_and_a_mover_refills_it() {
        let mut mem = Memory::new();

        // A tune in memory: 64 bytes counting up, so where the queue has got to
        // is readable off the sample.
        for byte in 0..64u32 {
            mem.write8(EWRAM + byte, byte as u8);
        }

        // Channel 1 feeds the first queue, in the special mode, repeating.
        let one = dma::BASE + 12;
        mem.write32(one, EWRAM);
        mem.write32(one + 4, sound::FIFO_A);
        mem.write16(one + 10, 0x8000 | 0x0200 | 0x3000 | 0x0400);

        // Timer 0 comes round every four cycles, and drives the queue.
        mem.write16(timers::BASE, 0xFFFC);
        mem.write16(timers::BASE + 2, 0x0080);

        // The queue starts empty, so it is hungry, so the first tick refills it
        // before anything can be played out of it.
        mem.tick(4);
        assert_eq!(mem.sound().playing()[0], 0, "nothing was in it to play yet");

        // Sixteen bytes arrived. Playing them out gives the tune in order.
        let mut heard = Vec::new();
        for _ in 0..16 {
            mem.tick(4);
            heard.push(mem.sound().playing()[0]);
        }
        assert_eq!(heard[0], 0, "the first sample of the tune");
        assert_eq!(heard[15], 15, "and the sixteenth");

        // And it kept going: the mover was asked again part way through and
        // posted the next sixteen, so the tune carries on rather than repeating
        // or falling silent.
        let mut more = Vec::new();
        for _ in 0..16 {
            mem.tick(4);
            more.push(mem.sound().playing()[0]);
        }
        assert_eq!(more[0], 16, "straight on into the next sixteen");
        assert_eq!(more[15], 31);
    }

    /// And with no timer running, nothing is drained and nothing is asked for.
    /// A queue is not a thing that empties on its own.
    #[test]
    fn a_queue_with_no_timer_driving_it_stays_where_it_is() {
        let mut mem = Memory::new();
        for byte in 0..64u32 {
            mem.write8(EWRAM + byte, 0x40 + byte as u8);
        }
        let one = dma::BASE + 12;
        mem.write32(one, EWRAM);
        mem.write32(one + 4, sound::FIFO_A);
        mem.write16(one + 10, 0x8000 | 0x0200 | 0x3000 | 0x0400);

        // The queue is empty, so it asks — and keeps asking until it is full,
        // which takes two refills of sixteen. That it stops asking is what says
        // the mark is a level and not a one-off.
        mem.tick(1);
        assert!(mem.sound().hungry()[0], "half full is still asking");
        mem.tick(1);
        assert!(!mem.sound().hungry()[0], "and now it is full and stops");

        // And through all of it nothing was played, because no timer is running
        // to play it. A queue does not empty on its own.
        mem.tick(1000);
        assert_eq!(mem.sound().playing()[0], 0, "no timer, so no samples");
        assert!(!mem.sound().hungry()[0], "and it stays full");
    }

    /// And the interrupt reaches the processor from there, which is the only
    /// interrupt on this machine whose source is a person.
    #[test]
    fn the_buttons_can_interrupt_through_the_clock() {
        let mut mem = Memory::new();
        mem.write16(keypad::KEYCNT, 0x4000 | 0x0008);
        mem.tick(1);
        assert_eq!(mem.interrupts().requested(), 0, "nothing held");

        mem.keypad_mut().set(Button::Start, true);
        mem.tick(1);
        assert_ne!(mem.interrupts().requested(), 0, "and now it is");
    }

    #[test]
    fn a_cartridge_appears_three_times_over_and_cannot_be_written() {
        let mut mem = Memory::new();
        mem.load_rom(&[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(mem.rom_len(), 4);

        for window in [0x0800_0000u32, 0x0A00_0000, 0x0C00_0000] {
            assert_eq!(mem.read32(window), 0x4433_2211, "window 0x{window:08X}");
        }

        mem.write32(0x0800_0000, 0xFFFF_FFFF);
        assert_eq!(mem.read32(0x0800_0000), 0x4433_2211, "a write to it is dropped");
    }

    /// Past the end of a cartridge - or with none in at all - the bus is left
    /// floating and settles into a pattern made of the address, each halfword
    /// reading back as its own index. Zeros here would be a different machine.
    #[test]
    fn cartridge_space_with_nothing_behind_it_reads_the_address_back() {
        let mut mem = Memory::new();
        assert_eq!(mem.read16(0x0800_0000), 0x0000);
        assert_eq!(mem.read16(0x0800_0002), 0x0001, "the next halfword, the next index");
        assert_eq!(mem.read16(0x0800_0008), 0x0004);
        assert_eq!(mem.read32(0x0800_0000), 0x0001_0000, "a word is two of them");

        // And it picks up immediately past a cartridge that is there.
        mem.load_rom(&[0xAA; 4]);
        assert_eq!(mem.read32(0x0800_0000), 0xAAAA_AAAA, "the cartridge");
        assert_eq!(mem.read16(0x0800_0004), 0x0002, "and the float just past it");
    }

    /// Save memory is eight bits wide, so a wider read does not fetch more: it
    /// fetches the one byte and hands back copies. A game reading its save a
    /// word at a time gets four of the first byte.
    #[test]
    fn save_memory_answers_every_width_with_one_byte() {
        let mut mem = Memory::new();
        mem.write8(0x0E00_0000, 0x5A);
        assert_eq!(mem.read8(0x0E00_0000), 0x5A);
        assert_eq!(mem.read16(0x0E00_0000), 0x5A5A, "a halfword is the byte twice");
        assert_eq!(mem.read32(0x0E00_0000), 0x5A5A_5A5A, "and a word is it four times");

        // Going out it is the same: only the bottom byte lands.
        mem.write32(0x0E00_0010, 0x1234_5678);
        assert_eq!(mem.read8(0x0E00_0010), 0x78);
        assert_eq!(mem.read8(0x0E00_0011), 0, "and the neighbour was not touched");
    }

    #[test]
    fn peeking_reads_without_moving_the_clock() {
        let mut mem = Memory::new();
        mem.write32(IWRAM, 0x1234_5678);
        mem.tick(6);
        let before = mem.cycles();

        assert_eq!(mem.peek32(IWRAM), 0x1234_5678);
        assert_eq!(mem.peek32(IWRAM + 3), 0x1234_5678, "aligned, like every other read");
        assert_eq!(mem.cycles(), before);
    }

    #[test]
    fn ticking_counts_the_cycles() {
        let mut mem = Memory::new();
        assert_eq!(mem.cycles(), 0);
        mem.tick(3);
        mem.tick(5);
        assert_eq!(mem.cycles(), 8);
    }

    /// The picture unit's four registers are reachable through the map, which
    /// is the only way a game can touch them — and the counter a stuck
    /// cartridge sat reading now answers something other than zero.
    #[test]
    fn the_picture_units_registers_are_reachable_through_the_memory_map() {
        let mut mem = Memory::new();

        mem.write16(0x0400_0000, 0x0403);
        assert_eq!(mem.read16(0x0400_0000), 0x0403, "what was written to the control register");
        assert_eq!(mem.ppu().mode(), 3);

        assert_eq!(mem.read16(0x0400_0006), 0, "the beam starts at the top");
        mem.tick(crate::ppu::FRAME_CYCLES / 2);
        assert_ne!(mem.read16(0x0400_0006), 0, "and the clock moves it");
        assert_eq!(mem.read16(0x0400_0006), mem.ppu().vcount());
    }

    /// The whole point of the movers: bytes that were in one memory are in
    /// another afterwards, without the processor having touched them.
    #[test]
    fn a_mover_carries_bytes_from_one_memory_to_another() {
        let mut mem = Memory::new();
        for index in 0..8u32 {
            mem.write32(EWRAM + index * 4, 0x1000_0000 + index);
        }

        // Channel 3, eight words, external RAM to video memory, go now.
        mem.write32(0x0400_00D4, EWRAM);
        mem.write32(0x0400_00D8, VRAM);
        mem.write16(0x0400_00DC, 8);
        mem.write16(0x0400_00DE, 0x8400);

        assert_eq!(mem.read32(VRAM), 0, "nothing has moved yet");
        mem.tick(1);

        for index in 0..8u32 {
            assert_eq!(mem.read32(VRAM + index * 4), 0x1000_0000 + index, "word {index}");
        }
        assert_eq!(mem.read16(0x0400_00DE) & 0x8000, 0, "and the channel switched itself off");
    }

    /// A mover waiting for the bottom of the frame runs when the beam gets
    /// there and not before, which is the arrangement every game uses to change
    /// what is on screen without tearing it.
    #[test]
    fn a_mover_waiting_for_the_gap_runs_when_the_beam_reaches_it() {
        let mut mem = Memory::new();
        mem.write32(EWRAM, 0xABCD_1234);

        mem.write32(0x0400_00D4, EWRAM);
        mem.write32(0x0400_00D8, PRAM);
        mem.write16(0x0400_00DC, 1);
        // Enabled, one word, when the beam reaches the bottom.
        mem.write16(0x0400_00DE, 0x8400 | 0x1000);

        mem.tick(crate::ppu::FRAME_CYCLES / 4);
        assert_eq!(mem.read32(PRAM), 0, "the beam is still on the screen");

        mem.tick(crate::ppu::FRAME_CYCLES);
        assert_eq!(mem.read32(PRAM), 0xABCD_1234, "and now it has been past the bottom");
    }

    /// A transfer goes through the same map the processor does, so a memory
    /// with a rule of its own still has it. Save memory takes the low byte of
    /// whatever is written and no more, mover or not.
    #[test]
    fn a_transfer_obeys_the_memory_it_lands_in() {
        let mut mem = Memory::new();
        mem.write32(EWRAM, 0x1122_3344);

        mem.write32(0x0400_00D4, EWRAM);
        mem.write32(0x0400_00D8, 0x0E00_0000);
        mem.write16(0x0400_00DC, 1);
        mem.write16(0x0400_00DE, 0x8400);
        mem.tick(1);

        assert_eq!(mem.read8(0x0E00_0000), 0x44, "the low byte, as with any other write");
        assert_eq!(mem.read8(0x0E00_0001), 0, "and nothing beside it");
    }

    /// The picture unit raises through the same controller everything else
    /// does, so a game hears it in the ordinary way.
    #[test]
    fn the_bottom_of_the_frame_arrives_at_the_interrupt_controller() {
        let mut mem = Memory::new();
        // Report the bottom of the frame, and listen for it.
        mem.write16(0x0400_0004, 1 << 3);
        mem.write16(0x0400_0200, crate::interrupts::Source::VBlank.bit());
        mem.write16(0x0400_0208, 1);

        assert!(!mem.interrupts().pending());
        mem.tick(crate::ppu::FRAME_CYCLES);
        assert!(mem.interrupts().pending(), "the beam reached the bottom");

        // And the flag it set is retired the way every other one is.
        mem.write16(0x0400_0202, crate::interrupts::Source::VBlank.bit());
        assert!(!mem.interrupts().pending());
    }
}
