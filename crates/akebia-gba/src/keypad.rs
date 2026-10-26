//! The ten buttons.
//!
//! # A register that means the opposite of what it looks like
//!
//! `KEYINPUT` reports a button as **zero** when it is held and one when it is
//! not. The buttons are wired to pull their lines down, and the register is
//! those lines read straight off, so the inversion is the hardware showing
//! through rather than a convention anybody chose.
//!
//! It matters more than a detail of encoding should, because it decides what a
//! machine without this register does. An unmapped address that answers zero is
//! answering *every button held down, for ever* — which is not a quiet absence
//! of input but a very loud presence of all of it. A game reads it, sees Start
//! and A and both shoulders and all four directions at once, and does whatever
//! it does with that; on this emulator one cartridge sat on its title screen
//! reading the register 64 times a second and never getting past it.
//!
//! So the resting value is `0x03FF` and not zero, and that is the whole of why
//! this module exists before anything can press a button.
//!
//! # The interrupt
//!
//! A game can ask to be interrupted by a chosen set of buttons, either any of
//! them or all of them together. Its purpose is not to save the polling — a
//! game reads the register every frame anyway — but to wake the machine from
//! the deep sleep the BIOS can put it in, where nothing else is running to do
//! the reading. It is the only interrupt whose source is a person.

use crate::interrupts::{Interrupts, Source};

/// Where the two registers are.
pub const KEYINPUT: u32 = 0x0400_0130;
pub const KEYCNT: u32 = 0x0400_0132;
pub const LAST: u32 = 0x0400_0133;

/// The ten lines that exist. The register is sixteen bits and the top six are
/// not buttons.
const BUTTONS: u16 = 0x03FF;

/// `KEYCNT`: which buttons are being watched, whether to interrupt at all, and
/// whether one of them is enough.
const WATCHED: u16 = BUTTONS;
const IRQ_ENABLED: u16 = 1 << 14;
const ALL_TOGETHER: u16 = 1 << 15;

/// One of the ten.
///
/// The order is the register's own, and it is not the order anybody would
/// choose: the four directions sit between Start and the shoulders because that
/// is how the lines came out of the chip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    A = 0,
    B = 1,
    Select = 2,
    Start = 3,
    Right = 4,
    Left = 5,
    Up = 6,
    Down = 7,
    R = 8,
    L = 9,
}

impl Button {
    /// Every one of them, for a frontend that wants to loop over the lot.
    pub const ALL: [Button; 10] = [
        Button::A,
        Button::B,
        Button::Select,
        Button::Start,
        Button::Right,
        Button::Left,
        Button::Up,
        Button::Down,
        Button::R,
        Button::L,
    ];

    const fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

/// The buttons, as the machine sees them.
pub struct Keypad {
    /// Which are held, one bit each. A **set** bit is a button **down**, which
    /// is the opposite of what the register reports — the inversion happens
    /// once, on the way out, rather than at every place that asks.
    held: u16,
    control: u16,
}

impl Default for Keypad {
    fn default() -> Self {
        Self::new()
    }
}

impl Keypad {
    pub const fn new() -> Self {
        Self { held: 0, control: 0 }
    }

    /// Holds or releases one button.
    ///
    /// Nothing here raises the interrupt: that is [`Keypad::poll`], which the
    /// clock calls. The hardware compares the lines continuously rather than at
    /// the moment a button moves, and a game that sets `KEYCNT` while a button
    /// is already held expects to be interrupted by it.
    pub fn set(&mut self, button: Button, down: bool) {
        if down {
            self.held |= button.bit();
        } else {
            self.held &= !button.bit();
        }
    }

    /// Whether a button is held.
    pub fn is_down(&self, button: Button) -> bool {
        self.held & button.bit() != 0
    }

    /// Releases everything. For a machine being reset, or a window losing
    /// focus with a button still down — which would otherwise stay down for
    /// ever, since nothing would ever report it coming up.
    pub fn release_all(&mut self) {
        self.held = 0;
    }

    /// Raises the interrupt if the buttons a game asked to watch are held.
    ///
    /// Either any one of them or all of them together, which is the difference
    /// between "a button" and "this combination" — the second is how a game
    /// gives a soft reset to a chord nobody presses by accident.
    pub fn poll(&self, irq: &mut Interrupts) {
        if self.control & IRQ_ENABLED == 0 {
            return;
        }
        let watched = self.control & WATCHED;
        if watched == 0 {
            return;
        }
        let matched = if self.control & ALL_TOGETHER != 0 {
            self.held & watched == watched
        } else {
            self.held & watched != 0
        };
        if matched {
            irq.raise(Source::Keypad);
        }
    }

    pub fn read8(&self, addr: u32) -> u8 {
        let half = |value: u16| (value >> ((addr & 1) * 8)) as u8;
        match addr & !1 {
            // Held is low. Everything up is `0x03FF`, and the six bits above
            // the buttons read as ones as well.
            KEYINPUT => half(!self.held & BUTTONS),
            _ => half(self.control),
        }
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        let shift = (addr & 1) * 8;
        let widened = |existing: u16| (existing & !(0xFFu16 << shift)) | (u16::from(value) << shift);
        match addr & !1 {
            // The buttons are not a game's to set. Writing here does nothing at
            // all, and storing the value would let a game read back a press
            // that never happened.
            KEYINPUT => {}
            _ => self.control = widened(self.control),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value a machine with nobody touching it reports — and the one whose
    /// absence left a cartridge stuck on its title screen, reading every button
    /// as held.
    #[test]
    fn nothing_held_reads_as_every_bit_set() {
        let keypad = Keypad::new();
        assert_eq!(keypad.read8(KEYINPUT), 0xFF);
        assert_eq!(keypad.read8(KEYINPUT + 1), 0x03);
    }

    #[test]
    fn a_held_button_reads_as_a_zero() {
        let mut keypad = Keypad::new();
        keypad.set(Button::Start, true);
        // Start is bit 3, and a held button is the one that reads low.
        assert_eq!(keypad.read8(KEYINPUT), 0b1111_0111);
        assert!(keypad.is_down(Button::Start));
        assert!(!keypad.is_down(Button::A));

        keypad.set(Button::Start, false);
        assert_eq!(keypad.read8(KEYINPUT), 0xFF, "and comes back up");
    }

    /// The two shoulders are the reason this cannot be the older machine's
    /// button type: they are in the register's high byte, where the older
    /// machine has nothing at all.
    #[test]
    fn the_shoulders_are_in_the_high_byte() {
        let mut keypad = Keypad::new();
        keypad.set(Button::L, true);
        assert_eq!(keypad.read8(KEYINPUT), 0xFF, "the low byte is untouched");
        assert_eq!(keypad.read8(KEYINPUT + 1), 0x01, "and L is bit 9");
    }

    #[test]
    fn every_button_has_its_own_bit() {
        for button in Button::ALL {
            let mut keypad = Keypad::new();
            keypad.set(button, true);
            let reported = u16::from(keypad.read8(KEYINPUT))
                | (u16::from(keypad.read8(KEYINPUT + 1)) << 8);
            assert_eq!(!reported & BUTTONS, button.bit(), "{button:?} alone");
        }
    }

    /// A game may not press its own buttons.
    #[test]
    fn writing_the_buttons_does_nothing() {
        let mut keypad = Keypad::new();
        keypad.write8(KEYINPUT, 0);
        keypad.write8(KEYINPUT + 1, 0);
        assert_eq!(keypad.read8(KEYINPUT), 0xFF, "still nothing held");
    }

    #[test]
    fn the_control_register_reads_back_what_was_written() {
        let mut keypad = Keypad::new();
        keypad.write8(KEYCNT, 0x0F);
        keypad.write8(KEYCNT + 1, 0xC0);
        assert_eq!(keypad.read8(KEYCNT), 0x0F);
        assert_eq!(keypad.read8(KEYCNT + 1), 0xC0);
    }

    fn watching(control: u16) -> Keypad {
        let mut keypad = Keypad::new();
        keypad.write8(KEYCNT, control as u8);
        keypad.write8(KEYCNT + 1, (control >> 8) as u8);
        keypad
    }

    #[test]
    fn no_interrupt_is_raised_when_it_was_not_asked_for() {
        let mut irq = Interrupts::new();
        // The buttons are watched but the enable bit is not set.
        let mut keypad = watching(Button::A.bit());
        keypad.set(Button::A, true);
        keypad.poll(&mut irq);
        assert_eq!(irq.requested(), 0);
    }

    #[test]
    fn any_of_the_watched_buttons_raises_the_interrupt() {
        let mut irq = Interrupts::new();
        let mut keypad = watching(IRQ_ENABLED | Button::A.bit() | Button::B.bit());

        keypad.poll(&mut irq);
        assert_eq!(irq.requested(), 0, "nothing held yet");

        keypad.set(Button::B, true);
        keypad.poll(&mut irq);
        assert_ne!(irq.requested(), 0, "one of the two is enough");
    }

    /// The other mode: all of them together, which is how a game gives itself a
    /// reset chord that cannot be hit by accident.
    #[test]
    fn all_together_needs_every_watched_button() {
        let mut irq = Interrupts::new();
        let watched = Button::A.bit() | Button::B.bit() | Button::Start.bit();
        let mut keypad = watching(IRQ_ENABLED | ALL_TOGETHER | watched);

        keypad.set(Button::A, true);
        keypad.set(Button::B, true);
        keypad.poll(&mut irq);
        assert_eq!(irq.requested(), 0, "two of the three is not the chord");

        keypad.set(Button::Start, true);
        keypad.poll(&mut irq);
        assert_ne!(irq.requested(), 0, "and now it is");
    }

    /// A button held while the game is *not* watching it, and then watched,
    /// interrupts — because the hardware compares the lines continuously and
    /// does not wait for the button to move.
    #[test]
    fn a_button_already_held_when_the_watch_begins_still_interrupts() {
        let mut irq = Interrupts::new();
        let mut keypad = Keypad::new();
        keypad.set(Button::L, true);
        keypad.poll(&mut irq);
        assert_eq!(irq.requested(), 0, "nothing was being watched");

        keypad.write8(KEYCNT, Button::L.bit() as u8);
        keypad.write8(KEYCNT + 1, ((IRQ_ENABLED | Button::L.bit()) >> 8) as u8);
        keypad.poll(&mut irq);
        assert_ne!(irq.requested(), 0, "the line was already down");
    }

    /// Watching nothing interrupts on nothing, in either mode. Under "all
    /// together" an empty set is otherwise trivially satisfied, which would
    /// interrupt for ever.
    #[test]
    fn watching_no_buttons_never_interrupts() {
        for mode in [0, ALL_TOGETHER] {
            let mut irq = Interrupts::new();
            let mut keypad = watching(IRQ_ENABLED | mode);
            for button in Button::ALL {
                keypad.set(button, true);
            }
            keypad.poll(&mut irq);
            assert_eq!(irq.requested(), 0, "mode {mode:04X}");
        }
    }

    #[test]
    fn releasing_everything_lets_every_button_up() {
        let mut keypad = Keypad::new();
        for button in Button::ALL {
            keypad.set(button, true);
        }
        assert_eq!(keypad.read8(KEYINPUT), 0x00);

        keypad.release_all();
        assert_eq!(keypad.read8(KEYINPUT), 0xFF);
        assert_eq!(keypad.read8(KEYINPUT + 1), 0x03);
    }
}
