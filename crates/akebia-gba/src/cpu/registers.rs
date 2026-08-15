//! The sixteen registers a program can name, and the several more it cannot.
//!
//! # Why there are more registers than names
//!
//! An exception on this processor has to run without first finding somewhere to
//! put what it interrupted. So it does not save anything: entering a mode swaps
//! some of the names onto a different set of registers, and the handler writes
//! to `R13` and `R14` that belong to it alone. Coming back swaps them out again
//! and the interrupted program finds its own untouched. The cost is that "the
//! stack pointer" is not one register but six, and which one a program means
//! depends on the mode it is in when it asks.
//!
//! `R0`-`R7` and `R15` are the same registers in every mode. `R13` and `R14`
//! are banked six ways. `R8`-`R12` are banked twice, and only for FIQ, which
//! exists to be fast and is given five scratch registers it need not save.
//!
//! What is kept here is the *visible* sixteen in [`Registers::r`], with the
//! parked copies alongside. A mode switch moves values between the two, so
//! reading a register is an array index and not a decision — which matters,
//! because every instruction does it two or three times and mode switches are
//! rare.
//!
//! # Why the pipeline offset is not applied here
//!
//! Reading `R15` gives an address ahead of the instruction doing the reading:
//! the processor has already fetched further on. How far ahead is not one
//! number — eight bytes in ARM state, four in THUMB, and twelve in the one ARM
//! case where a shift amount comes from a register — so applying it inside
//! [`Registers::get`] would make the third case impossible to say. What is
//! stored is the address of the instruction being executed, and the offset is
//! the executor's business, where the context to choose it exists.

/// What the processor is doing, as the low five bits of `CPSR` encode it.
///
/// The numbers are the architecture's and not a choice: they are written into
/// `CPSR` by an exception and read back out of it by `MSR`, so a program can
/// see them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// What a game runs in. The only mode that cannot leave itself: writing to
    /// `CPSR` from here changes the flags and nothing else.
    User = 0b1_0000,
    /// Fast interrupt. Unused on this machine — nothing is wired to it — but a
    /// program can still enter it, and one that does must find it there.
    Fiq = 0b1_0001,
    /// Interrupt. Where the machine spends every VBlank.
    Irq = 0b1_0010,
    /// What `SWI` enters, which is how every BIOS call arrives.
    Supervisor = 0b1_0011,
    /// A memory access that failed. Nothing on this machine raises it.
    Abort = 0b1_0111,
    /// An instruction that decodes to nothing.
    Undefined = 0b1_1011,
    /// User's registers with a supervisor's rights. An exception handler drops
    /// into it to reach the interrupted program's `R13` and `R14`.
    System = 0b1_1111,
}

impl Mode {
    /// The mode those five bits name, or nothing if they name none.
    ///
    /// Twenty-five of the thirty-two encodings are unused, and what the
    /// hardware does when one is written is not documented. Refusing them here
    /// leaves the choice to the caller, which is the only place that knows
    /// whether it is reading a `CPSR` a program wrote or restoring one the
    /// processor saved.
    pub const fn from_bits(bits: u32) -> Option<Self> {
        match bits & 0x1F {
            0b1_0000 => Some(Self::User),
            0b1_0001 => Some(Self::Fiq),
            0b1_0010 => Some(Self::Irq),
            0b1_0011 => Some(Self::Supervisor),
            0b1_0111 => Some(Self::Abort),
            0b1_1011 => Some(Self::Undefined),
            0b1_1111 => Some(Self::System),
            _ => None,
        }
    }

    pub const fn bits(self) -> u32 {
        self as u32
    }

    /// Which set of `R13` and `R14` this mode uses.
    ///
    /// User and System share one: that is the whole point of System, which
    /// exists so a handler can reach the registers of the program it
    /// interrupted.
    const fn bank(self) -> usize {
        match self {
            Self::User | Self::System => 0,
            Self::Fiq => 1,
            Self::Irq => 2,
            Self::Supervisor => 3,
            Self::Abort => 4,
            Self::Undefined => 5,
        }
    }

    /// Whether this mode has somewhere to save a `CPSR`.
    ///
    /// User and System do not, because nothing enters them by exception and so
    /// there is never a `CPSR` of somebody else's to keep. Reading or writing
    /// `SPSR` from either is unpredictable on hardware; here it is refused.
    pub const fn has_spsr(self) -> bool {
        !matches!(self, Self::User | Self::System)
    }

    /// Whether a program in this mode may change the mode, the interrupt masks
    /// and the THUMB bit, or only the flags.
    pub const fn is_privileged(self) -> bool {
        !matches!(self, Self::User)
    }
}

/// `CPSR` bit positions, by the names the architecture gives them.
pub const N: u32 = 1 << 31;
pub const Z: u32 = 1 << 30;
pub const C: u32 = 1 << 29;
pub const V: u32 = 1 << 28;
/// IRQ disable. Set means masked, which reads backwards and is the
/// architecture's doing.
pub const I: u32 = 1 << 7;
/// FIQ disable, likewise.
pub const F: u32 = 1 << 6;
/// THUMB state. The one `CPSR` bit that changes what an instruction *is*.
pub const T: u32 = 1 << 5;

/// The part of `CPSR` a program in any mode may write: the four condition
/// flags. Everything else is the processor's, and User mode is refused it.
const FLAGS: u32 = N | Z | C | V;

/// Bit 4 reads as one on this processor and cannot be cleared. It is part of
/// every valid mode encoding, and keeping it here means a `CPSR` assembled from
/// pieces still names a mode.
const ALWAYS_SET: u32 = 1 << 4;

/// The register file, with the mode's bank already swapped in.
#[derive(Clone)]
pub struct Registers {
    /// The sixteen a program can name right now.
    r: [u32; 16],
    /// `R8`-`R12` as everything except FIQ left them, parked while FIQ has the
    /// visible ones. When the mode is not FIQ this is a stale copy and `r` is
    /// the truth.
    r8_r12: [u32; 5],
    /// `R8`-`R12` as FIQ left them. Stale whenever the mode *is* FIQ.
    r8_r12_fiq: [u32; 5],
    /// `R13` and `R14` of the five banks that are not the current one. The
    /// current bank's entry is stale; `r` holds it.
    sp_lr: [[u32; 2]; 6],
    /// Where each exception mode keeps the `CPSR` it interrupted. Indexed by
    /// the same bank number, of which entry 0 — User and System — is never
    /// read.
    spsr: [u32; 6],
    cpsr: u32,
    /// The mode `cpsr` names, kept beside it so that reading it is not a
    /// decode. The two are only ever written together.
    mode: Mode,
}

impl Default for Registers {
    fn default() -> Self {
        Self::new()
    }
}

impl Registers {
    /// The state a reset leaves: Supervisor, ARM, both interrupts masked, and
    /// every register zero.
    ///
    /// It is the architecture's reset and not this machine's boot. A GBA comes
    /// up here and then runs its BIOS, which hands the game something rather
    /// different; that belongs to whatever stands in for the BIOS, not to the
    /// register file.
    pub const fn new() -> Self {
        Self {
            r: [0; 16],
            r8_r12: [0; 5],
            r8_r12_fiq: [0; 5],
            sp_lr: [[0; 2]; 6],
            spsr: [0; 6],
            cpsr: Mode::Supervisor.bits() | I | F,
            mode: Mode::Supervisor,
        }
    }

    /// One of the sixteen the current mode can see.
    ///
    /// `R15` comes back as the address of the instruction being executed. See
    /// the note at the top of this module about the offset that is not added.
    pub fn get(&self, index: usize) -> u32 {
        self.r[index]
    }

    pub fn set(&mut self, index: usize, value: u32) {
        self.r[index] = value;
    }

    pub fn pc(&self) -> u32 {
        self.r[15]
    }

    pub fn set_pc(&mut self, value: u32) {
        self.r[15] = value;
    }

    /// `R13`, the stack pointer of whichever mode is current.
    pub fn sp(&self) -> u32 {
        self.r[13]
    }

    /// `R14`, where a `BL` or an exception left the way back.
    pub fn lr(&self) -> u32 {
        self.r[14]
    }

    pub fn cpsr(&self) -> u32 {
        self.cpsr
    }

    /// Writes `CPSR` whole, swapping banks if the mode changed.
    ///
    /// This is the privileged path: the mode, the masks and the THUMB bit all
    /// land. Five bits that name no mode are refused and the mode is left as it
    /// was, the rest of the write going through — the hardware's behaviour here
    /// is undocumented, and the one thing worse than guessing it is banking
    /// onto a register set that does not exist.
    pub fn set_cpsr(&mut self, value: u32) {
        let wanted = Mode::from_bits(value);
        self.cpsr = (value & !0x1F) | ALWAYS_SET | wanted.unwrap_or(self.mode).bits();
        if let Some(mode) = wanted {
            self.switch_to(mode);
        }
    }

    /// Writes `CPSR` the way a program in the given mode may.
    ///
    /// User mode gets the flags and nothing else: an application cannot promote
    /// itself by writing to `CPSR`, which is the whole reason there is a User
    /// mode. Every other mode may write the lot.
    pub fn set_cpsr_as(&mut self, value: u32, mode: Mode) {
        if mode.is_privileged() {
            self.set_cpsr(value);
        } else {
            self.cpsr = (self.cpsr & !FLAGS) | (value & FLAGS);
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Moves to another mode, parking the registers the old one owned and
    /// bringing in the new one's.
    ///
    /// Going to the mode already current does nothing at all. That is not an
    /// optimisation: swapping a bank with itself would write the visible
    /// registers out and read the same values back, which is harmless, but the
    /// FIQ pair would then be stale in *both* copies, and the next real switch
    /// would restore whichever was written last.
    pub fn set_mode(&mut self, mode: Mode) {
        self.switch_to(mode);
        self.cpsr = (self.cpsr & !0x1F) | ALWAYS_SET | mode.bits();
    }

    fn switch_to(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }

        // `R8`-`R12` move only when FIQ is on one side of the switch or the
        // other. Between IRQ and Supervisor, say, they are the same five
        // registers and touching them would be wrong.
        let was_fiq = self.mode == Mode::Fiq;
        let now_fiq = mode == Mode::Fiq;
        if was_fiq != now_fiq {
            let visible = [self.r[8], self.r[9], self.r[10], self.r[11], self.r[12]];
            let incoming = if now_fiq {
                self.r8_r12 = visible;
                self.r8_r12_fiq
            } else {
                self.r8_r12_fiq = visible;
                self.r8_r12
            };
            self.r[8..13].copy_from_slice(&incoming);
        }

        let from = self.mode.bank();
        let to = mode.bank();
        if from != to {
            self.sp_lr[from] = [self.r[13], self.r[14]];
            self.r[13] = self.sp_lr[to][0];
            self.r[14] = self.sp_lr[to][1];
        }

        self.mode = mode;
    }

    /// The saved `CPSR` of the current mode, or the live one when there is no
    /// bank to read — which is what the hardware gives back in User and System.
    pub fn spsr(&self) -> u32 {
        if self.mode.has_spsr() {
            self.spsr[self.mode.bank()]
        } else {
            self.cpsr
        }
    }

    /// Writes the current mode's saved `CPSR`. Silently dropped in User and
    /// System, which have nowhere to put it.
    pub fn set_spsr(&mut self, value: u32) {
        if self.mode.has_spsr() {
            self.spsr[self.mode.bank()] = value;
        }
    }

    /// Restores `CPSR` from the current mode's `SPSR`, which is how an
    /// exception returns. In a mode with no `SPSR` it does nothing, there being
    /// nothing to come back from.
    pub fn restore_cpsr(&mut self) {
        if self.mode.has_spsr() {
            self.set_cpsr(self.spsr[self.mode.bank()]);
        }
    }

    pub fn thumb(&self) -> bool {
        self.cpsr & T != 0
    }

    pub fn set_thumb(&mut self, thumb: bool) {
        self.set_flag(T, thumb);
    }

    /// Whether IRQs are masked. The bit is named for disabling, so this reads
    /// the way the bit is set and not the way one would say it.
    pub fn irq_disabled(&self) -> bool {
        self.cpsr & I != 0
    }

    pub fn fiq_disabled(&self) -> bool {
        self.cpsr & F != 0
    }

    pub fn n(&self) -> bool {
        self.cpsr & N != 0
    }

    pub fn z(&self) -> bool {
        self.cpsr & Z != 0
    }

    pub fn c(&self) -> bool {
        self.cpsr & C != 0
    }

    pub fn v(&self) -> bool {
        self.cpsr & V != 0
    }

    pub fn set_n(&mut self, on: bool) {
        self.set_flag(N, on);
    }

    pub fn set_z(&mut self, on: bool) {
        self.set_flag(Z, on);
    }

    pub fn set_c(&mut self, on: bool) {
        self.set_flag(C, on);
    }

    pub fn set_v(&mut self, on: bool) {
        self.set_flag(V, on);
    }

    /// Sets `N` and `Z` from a result, which almost every instruction that
    /// writes flags does before deciding what to do about `C` and `V`.
    pub fn set_nz(&mut self, result: u32) {
        self.set_n(result & 0x8000_0000 != 0);
        self.set_z(result == 0);
    }

    fn set_flag(&mut self, bit: u32, on: bool) {
        if on {
            self.cpsr |= bit;
        } else {
            self.cpsr &= !bit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mode encoding the architecture defines survives the round trip,
    /// and the twenty-five it does not define are refused rather than guessed.
    #[test]
    fn the_five_bits_name_seven_modes_and_no_others() {
        const MODES: [Mode; 7] = [
            Mode::User,
            Mode::Fiq,
            Mode::Irq,
            Mode::Supervisor,
            Mode::Abort,
            Mode::Undefined,
            Mode::System,
        ];
        for mode in MODES {
            assert_eq!(Mode::from_bits(mode.bits()), Some(mode), "{mode:?}");
        }
        let named = MODES.map(Mode::bits);
        for bits in 0..32u32 {
            if !named.contains(&bits) {
                assert_eq!(Mode::from_bits(bits), None, "0b{bits:05b} names no mode");
            }
        }
    }

    /// A handler writing to its own stack pointer must not be writing to the
    /// stack pointer of the program it interrupted. That is what the banking is
    /// for, and it is the one thing here that cannot be got wrong quietly.
    #[test]
    fn each_mode_keeps_its_own_stack_pointer_and_link_register() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::User);
        regs.set(13, 0x0300_7F00);
        regs.set(14, 0xDEAD_BEEF);

        regs.set_mode(Mode::Irq);
        assert_ne!(regs.sp(), 0x0300_7F00, "IRQ must not see the user's stack");
        regs.set(13, 0x0300_7FA0);
        regs.set(14, 0x0800_0000);

        regs.set_mode(Mode::Supervisor);
        assert_ne!(regs.sp(), 0x0300_7FA0, "nor Supervisor the IRQ's");
        regs.set(13, 0x0300_7FE0);

        regs.set_mode(Mode::User);
        assert_eq!(regs.sp(), 0x0300_7F00, "and the user finds it as it was left");
        assert_eq!(regs.lr(), 0xDEAD_BEEF);

        regs.set_mode(Mode::Irq);
        assert_eq!(regs.sp(), 0x0300_7FA0);
        assert_eq!(regs.lr(), 0x0800_0000);
    }

    /// System exists so a handler can reach the interrupted program's registers,
    /// so it is the one pair of modes that must *not* be kept apart.
    #[test]
    fn user_and_system_share_the_one_bank() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::User);
        regs.set(13, 0x0300_7F00);
        regs.set(14, 0x0800_1234);

        regs.set_mode(Mode::System);
        assert_eq!(regs.sp(), 0x0300_7F00);
        assert_eq!(regs.lr(), 0x0800_1234);

        regs.set(13, 0x0300_7E00);
        regs.set_mode(Mode::User);
        assert_eq!(regs.sp(), 0x0300_7E00, "and a write from either is seen by both");
    }

    /// FIQ gets five scratch registers of its own; every other mode shares the
    /// one set. Between two modes that are both not FIQ they must not move.
    #[test]
    fn only_fiq_banks_the_five_scratch_registers() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::User);
        for index in 8..13 {
            regs.set(index, index as u32);
        }

        regs.set_mode(Mode::Irq);
        for index in 8..13 {
            assert_eq!(regs.get(index), index as u32, "R{index} is not banked for IRQ");
        }

        regs.set_mode(Mode::Fiq);
        for index in 8..13 {
            assert_eq!(regs.get(index), 0, "R{index} is banked for FIQ");
            regs.set(index, 0xF0 + index as u32);
        }

        regs.set_mode(Mode::User);
        for index in 8..13 {
            assert_eq!(regs.get(index), index as u32, "and the user's R{index} came back");
        }

        regs.set_mode(Mode::Fiq);
        for index in 8..13 {
            assert_eq!(regs.get(index), 0xF0 + index as u32, "as did FIQ's");
        }
    }

    /// R0-R7 and R15 are the same registers everywhere. Nothing swaps them.
    #[test]
    fn the_low_registers_and_the_program_counter_are_shared_by_every_mode() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::User);
        for index in 0..8 {
            regs.set(index, 0xA0 + index as u32);
        }
        regs.set_pc(0x0800_0000);

        for mode in [Mode::Fiq, Mode::Irq, Mode::Supervisor, Mode::Abort, Mode::Undefined] {
            regs.set_mode(mode);
            for index in 0..8 {
                assert_eq!(regs.get(index), 0xA0 + index as u32, "R{index} in {mode:?}");
            }
            assert_eq!(regs.pc(), 0x0800_0000, "PC in {mode:?}");
        }
    }

    /// Switching to the mode already current has to be a no-op. Swapping a bank
    /// with itself would leave the FIQ pair stale in both copies.
    #[test]
    fn switching_to_the_mode_already_current_changes_nothing() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::Fiq);
        regs.set(8, 0x1111);
        regs.set(13, 0x2222);

        regs.set_mode(Mode::Fiq);
        assert_eq!(regs.get(8), 0x1111);
        assert_eq!(regs.sp(), 0x2222);

        regs.set_mode(Mode::User);
        regs.set_mode(Mode::Fiq);
        assert_eq!(regs.get(8), 0x1111, "and the bank still comes back after a real switch");
        assert_eq!(regs.sp(), 0x2222);
    }

    /// An application cannot promote itself by writing to `CPSR`. It is the
    /// whole reason there is a User mode.
    #[test]
    fn user_mode_may_write_the_flags_and_nothing_else() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::User);
        regs.set_cpsr_as(N | C | Mode::Supervisor.bits() | T, Mode::User);

        assert!(regs.n(), "the flags landed");
        assert!(regs.c());
        assert_eq!(regs.mode(), Mode::User, "and the mode did not");
        assert!(!regs.thumb(), "nor the state");
    }

    #[test]
    fn a_privileged_mode_may_write_the_lot() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::Irq);
        regs.set_cpsr_as(Z | Mode::Supervisor.bits() | T | I, Mode::Irq);

        assert!(regs.z());
        assert_eq!(regs.mode(), Mode::Supervisor);
        assert!(regs.thumb());
        assert!(regs.irq_disabled());
    }

    /// Writing five bits that name no mode must not bank onto a register set
    /// that does not exist. The rest of the write still goes through.
    #[test]
    fn a_cpsr_naming_no_mode_leaves_the_mode_alone() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::Irq);
        regs.set(13, 0x0300_7FA0);

        regs.set_cpsr(N | 0b0_0101);
        assert_eq!(regs.mode(), Mode::Irq, "the mode stayed");
        assert_eq!(regs.sp(), 0x0300_7FA0, "and so did its bank");
        assert!(regs.n(), "the flags went in");
    }

    /// Coming back from an exception is `SPSR` going back into `CPSR`, banks and
    /// all. This is the path every BIOS call returns by.
    #[test]
    fn an_exception_returns_by_putting_the_spsr_back() {
        let mut regs = Registers::new();
        regs.set_mode(Mode::User);
        regs.set(13, 0x0300_7F00);
        let interrupted = regs.cpsr();

        // What `SWI` does: keep the caller's `CPSR`, then change mode.
        regs.set_mode(Mode::Supervisor);
        regs.set_spsr(interrupted);
        regs.set(13, 0x0300_7FE0);
        assert_eq!(regs.spsr(), interrupted);

        regs.restore_cpsr();
        assert_eq!(regs.mode(), Mode::User);
        assert_eq!(regs.sp(), 0x0300_7F00, "the caller's stack pointer came back with it");
    }

    /// User and System have no `SPSR`. Reading one gives the live `CPSR` and
    /// writing one is dropped, rather than corrupting the bank User shares.
    #[test]
    fn the_modes_with_no_saved_cpsr_say_so() {
        let mut regs = Registers::new();
        for mode in [Mode::User, Mode::System] {
            regs.set_mode(mode);
            assert!(!mode.has_spsr());
            assert_eq!(regs.spsr(), regs.cpsr(), "{mode:?} gives back the live one");
            regs.set_spsr(0xDEAD_BEEF);
            assert_eq!(regs.spsr(), regs.cpsr(), "{mode:?} kept nothing");
            regs.restore_cpsr();
            assert_eq!(regs.mode(), mode, "and returning from nowhere goes nowhere");
        }
    }

    /// Every exception mode keeps its own, so one arriving inside another does
    /// not lose the first one's way back.
    #[test]
    fn each_exception_mode_saves_its_own_cpsr() {
        let mut regs = Registers::new();
        for (index, mode) in
            [Mode::Fiq, Mode::Irq, Mode::Supervisor, Mode::Abort, Mode::Undefined].iter().enumerate()
        {
            regs.set_mode(*mode);
            regs.set_spsr(0x1000 + index as u32);
        }
        for (index, mode) in
            [Mode::Fiq, Mode::Irq, Mode::Supervisor, Mode::Abort, Mode::Undefined].iter().enumerate()
        {
            regs.set_mode(*mode);
            assert_eq!(regs.spsr(), 0x1000 + index as u32, "{mode:?}");
        }
    }

    /// Bit 4 reads as one on this processor. A `CPSR` assembled without it
    /// would name no mode at all.
    #[test]
    fn bit_four_is_always_set() {
        let mut regs = Registers::new();
        regs.set_cpsr(0);
        assert_eq!(regs.cpsr() & ALWAYS_SET, ALWAYS_SET);
        regs.set_mode(Mode::User);
        assert_eq!(regs.cpsr() & 0x1F, Mode::User.bits());
    }

    #[test]
    fn a_reset_leaves_supervisor_with_both_interrupts_masked() {
        let regs = Registers::new();
        assert_eq!(regs.mode(), Mode::Supervisor);
        assert!(regs.irq_disabled());
        assert!(regs.fiq_disabled());
        assert!(!regs.thumb(), "and in ARM state");
    }

    #[test]
    fn n_and_z_come_from_the_result() {
        let mut regs = Registers::new();
        regs.set_nz(0);
        assert!(regs.z() && !regs.n());
        regs.set_nz(0x8000_0000);
        assert!(regs.n() && !regs.z());
        regs.set_nz(1);
        assert!(!regs.n() && !regs.z());
    }
}
