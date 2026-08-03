//! The port the CPU needs from the rest of the system.
//!
//! **Dependency inversion.** The CPU does not know the concrete [`SystemBus`]:
//! it depends on this trait. That makes it possible to run it against a flat
//! 64 KiB memory in the unit tests, with no PPU or timer in the way.
//!
//! # Bus-driven timing
//!
//! This is the most important design point in the whole emulator. Every memory
//! access by the CPU consumes exactly one M-cycle (4 T-cycles) **and advances
//! the rest of the system at that very instant**. The instruction is not run to
//! completion to then add up its cycles in one go.
//!
//! The difference matters: in `LD (HL),A` with `HL = 0xFF41` the PPU has
//! already advanced 8 T-cycles by the time the write reaches the `STAT`
//! register. An emulator that accumulates cycles at the end writes with the
//! wrong state and fails Blargg's `mem_timing` tests.
//!
//! That is why [`Bus::read`] and [`Bus::write`] take `&mut self`: reading has
//! side effects on the world.
//!
//! [`SystemBus`]: crate::bus::SystemBus

use super::interrupts::InterruptController;

pub trait Bus {
    /// Reads a byte and advances the system one M-cycle.
    fn read(&mut self, addr: u16) -> u8;

    /// Writes a byte and advances the system one M-cycle.
    fn write(&mut self, addr: u16, value: u8);

    /// Advances one M-cycle without touching memory.
    ///
    /// Several instructions spend cycles on internal ALU work: the extra
    /// M-cycle of `JP nn` when writing the PC, the one in `ADD HL,rr`, the one
    /// in `PUSH` before decrementing `SP`. Skipping them leaves the timing
    /// short.
    fn tick(&mut self);

    fn interrupts(&self) -> &InterruptController;

    fn interrupts_mut(&mut self) -> &mut InterruptController;

    /// Reads without advancing the clock or causing side effects.
    ///
    /// For the debugger and the disassembler only; the emulator must never use
    /// it to execute.
    fn peek(&self, addr: u16) -> u8;

    /// Performs the CGB speed switch if it was armed in `KEY1`.
    ///
    /// Returns `true` if it did. This is the answer to `STOP`: on a CGB with
    /// the switch armed, `STOP` does not put the console to sleep but toggles
    /// between 4 and 8 MHz. Unarmed —and always on a DMG— it returns `false`
    /// and `STOP` does its usual thing.
    ///
    /// The default implementation is the DMG one: there is nothing to switch.
    fn perform_speed_switch(&mut self) -> bool {
        false
    }

    /// Reads two bytes in little-endian, the Game Boy's native order.
    /// Consumes two M-cycles.
    fn read16(&mut self, addr: u16) -> u16 {
        let lo = self.read(addr);
        let hi = self.read(addr.wrapping_add(1));
        u16::from_le_bytes([lo, hi])
    }

    /// Writes two bytes in little-endian. Consumes two M-cycles.
    fn write16(&mut self, addr: u16, value: u16) {
        let [lo, hi] = value.to_le_bytes();
        self.write(addr, lo);
        self.write(addr.wrapping_add(1), hi);
    }
}

/// Flat 64 KiB memory with no peripherals, for CPU unit tests.
///
/// It is a *test double* of the real bus: it records how many M-cycles the
/// instruction consumed, which is exactly what needs checking.
#[derive(Clone)]
pub struct FlatBus {
    pub memory: Box<[u8; 0x1_0000]>,
    pub interrupts: InterruptController,
    /// M-cycles consumed since the last `reset_cycles`.
    pub cycles: u32,
}

impl FlatBus {
    pub fn new() -> Self {
        Self { memory: Box::new([0; 0x1_0000]), interrupts: InterruptController::new(), cycles: 0 }
    }

    /// Copies `code` starting at `addr`. Shortcut for setting up a test.
    pub fn load(&mut self, addr: u16, code: &[u8]) -> &mut Self {
        self.memory[addr as usize..addr as usize + code.len()].copy_from_slice(code);
        self
    }

    pub fn reset_cycles(&mut self) {
        self.cycles = 0;
    }
}

impl Default for FlatBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for FlatBus {
    fn read(&mut self, addr: u16) -> u8 {
        self.cycles += 1;
        self.memory[addr as usize]
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.cycles += 1;
        self.memory[addr as usize] = value;
    }

    fn tick(&mut self) {
        self.cycles += 1;
    }

    fn interrupts(&self) -> &InterruptController {
        &self.interrupts
    }

    fn interrupts_mut(&mut self) -> &mut InterruptController {
        &mut self.interrupts
    }

    fn peek(&self, addr: u16) -> u8 {
        self.memory[addr as usize]
    }
}
