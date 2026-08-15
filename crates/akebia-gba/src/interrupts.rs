//! What can interrupt the processor, and the three registers that decide
//! whether anything does.
//!
//! # Three switches and not one
//!
//! An interrupt has to get past all of them, and they exist at different levels
//! for a reason.
//!
//! **`IE`** says which of the fourteen sources are wanted at all. A game that
//! never uses a timer leaves its bit clear for ever.
//!
//! **`IME`** is a single bit that turns the lot off. It is what a critical
//! section uses: two writes around the part that must not be interrupted, and
//! no need to remember which sources were enabled.
//!
//! **The `I` bit in the processor's own status register** is the third, and it
//! belongs to the processor rather than to this. An exception sets it on the
//! way in so that a handler cannot be interrupted before it has saved anything.
//!
//! # The register that is cleared by writing ones to it
//!
//! `IF` says what is *waiting*, and a handler clears the bit it has dealt with
//! by **writing a one to it**. Writing a zero does nothing at all.
//!
//! It reads backwards and it is the right design: two interrupts can arrive
//! while a handler runs, and a handler that cleared `IF` by writing what it
//! wanted to keep would throw away whichever arrived between its read and its
//! write. Writing ones to the bits being retired cannot lose one that arrived
//! in the meantime, because that bit was never in the value being written.
//!
//! An emulator that treats this as an ordinary register loses interrupts under
//! load and nowhere else, which is the worst way for a bug to behave.

/// The fourteen things that can ask for attention, in the order `IE` and `IF`
/// keep them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The picture unit reaching the bottom of the screen, which is the one
    /// every game waits on.
    VBlank = 0,
    /// The end of a scanline.
    HBlank = 1,
    /// A chosen scanline being reached, which is how an effect is timed to part
    /// of the screen.
    VCount = 2,
    Timer0 = 3,
    Timer1 = 4,
    Timer2 = 5,
    Timer3 = 6,
    Serial = 7,
    Dma0 = 8,
    Dma1 = 9,
    Dma2 = 10,
    Dma3 = 11,
    /// A chosen combination of buttons, which can wake the machine from sleep.
    Keypad = 12,
    /// Something on the cartridge itself.
    GamePak = 13,
}

impl Source {
    pub const fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

/// The bits of `IE` and `IF` that name a source. The top two are not used.
const USED: u16 = 0x3FFF;

#[derive(Clone, Default)]
pub struct Interrupts {
    /// `IE`: which sources are wanted.
    enabled: u16,
    /// `IF`: which are waiting to be dealt with.
    requested: u16,
    /// `IME`: the one bit that turns them all off.
    master: bool,
    /// Whether the processor has been told to stop until something arrives.
    halted: bool,
}

impl Interrupts {
    pub fn new() -> Self {
        Self::default()
    }

    /// A source asking for attention.
    ///
    /// The request lands in `IF` whether or not anything is listening, which is
    /// what lets a game poll a bit it has not enabled. What `IE` and `IME`
    /// decide is whether the *processor* is told, not whether the flag is set.
    pub fn raise(&mut self, source: Source) {
        self.requested |= source.bit();
        // Waking is not the same as being interrupted. A halted processor comes
        // back as soon as an enabled source is waiting, even with `IME` off —
        // otherwise a game that halts inside a critical section never wakes.
        if self.enabled & source.bit() != 0 {
            self.halted = false;
        }
    }

    /// Whether the processor should be interrupted now.
    ///
    /// This does not look at the processor's own `I` bit: that is the
    /// processor's business and it is checked where the decision is made.
    pub fn pending(&self) -> bool {
        self.master && self.enabled & self.requested & USED != 0
    }

    /// Whether anything an enabled source could do would wake a halted
    /// processor. `IME` plays no part, for the reason given in [`raise`].
    pub fn waiting(&self) -> bool {
        self.enabled & self.requested & USED != 0
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    pub fn halt(&mut self) {
        self.halted = true;
    }

    pub fn enabled(&self) -> u16 {
        self.enabled
    }

    pub fn set_enabled(&mut self, value: u16) {
        self.enabled = value & USED;
        // Enabling a source that was already waiting is enough to wake up: the
        // request was there all along and only the listening had been off.
        if self.waiting() {
            self.halted = false;
        }
    }

    pub fn requested(&self) -> u16 {
        self.requested
    }

    /// Retires the bits written as ones, and leaves alone the bits written as
    /// zeros. See the note at the top of this module.
    pub fn acknowledge(&mut self, value: u16) {
        self.requested &= !value;
    }

    pub fn master(&self) -> bool {
        self.master
    }

    pub fn set_master(&mut self, on: bool) {
        self.master = on;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_fourteen_sources_have_a_bit_of_their_own() {
        const ALL: [Source; 14] = [
            Source::VBlank,
            Source::HBlank,
            Source::VCount,
            Source::Timer0,
            Source::Timer1,
            Source::Timer2,
            Source::Timer3,
            Source::Serial,
            Source::Dma0,
            Source::Dma1,
            Source::Dma2,
            Source::Dma3,
            Source::Keypad,
            Source::GamePak,
        ];
        let mut seen = 0u16;
        for source in ALL {
            assert_eq!(seen & source.bit(), 0, "{source:?} shares a bit");
            seen |= source.bit();
        }
        assert_eq!(seen, USED, "and between them they are every bit that is used");
    }

    /// All three switches have to be closed. Any one of them open and the
    /// processor is not told.
    #[test]
    fn an_interrupt_has_to_get_past_all_three_switches() {
        let mut irq = Interrupts::new();
        irq.raise(Source::VBlank);
        assert!(!irq.pending(), "nothing enabled");

        irq.set_enabled(Source::VBlank.bit());
        assert!(!irq.pending(), "enabled but the master switch is off");

        irq.set_master(true);
        assert!(irq.pending(), "and now it is");

        irq.set_master(false);
        assert!(!irq.pending(), "the master switch turns the lot off at once");
    }

    /// A source that is not enabled still sets its flag, which is what lets a
    /// game poll something it never asked to be interrupted by.
    #[test]
    fn a_request_lands_whether_or_not_anything_is_listening() {
        let mut irq = Interrupts::new();
        irq.raise(Source::Timer2);
        assert_eq!(irq.requested(), Source::Timer2.bit(), "the flag is set");
        assert!(!irq.pending(), "and nobody is told");
    }

    /// Clearing by writing ones is not a quirk to be tidied away: it is what
    /// makes a handler unable to lose an interrupt that arrived while it ran.
    #[test]
    fn a_request_is_retired_by_writing_a_one_over_it() {
        let mut irq = Interrupts::new();
        irq.raise(Source::VBlank);
        irq.raise(Source::Timer0);

        irq.acknowledge(Source::VBlank.bit());
        assert_eq!(irq.requested(), Source::Timer0.bit(), "one retired, one left");

        // Writing zeros is what an ordinary register would take as "clear
        // these", and here it does nothing whatsoever.
        irq.acknowledge(0);
        assert_eq!(irq.requested(), Source::Timer0.bit(), "a zero retires nothing");
    }

    /// The case the design exists for: an interrupt arriving while a handler is
    /// dealing with another one must not be lost when the handler retires its
    /// own.
    #[test]
    fn retiring_one_request_cannot_lose_another_that_arrived_meanwhile() {
        let mut irq = Interrupts::new();
        irq.raise(Source::VBlank);

        // The handler reads IF, finds VBlank, and starts work. A timer goes off
        // while it is busy.
        let dealing_with = irq.requested();
        irq.raise(Source::Timer1);

        // It retires what it dealt with, by writing exactly that back.
        irq.acknowledge(dealing_with);
        assert_eq!(irq.requested(), Source::Timer1.bit(), "the timer survived");
    }

    /// A halted processor wakes on an enabled source even with the master
    /// switch off, or a game that halts inside a critical section never comes
    /// back.
    #[test]
    fn halting_ends_when_an_enabled_source_arrives_master_switch_or_not() {
        let mut irq = Interrupts::new();
        irq.set_enabled(Source::VBlank.bit());
        irq.halt();
        assert!(irq.halted());

        irq.raise(Source::HBlank);
        assert!(irq.halted(), "a source nobody enabled does not wake it");

        irq.raise(Source::VBlank);
        assert!(!irq.halted(), "an enabled one does, with IME off");
        assert!(!irq.pending(), "without the processor being interrupted");
    }

    /// And enabling a source that was already waiting wakes it too: the request
    /// was there all along, only the listening was off.
    #[test]
    fn enabling_a_source_that_was_already_waiting_wakes_a_halted_processor() {
        let mut irq = Interrupts::new();
        irq.raise(Source::Timer3);
        irq.halt();
        assert!(irq.halted());

        irq.set_enabled(Source::Timer3.bit());
        assert!(!irq.halted());
    }

    /// The top two bits name nothing and must not be able to hold a request.
    #[test]
    fn the_two_unused_bits_cannot_enable_or_request_anything() {
        let mut irq = Interrupts::new();
        irq.set_enabled(0xFFFF);
        assert_eq!(irq.enabled(), USED);

        irq.set_master(true);
        irq.acknowledge(0xFFFF);
        assert!(!irq.pending(), "nothing left to be pending on");
    }
}
