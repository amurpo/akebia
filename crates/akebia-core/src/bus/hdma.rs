//! HDMA: the Game Boy Color's VRAM transfer.
//!
//! The DMG only knows how to copy to OAM. The CGB adds an engine that copies to
//! **VRAM**, and that changes what a game can afford: in HBlank mode it copies
//! 16 bytes on each line, taking advantage of the gaps in which the PPU is not
//! using the memory, and thereby spreads a large transfer over a whole frame
//! without stealing time from the game. It is what makes possible the
//! full-screen animated backgrounds a DMG cannot move.
//!
//! | Register | Contents                                               |
//! |----------|--------------------------------------------------------|
//! | `0xFF51` / `0xFF52` | source, aligned to 16 bytes                 |
//! | `0xFF53` / `0xFF54` | destination; always inside VRAM             |
//! | `0xFF55` | starts the transfer and reports what is left           |
//!
//! # The two modes
//!
//! Bit 7 of `0xFF55` chooses:
//!
//! - **General (GDMA)**, bit at 0: copies everything at once and **stalls the
//!   CPU** while it lasts. It is only safe during the vertical blanking.
//! - **HBlank (HDMA)**, bit at 1: copies a 16-byte block at the start of each
//!   HBlank and hands control back to the game between blocks.
//!
//! Writing to `0xFF55` with bit 7 at 0 **while an HBlank transfer is in
//! progress** does not start a new one: it cancels it. That is the only way to
//! abort it.

/// Bytes each block copies.
pub const BLOCK_SIZE: u16 = 0x10;

/// Value `0xFF55` returns when no transfer is active.
const INACTIVE: u8 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Everything at once, with the CPU stalled.
    General,
    /// One block per HBlank.
    HBlank,
}

/// What has to be done on this M-cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing: there is no transfer, or there is one but it is not time to copy
    /// yet.
    Idle,
    /// Copy one block from `source` to `dest`.
    Copy { source: u16, dest: u16 },
}

#[derive(Debug, Clone, Copy)]
pub struct Hdma {
    source: u16,
    dest: u16,
    /// Blocks left to copy. 0 means the transfer is finished.
    remaining: u16,
    mode: Option<Mode>,
    /// Previous level of the HBlank signal, to copy on the edge and not once per
    /// M-cycle for the whole line.
    was_in_hblank: bool,
}

impl Hdma {
    pub const fn new() -> Self {
        Self { source: 0, dest: 0x8000, remaining: 0, mode: None, was_in_hblank: false }
    }

    pub fn is_active(&self) -> bool {
        self.mode.is_some()
    }

    /// Mode of the transfer in progress, if any.
    pub fn mode(&self) -> Option<Mode> {
        self.mode
    }

    pub fn write_register(&mut self, addr: u16, value: u8) {
        match addr {
            // The low 4 bits of the source are ignored: the copy is 16-aligned.
            0xFF51 => self.source = (self.source & 0x00FF) | (u16::from(value) << 8),
            0xFF52 => self.source = (self.source & 0xFF00) | u16::from(value & 0xF0),
            // The destination always lands in VRAM: the high bits are forced.
            0xFF53 => self.dest = 0x8000 | (self.dest & 0x00FF) | (u16::from(value & 0x1F) << 8),
            0xFF54 => self.dest = (self.dest & 0xFF00) | u16::from(value & 0xF0),
            0xFF55 => self.start(value),
            _ => {}
        }
    }

    fn start(&mut self, value: u8) {
        let hblank = value & 0x80 != 0;

        // With an HBlank transfer in progress, writing a 0 into bit 7 does not
        // start anything: it cancels it. The source and destination registers
        // are preserved, so it can be resumed from where it left off.
        if self.mode == Some(Mode::HBlank) && !hblank {
            self.mode = None;
            return;
        }

        self.remaining = u16::from(value & 0x7F) + 1;
        self.mode = Some(if hblank { Mode::HBlank } else { Mode::General });
        self.was_in_hblank = false;
    }

    /// Value of `0xFF55`: bit 7 signals **inactivity**, and bits 0-6, the blocks
    /// remaining minus one.
    pub fn read_status(&self) -> u8 {
        match self.mode {
            None => INACTIVE,
            Some(_) => (self.remaining.saturating_sub(1) & 0x7F) as u8,
        }
    }

    /// Decides what to do on this M-cycle.
    ///
    /// `in_hblank` is the PPU's current state. The HBlank transfer copies one
    /// block on the entry **edge**, not continuously.
    pub fn step(&mut self, in_hblank: bool) -> Step {
        let Some(mode) = self.mode else {
            self.was_in_hblank = in_hblank;
            return Step::Idle;
        };

        let copy_now = match mode {
            Mode::General => true,
            Mode::HBlank => in_hblank && !self.was_in_hblank,
        };
        self.was_in_hblank = in_hblank;

        if !copy_now {
            return Step::Idle;
        }

        let step = Step::Copy { source: self.source, dest: self.dest };
        self.source = self.source.wrapping_add(BLOCK_SIZE);
        // The destination stays inside VRAM even if the transfer is longer than
        // the memory itself.
        self.dest = 0x8000 | (self.dest.wrapping_add(BLOCK_SIZE) & 0x1FFF);

        self.remaining -= 1;
        if self.remaining == 0 {
            self.mode = None;
        }
        step
    }
}

impl Default for Hdma {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Programs a transfer of `blocks` blocks from `src`.
    fn program(hdma: &mut Hdma, src: u16, dest: u16, blocks: u8, hblank: bool) {
        hdma.write_register(0xFF51, (src >> 8) as u8);
        hdma.write_register(0xFF52, src as u8);
        hdma.write_register(0xFF53, (dest >> 8) as u8);
        hdma.write_register(0xFF54, dest as u8);
        hdma.write_register(0xFF55, (blocks - 1) | (u8::from(hblank) << 7));
    }

    #[test]
    fn inactive_returns_every_bit_set() {
        let hdma = Hdma::new();
        assert_eq!(hdma.read_status(), 0xFF);
        assert!(!hdma.is_active());
    }

    #[test]
    fn the_general_mode_copies_on_consecutive_cycles() {
        let mut hdma = Hdma::new();
        program(&mut hdma, 0xC000, 0x8000, 3, false);

        assert_eq!(hdma.step(false), Step::Copy { source: 0xC000, dest: 0x8000 });
        assert_eq!(hdma.step(false), Step::Copy { source: 0xC010, dest: 0x8010 });
        assert_eq!(hdma.step(false), Step::Copy { source: 0xC020, dest: 0x8020 });
        assert_eq!(hdma.step(false), Step::Idle, "three blocks and it is done");
        assert_eq!(hdma.read_status(), 0xFF);
    }

    #[test]
    fn the_hblank_mode_copies_one_block_per_edge() {
        let mut hdma = Hdma::new();
        program(&mut hdma, 0xC000, 0x8000, 4, true);

        assert_eq!(hdma.step(false), Step::Idle, "outside HBlank it does not copy");
        assert_eq!(hdma.step(true), Step::Copy { source: 0xC000, dest: 0x8000 });
        assert_eq!(hdma.step(true), Step::Idle, "a single block per HBlank");
        assert_eq!(hdma.step(true), Step::Idle);

        assert_eq!(hdma.step(false), Step::Idle);
        assert_eq!(hdma.step(true), Step::Copy { source: 0xC010, dest: 0x8010 });
    }

    #[test]
    fn the_low_four_bits_of_the_addresses_are_ignored() {
        let mut hdma = Hdma::new();
        program(&mut hdma, 0xC00F, 0x800F, 1, false);
        assert_eq!(hdma.step(false), Step::Copy { source: 0xC000, dest: 0x8000 });
    }

    #[test]
    fn the_destination_always_lands_in_vram() {
        let mut hdma = Hdma::new();
        // 0x0000 as the destination must become 0x8000.
        program(&mut hdma, 0xC000, 0x0000, 1, false);
        assert_eq!(hdma.step(false), Step::Copy { source: 0xC000, dest: 0x8000 });
    }

    #[test]
    fn writing_with_bit_7_at_zero_cancels_an_hblank_transfer() {
        let mut hdma = Hdma::new();
        program(&mut hdma, 0xC000, 0x8000, 8, true);
        hdma.step(true); // copies one block

        hdma.write_register(0xFF55, 0x00);
        assert!(!hdma.is_active(), "bit 7 at 0 aborts instead of starting");
        assert_eq!(hdma.step(true), Step::Idle);
    }

    #[test]
    fn the_status_counts_the_remaining_blocks() {
        let mut hdma = Hdma::new();
        program(&mut hdma, 0xC000, 0x8000, 4, true);
        assert_eq!(hdma.read_status(), 3, "4 blocks left → it reads 3");
        hdma.step(true);
        assert_eq!(hdma.read_status(), 2);
    }
}
