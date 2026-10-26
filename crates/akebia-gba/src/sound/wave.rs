//! The third inherited channel: a table wave.
//!
//! It is the only one whose waveform the programmer chooses — a table of 4-bit
//! samples, two to a byte — which makes it the channel for basses and odd
//! timbres, and the one a game uses when it wants to play back a recorded
//! sound by rewriting the table several times a frame.
//!
//! # What this machine added to it
//!
//! The older console had one table of 32 samples. This one has **two banks of
//! 32**, and three things follow from that:
//!
//! - A game can play 64 samples end to end, which is a waveform of twice the
//!   resolution, by setting the bit that says so.
//! - Or it can play 32 and **write the other 32 while they play**, which is
//!   double buffering: the recorded-sound trick without the crackle of writing
//!   under the playing head.
//! - So the bank the processor reaches through the table's addresses is the one
//!   that is **not** playing. That is not a quirk to work around; it is the
//!   whole point of there being two.
//!
//! There is also a fourth volume setting the older machine has not got: three
//! quarters, which sits between the full and the half.
//!
//! # Differences from the squares
//!
//! - **No envelope**: the volume is one of a few fixed levels, done as a shift.
//! - **Its length reaches 256** instead of 64, because the register that loads
//!   it uses all eight bits.
//! - **Its timer runs twice as fast**, so with 32 samples to a cycle instead of
//!   eight it sits an octave below a square at the same frequency.

use super::components::LengthCounter;

/// Samples in one bank, and bytes to hold them at two apiece.
pub const SAMPLES_PER_BANK: usize = 32;
pub const BANK_BYTES: usize = SAMPLES_PER_BANK / 2;
pub const BANKS: usize = 2;

const MAX_LENGTH: u16 = 256;

/// How far to shift the sample right for each volume setting. The first mutes.
const VOLUME_SHIFT: [u8; 4] = [4, 0, 1, 2];

/// Cycles in one sample, per unit of the frequency register. Half a square's,
/// which is what puts this channel an octave down.
const STEP_CYCLES: i32 = 8;

#[derive(Clone)]
pub struct Wave {
    pub enabled: bool,
    /// Unlike the squares, the DAC here has a bit of its own rather than being
    /// worked out from an envelope.
    dac_enabled: bool,

    frequency: u16,
    timer: i32,
    /// Where in the table it is: 0..32 in one bank, 0..64 across both.
    position: usize,
    /// Which bank plays. In two-bank mode it is where playing *starts*.
    bank: usize,
    /// Whether both banks are played end to end.
    both_banks: bool,
    /// One of four levels, and not a linear volume.
    volume: u8,
    /// The setting the older machine has not got, which overrides the level.
    three_quarters: bool,

    length: LengthCounter,
    ram: [[u8; BANK_BYTES]; BANKS],
}

impl Default for Wave {
    fn default() -> Self {
        Self::new()
    }
}

impl Wave {
    pub const fn new() -> Self {
        Self {
            enabled: false,
            dac_enabled: false,
            frequency: 0,
            timer: 0,
            position: 0,
            bank: 0,
            both_banks: false,
            volume: 0,
            three_quarters: false,
            length: LengthCounter::new(MAX_LENGTH),
            ram: [[0; BANK_BYTES]; BANKS],
        }
    }

    fn period(&self) -> i32 {
        (2048 - i32::from(self.frequency)) * STEP_CYCLES
    }

    /// How many samples there are to go round before it repeats.
    fn length_in_samples(&self) -> usize {
        if self.both_banks { SAMPLES_PER_BANK * BANKS } else { SAMPLES_PER_BANK }
    }

    pub fn tick(&mut self, cycles: u32) {
        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.position = (self.position + 1) % self.length_in_samples();
        }
    }

    /// The bank and byte the playing head is on.
    ///
    /// In one-bank mode it never leaves the bank the game named. In two-bank
    /// mode it starts there and runs into the other, which is what makes the
    /// two of them one long table.
    fn playing_at(&self) -> (usize, usize) {
        let bank = (self.bank + self.position / SAMPLES_PER_BANK) % BANKS;
        (bank, (self.position % SAMPLES_PER_BANK) / 2)
    }

    pub fn sample(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        let (bank, byte) = self.playing_at();
        // Two samples to a byte, the earlier one in the top half.
        let held = self.ram[bank][byte];
        let nibble = if self.position % 2 == 0 { held >> 4 } else { held & 0x0F };

        if self.three_quarters {
            // Three quarters is not a shift, which is why it needs its own bit
            // and its own arithmetic.
            (u16::from(nibble) * 3 / 4) as u8
        } else {
            nibble >> VOLUME_SHIFT[self.volume as usize]
        }
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

    // ---- Registers ---------------------------------------------------------

    /// The DAC bit, which bank plays and whether both do.
    pub fn write_control_low(&mut self, value: u8) {
        self.both_banks = value & 0x20 != 0;
        self.bank = usize::from(value & 0x40 != 0);
        self.dac_enabled = value & 0x80 != 0;
        if !self.dac_enabled {
            self.enabled = false;
        }
    }

    pub fn read_control_low(&self) -> u8 {
        (u8::from(self.both_banks) << 5)
            | ((self.bank as u8) << 6)
            | (u8::from(self.dac_enabled) << 7)
    }

    /// The length load, with all eight bits, which does not read back.
    pub fn write_length(&mut self, value: u8) {
        self.length.load(u16::from(value));
    }

    pub fn write_volume(&mut self, value: u8) {
        self.volume = (value >> 5) & 0x03;
        self.three_quarters = value & 0x80 != 0;
    }

    pub fn read_volume(&self) -> u8 {
        (self.volume << 5) | (u8::from(self.three_quarters) << 7)
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
        u8::from(self.length.enabled) << 6
    }

    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        // The trigger goes back to the start of the table, unlike a square,
        // where the pattern carries on from where it was.
        self.position = 0;
        self.length.trigger();
    }

    // ---- The table itself --------------------------------------------------

    /// Which bank the processor reaches, which is the one not playing.
    fn cpu_bank(&self) -> usize {
        (self.bank + 1) % BANKS
    }

    pub fn read_ram(&self, offset: usize) -> u8 {
        self.ram[self.cpu_bank()][offset % BANK_BYTES]
    }

    pub fn write_ram(&mut self, offset: usize, value: u8) {
        let bank = self.cpu_bank();
        self.ram[bank][offset % BANK_BYTES] = value;
    }

    /// What the master switch does to it — except the table, which survives.
    /// A game that has written a waveform and then powers the sound off and on
    /// again expects its waveform to still be there.
    pub fn power_off(&mut self) {
        let ram = self.ram;
        *self = Self::new();
        self.ram = ram;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel playing bank 0 at full volume, with a table the processor has
    /// filled through the other one.
    fn playing(table: &[u8; BANK_BYTES]) -> Wave {
        let mut wave = Wave::new();
        // The processor reaches the bank that is not playing, so with bank 1
        // named the writes land in bank 0.
        wave.write_control_low(0x40);
        for (offset, byte) in table.iter().enumerate() {
            wave.write_ram(offset, *byte);
        }
        // Now play the bank just written.
        wave.write_control_low(0x80);
        wave.write_volume(0x20); // full
        wave.write_frequency_low(0);
        wave.write_control(0x87);
        wave
    }

    /// The samples come out two to a byte, the earlier one from the top half.
    /// Reading them the other way round plays every waveform with its halves
    /// swapped.
    #[test]
    fn the_earlier_sample_of_a_byte_is_the_top_half() {
        let mut table = [0u8; BANK_BYTES];
        table[0] = 0x3C;
        let mut wave = playing(&table);
        assert_eq!(wave.sample(), 3);
        wave.tick(wave.period() as u32);
        assert_eq!(wave.sample(), 0x0C);
    }

    /// The processor reaches the bank that is not playing. That is what lets a
    /// game write the next waveform while this one sounds, which is the whole
    /// reason there are two.
    #[test]
    fn the_processor_reaches_the_bank_that_is_not_playing() {
        let mut wave = Wave::new();
        wave.write_control_low(0x80); // playing bank 0
        wave.write_ram(0, 0xAB);
        assert_eq!(wave.read_ram(0), 0xAB, "written into bank 1");

        wave.write_control_low(0xC0); // now playing bank 1
        assert_ne!(wave.read_ram(0), 0xAB, "and the window moved to bank 0");
        wave.write_ram(0, 0xCD);

        wave.write_control_low(0x80);
        assert_eq!(wave.read_ram(0), 0xAB, "each bank kept its own");
    }

    /// Both banks end to end is one table of sixty-four, and it wraps at the
    /// end of the second rather than at the end of the first.
    #[test]
    fn both_banks_play_as_one_long_table() {
        let mut wave = Wave::new();
        wave.write_control_low(0x40);
        wave.write_ram(0, 0x10); // first sample of bank 0
        wave.write_control_low(0x80);
        wave.write_ram(0, 0x20); // first sample of bank 1
        wave.write_control_low(0xA0); // both banks, starting at 0
        wave.write_volume(0x20);
        wave.write_frequency_low(0);
        wave.write_control(0x87);

        assert_eq!(wave.sample(), 1, "the first of bank 0");
        wave.tick(wave.period() as u32 * SAMPLES_PER_BANK as u32);
        assert_eq!(wave.sample(), 2, "and then straight into bank 1");
        wave.tick(wave.period() as u32 * SAMPLES_PER_BANK as u32);
        assert_eq!(wave.sample(), 1, "and round again at sixty-four");
    }

    /// One bank is thirty-two and wraps there, whichever bank it is.
    #[test]
    fn one_bank_wraps_at_thirty_two() {
        let mut table = [0x11u8; BANK_BYTES];
        table[0] = 0x50;
        let mut wave = playing(&table);
        assert_eq!(wave.sample(), 5);
        wave.tick(wave.period() as u32 * SAMPLES_PER_BANK as u32);
        assert_eq!(wave.sample(), 5, "back at the start of the same bank");
    }

    /// The four levels are shifts, and the extra one this machine added is not:
    /// three quarters of fifteen is eleven, which no shift produces.
    #[test]
    fn the_volume_levels_are_shifts_and_the_extra_one_is_not() {
        let mut table = [0u8; BANK_BYTES];
        table[0] = 0xF0;
        let mut wave = playing(&table);

        for (setting, expected) in [(0x00u8, 0u8), (0x20, 15), (0x40, 7), (0x60, 3)] {
            wave.write_volume(setting);
            assert_eq!(wave.sample(), expected, "setting {setting:#04X}");
        }

        wave.write_volume(0x80);
        assert_eq!(wave.sample(), 11, "three quarters of fifteen");
        wave.write_volume(0xE0);
        assert_eq!(wave.sample(), 11, "and it beats whatever level is set");
    }

    /// This channel goes back to the start when triggered, which a square does
    /// not. A game restarting a bass note expects it from the top.
    #[test]
    fn a_trigger_goes_back_to_the_start_of_the_table() {
        let mut table = [0x11u8; BANK_BYTES];
        table[0] = 0x90;
        let mut wave = playing(&table);
        wave.tick(wave.period() as u32 * 5);
        assert_eq!(wave.sample(), 1);
        wave.write_control(0x87);
        assert_eq!(wave.sample(), 9, "back at the first sample");
    }

    /// The period is half a square's, which is what puts this channel an
    /// octave below one at the same frequency.
    #[test]
    fn the_period_is_half_a_squares() {
        let mut wave = Wave::new();
        wave.write_frequency_low(0);
        wave.write_control(0x86);
        assert_eq!(wave.period(), (2048 - 1536) * 8);
    }

    /// The waveform survives the master switch. A game that wrote its table
    /// then switched the sound off and on expects to find it still there.
    #[test]
    fn the_table_survives_the_power_off() {
        let mut table = [0u8; BANK_BYTES];
        table[0] = 0x7E;
        let mut wave = playing(&table);
        wave.power_off();
        assert!(!wave.enabled);

        wave.write_control_low(0x40);
        assert_eq!(wave.read_ram(0), 0x7E);
    }
}
