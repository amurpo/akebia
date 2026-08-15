//! The ARM7TDMI: two instruction sets, seven modes and a three-stage pipeline.

pub mod alu;
pub mod arm;
pub mod bus;
pub mod condition;
pub mod registers;
pub mod shift;

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
    /// An instruction in THUMB state, which is not implemented yet.
    ///
    /// A placeholder with a date on it: it goes when the second instruction set
    /// does, and until then it tells the difference between "this decoded to
    /// nothing" and "this has not been written".
    Thumb { addr: u32 },
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Undefined { addr, instruction } => {
                write!(f, "undefined instruction {instruction:08X} at {addr:08X}")
            }
            Self::Thumb { addr } => write!(f, "THUMB is not implemented yet (at {addr:08X})"),
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
        let addr = self.regs.pc();

        if self.regs.thumb() {
            return Err(Fault::Thumb { addr });
        }

        let instruction = bus.read32(addr);
        self.regs.set_pc(addr.wrapping_add(4));
        arm::execute(&mut self.regs, bus, addr, instruction)
    }
}
