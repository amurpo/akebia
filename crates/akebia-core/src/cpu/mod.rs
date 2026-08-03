//! The Sharp SM83 CPU.
//!
//! It is neither a Z80 nor an Intel 8080: it is a hybrid with the Z80's load
//! block, its own bit instruction set and several quirks of its own (`LDH`,
//! `LD (HL+),A`, a peculiar `DAA`, and no alternate registers or IX/IY modes).
//!
//! # Execution cycle
//!
//! [`Cpu::step`] executes **one** complete instruction and returns the M-cycles
//! consumed. It does not accumulate cycles to add them up at the end: every bus
//! access already advances the system (see [`bus`]). The returned value is
//! informational, for the frame scheduler.
//!
//! # State not visible in the registers
//!
//! Three pieces of internal state that are not in [`Registers`] and that are the
//! source of most of the subtle bugs:
//!
//! - `ime`: global interrupt enable. It is not an addressable register; only
//!   `EI`, `DI` and `RETI` touch it.
//! - `ime_pending`: `EI` does not take effect until *after* the following
//!   instruction. The sequence `EI; DI` never enables anything.
//! - `halt_bug`: if `HALT` runs with `IME=0` and an interrupt pending, the CPU
//!   does not stop and the next byte is read twice because the `PC` does not
//!   advance. Several commercial games depend on it.

pub mod bus;
pub mod interrupts;
pub mod registers;

pub use bus::{Bus, FlatBus};
pub use interrupts::{Interrupt, InterruptController};
pub use registers::{Flags, Registers, R16, R8};

/// Reason why the CPU stopped before completing an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Opcode not implemented in this emulator yet.
    Unimplemented { opcode: u8, prefixed: bool, pc: u16 },
    /// One of the eleven opcodes that do not exist on the SM83. On real
    /// hardware they hang the CPU unrecoverably.
    Illegal { opcode: u8, pc: u16 },
}

/// Low-power mode the CPU can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Running,
    /// `HALT`: resumes on any pending and enabled interrupt.
    Halted,
    /// `STOP`: also shuts down the system clock. On CGB it additionally toggles
    /// double speed; on DMG only the joypad wakes it up.
    Stopped,
}

pub struct Cpu {
    pub regs: Registers,
    pub power: Power,
    /// Interrupt Master Enable.
    ime: bool,
    /// `EI` armed the enable; it applies when the next instruction finishes.
    ime_pending: bool,
    /// The next fetch must repeat the byte without advancing the `PC`.
    halt_bug: bool,
}

impl Cpu {
    /// CPU in the state the DMG BootROM leaves the machine in.
    pub fn new_dmg() -> Self {
        Self::with_registers(Registers::post_boot_dmg())
    }

    pub fn new_cgb() -> Self {
        Self::with_registers(Registers::post_boot_cgb())
    }

    pub fn with_registers(regs: Registers) -> Self {
        Self { regs, power: Power::Running, ime: false, ime_pending: false, halt_bug: false }
    }

    pub fn ime(&self) -> bool {
        self.ime
    }

    /// Executes one instruction, or services an interrupt, or burns a cycle in
    /// `HALT`. Returns the M-cycles elapsed.
    pub fn step(&mut self, bus: &mut impl Bus) -> Result<u32, Fault> {
        // 1. A pending interrupt pulls the CPU out of HALT even with IME at 0.
        //    That is the basis of the "HALT + polling" idiom without
        //    interrupts.
        //
        //    `STOP` is treated the same way as a simplification: on hardware
        //    only the joypad wakes it, without going through `IE`. Since
        //    everything here does respect `IE`, a game using `STOP` with the
        //    joypad disabled in `IE` would stay asleep on a real console but
        //    not here. Nobody does that, but it is worth knowing.
        if self.power != Power::Running && bus.interrupts().any_pending() {
            self.power = Power::Running;
        }

        // 2. Interrupt dispatch happens between instructions, never in the
        //    middle of one.
        if self.ime
            && let Some(int) = bus.interrupts().pending()
        {
            return Ok(self.service_interrupt(bus, int));
        }

        // 3. With nothing to do, HALT and STOP burn a cycle.
        if self.power != Power::Running {
            bus.tick();
            return Ok(1);
        }

        // 4. `EI` takes effect here: after the dispatch in step 2 —so that the
        //    instruction following `EI` runs in full without being interrupted—
        //    but before executing that instruction, so an immediate `DI` can
        //    turn it back off. That is why `EI; DI` never enables anything.
        if core::mem::take(&mut self.ime_pending) {
            self.ime = true;
        }

        let opcode = self.fetch8(bus);
        self.execute(bus, opcode)
    }

    /// Interrupt service sequence: 5 M-cycles.
    ///
    /// Two of internal work, two pushing the `PC` onto the stack and one
    /// loading the vector. `IME` is set to 0 and the `IF` bit is cleared.
    fn service_interrupt(&mut self, bus: &mut impl Bus, int: Interrupt) -> u32 {
        self.ime = false;
        self.power = Power::Running;

        bus.tick();
        bus.tick();

        let [hi, lo] = self.regs.pc.to_be_bytes();
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, hi);

        // The IF bit is cleared here, between the two halves of the push. If
        // the high byte of the PC landed on IE and modified it, the vector
        // could change or be cancelled; that is the "IRQ cancellation bug".
        bus.interrupts_mut().acknowledge(int);

        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, lo);

        self.regs.pc = int.vector();
        bus.tick();
        5
    }

    // ---- Fetch -------------------------------------------------------------

    /// Reads the byte pointed at by `PC` and advances it, unless `halt_bug` is
    /// active.
    fn fetch8(&mut self, bus: &mut impl Bus) -> u8 {
        let byte = bus.read(self.regs.pc);
        if core::mem::take(&mut self.halt_bug) {
            // The PC does not advance: the same byte will be read again.
        } else {
            self.regs.pc = self.regs.pc.wrapping_add(1);
        }
        byte
    }

    fn fetch16(&mut self, bus: &mut impl Bus) -> u16 {
        let lo = self.fetch8(bus);
        let hi = self.fetch8(bus);
        u16::from_le_bytes([lo, hi])
    }

    // ---- Stack -------------------------------------------------------------

    fn push16(&mut self, bus: &mut impl Bus, value: u16) {
        let [hi, lo] = value.to_be_bytes();
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, hi);
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, lo);
    }

    fn pop16(&mut self, bus: &mut impl Bus) -> u16 {
        let lo = bus.read(self.regs.sp);
        self.regs.sp = self.regs.sp.wrapping_add(1);
        let hi = bus.read(self.regs.sp);
        self.regs.sp = self.regs.sp.wrapping_add(1);
        u16::from_le_bytes([lo, hi])
    }
}

mod alu;
mod bitops;
mod execute;
mod prefix_cb;

#[cfg(test)]
mod tests;
