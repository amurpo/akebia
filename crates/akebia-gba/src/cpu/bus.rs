//! What the processor is allowed to know about the machine around it.
//!
//! **Dependency inversion**, the same as on the older console: the CPU depends
//! on this trait and not on the concrete memory map, which lets the instruction
//! sets be tested against a flat array with no video, no cartridge and no
//! timing.
//!
//! # Why there are three widths and not one
//!
//! Because the machine can tell them apart. A halfword write to video memory is
//! one access; two byte writes are something the hardware handles differently
//! and in one region refuses outright. Folding them into a single byte-wise
//! path would lose exactly the distinctions the memory map exists to make.
//!
//! # Alignment is the caller's business, not this trait's
//!
//! An implementation here **ignores the low address bits**: `read32` of
//! `0x0300_0002` reads the word at `0x0300_0000`. That is the memory system's
//! real behaviour — it has no way to fetch a word from an odd address and does
//! not try.
//!
//! What the hardware then does on top, for `LDR` and `LDRH` only, is rotate the
//! value it got so the addressed byte comes out at the bottom. That rotation is
//! the *processor's* and belongs with the instruction that asked for it: it
//! applies to some loads and not others, never to stores, and `LDRSH` from an
//! odd address does something different again. Putting it here would apply it
//! to everything, including instruction fetches.

/// The memory the processor sees.
pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn read16(&mut self, addr: u32) -> u16;
    fn read32(&mut self, addr: u32) -> u32;

    fn write8(&mut self, addr: u32, value: u8);
    fn write16(&mut self, addr: u32, value: u16);
    fn write32(&mut self, addr: u32, value: u32);

    /// Moves the machine forward without touching memory.
    ///
    /// The processor calls this for the cycles an instruction spends on its own
    /// work — the extra ones a multiply takes, the internal cycle of a load
    /// that writes back.
    ///
    /// # Why timing is a call and not a return value
    ///
    /// On the older console every access cost the same, so the bus could count
    /// them itself. Here it cannot: what a read costs depends on the region, on
    /// whether the address follows the last one, and on wait states the
    /// cartridge configures at runtime. None of that is decided yet, and the
    /// shape that survives learning it is one where the count comes from the
    /// bus and the processor only says how much work it did *besides* the
    /// accesses.
    fn tick(&mut self, cycles: u32);

    /// The three registers that decide whether the processor is interrupted.
    ///
    /// They live on the bus and not on the processor because that is where the
    /// game reaches them: they are memory-mapped, and something other than the
    /// processor — a timer, the picture unit — is what sets them.
    fn interrupts(&self) -> &crate::interrupts::Interrupts;

    fn interrupts_mut(&mut self) -> &mut crate::interrupts::Interrupts;

    /// The cycles the accesses since the last call cost, and forgets them.
    ///
    /// This is the shape the note above predicted. What a read costs is the
    /// bus's business — the region, whether the address followed the last one,
    /// and a register the game writes — so the bus counts as it goes and the
    /// processor asks once an instruction is finished. A processor that tried
    /// to work it out itself would need to know the memory map, which is the
    /// dependency this trait exists to invert.
    fn owed(&mut self) -> u32;

    /// Moves the clock straight to the next thing that happens.
    ///
    /// For a halted processor, which has nothing to do until something wakes
    /// it and no way of knowing when that is. Charging it a cycle at a time
    /// works and is what it used to do, and it means a game that spends most of
    /// its frame asleep — which is most games, most of the time — costs the
    /// emulator a full pass of the machine for every one of those cycles to
    /// learn that nothing has changed.
    fn idle(&mut self);

    /// Reads without advancing the clock or causing side effects.
    ///
    /// For a debugger and a disassembler only. The emulator must never execute
    /// through it: half the point of the I/O registers is what reading them
    /// does.
    fn peek32(&self, addr: u32) -> u32;
}
