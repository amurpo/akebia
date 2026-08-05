//! Channels 1 and 2: square waves.
//!
//! They are identical except for the frequency sweep, which only channel 1 has.
//! Here they are modelled with the same struct and an `Option<Sweep>`, instead
//! of duplicating the file: the alternative —two nearly identical types—
//! guarantees that a fix gets applied to one and forgotten in the other.
//!
//! # The duty cycle
//!
//! A Game Boy square wave is not necessarily symmetric: two bits choose between
//! four 8-step patterns. Changing the duty cycle alters the timbre without
//! touching the volume or the frequency, and it is the most used trick to tell
//! apart two voices playing the same note.
//!
//! ```text
//!   12.5 %   ▁▁▁▁▁▁▁█
//!   25 %     █▁▁▁▁▁▁█
//!   50 %     █▁▁▁▁███
//!   75 %     ▁████████▁   (the complement of 25 %)
//! ```

use super::components::{LengthCounter, Sweep, VolumeEnvelope};

/// The four patterns, as 8-bit masks.
const DUTY_PATTERNS: [u8; 4] = [0b0000_0001, 0b1000_0001, 0b1000_0111, 0b0111_1110];

/// Maximum counter length on the square channels.
const MAX_LENGTH: u16 = 64;

#[derive(Clone)]
pub struct Square {
    /// `true` if the channel is sounding.
    pub enabled: bool,
    /// A disabled DAC disconnects the channel from the mixer even when it is
    /// enabled.
    dac_enabled: bool,

    /// 11-bit frequency. The real period is `(2048 - frequency) * 4`.
    frequency: u16,
    /// T-cycles until the next step of the pattern.
    timer: i32,
    /// Active pattern (0..3).
    duty: u8,
    /// Position within the pattern (0..7).
    duty_step: u8,

    length: LengthCounter,
    envelope: VolumeEnvelope,
    /// Only channel 1 has it.
    sweep: Option<Sweep>,
}

impl Square {
    /// Channel 1: with frequency sweep.
    pub const fn with_sweep() -> Self {
        Self::build(Some(Sweep::new()))
    }

    /// Channel 2: without sweep.
    pub const fn plain() -> Self {
        Self::build(None)
    }

    const fn build(sweep: Option<Sweep>) -> Self {
        Self {
            enabled: false,
            dac_enabled: false,
            frequency: 0,
            timer: 0,
            duty: 0,
            duty_step: 0,
            length: LengthCounter::new(MAX_LENGTH),
            envelope: VolumeEnvelope::new(),
            sweep,
        }
    }

    /// Period in T-cycles. A high frequency in the register is a **short**
    /// period: the value counts up towards 2048.
    fn period(&self) -> i32 {
        (2048 - i32::from(self.frequency)) * 4
    }

    pub fn tick(&mut self, t_cycles: u32) {
        self.timer -= t_cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.duty_step = (self.duty_step + 1) % 8;
        }
    }

    /// Current sample, from 0 to 15.
    pub fn sample(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        let on = DUTY_PATTERNS[self.duty as usize] >> self.duty_step & 1;
        on * self.envelope.volume()
    }

    /// Analogue DAC output, from -1.0 to 1.0.
    ///
    /// A disabled DAC is disconnected and delivers 0 V, the centre. An enabled
    /// one with digital sample 0 delivers the negative extreme. `sample()`
    /// collapses both cases into the same 0, so the mix has to use this method.
    pub fn dac_output(&self) -> f32 {
        if !self.dac_enabled {
            return 0.0;
        }
        f32::from(self.sample()) / 7.5 - 1.0
    }

    // ---- Sequencer steps ---------------------------------------------------

    pub fn tick_length(&mut self) {
        if self.length.tick() {
            self.enabled = false;
        }
    }

    pub fn tick_envelope(&mut self) {
        self.envelope.tick();
    }

    pub fn tick_sweep(&mut self) {
        let Some(sweep) = &mut self.sweep else { return };
        let (new_frequency, keep_on) = sweep.tick();
        if let Some(f) = new_frequency {
            self.frequency = f;
        }
        if !keep_on {
            self.enabled = false;
        }
    }

    // ---- Registers ---------------------------------------------------------

    /// `NR10`, on channel 1 only.
    pub fn write_sweep(&mut self, value: u8) {
        if let Some(sweep) = &mut self.sweep {
            sweep.write(value);
        }
    }

    pub fn read_sweep(&self) -> u8 {
        match &self.sweep {
            Some(s) => 0x80 | (s.period << 4) | (u8::from(s.decreasing) << 3) | s.shift,
            None => 0xFF,
        }
    }

    /// `NRx1`: duty cycle and length load.
    pub fn write_duty_length(&mut self, value: u8) {
        self.duty = value >> 6;
        self.length.load(u16::from(value & 0x3F));
    }

    pub fn read_duty(&self) -> u8 {
        // The loaded length cannot be read back: only the duty cycle can.
        0x3F | (self.duty << 6)
    }

    /// `NRx2`: volume envelope.
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

    /// `NRx3`: the low 8 bits of the frequency.
    pub fn write_frequency_low(&mut self, value: u8) {
        self.frequency = (self.frequency & 0x0700) | u16::from(value);
    }

    /// `NRx4`: the high 3 bits, the length enable and the trigger.
    pub fn write_control(&mut self, value: u8) {
        self.frequency = (self.frequency & 0x00FF) | (u16::from(value & 0x07) << 8);
        self.length.enabled = value & 0x40 != 0;
        if value & 0x80 != 0 {
            self.trigger();
        }
    }

    pub fn read_control(&self) -> u8 {
        0xBF | (u8::from(self.length.enabled) << 6)
    }

    /// Bit 7 of `NRx4`: starts the channel from scratch.
    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        self.length.trigger();
        self.envelope.trigger();

        if let Some(sweep) = &mut self.sweep
            && !sweep.trigger(self.frequency)
        {
            self.enabled = false;
        }
    }

    /// Turns the channel off when the APU loses power (`NR52`).
    pub fn power_off(&mut self) {
        let length_enabled = self.length.enabled;
        *self = if self.sweep.is_some() { Self::with_sweep() } else { Self::plain() };
        // The length counter survives the power-off on DMG hardware.
        self.length.enabled = length_enabled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Channel ready to sound: fixed volume 15, 50 % duty cycle.
    fn channel() -> Square {
        let mut c = Square::plain();
        c.write_duty_length(0b1000_0000); // duty 2 (50 %)
        c.write_envelope(0xF0); // volume 15, no envelope
        c.write_frequency_low(0x00);
        c.write_control(0x87); // high frequency 7 + trigger
        c
    }

    #[test]
    fn the_trigger_turns_the_channel_on() {
        let c = channel();
        assert!(c.enabled);
    }

    #[test]
    fn the_wave_alternates_between_zero_and_the_volume() {
        let mut c = channel();
        let period = (2048 - 0x0700) * 4;

        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            seen.insert(c.sample());
            c.tick(period as u32);
        }
        assert_eq!(seen, [0, 15].into_iter().collect());
    }

    #[test]
    fn the_duty_cycle_changes_the_ratio() {
        let mut c = channel();
        let period = ((2048 - 0x0700) * 4) as u32;

        let count_highs = |c: &mut Square| {
            (0..8)
                .filter(|_| {
                    let high = c.sample() > 0;
                    c.tick(period);
                    high
                })
                .count()
        };

        c.write_duty_length(0b0000_0000); // 12.5 %
        assert_eq!(count_highs(&mut c), 1);
        c.write_duty_length(0b1000_0000); // 50 %
        assert_eq!(count_highs(&mut c), 4);
        c.write_duty_length(0b1100_0000); // 75 %
        assert_eq!(count_highs(&mut c), 6);
    }

    #[test]
    fn a_higher_frequency_shortens_the_period() {
        let mut c = Square::plain();
        c.write_envelope(0xF0);
        c.write_frequency_low(0x00);
        c.write_control(0x80); // frequency 0
        let slow = c.period();

        c.write_frequency_low(0xFF);
        c.write_control(0x87); // frequency 2047
        assert!(c.period() < slow, "more value in the register, shorter period");
    }

    #[test]
    fn turning_the_dac_off_turns_the_channel_off() {
        let mut c = channel();
        assert!(c.enabled);
        c.write_envelope(0x00); // volume 0 and falling
        assert!(!c.enabled);
        assert_eq!(c.sample(), 0);
    }

    #[test]
    fn the_length_ends_up_turning_the_channel_off() {
        let mut c = channel();
        c.write_duty_length(0b1011_1111); // length 63 → 1 step left
        c.write_control(0xC7); // trigger with length enabled
        assert!(c.enabled);
        c.tick_length();
        assert!(!c.enabled);
    }

    #[test]
    fn the_sweep_only_exists_on_channel_one() {
        assert_eq!(Square::plain().read_sweep(), 0xFF);
        assert_ne!(Square::with_sweep().read_sweep(), 0xFF);
    }

    #[test]
    fn the_sweep_shifts_the_channel_frequency() {
        let mut c = Square::with_sweep();
        c.write_envelope(0xF0);
        c.write_sweep(0x11); // period 1, rising, shift 1
        c.write_frequency_low(0xE8);
        c.write_control(0x83); // frequency 1000, trigger
        assert_eq!(c.frequency, 1000);

        c.tick_sweep();
        assert_eq!(c.frequency, 1500);
    }
}
