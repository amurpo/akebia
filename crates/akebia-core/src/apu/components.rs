//! The three blocks the APU channels share.
//!
//! A Game Boy channel does not generate "a note": it generates a square, noise
//! or table wave, and on top of it mounts three independent modulators that the
//! *frame sequencer* pulses at different rates.
//!
//! ```text
//!   frame sequencer, 512 Hz
//!   step  0  1  2  3  4  5  6  7
//!         │     │     │     │  └── volume envelope           (64 Hz)
//!         │     │     │     └───── length                    (256 Hz)
//!         │     │     └─────────── length + sweep            (128 Hz)
//!         │     └───────────────── length
//!         └─────────────────────── length
//! ```
//!
//! That all three run at different rates **derived from the same counter** is
//! what gives the Game Boy its characteristic sound: there is no way to
//! desynchronise a vibrato from its envelope.

/// Length counter: turns the channel off after a fixed time.
///
/// Its maximum value depends on the channel —64 on the squares and the noise,
/// 256 on the wave one— because the register that loads it has 6 bits in some
/// and 8 in the other.
#[derive(Debug, Clone, Copy)]
pub struct LengthCounter {
    /// Bit 6 of `NRx4`: if cleared, the channel sounds indefinitely.
    pub enabled: bool,
    counter: u16,
    max: u16,
}

impl LengthCounter {
    pub const fn new(max: u16) -> Self {
        Self { enabled: false, counter: 0, max }
    }

    /// Writes `NRx1`. The register stores the time **already consumed**, so the
    /// counter starts at `max - value`.
    pub fn load(&mut self, value: u16) {
        self.counter = self.max - (value % self.max);
    }

    /// When the channel is triggered with the counter at zero, it is reloaded to
    /// the maximum. Without this, a channel triggered twice in a row would go
    /// silent the second time.
    pub fn trigger(&mut self) {
        if self.counter == 0 {
            self.counter = self.max;
        }
    }

    /// One sequencer step at 256 Hz. Returns `true` if the channel must be
    /// turned off.
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
/// It is what turns a flat square wave into a note with attack and decay. With
/// `period = 0` it is frozen: that is the way to ask for a constant volume.
#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeEnvelope {
    /// Initial volume (bits 7-4 of `NRx2`).
    pub initial: u8,
    /// Bit 3: `true` rises, `false` falls.
    pub increasing: bool,
    /// Bits 2-0: sequencer steps between each change.
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

    /// Writes `NRx2` and returns `false` if the channel goes mute.
    ///
    /// Initial volume 0 and a falling envelope means "the DAC is off": the
    /// channel is disconnected on the spot, it does not keep sounding at zero.
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

    /// One sequencer step at 64 Hz.
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
        // The envelope stops at the extremes; it does not wrap around.
        match (self.increasing, self.volume) {
            (true, v) if v < 15 => self.volume += 1,
            (false, v) if v > 0 => self.volume -= 1,
            _ => {}
        }
    }
}

/// Frequency sweep, exclusive to channel 1.
///
/// It shifts the frequency in steps proportional to itself, which produces
/// exponential glissandi: the laser-shot effect of half the console's library.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sweep {
    /// Bits 6-4 of `NR10`: sequencer steps between each shift.
    pub period: u8,
    /// Bit 3: `true` subtracts, `false` adds.
    pub decreasing: bool,
    /// Bits 2-0: how much the frequency shifts on each step.
    pub shift: u8,
    /// Copy of the frequency the sweep operates on.
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

    /// Starts the sweep with the channel's current frequency.
    ///
    /// Returns `false` if the channel must be turned off: the next frequency is
    /// computed **already at the trigger**, and if it overflows 2047 the channel
    /// goes silent before emitting a single sample.
    pub fn trigger(&mut self, frequency: u16) -> bool {
        self.shadow = frequency;
        self.timer = if self.period == 0 { 8 } else { self.period };
        self.enabled = self.period != 0 || self.shift != 0;

        if self.shift != 0 {
            return self.next_frequency() <= 2047;
        }
        true
    }

    /// Next frequency: the current one plus or minus itself, shifted.
    fn next_frequency(&self) -> u16 {
        let delta = self.shadow >> self.shift;
        if self.decreasing {
            self.shadow.saturating_sub(delta)
        } else {
            self.shadow + delta
        }
    }

    /// One sequencer step at 128 Hz.
    ///
    /// `Some(f)` is the new frequency to apply to the channel; `None` means
    /// nothing needs changing. The `bool` is `false` if the channel must be
    /// turned off because of an overflow.
    pub fn tick(&mut self) -> (Option<u16>, bool) {
        if self.timer > 0 {
            self.timer -= 1;
        }
        if self.timer != 0 {
            return (None, true);
        }

        // With period 0 the sweep does not advance, but the timer keeps
        // reloading to 8: that is what makes writing a different period take
        // effect on the next step.
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
        // It is computed a second time to check for overflow, but the result is
        // discarded. It is a hardware quirk, and the accuracy tests verify it.
        (Some(new), self.next_frequency() <= 2047)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_length_counts_down_from_the_maximum() {
        let mut l = LengthCounter::new(64);
        l.enabled = true;
        l.load(60); // 4 steps left

        for _ in 0..3 {
            assert!(!l.tick());
        }
        assert!(l.tick(), "the fourth step turns the channel off");
    }

    #[test]
    fn a_disabled_length_turns_nothing_off() {
        let mut l = LengthCounter::new(64);
        l.load(63);
        for _ in 0..100 {
            assert!(!l.tick());
        }
    }

    #[test]
    fn triggering_with_the_counter_exhausted_reloads_it() {
        let mut l = LengthCounter::new(64);
        l.enabled = true;
        l.load(63);
        assert!(l.tick(), "it runs out");

        l.trigger();
        assert!(!l.tick(), "after the trigger it has room again");
    }

    #[test]
    fn the_envelope_falls_and_stops_at_zero() {
        let mut e = VolumeEnvelope::new();
        e.write(0x21); // volume 2, falling, period 1
        e.trigger();
        assert_eq!(e.volume(), 2);

        e.tick();
        assert_eq!(e.volume(), 1);
        e.tick();
        assert_eq!(e.volume(), 0);
        e.tick();
        assert_eq!(e.volume(), 0, "it does not wrap around");
    }

    #[test]
    fn the_envelope_rises_and_stops_at_fifteen() {
        let mut e = VolumeEnvelope::new();
        e.write(0xE9); // volume 14, rising, period 1
        e.trigger();
        e.tick();
        assert_eq!(e.volume(), 15);
        e.tick();
        assert_eq!(e.volume(), 15);
    }

    #[test]
    fn with_period_zero_the_envelope_freezes() {
        let mut e = VolumeEnvelope::new();
        e.write(0x80); // volume 8, period 0
        e.trigger();
        for _ in 0..50 {
            e.tick();
        }
        assert_eq!(e.volume(), 8);
    }

    #[test]
    fn volume_zero_and_falling_turns_the_dac_off() {
        let mut e = VolumeEnvelope::new();
        assert!(!e.write(0x00), "volume 0 falling disconnects the channel");
        assert!(e.write(0x08), "volume 0 but rising does produce a signal");
        assert!(e.write(0x10));
    }

    #[test]
    fn the_sweep_raises_the_frequency_proportionally() {
        let mut s = Sweep::new();
        s.write(0x11); // period 1, rising, shift 1
        assert!(s.trigger(500));

        let (new, ok) = s.tick();
        assert!(ok);
        assert_eq!(new, Some(750), "500 + 500/2");
    }

    /// The hardware computes the next frequency **twice** per step: once to
    /// apply it and once only to check whether it would overflow. If the second
    /// goes past 2047, the channel turns off even though the first was valid.
    #[test]
    fn the_second_check_turns_the_channel_off_in_advance() {
        let mut s = Sweep::new();
        s.write(0x11); // period 1, rising, shift 1
        assert!(s.trigger(1000));

        let (new, ok) = s.tick();
        assert_eq!(new, Some(1500), "1500 is a valid frequency");
        assert!(!ok, "but the next one would be 2250, and that turns the channel off already");
    }

    #[test]
    fn a_falling_sweep_subtracts() {
        let mut s = Sweep::new();
        s.write(0x1A); // period 1, falling, shift 2
        s.trigger(1024);
        assert_eq!(s.tick().0, Some(768), "1024 - 1024/4");
    }

    #[test]
    fn the_overflow_turns_the_channel_off() {
        let mut s = Sweep::new();
        s.write(0x11);
        // 2000 + 1000 goes past 2047 already at the trigger.
        assert!(!s.trigger(2000), "the overflow is detected on trigger");
    }

    #[test]
    fn with_shift_zero_the_sweep_does_not_move_the_frequency() {
        let mut s = Sweep::new();
        s.write(0x10); // period 1, shift 0
        s.trigger(1000);
        assert_eq!(s.tick(), (None, true));
    }
}
