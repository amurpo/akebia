//! The timer: `DIV`, `TIMA`, `TMA` and `TAC`.
//!
//! A small, deceptive module. The textbook description —"TIMA increments at the
//! rate TAC says"— produces an implementation that fails the tests and breaks
//! games. The hardware works like this:
//!
//! 1. There is **a single internal 16-bit counter** that advances on every
//!    T-cycle. `DIV` (0xFF04) is not a register of its own: it is its high 8
//!    bits.
//! 2. `TIMA` increments on the **falling edge** of `counter[bit] & TAC.enable`,
//!    where the bit is chosen by `TAC`. It is not a counting divider.
//!
//! The two observable consequences of that difference:
//!
//! - Writing to `DIV` zeroes the counter. If the watched bit was at 1, that
//!   produces a falling edge and **`TIMA` increments**. Games use that write to
//!   seed random numbers.
//! - Turning `TAC` off with the watched bit at 1 also generates the edge, and
//!   also increments `TIMA`.
//!
//! On top of that, `TIMA` overflow is not immediate: for 4 T-cycles the register
//! reads 0 and only afterwards is it reloaded with `TMA` and the interrupt
//! requested. Writing to `TIMA` within that window cancels the reload.

use crate::cpu::{Interrupt, InterruptController};

/// Bit of the internal counter each `TAC` value watches.
///
/// | TAC | bit | frequency  |
/// |-----|-----|------------|
/// | 00  | 9   | 4096 Hz    |
/// | 01  | 3   | 262144 Hz  |
/// | 10  | 5   | 65536 Hz   |
/// | 11  | 7   | 16384 Hz   |
const TAC_BIT: [u8; 4] = [9, 3, 5, 7];

/// T-cycles `TIMA` stays at 0 after overflowing, before loading `TMA`.
const RELOAD_DELAY: u32 = 4;

/// State of the `TIMA` overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overflow {
    None,
    /// Overflowed `n` T-cycles ago; at 4 it reloads.
    Pending(u32),
}

pub struct Timer {
    /// Internal 16-bit counter. `DIV` is its bits 15-8.
    counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,
    /// Previous level of the watched signal, to detect the falling edge.
    prev_signal: bool,
    overflow: Overflow,
}

impl Timer {
    pub const fn new() -> Self {
        Self {
            // Value the BootROM leaves the counter at.
            counter: 0xAB00,
            tima: 0,
            tma: 0,
            tac: 0xF8,
            prev_signal: false,
            overflow: Overflow::None,
        }
    }

    /// Signal whose falling edge increments `TIMA`.
    fn signal(&self) -> bool {
        let enabled = self.tac & 0x04 != 0;
        let bit = TAC_BIT[(self.tac & 0x03) as usize];
        enabled && (self.counter >> bit) & 1 != 0
    }

    /// Recomputes the signal and, if it fell, increments `TIMA`.
    ///
    /// It is called after **any** change to the counter or to `TAC`, which is
    /// precisely what makes writes to `DIV` have visible side effects.
    fn detect_edge(&mut self) {
        let signal = self.signal();
        if self.prev_signal && !signal {
            let (result, carried) = self.tima.overflowing_add(1);
            self.tima = result;
            if carried {
                // It does not reload yet: there are 4 T-cycles in which TIMA
                // reads 0.
                self.overflow = Overflow::Pending(0);
            }
        }
        self.prev_signal = signal;
    }

    pub fn tick(&mut self, t_cycles: u32, ic: &mut InterruptController) {
        for _ in 0..t_cycles {
            // The reload window is evaluated before advancing the counter, so
            // that the T-cycle in which the overflow happened does not count
            // within the 4 of delay.
            if let Overflow::Pending(elapsed) = self.overflow {
                let elapsed = elapsed + 1;
                if elapsed >= RELOAD_DELAY {
                    self.tima = self.tma;
                    ic.request(Interrupt::Timer);
                    self.overflow = Overflow::None;
                } else {
                    self.overflow = Overflow::Pending(elapsed);
                }
            }

            self.counter = self.counter.wrapping_add(1);
            self.detect_edge();
        }
    }

    /// A specific bit of the internal 16-bit counter.
    ///
    /// The APU queries it: its 512 Hz sequencer is derived from here and not
    /// from a divider of its own, which is what makes writing to `DIV` also
    /// reset the pace of the envelopes.
    pub fn counter_bit(&self, bit: u8) -> bool {
        (self.counter >> bit) & 1 != 0
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF04 => (self.counter >> 8) as u8,
            0xFF05 => self.tima,
            0xFF06 => self.tma,
            // The high 5 bits of TAC do not exist and read as 1.
            0xFF07 => self.tac | 0xF8,
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            // Any write to DIV zeroes the whole counter, whatever the value
            // written.
            0xFF04 => {
                self.counter = 0;
                self.detect_edge();
            }
            0xFF05 => {
                // Writing to TIMA during the reload window cancels it.
                self.tima = value;
                self.overflow = Overflow::None;
            }
            0xFF06 => self.tma = value,
            0xFF07 => {
                self.tac = value & 0x07;
                self.detect_edge();
            }
            _ => {}
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timer in a known state: counter at 0, `TIMA` at 0.
    fn clean_timer(tac: u8) -> (Timer, InterruptController) {
        let mut t = Timer::new();
        t.counter = 0;
        t.prev_signal = false;
        t.write(0xFF07, tac);
        let mut ic = InterruptController::new();
        ic.write_enable(0xFF);
        (t, ic)
    }

    #[test]
    fn div_advances_by_one_every_256_t_cycles() {
        let (mut t, mut ic) = clean_timer(0x00);
        t.tick(255, &mut ic);
        assert_eq!(t.read(0xFF04), 0);
        t.tick(1, &mut ic);
        assert_eq!(t.read(0xFF04), 1);
    }

    #[test]
    fn writing_to_div_zeroes_it() {
        let (mut t, mut ic) = clean_timer(0x00);
        t.tick(1000, &mut ic);
        assert_ne!(t.read(0xFF04), 0);
        t.write(0xFF04, 0xFF);
        assert_eq!(t.read(0xFF04), 0, "the value written is irrelevant");
    }

    #[test]
    fn tima_advances_at_the_rate_of_tac() {
        // TAC = 0b101: enabled, bit 3 → one increment every 16 T-cycles.
        let (mut t, mut ic) = clean_timer(0x05);
        t.tick(16, &mut ic);
        assert_eq!(t.read(0xFF05), 1);
        t.tick(16 * 4, &mut ic);
        assert_eq!(t.read(0xFF05), 5);
    }

    #[test]
    fn tima_does_not_advance_with_tac_disabled() {
        let (mut t, mut ic) = clean_timer(0x01); // bit 3, but not enabled
        t.tick(1000, &mut ic);
        assert_eq!(t.read(0xFF05), 0);
    }

    #[test]
    fn writing_to_div_can_increment_tima() {
        let (mut t, mut ic) = clean_timer(0x05); // watches bit 3
        t.tick(8, &mut ic); // bit 3 goes to 1
        assert_eq!(t.read(0xFF05), 0);

        t.write(0xFF04, 0x00); // the counter drops to 0 → falling edge
        assert_eq!(t.read(0xFF05), 1, "resetting DIV with the watched bit at 1 increments TIMA");
    }

    #[test]
    fn the_overflow_reloads_after_a_delay() {
        let (mut t, mut ic) = clean_timer(0x05);
        t.write(0xFF06, 0x42); // TMA
        t.write(0xFF05, 0xFF); // TIMA one step from overflowing

        t.tick(16, &mut ic); // it overflows
        assert_eq!(t.read(0xFF05), 0x00, "for 4 T-cycles TIMA reads 0");
        assert_eq!(ic.pending(), None, "the interrupt is not immediate either");

        t.tick(4, &mut ic);
        assert_eq!(t.read(0xFF05), 0x42, "now it does reload from TMA");
        assert_eq!(ic.pending(), Some(Interrupt::Timer));
    }

    #[test]
    fn writing_tima_during_the_delay_cancels_the_reload() {
        let (mut t, mut ic) = clean_timer(0x05);
        t.write(0xFF06, 0x42);
        t.write(0xFF05, 0xFF);

        t.tick(16, &mut ic);
        t.write(0xFF05, 0x10); // inside the window
        t.tick(8, &mut ic);
        assert_eq!(t.read(0xFF05), 0x10, "TMA never got loaded");
        assert_eq!(ic.pending(), None);
    }
}
