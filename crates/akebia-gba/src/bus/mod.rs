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
//! The I/O registers, the cartridge and its save memory. Reads of those regions
//! come back zero, and the real answer is not zero — unmapped addresses on this
//! machine return whatever the processor last fetched, which some games read on
//! purpose. That wants a pipeline to ask, and there is not one yet.

use crate::cpu::Bus;

pub const BIOS_LEN: usize = 16 * 1024;
pub const EWRAM_LEN: usize = 256 * 1024;
pub const IWRAM_LEN: usize = 32 * 1024;
pub const PRAM_LEN: usize = 1024;
pub const VRAM_LEN: usize = 96 * 1024;
pub const OAM_LEN: usize = 1024;

/// Where video memory stops being backgrounds and starts being sprites, in
/// tiled modes. In the bitmap modes it is `0x1_4000` instead, and only the
/// picture unit knows which mode is running — hence the setter.
pub const OBJ_BASE_TILED: u32 = 0x1_0000;

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
    /// Palette memory: 512 colours in 15 bits each.
    pram: Box<[u8; PRAM_LEN]>,
    vram: Box<[u8; VRAM_LEN]>,
    /// 128 sprites' worth of attributes.
    oam: Box<[u8; OAM_LEN]>,
    /// Where sprites begin in video memory, which decides whether a byte write
    /// lands or is dropped. See [`Memory::set_obj_base`].
    obj_base: u32,
    cycles: u64,
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
            pram: Box::new([0; PRAM_LEN]),
            vram: Box::new([0; VRAM_LEN]),
            oam: Box::new([0; OAM_LEN]),
            obj_base: OBJ_BASE_TILED,
            cycles: 0,
        }
    }

    /// Puts a BIOS image in. Anything shorter than 16 KiB fills from the start
    /// and leaves the rest zero.
    pub fn load_bios(&mut self, image: &[u8]) {
        let len = image.len().min(BIOS_LEN);
        self.bios[..len].copy_from_slice(&image[..len]);
    }

    /// Tells the memory where sprites begin in video memory, which the picture
    /// unit knows and this does not: `0x1_0000` in the tiled modes and
    /// `0x1_4000` in the bitmap ones.
    ///
    /// It matters for one thing only, and only for writes of a single byte.
    pub fn set_obj_base(&mut self, base: u32) {
        self.obj_base = base;
    }

    pub fn cycles(&self) -> u64 {
        self.cycles
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
            0x02 => Where::Ram(Bank::Ewram, addr as usize & (EWRAM_LEN - 1)),
            0x03 => Where::Ram(Bank::Iwram, addr as usize & (IWRAM_LEN - 1)),
            0x05 => Where::Ram(Bank::Pram, addr as usize & (PRAM_LEN - 1)),
            0x06 => Where::Ram(Bank::Vram, vram_offset(addr)),
            0x07 => Where::Ram(Bank::Oam, addr as usize & (OAM_LEN - 1)),
            _ => Where::Nowhere,
        }
    }

    fn bank(&self, bank: Bank) -> &[u8] {
        match bank {
            Bank::Ewram => &self.ewram[..],
            Bank::Iwram => &self.iwram[..],
            Bank::Pram => &self.pram[..],
            Bank::Vram => &self.vram[..],
            Bank::Oam => &self.oam[..],
        }
    }

    fn bank_mut(&mut self, bank: Bank) -> &mut [u8] {
        match bank {
            Bank::Ewram => &mut self.ewram[..],
            Bank::Iwram => &mut self.iwram[..],
            Bank::Pram => &mut self.pram[..],
            Bank::Vram => &mut self.vram[..],
            Bank::Oam => &mut self.oam[..],
        }
    }

    /// The bytes an access covers, or nothing if the address is not backed by
    /// memory that can be written.
    fn writable(&mut self, addr: u32) -> Option<(Bank, usize)> {
        match self.locate(addr) {
            Where::Ram(bank, offset) => Some((bank, offset)),
            // The BIOS is read-only and unmapped space is not there at all.
            Where::Rom(..) | Where::Nowhere => None,
        }
    }

    fn read_bytes(&self, addr: u32, len: usize) -> u32 {
        let (bytes, offset) = match self.locate(addr) {
            Where::Rom(bytes, offset) => (bytes, offset),
            Where::Ram(bank, offset) => (self.bank(bank), offset),
            Where::Nowhere => return 0,
        };
        let mut value = 0u32;
        for index in 0..len {
            value |= u32::from(bytes[offset + index]) << (index * 8);
        }
        value
    }

    fn write_bytes(&mut self, addr: u32, value: u32, len: usize) {
        let Some((bank, offset)) = self.writable(addr) else {
            return;
        };
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
}

enum Where<'a> {
    /// Readable and not writable.
    Rom(&'a [u8], usize),
    Ram(Bank, usize),
    /// Not backed by anything.
    Nowhere,
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
        let Some((bank, offset)) = self.writable(addr) else {
            return;
        };
        match bank {
            Bank::Ewram | Bank::Iwram => self.bank_mut(bank)[offset] = value,
            Bank::Oam => {}
            Bank::Pram => {
                let pair = offset & !1;
                self.pram[pair] = value;
                self.pram[pair + 1] = value;
            }
            Bank::Vram => {
                if (offset as u32) < self.obj_base {
                    let pair = offset & !1;
                    self.vram[pair] = value;
                    self.vram[pair + 1] = value;
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

    fn tick(&mut self, cycles: u32) {
        self.cycles += u64::from(cycles);
    }

    fn peek32(&self, addr: u32) -> u32 {
        self.read_bytes(addr & !3, 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EWRAM: u32 = 0x0200_0000;
    const IWRAM: u32 = 0x0300_0000;
    const PRAM: u32 = 0x0500_0000;
    const VRAM: u32 = 0x0600_0000;
    const OAM: u32 = 0x0700_0000;

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
        mem.write16(VRAM + OBJ_BASE_TILED, 0xCAFE);
        mem.write8(VRAM + OBJ_BASE_TILED, 0x00);
        assert_eq!(mem.read16(VRAM + OBJ_BASE_TILED), 0xCAFE, "a sprite byte is dropped");
    }

    /// Where sprites begin moves with the video mode, and only the picture unit
    /// knows it. Until it says otherwise the tiled boundary is assumed.
    #[test]
    fn the_boundary_a_byte_write_respects_can_be_moved() {
        let mut mem = Memory::new();
        let bitmap_base = 0x1_4000;

        mem.write16(VRAM + OBJ_BASE_TILED, 0xCAFE);
        mem.write8(VRAM + OBJ_BASE_TILED, 0x55);
        assert_eq!(mem.read16(VRAM + OBJ_BASE_TILED), 0xCAFE, "dropped, at the tiled boundary");

        mem.set_obj_base(bitmap_base);
        mem.write8(VRAM + OBJ_BASE_TILED, 0x55);
        assert_eq!(mem.read16(VRAM + OBJ_BASE_TILED), 0x5555, "and lands once it is background");
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
    #[test]
    fn what_is_not_mapped_yet_reads_as_zero() {
        let mut mem = Memory::new();
        for addr in [0x0400_0000, 0x0800_0000, 0x0E00_0000, 0x1000_0000, BIOS_LEN as u32] {
            assert_eq!(mem.read32(addr), 0, "0x{addr:08X}");
            mem.write32(addr, 0xFFFF_FFFF);
            assert_eq!(mem.read32(addr), 0, "0x{addr:08X} after a write");
        }
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
}
