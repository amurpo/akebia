//! The joypad (0xFF00).
//!
//! Eight buttons multiplexed into a single 4-bit register through two select
//! lines. And everything is **active low**: a bit at 0 means "pressed". It is
//! the origin of the classic inverted-controls bug.
//!
//! ```text
//!   bit 7 6   5     4     3     2     1     0
//!       - -  ¬sel  ¬sel   ↓/St  ↑/Sel ←/B   →/A
//!            buttons dpad
//! ```
//!
//! If the game sets both select bits to 1 (no line selected), the low nibble
//! reads all ones: no button pressed.

use crate::cpu::{Interrupt, InterruptController};

/// The eight buttons. The discriminant is their bit within the nibble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Right,
    Left,
    Up,
    Down,
    A,
    B,
    Select,
    Start,
}

impl Button {
    pub const ALL: [Button; 8] = [
        Self::Right,
        Self::Left,
        Self::Up,
        Self::Down,
        Self::A,
        Self::B,
        Self::Select,
        Self::Start,
    ];

    /// Bit within the low nibble of 0xFF00.
    const fn bit(self) -> u8 {
        match self {
            Self::Right | Self::A => 0,
            Self::Left | Self::B => 1,
            Self::Up | Self::Select => 2,
            Self::Down | Self::Start => 3,
        }
    }

    /// `true` if it belongs to the action button group (line `P15`).
    const fn is_action(self) -> bool {
        matches!(self, Self::A | Self::B | Self::Select | Self::Start)
    }
}

pub struct Joypad {
    /// Real button state, one per bit. 1 = pressed (direct logic; the inversion
    /// is applied only when reading the register).
    pressed: u8,
    /// `true` if the game selected the action button line (`P15`).
    select_action: bool,
    /// `true` if it selected the d-pad (`P14`).
    select_direction: bool,
}

impl Joypad {
    pub const fn new() -> Self {
        Self { pressed: 0, select_action: false, select_direction: false }
    }

    fn mask(button: Button) -> u8 {
        1 << (button.bit() + if button.is_action() { 4 } else { 0 })
    }

    /// Updates a button. Requests the joypad interrupt on the press edge, which
    /// is what wakes the console up from `STOP`.
    pub fn set_button(&mut self, button: Button, down: bool, ic: &mut InterruptController) {
        let mask = Self::mask(button);
        let was_down = self.pressed & mask != 0;

        if down {
            self.pressed |= mask;
        } else {
            self.pressed &= !mask;
        }

        if down && !was_down && self.is_selected(button) {
            ic.request(Interrupt::Joypad);
        }
    }

    fn is_selected(&self, button: Button) -> bool {
        if button.is_action() {
            self.select_action
        } else {
            self.select_direction
        }
    }

    pub fn read(&self) -> u8 {
        let mut nibble = 0x0F;

        if self.select_action {
            nibble &= !(self.pressed >> 4) & 0x0F;
        }
        if self.select_direction {
            nibble &= !self.pressed & 0x0F;
        }

        // Bits 7-6 do not exist and read as 1; the select bits are active low
        // too.
        0xC0 | (u8::from(!self.select_action) << 5)
            | (u8::from(!self.select_direction) << 4)
            | nibble
    }

    pub fn write(&mut self, value: u8) {
        self.select_action = value & 0x20 == 0;
        self.select_direction = value & 0x10 == 0;
    }
}

impl Default for Joypad {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_line_selected_there_are_no_buttons() {
        let mut j = Joypad::new();
        let mut ic = InterruptController::new();
        j.set_button(Button::A, true, &mut ic);
        j.write(0x30); // both lines deselected
        assert_eq!(j.read() & 0x0F, 0x0F);
    }

    #[test]
    fn pressed_buttons_read_as_zero() {
        let mut j = Joypad::new();
        let mut ic = InterruptController::new();
        j.write(0x10); // select the action buttons (bit 5 at 0)

        assert_eq!(j.read() & 0x0F, 0x0F, "nothing pressed");
        j.set_button(Button::A, true, &mut ic);
        assert_eq!(j.read() & 0x0F, 0x0E, "A takes bit 0 and reads as 0");
        j.set_button(Button::Start, true, &mut ic);
        assert_eq!(j.read() & 0x0F, 0x06);
    }

    #[test]
    fn the_two_lines_are_multiplexed() {
        let mut j = Joypad::new();
        let mut ic = InterruptController::new();
        j.set_button(Button::A, true, &mut ic); // bit 0 of the action group
        j.set_button(Button::Down, true, &mut ic); // bit 3 of the d-pad

        j.write(0x10); // action only
        assert_eq!(j.read() & 0x0F, 0x0E);

        j.write(0x20); // d-pad only
        assert_eq!(j.read() & 0x0F, 0x07);
    }

    #[test]
    fn pressing_a_selected_button_requests_an_interrupt() {
        let mut j = Joypad::new();
        let mut ic = InterruptController::new();
        ic.write_enable(0xFF);
        j.write(0x10);

        j.set_button(Button::B, true, &mut ic);
        assert_eq!(ic.pending(), Some(Interrupt::Joypad));

        ic.acknowledge(Interrupt::Joypad);
        j.set_button(Button::B, true, &mut ic);
        assert_eq!(ic.pending(), None, "holding it down does not repeat the interrupt");
    }
}
