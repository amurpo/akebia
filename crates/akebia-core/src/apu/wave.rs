//! Channel 3: table wave.
//!
//! It is the only channel whose waveform the programmer chooses: 32 samples of
//! 4 bits in the wave RAM (`0xFF30..0xFF40`), two per byte. That makes it the
//! channel for basses and odd timbres, and also the one some games use to play
//! back sampled speech, rewriting the whole table several times per frame.
//!
//! # Differences from the squares
//!
//! - **It has no envelope**: the volume is chosen among four fixed levels
//!   (100 %, 50 %, 25 % and mute) implemented as a shift.
//! - **Its length counter reaches 256** instead of 64, because the register that
//!   loads it uses all 8 bits.
//! - **Its timer runs twice as fast**: the period is `(2048 - f) * 2` instead of
//!   `* 4`. With 32 samples per cycle instead of 8, that leaves the wave one
//!   octave below a square at the same frequency.

use super::components::LengthCounter;

/// Samples in the table: 32 of 4 bits.
pub const WAVE_SAMPLES: usize = 32;
/// Bytes of wave RAM, two samples each.
pub const WAVE_RAM_SIZE: usize = WAVE_SAMPLES / 2;

const MAX_LENGTH: u16 = 256;

/// Right shift for each volume level.
/// Level 0 does not shift: it mutes.
const VOLUME_SHIFT: [u8; 4] = [4, 0, 1, 2];

#[derive(Clone)]
pub struct Wave {
    pub enabled: bool,
    /// Bit 7 of `NR30`. Unlike the other channels, here the DAC has its own bit
    /// instead of being deduced from the envelope.
    dac_enabled: bool,

    frequency: u16,
    timer: i32,
    /// Current sample within the table (0..31).
    position: usize,
    /// Volume level (0..3), not a linear volume.
    volume: u8,

    length: LengthCounter,
    ram: [u8; WAVE_RAM_SIZE],
}

impl Wave {
    pub const fn new() -> Self {
        Self {
            enabled: false,
            dac_enabled: false,
            frequency: 0,
            timer: 0,
            position: 0,
            volume: 0,
            length: LengthCounter::new(MAX_LENGTH),
            ram: [0; WAVE_RAM_SIZE],
        }
    }

    fn period(&self) -> i32 {
        (2048 - i32::from(self.frequency)) * 2
    }

    pub fn tick(&mut self, t_cycles: u32) {
        self.timer -= t_cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.position = (self.position + 1) % WAVE_SAMPLES;
        }
    }

    pub fn sample(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        // Two samples per byte: the first one in the high nibble.
        let byte = self.ram[self.position / 2];
        let nibble = if self.position % 2 == 0 { byte >> 4 } else { byte & 0x0F };
        nibble >> VOLUME_SHIFT[self.volume as usize]
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

    // ---- Registers ---------------------------------------------------------

    /// `NR30`: DAC enable.
    pub fn write_dac(&mut self, value: u8) {
        self.dac_enabled = value & 0x80 != 0;
        if !self.dac_enabled {
            self.enabled = false;
        }
    }

    pub fn read_dac(&self) -> u8 {
        0x7F | (u8::from(self.dac_enabled) << 7)
    }

    /// `NR31`: length load, with all 8 bits.
    pub fn write_length(&mut self, value: u8) {
        self.length.load(u16::from(value));
    }

    /// `NR32`: volume level.
    pub fn write_volume(&mut self, value: u8) {
        self.volume = (value >> 5) & 0x03;
    }

    pub fn read_volume(&self) -> u8 {
        0x9F | (self.volume << 5)
    }

    pub fn write_frequency_low(&mut self, value: u8) {
        self.frequency = (self.frequency & 0x0700) | u16::from(value);
    }

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

    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        // The trigger resets the position, unlike the squares, where the pattern
        // continues where it left off.
        self.position = 0;
        self.length.trigger();
    }

    // ---- Wave RAM ----------------------------------------------------------

    pub fn read_ram(&self, addr: u16) -> u8 {
        self.ram[(addr as usize - 0xFF30) % WAVE_RAM_SIZE]
    }

    pub fn write_ram(&mut self, addr: u16, value: u8) {
        self.ram[(addr as usize - 0xFF30) % WAVE_RAM_SIZE] = value;
    }

    /// The wave RAM **survives** the APU power-off: it is memory, not a
    /// register, and games rely on that to load the table before turning the
    /// sound on.
    pub fn power_off(&mut self) {
        let ram = self.ram;
        let length_enabled = self.length.enabled;
        *self = Self::new();
        self.ram = ram;
        self.length.enabled = length_enabled;
    }
}

impl Default for Wave {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Channel with a ramp 0,1,2,…,15,0,1,… in the table.
    fn channel() -> Wave {
        let mut w = Wave::new();
        for i in 0..WAVE_RAM_SIZE {
            let high = (i * 2 % 16) as u8;
            let low = ((i * 2 + 1) % 16) as u8;
            w.write_ram(0xFF30 + i as u16, (high << 4) | low);
        }
        w.write_dac(0x80);
        w.write_volume(0x20); // volume 1 = 100 %
        w.write_frequency_low(0x00);
        w.write_control(0x87);
        w
    }

    #[test]
    fn it_walks_the_table_sample_by_sample() {
        let mut w = channel();
        let period = ((2048 - 0x0700) * 2) as u32;

        assert_eq!(w.sample(), 0);
        w.tick(period);
        assert_eq!(w.sample(), 1);
        w.tick(period);
        assert_eq!(w.sample(), 2);
    }

    #[test]
    fn the_volume_is_a_shift_not_a_factor() {
        let mut w = channel();
        let period = ((2048 - 0x0700) * 2) as u32;
        w.tick(period * 8); // a sample with value 8

        w.write_volume(0x20); // 100 %
        assert_eq!(w.sample(), 8);
        w.write_volume(0x40); // 50 %
        assert_eq!(w.sample(), 4);
        w.write_volume(0x60); // 25 %
        assert_eq!(w.sample(), 2);
        w.write_volume(0x00); // mute
        assert_eq!(w.sample(), 0);
    }

    #[test]
    fn turning_the_dac_off_turns_the_channel_off() {
        let mut w = channel();
        assert!(w.enabled);
        w.write_dac(0x00);
        assert!(!w.enabled);
    }

    #[test]
    fn the_trigger_returns_to_the_start_of_the_table() {
        let mut w = channel();
        let period = ((2048 - 0x0700) * 2) as u32;
        w.tick(period * 5);
        assert_ne!(w.sample(), 0);

        w.write_control(0x87);
        assert_eq!(w.sample(), 0, "the trigger resets the position");
    }

    #[test]
    fn the_length_reaches_256() {
        let mut w = channel();
        w.write_length(255); // 1 step left
        w.write_control(0xC7);
        w.tick_length();
        assert!(!w.enabled);

        let mut w = channel();
        w.write_length(0); // 256 steps left
        w.write_control(0xC7);
        for _ in 0..255 {
            w.tick_length();
        }
        assert!(w.enabled, "with 8 bits of length it goes much further");
    }

    #[test]
    fn the_table_survives_the_power_off() {
        let mut w = channel();
        let before = w.read_ram(0xFF35);
        w.power_off();
        assert_eq!(w.read_ram(0xFF35), before, "the wave RAM is memory, not a register");
        assert!(!w.enabled);
    }
}
