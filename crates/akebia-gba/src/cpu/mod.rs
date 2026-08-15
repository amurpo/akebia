//! The ARM7TDMI: two instruction sets, seven modes and a three-stage pipeline.

pub mod alu;
pub mod arm;
pub mod bus;
pub mod condition;
pub mod registers;
pub mod shift;
pub mod thumb;

pub use bus::Bus;
pub use condition::Condition;
pub use registers::{Mode, Registers};

/// Something the processor cannot carry on from.
///
/// It is not an interrupt or an exception: those are ordinary and the processor
/// handles them itself. This is for the cases where continuing would mean
/// guessing, and where a game reaching one means the emulator is wrong rather
/// than the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// A bit pattern that decodes to no instruction.
    ///
    /// On hardware this raises an exception the BIOS handles. Here it stops,
    /// because reaching one in practice means a branch went somewhere it should
    /// not have and the interesting information is *where*, not what happens
    /// next.
    Undefined { addr: u32, instruction: u32 },
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Undefined { addr, instruction } => {
                write!(f, "undefined instruction {instruction:08X} at {addr:08X}")
            }
        }
    }
}

impl core::error::Error for Fault {}

/// The processor.
#[derive(Clone, Default)]
pub struct Cpu {
    pub regs: Registers,
}

impl Cpu {
    pub fn new() -> Self {
        Self { regs: Registers::new() }
    }

    /// Fetches one instruction and runs it.
    ///
    /// # Why the program counter moves before the instruction runs
    ///
    /// So that not branching needs no decision. `R15` is set to the next
    /// instruction first, and anything that wants to go elsewhere simply writes
    /// over it — which is exactly what the hardware does, and it means a branch
    /// to its own address is not mistaken for an instruction that fell through.
    /// Every read of `R15` as an *operand* is a different number again, eight
    /// or twelve bytes on from the instruction, and is worked out from the
    /// address rather than from the register. See [`arm`].
    ///
    /// # Timing, and why one
    ///
    /// A step charges the bus a single cycle, and that is **not** an estimate
    /// of what an instruction costs. It is the floor: nothing on this processor
    /// takes less than one cycle, so a machine driven this way runs everything
    /// too fast by some factor and never too slow. What an instruction really
    /// costs depends on the region each access lands in, on whether an address
    /// follows the one before it, and on wait states a cartridge configures at
    /// runtime, and none of that is decided here yet.
    ///
    /// The alternative was to keep charging nothing, and that turned out to be
    /// untenable rather than merely incomplete. With a clock that never moves
    /// the picture unit never sweeps, and with no sweep a game waiting for the
    /// beam — which is every game, within a few hundred instructions of
    /// starting — waits for ever. A wrong rate can be corrected against; a
    /// stopped clock cannot be corrected against anything.
    ///
    /// So the shape is the one that survives learning the real numbers: the
    /// count comes from here and only this line changes. Pinned by a test, so
    /// that the test is what has to change with it.
    pub fn step(&mut self, bus: &mut impl Bus) -> Result<(), Fault> {
        // Before anything else, and before the halt below in particular. The
        // machine goes on whether or not the processor does, and a halted
        // processor that stopped the clock could never be woken by the picture
        // unit it halted to wait for.
        bus.tick(1);

        // Asked before the fetch, so the address left in the link register is
        // the instruction that has not run yet rather than the one that just
        // did. The processor's own mask is checked here and not in the
        // controller, because it is the processor's and nothing else may read
        // it.
        if bus.interrupts().pending() && !self.regs.irq_disabled() {
            arm::enter_irq(&mut self.regs);
            return Ok(());
        }

        // Stopped until something arrives. Nothing is fetched and nothing
        // advances, which is the whole point: a game halts to save the battery
        // and to leave the picture unit the bus to itself.
        if bus.interrupts().halted() {
            return Ok(());
        }

        let addr = self.regs.pc();

        if self.regs.thumb() {
            let instruction = bus.read16(addr);
            self.regs.set_pc(addr.wrapping_add(2));
            return thumb::execute(&mut self.regs, bus, addr, instruction);
        }

        let instruction = bus.read32(addr);
        self.regs.set_pc(addr.wrapping_add(4));
        arm::execute(&mut self.regs, bus, addr, instruction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Memory;
    use crate::interrupts::Source;

    const BASE: u32 = 0x0300_0000;
    /// `MOV r0, #1`, as something harmless to fill a program with.
    const NOTHING_MUCH: u32 = 0xE3A0_0001;

    fn machine() -> (Cpu, Memory) {
        let mut mem = Memory::new();
        for index in 0..8 {
            mem.write32(BASE + index * 4, NOTHING_MUCH);
        }
        let mut cpu = Cpu::new();
        cpu.regs.set_mode(Mode::System);
        cpu.regs.set_pc(BASE);
        (cpu, mem)
    }

    /// The three registers reach the controller through the memory map, which
    /// is the only way a game can touch them.
    #[test]
    fn the_three_registers_are_reachable_through_the_memory_map() {
        let (_, mut mem) = machine();

        mem.write16(0x0400_0200, 0x0001);
        assert_eq!(mem.interrupts().enabled(), 0x0001);
        assert_eq!(mem.read16(0x0400_0200), 0x0001, "and read back");

        mem.write16(0x0400_0208, 0x0001);
        assert!(mem.interrupts().master());
        assert_eq!(mem.read16(0x0400_0208), 0x0001);

        mem.write16(0x0400_0208, 0x0000);
        assert!(!mem.interrupts().master());
    }

    /// `IE` and `IF` sit next to each other and a game reads both in one go.
    #[test]
    fn the_two_neighbouring_registers_can_be_read_as_one_word() {
        let (_, mut mem) = machine();
        mem.write16(0x0400_0200, 0x0005);
        mem.interrupts_mut().raise(Source::VBlank);

        let both = mem.read32(0x0400_0200);
        assert_eq!(both & 0xFFFF, 0x0005, "the low half is what is enabled");
        assert_eq!(both >> 16, u32::from(Source::VBlank.bit()), "and the high half what waits");
    }

    /// Writing ones retires requests and writing zeros leaves them, through the
    /// memory map as much as through the controller.
    #[test]
    fn a_request_is_retired_through_the_map_by_writing_a_one_over_it() {
        let (_, mut mem) = machine();
        mem.interrupts_mut().raise(Source::VBlank);
        mem.interrupts_mut().raise(Source::Timer0);

        mem.write16(0x0400_0202, Source::VBlank.bit());
        assert_eq!(mem.interrupts().requested(), Source::Timer0.bit());

        mem.write16(0x0400_0202, 0);
        assert_eq!(mem.interrupts().requested(), Source::Timer0.bit(), "a zero retires nothing");
    }

    /// A byte written to the low half of the acknowledging register must not
    /// clear the high half, which composing it as an ordinary register would.
    #[test]
    fn retiring_one_byte_of_requests_leaves_the_other_byte_alone() {
        let (_, mut mem) = machine();
        mem.interrupts_mut().raise(Source::VBlank);
        mem.interrupts_mut().raise(Source::Dma0);

        mem.write8(0x0400_0202, Source::VBlank.bit() as u8);
        assert_eq!(mem.interrupts().requested(), Source::Dma0.bit(), "the high byte survived");
    }

    /// The whole point: an interrupt takes the processor to the vector, in the
    /// mode with the rights, with the way back saved.
    #[test]
    fn an_interrupt_takes_the_processor_to_the_vector() {
        let (mut cpu, mut mem) = machine();
        cpu.regs.set_mode(Mode::User);
        cpu.regs.set_cpsr(cpu.regs.cpsr() & !registers::I);
        cpu.regs.set(13, 0x0300_7F00);
        let interrupted = cpu.regs.cpsr();

        mem.write16(0x0400_0200, Source::VBlank.bit());
        mem.write16(0x0400_0208, 1);
        mem.interrupts_mut().raise(Source::VBlank);

        cpu.step(&mut mem).unwrap();

        assert_eq!(cpu.regs.pc(), 0x18, "the interrupt vector");
        assert_eq!(cpu.regs.mode(), Mode::Irq);
        assert_eq!(cpu.regs.spsr(), interrupted, "with the interrupted status saved");
        assert!(cpu.regs.irq_disabled(), "and masked so the handler is not interrupted");
        assert!(!cpu.regs.thumb(), "in ARM state whatever was running");
    }

    /// The way back is four further on than the instruction that has not run,
    /// because a handler returns with `SUBS pc, lr, #4`.
    #[test]
    fn the_way_back_suits_the_instruction_a_handler_returns_with() {
        let (mut cpu, mut mem) = machine();
        cpu.regs.set_cpsr(cpu.regs.cpsr() & !registers::I);
        mem.write16(0x0400_0200, Source::VBlank.bit());
        mem.write16(0x0400_0208, 1);

        // One instruction runs, then the interrupt arrives.
        cpu.step(&mut mem).unwrap();
        mem.interrupts_mut().raise(Source::VBlank);
        cpu.step(&mut mem).unwrap();

        assert_eq!(cpu.regs.lr(), BASE + 4 + 4, "the untaken instruction, plus four");

        // And `SUBS pc, lr, #4` lands back on it.
        mem.write32(0x0300_0100, 0xE25E_F004);
        cpu.regs.set_pc(0x0300_0100);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 4, "exactly where it was interrupted");
    }

    /// The processor's own mask is the third switch and it belongs to the
    /// processor: with it set, nothing gets through however the registers read.
    #[test]
    fn the_processors_own_mask_refuses_an_interrupt_the_registers_allow() {
        let (mut cpu, mut mem) = machine();
        mem.write16(0x0400_0200, Source::VBlank.bit());
        mem.write16(0x0400_0208, 1);
        mem.interrupts_mut().raise(Source::VBlank);
        assert!(mem.interrupts().pending(), "the registers say yes");

        // A reset leaves the processor's mask set.
        assert!(cpu.regs.irq_disabled());
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 4, "and it carried on regardless");
        assert_eq!(cpu.regs.mode(), Mode::System);
    }

    /// A halted processor fetches nothing and goes nowhere.
    #[test]
    fn a_halted_processor_does_not_move() {
        let (mut cpu, mut mem) = machine();
        mem.write8(0x0400_0301, 0);
        assert!(mem.interrupts().halted());

        for _ in 0..10 {
            cpu.step(&mut mem).unwrap();
        }
        assert_eq!(cpu.regs.pc(), BASE, "it never fetched anything");
        assert_eq!(cpu.regs.get(0), 0, "so nothing ran");
    }

    /// And it comes back when an enabled source arrives, carrying straight on
    /// if nothing is listening for the interrupt itself.
    #[test]
    fn a_halted_processor_wakes_when_something_it_enabled_arrives() {
        let (mut cpu, mut mem) = machine();
        mem.write16(0x0400_0200, Source::VBlank.bit());
        mem.write8(0x0400_0301, 0);

        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE, "asleep");

        mem.interrupts_mut().raise(Source::VBlank);
        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 4, "awake, and running the next instruction");
    }

    /// One cycle a step is the floor and not a measurement: nothing costs less,
    /// and almost everything costs more. Pinned here so that when the real
    /// numbers arrive this is the test that has to change with them.
    #[test]
    fn a_step_charges_the_bus_one_cycle_whatever_the_instruction() {
        let (mut cpu, mut mem) = machine();
        assert_eq!(mem.cycles(), 0);

        for step in 1..=8 {
            cpu.step(&mut mem).unwrap();
            assert_eq!(mem.cycles(), step, "after {step} instructions");
        }
    }

    /// And a halted processor is charged too. It has to be: the thing it is
    /// waiting for is driven by the clock, so a halt that stopped the clock
    /// would be a halt nothing could ever end.
    #[test]
    fn the_clock_moves_while_the_processor_is_halted() {
        let (mut cpu, mut mem) = machine();
        mem.write8(0x0400_0301, 0);
        assert!(mem.interrupts().halted());

        for _ in 0..10 {
            cpu.step(&mut mem).unwrap();
        }
        assert_eq!(cpu.regs.pc(), BASE, "the processor went nowhere");
        assert_eq!(mem.cycles(), 10, "and the machine went on regardless");
    }

    /// The whole reason the clock had to start moving: a game asks to be told
    /// when the beam reaches the bottom, halts, and is woken by the picture
    /// unit. Every step of that crosses a boundary in this crate.
    #[test]
    fn halting_until_the_beam_reaches_the_bottom_ends_by_itself() {
        let (mut cpu, mut mem) = machine();
        cpu.regs.set_cpsr(cpu.regs.cpsr() & !registers::I);

        // Ask the picture unit to report the bottom of the frame, enable that
        // one interrupt, and stop.
        mem.write16(0x0400_0004, 1 << 3);
        mem.write16(0x0400_0200, Source::VBlank.bit());
        mem.write16(0x0400_0208, 1);
        mem.write8(0x0400_0301, 0);

        // Long enough for the beam to reach line 160 at a cycle a step, and no
        // longer, so that a machine which never woke fails here.
        for _ in 0..crate::ppu::FRAME_CYCLES {
            cpu.step(&mut mem).unwrap();
            if !mem.interrupts().halted() {
                break;
            }
        }

        assert!(!mem.interrupts().halted(), "the beam woke it");
        assert_eq!(mem.ppu().vcount(), 160, "at the line below the screen");
        assert_eq!(cpu.regs.pc(), 0x18, "and it went to the handler");
    }

    /// Waking and being interrupted are different things, and a game that halts
    /// with the master switch off depends on the difference.
    #[test]
    fn waking_from_a_halt_is_not_the_same_as_being_interrupted() {
        let (mut cpu, mut mem) = machine();
        cpu.regs.set_cpsr(cpu.regs.cpsr() & !registers::I);
        mem.write16(0x0400_0200, Source::VBlank.bit());
        // The master switch stays off, which is the point.
        mem.write8(0x0400_0301, 0);
        mem.interrupts_mut().raise(Source::VBlank);

        cpu.step(&mut mem).unwrap();
        assert_eq!(cpu.regs.pc(), BASE + 4, "it woke and carried on");
        assert_eq!(cpu.regs.mode(), Mode::System, "rather than going to a handler");
    }
}
