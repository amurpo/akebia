//! The three blocks the four inherited channels share.
//!
//! None of these channels generates "a note". Each generates a square, a table
//! wave or noise, and mounts on top of it three independent modulators that a
//! **512 Hz sequencer** pulses at different rates:
//!
//! ```text
//!   sequencer, 512 Hz
//!   step  0  1  2  3  4  5  6  7
//!         │     │     │     │  └── volume envelope           (64 Hz)
//!         │     │     │     └───── length                    (256 Hz)
//!         │     │     └─────────── length + sweep            (128 Hz)
//!         │     └───────────────── length
//!         └─────────────────────── length
//! ```
//!
//! That all three run at rates **derived from one counter** is what these
//! channels sound like: a vibrato cannot be got out of step with its envelope.
//!
//! # Why this is written twice
//!
//! Because the Game Boy core has the same three blocks and this crate does not
//! depend on it, deliberately: see the note at the top of [`crate`]. They are
//! the same hardware and the logic below is the same logic, and the day that
//! costs more than it saves the answer is a crate underneath both, not a
//! dependency from one to the other.
//!
//! What is not the same is the clock. Everything here counts in this machine's
//! cycles, which run four times as fast, so every period the older machine
//! measures in its own is four times as long here. That factor lives in the
//! channels, not in this file.

/// Length counter: turns the channel off after a fixed time.
///
/// Its maximum depends on the channel — 64 on the squares and the noise, 256 on
/// the wave one — because the register that loads it has six bits in some and
/// eight in the other.
#[derive(Debug, Clone, Copy)]
pub struct LengthCounter {
    /// Bit 6 of the control register: cleared, the channel sounds until
    /// something else stops it.
    pub enabled: bool,
    counter: u16,
    max: u16,
}

impl LengthCounter {
    pub const fn new(max: u16) -> Self {
        Self { enabled: false, counter: 0, max }
    }

    /// Loads it. The register holds the time **already used up**, so the
    /// counter starts at what is left of the maximum.
    pub fn load(&mut self, value: u16) {
        self.counter = self.max - (value % self.max);
    }

    /// A channel triggered with the counter at zero reloads it to the maximum.
    /// Without this, a channel triggered twice running goes silent the second
    /// time.
    pub fn trigger(&mut self) {
        if self.counter == 0 {
            self.counter = self.max;
        }
    }

    /// One step at 256 Hz. `true` means the channel is to be turned off.
    pub fn tick(&mut self) -> bool {
        if !self.enabled || self.counter == 0 {
            return false;
        }
        self.counter -= 1;
        self.counter == 0
    }
}

/// Volume envelope: raises or lowers the volume in regular steps.
///
/// It is what turns a flat square into a note with an attack and a decay. With
/// a period of zero it is frozen, which is how a constant volume is asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeEnvelope {
    /// Where it starts, from the top four bits of the register.
    pub initial: u8,
    /// Whether it rises or falls.
    pub increasing: bool,
    /// Sequencer steps between one change and the next.
    pub period: u8,
    volume: u8,
    timer: u8,
}

impl VolumeEnvelope {
    pub const fn new() -> Self {
        Self { initial: 0, increasing: false, period: 0, volume: 0, timer: 0 }
    }

    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// Writes the register, and answers whether the channel still has a DAC.
    ///
    /// A starting volume of zero with a falling envelope *is* "the DAC is off":
    /// the channel is disconnected there and then rather than left sounding at
    /// nothing, and the difference is audible — a disconnected channel is
    /// silence and a connected one at zero is a DC level.
    pub fn write(&mut self, value: u8) -> bool {
        self.initial = value >> 4;
        self.increasing = value & 0x08 != 0;
        self.period = value & 0x07;
        self.dac_enabled()
    }

    pub fn dac_enabled(&self) -> bool {
        self.initial != 0 || self.increasing
    }

    pub fn trigger(&mut self) {
        self.volume = self.initial;
        self.timer = self.period;
    }

    /// One step at 64 Hz.
    pub fn tick(&mut self) {
        if self.period == 0 {
            return;
        }
        if self.timer > 0 {
            self.timer -= 1;
        }
        if self.timer != 0 {
            return;
        }

        self.timer = self.period;
        // It stops at either end rather than wrapping round.
        match (self.increasing, self.volume) {
            (true, v) if v < 15 => self.volume += 1,
            (false, v) if v > 0 => self.volume -= 1,
            _ => {}
        }
    }
}

/// Frequency sweep, which only the first channel has.
///
/// It shifts the frequency by steps proportional to itself, so the glissando is
/// exponential: the laser shot of half this machine's library.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sweep {
    /// Sequencer steps between one shift and the next.
    pub period: u8,
    /// Whether it subtracts rather than adds.
    pub decreasing: bool,
    /// How far the frequency moves each step.
    pub shift: u8,
    /// The sweep's own copy of the frequency, which is what it works on.
    shadow: u16,
    timer: u8,
    enabled: bool,
}

impl Sweep {
    pub const fn new() -> Self {
        Self { period: 0, decreasing: false, shift: 0, shadow: 0, timer: 0, enabled: false }
    }

    pub fn write(&mut self, value: u8) {
        self.period = (value >> 4) & 0x07;
        self.decreasing = value & 0x08 != 0;
        self.shift = value & 0x07;
    }

    /// Starts the sweep at the channel's current frequency.
    ///
    /// `false` means the channel is to be turned off: the next frequency is
    /// worked out **at the trigger**, and one that overflows silences the
    /// channel before it has made a single sample.
    pub fn trigger(&mut self, frequency: u16) -> bool {
        self.shadow = frequency;
        self.timer = if self.period == 0 { 8 } else { self.period };
        self.enabled = self.period != 0 || self.shift != 0;

        if self.shift != 0 {
            return self.next_frequency() <= 2047;
        }
        true
    }

    /// The next frequency: this one plus or minus itself, shifted.
    fn next_frequency(&self) -> u16 {
        let delta = self.shadow >> self.shift;
        if self.decreasing {
            self.shadow.saturating_sub(delta)
        } else {
            self.shadow + delta
        }
    }

    /// One step at 128 Hz.
    ///
    /// `Some(f)` is a new frequency for the channel and `None` is nothing to
    /// change; the `bool` is `false` when the channel is to be turned off
    /// because the frequency ran off the top.
    pub fn tick(&mut self) -> (Option<u16>, bool) {
        if self.timer > 0 {
            self.timer -= 1;
        }
        if self.timer != 0 {
            return (None, true);
        }

        // With a period of zero the sweep does not move, but the timer keeps
        // reloading to eight: that is what makes a period written later take
        // effect at the next step.
        self.timer = if self.period == 0 { 8 } else { self.period };
        if !self.enabled || self.period == 0 {
            return (None, true);
        }

        let new = self.next_frequency();
        if new > 2047 {
            return (None, false);
        }
        if self.shift == 0 {
            return (None, true);
        }

        self.shadow = new;
        // Worked out a second time to check for the overflow, and the answer
        // thrown away. It is a quirk of the hardware and not an oversight.
        (Some(new), self.next_frequency() <= 2047)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_length_counts_down_from_what_is_left_of_the_maximum() {
        let mut length = LengthCounter::new(64);
        length.enabled = true;
        length.load(60);

        for _ in 0..3 {
            assert!(!length.tick());
        }
        assert!(length.tick(), "the fourth step turns the channel off");
    }

    #[test]
    fn a_length_that_is_not_enabled_turns_nothing_off() {
        let mut length = LengthCounter::new(64);
        length.load(63);
        for _ in 0..100 {
            assert!(!length.tick());
        }
    }

    /// Triggering an expired counter reloads it. Without this the second note
    /// of a repeated pair is silent.
    #[test]
    fn triggering_an_expired_length_reloads_it() {
        let mut length = LengthCounter::new(64);
        length.enabled = true;
        length.load(63);
        assert!(length.tick(), "used up");

        length.trigger();
        for _ in 0..63 {
            assert!(!length.tick(), "and it has the whole of it again");
        }
        assert!(length.tick());
    }

    #[test]
    fn the_envelope_climbs_and_stops_at_the_top() {
        let mut envelope = VolumeEnvelope::new();
        envelope.write(0xD9); // start at 13, rising (bit 3), period 1
        envelope.trigger();
        assert_eq!(envelope.volume(), 13);

        envelope.tick();
        envelope.tick();
        assert_eq!(envelope.volume(), 15);
        envelope.tick();
        assert_eq!(envelope.volume(), 15, "and no further");
    }

    #[test]
    fn an_envelope_with_no_period_does_not_move() {
        let mut envelope = VolumeEnvelope::new();
        envelope.write(0x80); // start at 8, falling, period 0
        envelope.trigger();
        for _ in 0..20 {
            envelope.tick();
        }
        assert_eq!(envelope.volume(), 8, "a period of zero is a held volume");
    }

    /// Nothing above and a falling envelope is the way a game says "disconnect
    /// this channel", and it has to be heard as silence rather than as a level.
    #[test]
    fn a_starting_volume_of_nothing_falling_is_the_dac_switched_off() {
        let mut envelope = VolumeEnvelope::new();
        assert!(!envelope.write(0x00), "nothing, falling");
        assert!(!envelope.write(0x07), "nor with a period on it");
        assert!(envelope.write(0x08), "nothing, rising, is a DAC that is on");
        assert!(envelope.write(0x10), "and so is anything above nothing");
    }

    /// The sweep moves the frequency by a fraction of itself, which is what
    /// makes the glissando exponential rather than straight.
    #[test]
    fn the_sweep_shifts_the_frequency_by_a_part_of_itself() {
        let mut sweep = Sweep::new();
        sweep.write(0x11); // period 1, rising, shift 1
        assert!(sweep.trigger(500));

        let (new, alive) = sweep.tick();
        assert_eq!(new, Some(750), "five hundred and half of it");
        assert!(alive);
    }

    /// The overflow is looked for **one step ahead**: the frequency that would
    /// come next is worked out, checked, and thrown away. So a sweep is
    /// silenced before it plays the note that would have run off the top, not
    /// after. It is a quirk of the hardware and it is audible — the last note
    /// of a rising sweep is missing.
    #[test]
    fn a_sweep_is_silenced_a_step_before_it_would_overflow() {
        let mut sweep = Sweep::new();
        sweep.write(0x11); // period 1, rising, shift 1
        assert!(sweep.trigger(1000), "1500 still fits");

        let (new, alive) = sweep.tick();
        assert_eq!(new, Some(1500), "and it is played");
        assert!(!alive, "but 2250 would not fit, so the channel is already off");
    }

    /// A sweep that would run off the top silences the channel, and it does so
    /// at the trigger rather than when it gets there.
    #[test]
    fn a_sweep_that_would_overflow_silences_the_channel_at_the_trigger() {
        let mut sweep = Sweep::new();
        sweep.write(0x01); // period 0, rising, shift 1
        assert!(!sweep.trigger(2000), "2000 and half of it is over the top");
    }

    #[test]
    fn a_falling_sweep_subtracts() {
        let mut sweep = Sweep::new();
        sweep.write(0x1A); // period 1, falling, shift 2
        assert!(sweep.trigger(1000));
        assert_eq!(sweep.tick().0, Some(750));
    }
}
