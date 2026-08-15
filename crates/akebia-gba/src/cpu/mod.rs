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
    /// # Timing
    ///
    /// Not modelled. Nothing here advances the bus's clock, because what an
    /// access costs on this machine depends on the region, on whether the
    /// address follows the last one, and on wait states a cartridge configures
    /// at runtime — none of which exists yet. Counting a plausible number in
    /// the meantime would be worse than counting none: it would look like
    /// timing and be wrong, and every later measurement would be taken against
    /// it.
    pub fn step(&mut self, bus: &mut impl Bus) -> Result<(), Fault> {
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
