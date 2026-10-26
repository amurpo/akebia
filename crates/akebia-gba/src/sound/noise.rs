//! The fourth inherited channel: noise.
//!
//! It makes no periodic wave at all but a pseudorandom sequence, out of a
//! 15-bit shift register with feedback. It is the channel for percussion,
//! explosions and wind.
//!
//! ```text
//!    ┌──── XOR ◄───┬──────────┐
//!    ▼             │          │
//!  ┌────────────────────────────┐
//!  │ 14 13 12 … 3  2  1  0      │  ──► the output is bit 0, inverted
//!  └────────────────────────────┘
//!        ▲ bit 6 as well, in the short mode
//! ```
//!
//! The trick is the **short mode**: feeding the same bit back into bit 6 as
//! well drops the sequence from 32767 steps to 127. It stops sounding like
//! white noise and starts sounding metallic and pitched, which is where snares
//! and cymbals come from.

use super::components::{LengthCounter, VolumeEnvelope};

const MAX_LENGTH: u16 = 64;

/// Divisors, by the bottom three bits of the control register.
///
/// The first is not zero but half of the next, which is why they are a table
/// and not a multiplication. They are the older machine's, multiplied out to
/// this clock in [`Noise::period`].
const DIVISORS: [u32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];

/// This clock against the one these divisors were written for.
const CLOCK_RATIO: i32 = 4;

#[derive(Clone)]
pub struct Noise {
    pub enabled: bool,
    dac_enabled: bool,

    /// The shift register. It starts with every bit set.
    lfsr: u16,
    /// Whether bit 6 is fed back as well.
    short_mode: bool,
    clock_shift: u8,
    divisor_code: u8,
    timer: i32,

    length: LengthCounter,
    envelope: VolumeEnvelope,
}

impl Default for Noise {
    fn default() -> Self {
        Self::new()
    }
}

impl Noise {
    pub const fn new() -> Self {
        Self {
            enabled: false,
            dac_enabled: false,
            lfsr: 0x7FFF,
            short_mode: false,
            clock_shift: 0,
            divisor_code: 0,
            timer: 0,
            length: LengthCounter::new(MAX_LENGTH),
            envelope: VolumeEnvelope::new(),
        }
    }

    fn period(&self) -> i32 {
        (DIVISORS[self.divisor_code as usize] << self.clock_shift) as i32 * CLOCK_RATIO
    }

    pub fn tick(&mut self, cycles: u32) {
        // A shift of fourteen or fifteen stops the channel in practice, and
        // the hardware makes nothing useful there either.
        if self.clock_shift >= 14 {
            return;
        }

        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.step();
        }
    }

    fn step(&mut self) {
        let feedback = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
        self.lfsr >>= 1;
        self.lfsr |= feedback << 14;
        if self.short_mode {
            self.lfsr = (self.lfsr & !(1 << 6)) | (feedback << 6);
        }
    }

    pub fn sample(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        // The output is bit 0 **inverted**.
        if self.lfsr & 1 == 0 { self.envelope.volume() } else { 0 }
    }

    /// What the DAC puts out, from -1 to 1. See [`super::square::Square`].
    pub fn dac_output(&self) -> f32 {
        if !self.dac_enabled {
            return 0.0;
        }
        f32::from(self.sample()) / 7.5 - 1.0
    }

    pub fn tick_length(&mut self) {
        if self.length.tick() {
            self.enabled = false;
        }
    }

    pub fn tick_envelope(&mut self) {
        self.envelope.tick();
    }

    // ---- Registers ---------------------------------------------------------

    /// The length load, which does not read back.
    pub fn write_length(&mut self, value: u8) {
        self.length.load(u16::from(value & 0x3F));
    }

    pub fn write_envelope(&mut self, value: u8) {
        self.dac_enabled = self.envelope.write(value);
        if !self.dac_enabled {
            self.enabled = false;
        }
    }

    pub fn read_envelope(&self) -> u8 {
        (self.envelope.initial << 4)
            | (u8::from(self.envelope.increasing) << 3)
            | self.envelope.period
    }

    /// How fast it runs and how long the sequence is.
    pub fn write_frequency(&mut self, value: u8) {
        self.clock_shift = value >> 4;
        self.short_mode = value & 0x08 != 0;
        self.divisor_code = value & 0x07;
    }

    pub fn read_frequency(&self) -> u8 {
        (self.clock_shift << 4) | (u8::from(self.short_mode) << 3) | self.divisor_code
    }

    pub fn write_control(&mut self, value: u8) {
        self.length.enabled = value & 0x40 != 0;
        if value & 0x80 != 0 {
            self.trigger();
        }
    }

    pub fn read_control(&self) -> u8 {
        u8::from(self.length.enabled) << 6
    }

    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        // Every bit set again. Without this, two shots of the same effect come
        // out different, because the second starts wherever the first stopped.
        self.lfsr = 0x7FFF;
        self.length.trigger();
        self.envelope.trigger();
    }

    pub fn power_off(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sounding() -> Noise {
        let mut noise = Noise::new();
        noise.write_envelope(0xF0);
        noise.write_frequency(0x00);
        noise.write_control(0x80);
        noise
    }

    /// The long sequence takes 32767 steps to come round, and that is what
    /// makes it sound like noise rather than like a tone.
    #[test]
    fn the_long_sequence_takes_a_long_time_to_repeat() {
        let mut noise = sounding();
        let start = noise.lfsr;
        let mut steps = 0;
        loop {
            noise.step();
            steps += 1;
            if noise.lfsr == start {
                break;
            }
            assert!(steps < 40_000, "it never came round");
        }
        assert_eq!(steps, 32_767);
    }

    /// The short one comes round in 127, which is short enough to be heard as
    /// a pitch. It is where the metallic percussion comes from.
    #[test]
    fn the_short_sequence_comes_round_soon_enough_to_be_a_pitch() {
        let mut noise = sounding();
        noise.write_frequency(0x08); // short mode
        // Long enough to be on the cycle. The register starts with every bit
        // set, and in short mode that state is not on the loop it falls into:
        // measuring from it would be measuring the way in, not the way round.
        for _ in 0..200 {
            noise.step();
        }
        let start = noise.lfsr;
        let mut steps = 0;
        loop {
            noise.step();
            steps += 1;
            if noise.lfsr == start {
                break;
            }
            assert!(steps < 1_000, "it never came round");
        }
        assert_eq!(steps, 127);
    }

    /// The output is bit 0 **inverted**, so a register full of ones is silence
    /// and not full volume. Getting it the right way round matters because the
    /// register starts full of ones.
    #[test]
    fn the_output_is_the_bottom_bit_inverted() {
        let mut noise = sounding();
        assert_eq!(noise.lfsr & 1, 1);
        assert_eq!(noise.sample(), 0, "a one at the bottom is silence");

        while noise.lfsr & 1 == 1 {
            noise.step();
        }
        assert_eq!(noise.sample(), 15);
    }

    /// The period counts in this machine's cycles, four to each of the ones
    /// these divisors were written against.
    #[test]
    fn the_period_is_measured_in_this_machines_cycles() {
        let mut noise = Noise::new();
        noise.write_frequency(0x34); // shift 3, divisor code 4
        assert_eq!(noise.period(), (64 << 3) * 4);
    }

    /// A trigger sets every bit again, so the same effect fired twice sounds
    /// the same twice.
    #[test]
    fn a_trigger_starts_the_sequence_over() {
        let mut noise = sounding();
        for _ in 0..50 {
            noise.step();
        }
        assert_ne!(noise.lfsr, 0x7FFF);
        noise.write_control(0x80);
        assert_eq!(noise.lfsr, 0x7FFF);
    }

    /// A shift that high stops the channel rather than running it impossibly
    /// fast.
    #[test]
    fn the_highest_shifts_stop_it() {
        let mut noise = sounding();
        noise.write_frequency(0xE0); // shift 14
        let before = noise.lfsr;
        noise.tick(1_000_000);
        assert_eq!(noise.lfsr, before);
    }
}
