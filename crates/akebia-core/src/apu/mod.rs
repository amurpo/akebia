//! The APU: audio generation.
//!
//! Four channels —two squares, one table and one noise— mixed in stereo. None of
//! them knows how to play a note: each produces a number from 0 to 15 that
//! changes thousands of times per second, and the music comes from modulating
//! those numbers with the three blocks in [`components`].
//!
//! # The frame sequencer
//!
//! The modulators do not run off the audio clock but off a 512 Hz counter that
//! **is derived from the timer's `DIV` register**, not from a divider of its
//! own. That dependency looks like an implementation detail and is not: writing
//! to `DIV` zeroes the counter, and with it the sequencer, so a game can alter
//! the pace of its envelopes by touching the timer. Some sound engines do it on
//! purpose.
//!
//! ```text
//!   timer's internal 16-bit counter
//!            │
//!            └── bit 12 (or 13 at double speed)
//!                     │  falling edge
//!                     ▼
//!               frame sequencer, 512 Hz
//! ```
//!
//! # From the console clock to the sound card
//!
//! The APU changes value at 4.19 MHz and a sound card wants 48 kHz: it has to be
//! resampled by a factor of ~87. Taking one sample out of every 87 produces
//! audible aliasing, so here everything that happens between two output samples
//! is **averaged**. It is a box filter, the cheapest one that works.
//!
//! Afterwards, a high-pass filter removes the DC component. It is not cosmetic:
//! without it, a silent channel with its DAC on leaves the signal offset from
//! zero, and every time a channel turns on or off there is an audible click. The
//! real hardware does it with a capacitor on the output.
//!
//! Last comes a low-pass filter, which the hardware does not apply on purpose
//! either: it is what the amplifier and the speaker cannot reproduce. Four
//! square waves with no treble rolled off sound harsher than the console ever
//! did, and the box filter above leaves enough aliasing to be heard. See
//! [`SPEAKER_CUTOFF_HZ`].

pub mod components;
mod noise;
mod square;
mod wave;

pub use noise::Noise;
pub use square::Square;
pub use wave::{Wave, WAVE_RAM_SIZE};

use crate::model::Model;

/// Sample rate the APU starts with if nobody says otherwise.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// Frame sequencer steps before it repeats.
const SEQUENCER_STEPS: u8 = 8;

/// High-pass filter constant, per T-cycle.
///
/// It is the DMG capacitor's. On a CGB the filter is more aggressive
/// (0.999958), but the difference only shows up in a spectral analysis.
const HIGH_PASS_FACTOR: f32 = 0.999_958;

/// Cutoff of the low-pass that stands in for the amplifier and the speaker.
///
/// It is not measured off a console: it is the point where the output stops
/// sounding harsh without going muffled. Lower values (4 kHz) do sound like the
/// built-in speaker, but through headphones they come out dull.
pub const SPEAKER_CUTOFF_HZ: f32 = 8_000.0;

/// A stereo sample, normalised to `-1.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StereoSample {
    pub left: f32,
    pub right: f32,
}

pub struct Apu {
    channel1: Square,
    channel2: Square,
    channel3: Wave,
    channel4: Noise,

    /// Bit 7 of `NR52`. With the APU off, every register except this one and the
    /// wave RAM is zeroed and writes are ignored.
    powered: bool,

    /// `NR50`: master volume of each side (0..7) and the `VIN` inputs, which no
    /// commercial cartridge ever used.
    left_volume: u8,
    right_volume: u8,
    vin_left: bool,
    vin_right: bool,

    /// `NR51`: which channel sounds on which side. One bit per channel and side.
    panning: u8,

    /// Current frame sequencer step (0..7).
    sequencer_step: u8,
    /// Previous level of the `DIV` bit that pulses it, to detect the edge.
    previous_div_bit: bool,

    // ---- Resampling --------------------------------------------------------
    /// T-cycles per output sample, in 16-bit fixed point so that rounding error
    /// does not accumulate over a play session.
    cycles_per_sample: u32,
    /// Fraction of a T-cycle accumulated towards the next sample.
    sample_accumulator: u32,
    /// Sum of everything that happened since the last emitted sample, and how
    /// many contributions it holds: the box filter.
    box_sum: (f32, f32),
    box_count: u32,
    /// High-pass filter state, one per stereo channel.
    capacitor: (f32, f32),
    /// Low-pass filter state, one per stereo channel.
    speaker: (f32, f32),
    /// Weight of the new sample in the low-pass, derived from the sample rate.
    /// `1.0` disables the filter by letting the input straight through.
    speaker_alpha: f32,
    /// Whether the low-pass is wanted at all, kept apart from `speaker_alpha` so
    /// that changing the sample rate does not switch the filter back on.
    speaker_enabled: bool,
    /// Samples ready for the frontend.
    output: Vec<StereoSample>,
}

/// Coefficient of a one-pole low-pass at `cutoff`, for a given sample rate.
///
/// It is the step response of an RC circuit: `1 - e^(-2π·fc/fs)`. Above Nyquist
/// it would exceed 1 and the filter would ring, so it is clamped.
fn low_pass_alpha(cutoff: f32, sample_rate: u32) -> f32 {
    let fs = sample_rate.max(1) as f32;
    (1.0 - (-2.0 * core::f32::consts::PI * cutoff / fs).exp()).clamp(0.0, 1.0)
}

impl Apu {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            channel1: Square::with_sweep(),
            channel2: Square::plain(),
            channel3: Wave::new(),
            channel4: Noise::new(),
            powered: false,
            left_volume: 0,
            right_volume: 0,
            vin_left: false,
            vin_right: false,
            panning: 0,
            sequencer_step: 0,
            previous_div_bit: false,
            cycles_per_sample: (crate::CLOCK_HZ << 8) / sample_rate.max(1),
            sample_accumulator: 0,
            box_sum: (0.0, 0.0),
            box_count: 0,
            capacitor: (0.0, 0.0),
            speaker: (0.0, 0.0),
            speaker_alpha: low_pass_alpha(SPEAKER_CUTOFF_HZ, sample_rate),
            speaker_enabled: true,
            output: Vec::new(),
        }
    }

    /// Changes the sample rate. Discards whatever was accumulated.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.cycles_per_sample = (crate::CLOCK_HZ << 8) / sample_rate.max(1);
        self.sample_accumulator = 0;
        self.box_sum = (0.0, 0.0);
        self.box_count = 0;
        // The cutoff is a frequency, so the coefficient has to be recomputed:
        // reusing the old one would move the filter along with the rate.
        self.speaker_alpha = low_pass_alpha(SPEAKER_CUTOFF_HZ, sample_rate);
    }

    /// Turns the speaker low-pass on or off.
    ///
    /// Off, the output is what the DAC produces: brighter, and closer to what
    /// the other emulators sound like.
    pub fn set_speaker_filter(&mut self, enabled: bool) {
        self.speaker_enabled = enabled;
        if !enabled {
            self.speaker = (0.0, 0.0);
        }
    }

    /// Empties and returns the samples generated since the last call.
    pub fn drain(&mut self) -> Vec<StereoSample> {
        core::mem::take(&mut self.output)
    }

    /// Discards what was accumulated without delivering it. For when the
    /// frontend has no audio and the buffer should not grow without bound.
    pub fn discard(&mut self) {
        self.output.clear();
    }

    /// Advances the APU.
    ///
    /// `div_bit` is bit 12 (or 13 at double speed) of the timer's internal
    /// counter. The sequencer advances on its **falling edge**.
    pub fn tick(&mut self, t_cycles: u32, div_bit: bool) {
        if self.previous_div_bit && !div_bit {
            self.step_sequencer();
        }
        self.previous_div_bit = div_bit;

        if self.powered {
            self.channel1.tick(t_cycles);
            self.channel2.tick(t_cycles);
            self.channel3.tick(t_cycles);
            self.channel4.tick(t_cycles);
        }

        self.resample(t_cycles);
    }

    /// One sequencer step. Each one pulses a different subset.
    fn step_sequencer(&mut self) {
        if !self.powered {
            return;
        }

        // Even steps: length, at 256 Hz.
        if self.sequencer_step % 2 == 0 {
            self.channel1.tick_length();
            self.channel2.tick_length();
            self.channel3.tick_length();
            self.channel4.tick_length();
        }
        // Steps 2 and 6: sweep, at 128 Hz.
        if self.sequencer_step == 2 || self.sequencer_step == 6 {
            self.channel1.tick_sweep();
        }
        // Step 7: envelopes, at 64 Hz.
        if self.sequencer_step == 7 {
            self.channel1.tick_envelope();
            self.channel2.tick_envelope();
            self.channel4.tick_envelope();
        }

        self.sequencer_step = (self.sequencer_step + 1) % SEQUENCER_STEPS;
    }

    /// Mixes the four channels according to `NR51` and `NR50`.
    fn mix(&self) -> (f32, f32) {
        if !self.powered {
            return (0.0, 0.0);
        }

        // Each channel already delivers its voltage: 0.0 if the DAC is off (it
        // is disconnected and contributes nothing), and -1.0..1.0 if it is on.
        let outputs = [
            self.channel1.dac_output(),
            self.channel2.dac_output(),
            self.channel3.dac_output(),
            self.channel4.dac_output(),
        ];

        let mut left = 0.0;
        let mut right = 0.0;
        for (i, &analog) in outputs.iter().enumerate() {
            // NR51: bits 3-0 are the right side; bits 7-4, the left one.
            if self.panning >> (i + 4) & 1 != 0 {
                left += analog;
            }
            if self.panning >> i & 1 != 0 {
                right += analog;
            }
        }

        // The master volume goes from 0 to 7, and 0 **is not silence**: it is the
        // minimum volume. That is why 1 is added before dividing.
        let scale = |v: f32, master: u8| v / 4.0 * (f32::from(master) + 1.0) / 8.0;
        (scale(left, self.left_volume), scale(right, self.right_volume))
    }

    /// Accumulates the signal and emits samples at the frontend's pace.
    fn resample(&mut self, t_cycles: u32) {
        let (left, right) = self.mix();
        self.box_sum.0 += left * t_cycles as f32;
        self.box_sum.1 += right * t_cycles as f32;
        self.box_count += t_cycles;

        self.sample_accumulator += t_cycles << 8;
        while self.sample_accumulator >= self.cycles_per_sample {
            self.sample_accumulator -= self.cycles_per_sample;
            self.emit_sample();
        }
    }

    fn emit_sample(&mut self) {
        let count = self.box_count.max(1) as f32;
        let raw = (self.box_sum.0 / count, self.box_sum.1 / count);
        self.box_sum = (0.0, 0.0);
        self.box_count = 0;

        // High-pass: the output is the signal minus its own moving average,
        // which is exactly what a series capacitor does.
        let left = raw.0 - self.capacitor.0;
        let right = raw.1 - self.capacitor.1;
        let decay = HIGH_PASS_FACTOR.powi(self.cycles_per_sample as i32 >> 8);
        self.capacitor.0 = raw.0 - left * decay;
        self.capacitor.1 = raw.1 - right * decay;

        // Low-pass, last: it has to see the signal that will actually be heard,
        // and the high-pass above it only moves the DC, which is already gone.
        let (left, right) = if self.speaker_enabled {
            self.speaker.0 += self.speaker_alpha * (left - self.speaker.0);
            self.speaker.1 += self.speaker_alpha * (right - self.speaker.1);
            (self.speaker.0, self.speaker.1)
        } else {
            (left, right)
        };

        self.output.push(StereoSample { left, right });
    }

    // ---- Registers ---------------------------------------------------------

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF10 => self.channel1.read_sweep(),
            0xFF11 => self.channel1.read_duty(),
            0xFF12 => self.channel1.read_envelope(),
            0xFF14 => self.channel1.read_control(),
            0xFF16 => self.channel2.read_duty(),
            0xFF17 => self.channel2.read_envelope(),
            0xFF19 => self.channel2.read_control(),
            0xFF1A => self.channel3.read_dac(),
            0xFF1C => self.channel3.read_volume(),
            0xFF1E => self.channel3.read_control(),
            0xFF21 => self.channel4.read_envelope(),
            0xFF22 => self.channel4.read_polynomial(),
            0xFF23 => self.channel4.read_control(),
            0xFF24 => {
                (u8::from(self.vin_left) << 7)
                    | (self.left_volume << 4)
                    | (u8::from(self.vin_right) << 3)
                    | self.right_volume
            }
            0xFF25 => self.panning,
            0xFF26 => self.read_status(),
            0xFF30..=0xFF3F => self.channel3.read_ram(addr),
            // The write-only registers (frequency and length load) and the gaps
            // in the map read as 0xFF.
            _ => 0xFF,
        }
    }

    /// `NR52`: power and status of the four channels.
    ///
    /// Bits 3-0 are **read-only**: they report which channels are still
    /// sounding, and that is how a game knows a note ended without keeping
    /// count itself.
    fn read_status(&self) -> u8 {
        0x70 | (u8::from(self.powered) << 7)
            | u8::from(self.channel1.enabled)
            | (u8::from(self.channel2.enabled) << 1)
            | (u8::from(self.channel3.enabled) << 2)
            | (u8::from(self.channel4.enabled) << 3)
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        // The wave RAM and NR52 always respond; the rest, only with the APU
        // powered on. That is what allows loading a table before turning the
        // sound on and keeps a game from configuring channels that are about to
        // be wiped.
        if addr == 0xFF26 {
            self.write_power(value);
            return;
        }
        if (0xFF30..=0xFF3F).contains(&addr) {
            self.channel3.write_ram(addr, value);
            return;
        }
        if !self.powered {
            return;
        }

        match addr {
            0xFF10 => self.channel1.write_sweep(value),
            0xFF11 => self.channel1.write_duty_length(value),
            0xFF12 => self.channel1.write_envelope(value),
            0xFF13 => self.channel1.write_frequency_low(value),
            0xFF14 => self.channel1.write_control(value),
            0xFF16 => self.channel2.write_duty_length(value),
            0xFF17 => self.channel2.write_envelope(value),
            0xFF18 => self.channel2.write_frequency_low(value),
            0xFF19 => self.channel2.write_control(value),
            0xFF1A => self.channel3.write_dac(value),
            0xFF1B => self.channel3.write_length(value),
            0xFF1C => self.channel3.write_volume(value),
            0xFF1D => self.channel3.write_frequency_low(value),
            0xFF1E => self.channel3.write_control(value),
            0xFF20 => self.channel4.write_length(value),
            0xFF21 => self.channel4.write_envelope(value),
            0xFF22 => self.channel4.write_polynomial(value),
            0xFF23 => self.channel4.write_control(value),
            0xFF24 => {
                self.vin_left = value & 0x80 != 0;
                self.left_volume = (value >> 4) & 0x07;
                self.vin_right = value & 0x08 != 0;
                self.right_volume = value & 0x07;
            }
            0xFF25 => self.panning = value,
            _ => {}
        }
    }

    fn write_power(&mut self, value: u8) {
        let on = value & 0x80 != 0;
        if self.powered && !on {
            // Turning it off wipes every register. It is the canonical way to
            // silence the console at once.
            self.channel1.power_off();
            self.channel2.power_off();
            self.channel3.power_off();
            self.channel4.power_off();
            self.left_volume = 0;
            self.right_volume = 0;
            self.vin_left = false;
            self.vin_right = false;
            self.panning = 0;
        } else if !self.powered && on {
            // On power-up, the sequencer starts from the beginning.
            self.sequencer_step = 0;
        }
        self.powered = on;
    }

    /// Bit of the timer counter that pulses the sequencer.
    ///
    /// At double speed bit 13 is used instead of 12, and that is why the sound
    /// does **not** speed up when the game switches to 8 MHz.
    pub const fn sequencer_div_bit(model: Model, double_speed: bool) -> u8 {
        if model.is_cgb() && double_speed {
            13
        } else {
            12
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// APU powered on, with the master volume at maximum and everything on both
    /// sides.
    fn apu() -> Apu {
        let mut a = Apu::new(48_000);
        a.write(0xFF26, 0x80); // power on
        a.write(0xFF24, 0x77); // volume 7 on both sides
        a.write(0xFF25, 0xFF); // all four channels, in stereo
        a
    }

    /// Advances `steps` of the frame sequencer by simulating the `DIV` edges.
    fn sequence(a: &mut Apu, steps: u32) {
        for _ in 0..steps {
            a.tick(4, true);
            a.tick(4, false);
        }
    }

    #[test]
    fn it_starts_powered_off() {
        let a = Apu::new(48_000);
        assert_eq!(a.read(0xFF26) & 0x80, 0);
    }

    #[test]
    fn with_the_apu_off_writes_are_ignored() {
        let mut a = Apu::new(48_000);
        a.write(0xFF24, 0x77);
        assert_eq!(a.read(0xFF24), 0x00, "with no power nothing gets configured");

        a.write(0xFF26, 0x80);
        a.write(0xFF24, 0x77);
        assert_eq!(a.read(0xFF24), 0x77);
    }

    #[test]
    fn turning_the_apu_off_wipes_the_registers() {
        let mut a = apu();
        a.write(0xFF12, 0xF0);
        a.write(0xFF14, 0x80);
        assert_ne!(a.read(0xFF24), 0x00);

        a.write(0xFF26, 0x00);
        assert_eq!(a.read(0xFF24), 0x00);
        assert_eq!(a.read(0xFF25), 0x00);
        assert_eq!(a.read(0xFF26) & 0x0F, 0, "the channels are left off");
    }

    #[test]
    fn the_wave_ram_is_reachable_with_the_apu_off() {
        let mut a = Apu::new(48_000);
        a.write(0xFF30, 0xAB);
        assert_eq!(a.read(0xFF30), 0xAB, "loading the table needs no power");
    }

    #[test]
    fn nr52_reports_the_active_channels() {
        let mut a = apu();
        assert_eq!(a.read(0xFF26) & 0x0F, 0);

        a.write(0xFF12, 0xF0); // channel 1: DAC on
        a.write(0xFF14, 0x80); // trigger
        assert_eq!(a.read(0xFF26) & 0x01, 0x01);

        a.write(0xFF21, 0xF0); // channel 4
        a.write(0xFF23, 0x80);
        assert_eq!(a.read(0xFF26) & 0x0F, 0b1001);
    }

    #[test]
    fn the_sequencer_pulses_the_length_at_256_hz() {
        let mut a = apu();
        a.write(0xFF11, 0x3F); // length 63 → 1 step left
        a.write(0xFF12, 0xF0);
        a.write(0xFF14, 0xC0); // trigger with length enabled
        assert_eq!(a.read(0xFF26) & 0x01, 1);

        // The even sequencer steps are the ones that count the length.
        sequence(&mut a, 2);
        assert_eq!(a.read(0xFF26) & 0x01, 0, "the channel turned itself off");
    }

    #[test]
    fn the_panning_splits_the_channels_between_the_two_sides() {
        let mut a = apu();
        a.write(0xFF11, 0x80);
        a.write(0xFF12, 0xF0);
        a.write(0xFF14, 0x87);

        a.write(0xFF25, 0x10); // channel 1 on the left only
        let (l, r) = a.mix();
        assert_ne!(l, 0.0);
        assert_eq!(r, 0.0, "the right side goes mute");

        a.write(0xFF25, 0x01); // and now on the right only
        let (l, r) = a.mix();
        assert_eq!(l, 0.0);
        assert_ne!(r, 0.0);
    }

    #[test]
    fn the_master_volume_scales_the_mix() {
        let mut a = apu();
        a.write(0xFF11, 0x80);
        a.write(0xFF12, 0xF0);
        a.write(0xFF14, 0x87);

        let loud = a.mix().0.abs();
        a.write(0xFF24, 0x00); // volume 0, which is not silence
        let quiet = a.mix().0.abs();

        assert!(quiet < loud);
        assert!(quiet > 0.0, "master volume 0 still lets signal through");
    }

    #[test]
    fn with_all_four_dacs_off_the_mix_is_silence() {
        // The APU is on, but no channel has turned its DAC on. Four
        // disconnected DACs contribute no voltage, so the output is the centre.
        // They used to give -1.0 each and the mix saturated at full scale.
        let a = apu();
        assert_eq!(a.mix(), (0.0, 0.0));
    }

    #[test]
    fn a_mute_channel_with_its_dac_on_does_offset_the_signal() {
        // Different from the previous case: here the DAC is connected and the
        // digital sample is 0, which is the negative extreme. That jump is real
        // and it is what justifies the high-pass filter.
        let mut a = apu();
        a.write(0xFF11, 0x3F); // length 63 → 1 step left
        a.write(0xFF12, 0xF0); // DAC on
        a.write(0xFF14, 0xC7); // trigger with length enabled
        assert!(a.mix().0 != 0.0);

        sequence(&mut a, 1); // the length reaches 0 and turns the channel off
        assert_eq!(a.read(0xFF26) & 0x01, 0, "the channel is mute");
        assert!(a.mix().0 < 0.0, "but the DAC is still connected at its extreme");
    }

    #[test]
    fn it_generates_samples_at_the_requested_rate() {
        let mut a = Apu::new(48_000);
        a.write(0xFF26, 0x80);

        // A whole second of T-cycles, in chunks of one M-cycle.
        for _ in 0..crate::CLOCK_HZ / 4 {
            a.tick(4, false);
        }
        let samples = a.drain().len();
        assert!(
            samples.abs_diff(48_000) < 100,
            "{samples} samples were generated, ~48000 were expected"
        );
    }

    #[test]
    fn the_sample_rate_is_configurable() {
        let mut a = Apu::new(48_000);
        a.set_sample_rate(44_100);
        a.write(0xFF26, 0x80);
        for _ in 0..crate::CLOCK_HZ / 4 {
            a.tick(4, false);
        }
        assert!(a.drain().len().abs_diff(44_100) < 100);
    }

    #[test]
    fn the_high_pass_filter_removes_the_dc_component() {
        let mut a = apu();
        // Channel 1 with a constant volume and a very slow frequency: the signal
        // is practically DC.
        a.write(0xFF11, 0x80);
        a.write(0xFF12, 0xF0);
        a.write(0xFF13, 0x00);
        a.write(0xFF14, 0x80);

        for _ in 0..crate::CLOCK_HZ / 4 {
            a.tick(4, false);
        }
        let samples = a.drain();
        let mean: f32 = samples.iter().map(|s| s.left).sum::<f32>() / samples.len() as f32;
        assert!(mean.abs() < 0.01, "the mean must tend to zero, it is {mean}");
    }

    #[test]
    fn the_samples_stay_within_range() {
        let mut a = apu();
        // All four channels at full blast at once, which is the worst case.
        for (duty, envelope, control) in [(0xFF11, 0xFF12, 0xFF14), (0xFF16, 0xFF17, 0xFF19)] {
            a.write(duty, 0x80);
            a.write(envelope, 0xF0);
            a.write(control, 0x80);
        }
        a.write(0xFF1A, 0x80);
        a.write(0xFF1C, 0x20);
        a.write(0xFF1E, 0x80);
        for i in 0..16 {
            a.write(0xFF30 + i, 0xFF);
        }
        a.write(0xFF21, 0xF0);
        a.write(0xFF23, 0x80);

        for _ in 0..crate::CLOCK_HZ / 40 {
            a.tick(4, false);
        }
        for s in a.drain() {
            assert!((-1.0..=1.0).contains(&s.left), "sample out of range: {s:?}");
            assert!((-1.0..=1.0).contains(&s.right));
        }
    }

    /// Level of channel 1 on a steady note, with the filter on or off.
    ///
    /// `period` is what goes into the frequency registers: the note comes out at
    /// `131072/(2048 - period)` Hz.
    fn tone_level(period: u16, filtered: bool) -> f32 {
        let mut a = apu();
        a.set_speaker_filter(filtered);
        a.write(0xFF11, 0x80); // 50% duty
        a.write(0xFF12, 0xF0); // maximum volume, no envelope
        a.write(0xFF13, (period & 0xFF) as u8);
        a.write(0xFF14, 0x80 | (period >> 8) as u8); // trigger

        for _ in 0..crate::CLOCK_HZ / 40 {
            a.tick(4, false);
        }
        let samples = a.drain();
        (samples.iter().map(|s| s.left * s.left).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn the_low_pass_cuts_the_treble_and_leaves_the_bass_alone() {
        // A filter is defined by what it does at each frequency, so both ends
        // are checked. Measuring only one would pass with a filter that simply
        // turns the volume down.

        // 1792 → 512 Hz, four octaves below the cutoff: it has to come through.
        let bass = tone_level(1792, true) / tone_level(1792, false);
        assert!(bass > 0.9, "the bass must survive almost untouched, it is at {bass}");

        // 2040 → 16384 Hz, one octave above the cutoff. A one-pole filter drops
        // 7 dB there, which is a factor of 0.45.
        let treble = tone_level(2040, true) / tone_level(2040, false);
        assert!(treble < 0.6, "an octave above the cutoff it must drop clearly, it is at {treble}");
        assert!(treble > 0.2, "but a one-pole filter does not silence it either: {treble}");
    }

    #[test]
    fn the_cutoff_follows_the_sample_rate() {
        // The same cutoff in Hz is a different coefficient at each rate. If it
        // were not recomputed, the filter would slide with the sample rate.
        let fast = low_pass_alpha(SPEAKER_CUTOFF_HZ, 48_000);
        let slow = low_pass_alpha(SPEAKER_CUTOFF_HZ, 22_050);
        assert!(slow > fast, "at a lower rate each sample weighs more: {slow} vs {fast}");
        assert!((0.0..=1.0).contains(&fast));

        // Way below Nyquist the filter has to stay out of the way.
        assert_eq!(low_pass_alpha(SPEAKER_CUTOFF_HZ, 1_000), 1.0, "it must not ring");
    }

    #[test]
    fn set_sample_rate_keeps_the_filter_off_if_it_was_off() {
        let mut a = apu();
        a.set_speaker_filter(false);
        a.set_sample_rate(44_100);
        assert!(!a.speaker_enabled, "changing the rate must not turn it back on");
    }

    #[test]
    fn at_double_speed_the_sequencer_uses_another_div_bit() {
        assert_eq!(Apu::sequencer_div_bit(Model::Dmg, false), 12);
        assert_eq!(Apu::sequencer_div_bit(Model::Cgb, false), 12);
        assert_eq!(
            Apu::sequencer_div_bit(Model::Cgb, true),
            13,
            "at double speed the next bit is used, and the sound does not speed up"
        );
    }
}
