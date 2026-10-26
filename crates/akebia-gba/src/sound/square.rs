//! The first two inherited channels: square waves.
//!
//! They are the same but for the frequency sweep, which only the first has, so
//! they are one type with an optional sweep rather than two files. Two nearly
//! identical types guarantee a fix applied to one and forgotten in the other.
//!
//! # The duty cycle
//!
//! A square here need not be symmetric: two bits pick one of four eight-step
//! patterns. Changing it alters the timbre without touching the volume or the
//! pitch, and it is the usual way to tell two voices playing the same note
//! apart.
//!
//! ```text
//!   12.5 %   ▁▁▁▁▁▁▁█
//!   25 %     █▁▁▁▁▁▁█
//!   50 %     █▁▁▁▁███
//!   75 %     ▁██████▁
//! ```

use super::components::{LengthCounter, Sweep, VolumeEnvelope};

/// The four patterns, as eight-bit masks.
const DUTY_PATTERNS: [u8; 4] = [0b0000_0001, 0b1000_0001, 0b1000_0111, 0b0111_1110];

const MAX_LENGTH: u16 = 64;

/// Cycles in one step of the pattern, per unit of the frequency register.
///
/// The older machine counts four of its own; this clock is four times as fast,
/// so it is sixteen of these. Getting this wrong does not distort a note, it
/// transposes every note in the game by two octaves.
const STEP_CYCLES: i32 = 16;

#[derive(Clone)]
pub struct Square {
    /// Whether the channel is sounding.
    pub enabled: bool,
    /// A DAC that is off disconnects the channel from the mixer even while the
    /// channel is enabled.
    dac_enabled: bool,

    /// Eleven bits. The period is *longer* the smaller this is: the value
    /// counts up towards 2048.
    frequency: u16,
    /// Cycles until the pattern steps.
    timer: i32,
    duty: u8,
    /// Where in the eight steps it is.
    duty_step: u8,

    length: LengthCounter,
    envelope: VolumeEnvelope,
    /// Only the first channel has one.
    sweep: Option<Sweep>,
}

impl Square {
    /// The first channel, which sweeps.
    pub const fn with_sweep() -> Self {
        Self::build(Some(Sweep::new()))
    }

    /// The second, which does not.
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

    fn period(&self) -> i32 {
        (2048 - i32::from(self.frequency)) * STEP_CYCLES
    }

    pub fn tick(&mut self, cycles: u32) {
        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.duty_step = (self.duty_step + 1) % 8;
        }
    }

    /// What the channel is on, from 0 to 15.
    pub fn sample(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        let on = DUTY_PATTERNS[self.duty as usize] >> self.duty_step & 1;
        on * self.envelope.volume()
    }

    /// What the DAC puts out, from -1 to 1.
    ///
    /// A DAC that is off is disconnected and sits at the middle; one that is on
    /// with a sample of zero sits at the bottom. [`Square::sample`] collapses
    /// those two into the same zero, so the mixer has to ask this instead.
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
        if let Some(frequency) = new_frequency {
            self.frequency = frequency;
        }
        if !keep_on {
            self.enabled = false;
        }
    }

    // ---- Registers ---------------------------------------------------------

    /// The sweep register, which only the first channel answers to.
    pub fn write_sweep(&mut self, value: u8) {
        if let Some(sweep) = &mut self.sweep {
            sweep.write(value);
        }
    }

    pub fn read_sweep(&self) -> u8 {
        match &self.sweep {
            Some(sweep) => (sweep.period << 4) | (u8::from(sweep.decreasing) << 3) | sweep.shift,
            None => 0,
        }
    }

    /// Duty cycle and the length load.
    pub fn write_duty_length(&mut self, value: u8) {
        self.duty = value >> 6;
        self.length.load(u16::from(value & 0x3F));
    }

    /// Only the duty comes back: the length that was loaded is gone.
    pub fn read_duty(&self) -> u8 {
        self.duty << 6
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

    /// The low eight bits of the frequency, which do not read back.
    pub fn write_frequency_low(&mut self, value: u8) {
        self.frequency = (self.frequency & 0x0700) | u16::from(value);
    }

    /// The high three bits, whether the length stops it, and the trigger.
    pub fn write_control(&mut self, value: u8) {
        self.frequency = (self.frequency & 0x00FF) | (u16::from(value & 0x07) << 8);
        self.length.enabled = value & 0x40 != 0;
        if value & 0x80 != 0 {
            self.trigger();
        }
    }

    pub fn read_control(&self) -> u8 {
        u8::from(self.length.enabled) << 6
    }

    /// Starts the channel from the beginning.
    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        self.length.trigger();
        self.envelope.trigger();

        if let Some(sweep) = &mut self.sweep {
            if !sweep.trigger(self.frequency) {
                self.enabled = false;
            }
        }
    }

    /// What the master switch does to it: everything back to nothing.
    pub fn power_off(&mut self) {
        *self = if self.sweep.is_some() { Self::with_sweep() } else { Self::plain() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sounding() -> Square {
        let mut square = Square::plain();
        square.write_envelope(0xF0); // full volume, no envelope movement
        square.write_duty_length(0x80); // half duty
        square.write_frequency_low(0x00);
        square.write_control(0x87); // frequency 0x700, triggered
        square
    }

    /// A channel with no DAC is disconnected, and disconnected is the middle
    /// rather than the bottom. Getting that wrong is a click every time a
    /// channel stops.
    #[test]
    fn a_channel_with_no_dac_sits_in_the_middle_and_not_at_the_bottom() {
        let mut square = Square::plain();
        square.write_envelope(0x00);
        assert_eq!(square.dac_output(), 0.0);

        let quiet = sounding();
        assert_ne!(quiet.dac_output(), 0.0, "a channel that is on is somewhere else");
    }

    /// The pattern is what the timbre is. Half of the eight steps are on at
    /// 50%, one at 12.5%.
    #[test]
    fn the_duty_says_how_much_of_the_cycle_is_on() {
        for (duty, expected) in [(0u8, 1), (1, 2), (2, 4), (3, 6)] {
            let mut square = Square::plain();
            square.write_envelope(0xF0);
            square.write_duty_length(duty << 6);
            square.write_control(0x80);
            let on = (0..8)
                .filter(|_| {
                    let sample = square.sample();
                    square.tick(square.period() as u32);
                    sample > 0
                })
                .count();
            assert_eq!(on, expected, "duty {duty}");
        }
    }

    /// The period is sixteen cycles per unit and not four: this machine's clock
    /// is four times the one these channels were designed for, and the mistake
    /// would transpose the whole game by two octaves.
    #[test]
    fn the_period_is_measured_in_this_machines_cycles() {
        let mut square = Square::plain();
        square.write_frequency_low(0x00);
        square.write_control(0x86); // frequency 0x600 = 1536
        assert_eq!(square.period(), (2048 - 1536) * 16);

        // 2048 - 1536 = 512, times 16 is 8192 cycles a step, 65536 a cycle of
        // eight — which is 256 Hz on a 16.78 MHz clock.
        assert_eq!(crate::CLOCK_HZ / (square.period() as u32 * 8), 256);
    }

    /// Triggering does not restart the pattern, unlike the wave channel. A
    /// square carries on from where it was.
    #[test]
    fn a_trigger_leaves_the_pattern_where_it_was() {
        let mut square = sounding();
        square.tick(square.period() as u32 * 3);
        let step = square.duty_step;
        square.write_control(0x87);
        assert_eq!(square.duty_step, step);
    }

    /// The length turns the channel off, and only when it is enabled.
    #[test]
    fn the_length_stops_the_channel_when_it_is_asked_to() {
        let mut square = sounding();
        square.write_duty_length(0x3F); // one step left
        square.write_control(0xC7); // triggered, length enabled
        assert!(square.enabled);
        square.tick_length();
        assert!(!square.enabled);
    }

    #[test]
    fn without_the_length_bit_it_sounds_on() {
        let mut square = sounding();
        square.write_duty_length(0x3F);
        square.write_control(0x87); // triggered, length not enabled
        for _ in 0..200 {
            square.tick_length();
        }
        assert!(square.enabled);
    }

    /// The sweep is the first channel's alone, and the second must not grow one
    /// by accident.
    #[test]
    fn only_the_first_channel_sweeps() {
        let mut plain = Square::plain();
        plain.write_envelope(0xF0);
        plain.write_frequency_low(0x00);
        plain.write_sweep(0x11);
        plain.write_control(0x84);
        let before = plain.frequency;
        plain.tick_sweep();
        assert_eq!(plain.frequency, before);

        let mut swept = Square::with_sweep();
        swept.write_envelope(0xF0);
        swept.write_sweep(0x11);
        swept.write_frequency_low(0x00);
        swept.write_control(0x84);
        swept.tick_sweep();
        assert_ne!(swept.frequency, before, "and this one moved");
    }

    /// The master switch takes the channel back to nothing.
    #[test]
    fn the_power_off_leaves_nothing_sounding() {
        let mut square = sounding();
        assert!(square.enabled);
        square.power_off();
        assert!(!square.enabled);
        assert_eq!(square.dac_output(), 0.0);
    }
}
