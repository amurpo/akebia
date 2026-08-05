//! Channel 4: noise.
//!
//! It does not generate a periodic wave but a pseudorandom sequence, with a
//! 15-bit **LFSR** (linear feedback shift register). It is the channel for
//! percussion, explosions and wind.
//!
//! ```text
//!    ┌──── XNOR ◄──┬──────────┐
//!    ▼             │          │
//!  ┌────────────────────────────┐
//!  │ 14 13 12 … 3  2  1  0      │  ──► output (bit 0 inverted)
//!  └────────────────────────────┘
//!        ▲ bit 6 if the mode is 7-bit
//! ```
//!
//! The trick is the **7-bit mode**: by also feeding back bit 6, the period of
//! the sequence drops from 32767 to 127 steps. It stops sounding like white
//! noise and starts sounding metallic and tonal, which is how snare and cymbal
//! timbres are obtained.

use super::components::{LengthCounter, VolumeEnvelope};

const MAX_LENGTH: u16 = 64;

/// Clock divisors, indexed by bits 2-0 of `NR43`.
///
/// Code 0 is not 0 but 8: the hardware uses half a divisor, and all the others
/// are multiples of 16.
const DIVISORS: [u32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];

#[derive(Clone)]
pub struct Noise {
    pub enabled: bool,
    dac_enabled: bool,

    /// Shift register. It starts with every bit set to 1.
    lfsr: u16,
    /// Bit 3 of `NR43`: also feed back bit 6.
    short_mode: bool,
    /// Bits 7-4 of `NR43`.
    clock_shift: u8,
    /// Bits 2-0 of `NR43`.
    divisor_code: u8,
    timer: i32,

    length: LengthCounter,
    envelope: VolumeEnvelope,
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
        (DIVISORS[self.divisor_code as usize] << self.clock_shift) as i32
    }

    pub fn tick(&mut self, t_cycles: u32) {
        // A shift of 14 or 15 stops the channel in practice; the hardware does
        // not produce anything useful there either.
        if self.clock_shift >= 14 {
            return;
        }

        self.timer -= t_cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.step_lfsr();
        }
    }

    fn step_lfsr(&mut self) {
        let feedback = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
        self.lfsr >>= 1;
        self.lfsr |= feedback << 14;
        if self.short_mode {
            // In short mode the feedback also enters through bit 6.
            self.lfsr = (self.lfsr & !(1 << 6)) | (feedback << 6);
        }
    }

    pub fn sample(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        // The output is bit 0 **inverted**.
        if self.lfsr & 1 == 0 {
            self.envelope.volume()
        } else {
            0
        }
    }

    /// Analogue DAC output, from -1.0 to 1.0. See `Square::dac_output`.
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

    /// `NR41`: length load. The high bits are unused.
    pub fn write_length(&mut self, value: u8) {
        self.length.load(u16::from(value & 0x3F));
    }

    /// `NR42`: envelope.
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

    /// `NR43`: noise parameters.
    pub fn write_polynomial(&mut self, value: u8) {
        self.clock_shift = value >> 4;
        self.short_mode = value & 0x08 != 0;
        self.divisor_code = value & 0x07;
    }

    pub fn read_polynomial(&self) -> u8 {
        (self.clock_shift << 4) | (u8::from(self.short_mode) << 3) | self.divisor_code
    }

    /// `NR44`: length and trigger. This channel has no frequency.
    pub fn write_control(&mut self, value: u8) {
        self.length.enabled = value & 0x40 != 0;
        if value & 0x80 != 0 {
            self.trigger();
        }
    }

    pub fn read_control(&self) -> u8 {
        0xBF | (u8::from(self.length.enabled) << 6)
    }

    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        // Every bit set to 1: if it started at zero, the LFSR would stay stuck
        // there forever, because 0 is a fixed point of the feedback.
        self.lfsr = 0x7FFF;
        self.length.trigger();
        self.envelope.trigger();
    }

    pub fn power_off(&mut self) {
        let length_enabled = self.length.enabled;
        *self = Self::new();
        self.length.enabled = length_enabled;
    }
}

impl Default for Noise {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn channel() -> Noise {
        let mut n = Noise::new();
        n.write_envelope(0xF0); // volume 15, no envelope
        n.write_polynomial(0x00); // divisor 8, no shift
        n.write_control(0x80);
        n
    }

    #[test]
    fn the_lfsr_produces_a_varied_sequence() {
        let mut n = channel();
        let mut outputs = Vec::new();
        for _ in 0..64 {
            outputs.push(n.sample());
            n.tick(8);
        }
        let distinct: HashSet<_> = outputs.iter().collect();
        assert_eq!(distinct.len(), 2, "it alternates between 0 and the volume");
        // With real noise the sequence is not periodic within 64 steps.
        assert_ne!(outputs[..32], outputs[32..], "it does not repeat that soon");
    }

    /// The output sequence in short mode repeats every 127 steps.
    ///
    /// It is measured on the **output**, not on the whole register: in 7-bit
    /// mode the high bits keep shifting and the 15-bit value does not return to
    /// its starting point, but what is heard is indeed periodic. That short
    /// period is exactly what makes it sound metallic instead of noisy.
    #[test]
    fn the_short_mode_shortens_the_period_of_the_sequence() {
        let sequence = |short: bool, steps: usize| {
            let mut n = Noise::new();
            n.write_envelope(0xF0);
            n.write_polynomial(if short { 0x08 } else { 0x00 });
            n.write_control(0x80);
            (0..steps)
                .map(|_| {
                    let bit = n.sample() > 0;
                    n.tick(8);
                    bit
                })
                .collect::<Vec<_>>()
        };

        let short = sequence(true, 254);
        assert_eq!(short[..127], short[127..], "the 7-bit mode repeats every 127");

        let long = sequence(false, 254);
        assert_ne!(long[..127], long[127..], "the 15-bit one does not");
    }

    #[test]
    fn the_divisor_and_the_shift_set_the_period() {
        let mut n = channel();
        n.write_polynomial(0x00); // divisor 8, shift 0
        assert_eq!(n.period(), 8);
        n.write_polynomial(0x03); // divisor 48
        assert_eq!(n.period(), 48);
        n.write_polynomial(0x23); // divisor 48, shift 2
        assert_eq!(n.period(), 48 * 4);
    }

    #[test]
    fn the_trigger_resets_the_lfsr() {
        let mut n = channel();
        n.tick(8 * 100);
        assert_ne!(n.lfsr, 0x7FFF);
        n.write_control(0x80);
        assert_eq!(n.lfsr, 0x7FFF, "it starts with every bit set to 1");
    }

    #[test]
    fn turning_the_dac_off_turns_the_channel_off() {
        let mut n = channel();
        assert!(n.enabled);
        n.write_envelope(0x00);
        assert!(!n.enabled);
    }
}
