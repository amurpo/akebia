//! Sound: the two queues, the mixer, and what comes out of it.
//!
//! # The two halves of this machine's sound
//!
//! The Advance makes sound two ways at once. It inherited the older machine's
//! four channels — two squares, a wave table and a noise generator, each one a
//! number the hardware *generates* — and it added two **queues** of samples the
//! game generates itself, in software, and posts. Almost every commercial tune
//! comes out of the queues; the four inherited channels are what a game reaches
//! for when it wants one more voice cheaply.
//!
//! Only the queues are here so far. What plays them is [`Sound::mix`], and
//! where the other four will plug in is marked.
//!
//! # How a queue gets played
//!
//! A game does not hand the hardware a sample at a time; it hands it a queue,
//! and three separate pieces keep that queue full:
//!
//! 1. A **timer** comes round at the sample rate — eleven thousand times a
//!    second, say — and one sample leaves the queue each time.
//! 2. When the queue is half empty, it asks for more.
//! 3. A **memory mover** in its special mode answers, and posts sixteen bytes.
//!
//! So the sample a queue is "on" changes at the timer's pace and is *held*
//! between times, which is what makes it a signal at all rather than a series
//! of spikes. [`Sound::tick`] is what reads that held value, over and over, and
//! turns it into samples at the rate a sound card wants.
//!
//! # From the machine's clock to the sound card
//!
//! The mixer changes value at 16.78 MHz and a sound card wants 48 kHz, so
//! everything that happens between two output samples is **averaged** rather
//! than one value in every 350 being picked out. It is a box filter, the
//! cheapest one that works, and it is the same one the older machine's core
//! uses.
//!
//! Afterwards a high-pass removes the DC. It is not cosmetic: a queue sitting
//! at some constant sample is an offset, and every time one starts or stops
//! there would be an audible click. The hardware does it with a capacitor on
//! the output.
//!
//! What is deliberately **not** here is a speaker low-pass. The older machine's
//! core has one because four raw square waves through a tiny speaker are
//! harsher than that console ever sounded. This machine's music arrives already
//! mixed down by the game to eight bits at some 16 kHz, band-limited by the
//! game itself, and rolling more treble off it only muffles it.

mod fifo;

use crate::timers::COUNT as TIMERS;
use crate::CLOCK_HZ;
use fifo::Fifo;

/// Where a game posts samples. Two addresses, four bytes each, and write-only:
/// they are the mouth of a queue, and there is nothing there to read.
pub const FIFO_A: u32 = 0x0400_00A0;
pub const FIFO_B: u32 = 0x0400_00A7;

/// `SOUNDCNT_L`: how loud the four inherited channels are on each side, and
/// which of them sounds on which. Kept, and so far nothing is listening to it:
/// see [`Sound::psg`].
pub const VOLUME: u32 = 0x0400_0080;
/// `SOUNDCNT_H`: how the two queues are driven and how loud they are.
pub const CONTROL: u32 = 0x0400_0082;
/// `SOUNDCNT_X`: the one bit that turns all of it on.
pub const ENABLE: u32 = 0x0400_0084;
/// `SOUNDBIAS`: where the middle of the output sits. See [`Sound::offset`].
pub const BIAS: u32 = 0x0400_0088;

/// The whole sound block, which nothing else shares.
///
/// The four inherited channels live at the bottom of it and are not read or
/// written yet; routing the range whole means the day they are is a change to
/// this module and to nothing else.
pub const FIRST: u32 = 0x0400_0060;
pub const LAST: u32 = FIFO_B;

/// What `SOUNDBIAS` holds at reset.
///
/// It matters more than a default usually does. The BIOS routine that changes
/// the level walks it two at a time, re-reading until it reaches `0x200`, so a
/// register that answered zero was an endless loop — which is how this register
/// came to exist before any sound did.
pub const BIAS_AT_RESET: u16 = 0x0200;

/// The sample rate the machine starts at.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

// ---- `SOUNDCNT_H` ----------------------------------------------------------

/// How loud the four inherited channels are against the queues: a quarter, a
/// half, or all of what they can be. The fourth value is not one the hardware
/// documents, and is treated as the quietest.
const PSG_RATIO: u16 = 0x0003;
const A_FULL_VOLUME: u16 = 1 << 2;
const B_FULL_VOLUME: u16 = 1 << 3;
const A_RIGHT: u16 = 1 << 8;
const A_LEFT: u16 = 1 << 9;
/// Only two timers can drive a queue. A sample rate is a fast, plain count,
/// which is what timers 0 and 1 are usually left free for.
const A_USES_TIMER_1: u16 = 1 << 10;
const A_RESET: u16 = 1 << 11;
const B_RIGHT: u16 = 1 << 12;
const B_LEFT: u16 = 1 << 13;
const B_USES_TIMER_1: u16 = 1 << 14;
const B_RESET: u16 = 1 << 15;
/// The two reset bits are not stored: they are an action, and a game that read
/// one back would find a queue-emptying instruction sitting in a register.
const CONTROL_KEPT: u16 = !(A_RESET | B_RESET);

// ---- `SOUNDCNT_X` ----------------------------------------------------------

/// The master switch. Cleared, nothing sounds — the queues included.
const MASTER: u16 = 1 << 7;

// ---- The mixer's own scale -------------------------------------------------

/// What one queue at full volume is worth, as a fraction of everything the
/// output can swing.
///
/// Half, so that **both queues at once come to exactly full scale**. That is
/// the case worth getting right: a game with music in one queue and effects in
/// the other is the ordinary arrangement, and it should be able to fill both
/// without the sum clipping.
///
/// The absolute levels here are a considered choice and not a measurement.
/// Nothing on this machine has been compared against a recording of the
/// hardware, and the ratios below — a queue at half scale, the four inherited
/// channels at a quarter between them — are the ones the documentation implies
/// rather than ones anybody here has heard.
const QUEUE_AT_FULL_VOLUME: f32 = 0.5;

/// What the mixer can swing either side of nothing, in the units the bias is
/// written in.
///
/// The mixer's own output is signed and nine bits wide, and `SOUNDBIAS` is
/// documented as the number added to it to make it unsigned — which is why its
/// default is exactly this. So the level a game writes, measured against this,
/// says how far off centre it has moved the middle of the output.
const HALF_SCALE: f32 = 256.0;

/// The capacitor on the output, per cycle.
///
/// Nobody here has measured this machine's. It is the older console's constant
/// taken to the fourth root, this clock being four times as fast, so that the
/// two cores roll off at the same frequency rather than an octave and a half
/// apart.
const HIGH_PASS_FACTOR: f32 = 0.999_989_5;

/// A stereo sample, normalised to `-1.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StereoSample {
    pub left: f32,
    pub right: f32,
}

/// Both queues, the registers that drive them, and the mixer they come out of.
#[derive(Clone)]
pub struct Sound {
    queues: [Fifo; 2],
    /// The sample most recently taken from each queue, held until the next one
    /// is due. This *is* the signal: see the module's second section.
    playing: [i8; 2],

    volume: u16,
    control: u16,
    enabled: bool,
    bias: u16,

    // ---- Resampling --------------------------------------------------------
    /// Cycles per output sample, in 16-bit fixed point so that the rounding
    /// error does not accumulate over a play session.
    cycles_per_sample: u64,
    /// Fraction of a cycle accumulated towards the next sample.
    accumulator: u64,
    /// Everything that happened since the last sample was emitted, and how many
    /// cycles it covers: the box filter.
    box_sum: (f32, f32),
    box_cycles: f32,
    /// High-pass state, one per side.
    capacitor: (f32, f32),
    /// Samples ready for whoever is playing them.
    output: Vec<StereoSample>,
}

impl Default for Sound {
    fn default() -> Self {
        Self::new()
    }
}

impl Sound {
    pub fn new() -> Self {
        Self {
            queues: [Fifo::default(); 2],
            playing: [0; 2],
            volume: 0,
            control: 0,
            enabled: false,
            bias: BIAS_AT_RESET,
            cycles_per_sample: cycles_per_sample(DEFAULT_SAMPLE_RATE),
            accumulator: 0,
            box_sum: (0.0, 0.0),
            box_cycles: 0.0,
            capacitor: (0.0, 0.0),
            output: Vec::new(),
        }
    }

    /// Changes the rate samples come out at. Whatever was part-way accumulated
    /// is dropped, because it was measured against the old one.
    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cycles_per_sample = cycles_per_sample(rate);
        self.accumulator = 0;
        self.box_sum = (0.0, 0.0);
        self.box_cycles = 0.0;
    }

    /// Empties and returns the samples made since the last call.
    pub fn drain(&mut self) -> Vec<StereoSample> {
        std::mem::take(&mut self.output)
    }

    /// Throws them away instead. For a frontend with no sound card, which would
    /// otherwise grow this without bound.
    pub fn discard(&mut self) {
        self.output.clear();
    }

    /// Which timer drives a queue.
    fn timer_of(&self, which: usize) -> usize {
        let bit = if which == 0 { A_USES_TIMER_1 } else { B_USES_TIMER_1 };
        usize::from(self.control & bit != 0)
    }

    /// Takes a sample from every queue the given timers drive.
    ///
    /// `overflowed` is a bit per timer. A queue is drained once per time round,
    /// which is what makes the timer's period the sample rate.
    ///
    /// Nothing is reported back, because whether a queue wants refilling is a
    /// *state* and not something that happens: see [`Sound::hungry`].
    pub fn at_timers(&mut self, overflowed: u8) {
        for which in 0..2 {
            let timer = self.timer_of(which);
            // Only two of the four can drive a queue, so a game that set the
            // bit for a timer it never started gets nothing rather than the
            // wrong rate.
            if timer >= TIMERS || overflowed & (1 << timer) == 0 {
                continue;
            }
            self.playing[which] = self.queues[which].pop();
        }
    }

    /// Moves the clock, and with it the output.
    ///
    /// This is called with the same cycles as everything else, and what it does
    /// with them is read the held sample over and over. That is not a waste: it
    /// is the only way an output sample can land between two of the timer's,
    /// which at 48 kHz against a game's 16 kHz is most of them.
    pub fn tick(&mut self, cycles: u32) {
        let (left, right) = self.mix();
        // The cycles are handed out to the samples they fall in rather than all
        // to the one that happens to be current. It costs a loop that almost
        // never goes round twice — a step of the processor charges one cycle
        // and a sample is three hundred and fifty of them — and it is what
        // keeps a caller that hands over a large lump from emitting one sample
        // holding the lot and a string of empty ones after it.
        let mut left_to_give = u64::from(cycles) << 16;
        while left_to_give > 0 {
            let until_due = self.cycles_per_sample - self.accumulator;
            let step = left_to_give.min(until_due);
            let weight = step as f32 / 65536.0;

            self.box_sum.0 += left * weight;
            self.box_sum.1 += right * weight;
            self.box_cycles += weight;

            self.accumulator += step;
            left_to_give -= step;
            if self.accumulator >= self.cycles_per_sample {
                self.accumulator -= self.cycles_per_sample;
                self.emit();
            }
        }
    }

    /// What the mixer is putting out this instant, before it is sampled.
    ///
    /// Zero when the master switch is off, and that is the whole of what the
    /// switch does: a game that never sets it is silent, on this machine and on
    /// the real one.
    fn mix(&self) -> (f32, f32) {
        if !self.enabled {
            return (0.0, 0.0);
        }

        let (mut left, mut right) = self.psg();

        for which in 0..2 {
            let sample = f32::from(self.playing[which]) / 128.0;
            let (full, on_left, on_right) = if which == 0 {
                (A_FULL_VOLUME, A_LEFT, A_RIGHT)
            } else {
                (B_FULL_VOLUME, B_LEFT, B_RIGHT)
            };
            // Half volume is half, and it is a bit and not a level: there is no
            // third setting.
            let gain = if self.control & full != 0 {
                QUEUE_AT_FULL_VOLUME
            } else {
                QUEUE_AT_FULL_VOLUME / 2.0
            };
            if self.control & on_left != 0 {
                left += sample * gain;
            }
            if self.control & on_right != 0 {
                right += sample * gain;
            }
        }

        let offset = self.offset();
        (clip(left, offset), clip(right, offset))
    }

    /// What the four inherited channels are putting out, each side.
    ///
    /// Nothing yet. They are the older machine's squares, wave table and noise
    /// generator, reached through `0x04000060`–`0x0400007C` and the table at
    /// `0x04000090`; `SOUNDCNT_L` is already kept for them and read back, which
    /// is why a game that sets it up finds what it wrote.
    ///
    /// When they arrive they belong here, the four of them together worth a
    /// quarter of full scale before the ratio in `SOUNDCNT_H` is applied to
    /// them, and nothing above this line changes.
    fn psg(&self) -> (f32, f32) {
        let _ = (self.volume, self.ratio());
        (0.0, 0.0)
    }

    /// How much of their full loudness the four inherited channels get.
    fn ratio(&self) -> f32 {
        match self.control & PSG_RATIO {
            1 => 0.5,
            2 => 1.0,
            // 0 is a quarter, and 3 is a value the hardware does not define.
            // The quietest is the safe reading of an undefined one: it cannot
            // make anything louder than a game asked for.
            _ => 0.25,
        }
    }

    /// Where the middle of the output sits, as a fraction of full scale.
    ///
    /// `SOUNDBIAS` is a DC offset added before the ten-bit output is clipped,
    /// and the high-pass downstream takes the offset itself straight back out
    /// again. So the *only* thing it can be heard doing is moving where
    /// clipping starts, which is exactly what it does here — and exactly what
    /// it does on the hardware, where it is used to trade headroom on one side
    /// for headroom on the other.
    fn offset(&self) -> f32 {
        let level = i32::from((self.bias >> 1) & 0x1FF);
        (level - 0x100) as f32 / HALF_SCALE
    }

    /// One sample, out of everything accumulated since the last one.
    fn emit(&mut self) {
        let cycles = if self.box_cycles > 0.0 { self.box_cycles } else { 1.0 };
        let raw = (self.box_sum.0 / cycles, self.box_sum.1 / cycles);
        self.box_sum = (0.0, 0.0);
        self.box_cycles = 0.0;

        // The output is the signal minus its own moving average, which is what
        // a capacitor in series does.
        let left = raw.0 - self.capacitor.0;
        let right = raw.1 - self.capacitor.1;
        let decay = HIGH_PASS_FACTOR.powi((self.cycles_per_sample >> 16) as i32);
        self.capacitor.0 = raw.0 - left * decay;
        self.capacitor.1 = raw.1 - right * decay;

        self.output.push(StereoSample { left, right });
    }

    /// The sample each queue is on.
    pub fn playing(&self) -> [i8; 2] {
        self.playing
    }

    /// Whether each queue wants refilling.
    ///
    /// A state and not an event, and the difference matters at both ends. A
    /// queue that has never been played from is empty and wants filling *now*,
    /// without waiting for a first sample to be due — which is how a tune
    /// starts at all. And a queue still below the mark after one refill asks
    /// again, which is how an empty one is filled the whole way rather than
    /// half way.
    pub fn hungry(&self) -> [bool; 2] {
        [self.queues[0].hungry(), self.queues[1].hungry()]
    }

    pub fn read8(&self, addr: u32) -> u8 {
        let half = |value: u16| (value >> ((addr & 1) * 8)) as u8;
        match addr & !1 {
            VOLUME => half(self.volume),
            CONTROL => half(self.control),
            // Bits 0-3 say which of the four inherited channels is still
            // sounding. None of them is, because there are none.
            ENABLE => half(u16::from(self.enabled) << 7),
            BIAS => half(self.bias),
            // The queues are write-only. There is nothing at the mouth of a
            // queue to read, and answering with a sample would be answering
            // with one the hardware has already played or not yet reached.
            _ => 0,
        }
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        if (FIFO_A..=FIFO_B).contains(&addr) {
            // Which queue, by which half of the eight bytes it landed in.
            let which = usize::from(addr >= FIFO_A + 4);
            self.queues[which].push(value);
            return;
        }

        let shift = (addr & 1) * 8;
        let widened = |old: u16| (old & !(0xFFu16 << shift)) | (u16::from(value) << shift);

        match addr & !1 {
            VOLUME => self.volume = widened(self.volume),
            CONTROL => {
                let written = widened(self.control);
                // Emptying a queue is what a game does when it changes tune:
                // whatever is still queued belongs to the old one and must not
                // be played over the new.
                if written & A_RESET != 0 {
                    self.queues[0].clear();
                }
                if written & B_RESET != 0 {
                    self.queues[1].clear();
                }
                self.control = written & CONTROL_KEPT;
            }
            ENABLE => {
                // The low byte is where the switch is; the high byte of this
                // register is nothing at all.
                if shift == 0 {
                    self.enabled = u16::from(value) & MASTER != 0;
                }
            }
            BIAS => self.bias = widened(self.bias),
            _ => {}
        }
    }
}

/// Cycles per output sample, in 16-bit fixed point.
fn cycles_per_sample(rate: u32) -> u64 {
    (u64::from(CLOCK_HZ) << 16) / u64::from(rate.max(1))
}

/// The ten-bit output, which cannot swing past its ends.
///
/// Clipping is done in the mixer's own units and about the bias, not about
/// zero: that is the point of the bias, and doing it about zero would make the
/// register unhearable.
fn clip(value: f32, offset: f32) -> f32 {
    (value + offset).clamp(-1.0, 1.0) - offset
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine with the master switch on, which is the state a game puts it
    /// in before anything else. Timer 0 drives both queues, which is the
    /// resting state of the register.
    fn machine() -> Sound {
        let mut sound = Sound::new();
        sound.write8(ENABLE, MASTER as u8);
        sound
    }

    fn post(sound: &mut Sound, which: u32, bytes: &[u8]) {
        let at = FIFO_A + which * 4;
        for byte in bytes {
            sound.write8(at, *byte);
        }
    }

    fn write16(sound: &mut Sound, addr: u32, value: u16) {
        sound.write8(addr, value as u8);
        sound.write8(addr + 1, (value >> 8) as u8);
    }

    /// Both queues on, both sides, both at full volume.
    fn wide_open(sound: &mut Sound) {
        write16(
            sound,
            CONTROL,
            A_FULL_VOLUME | B_FULL_VOLUME | A_LEFT | A_RIGHT | B_LEFT | B_RIGHT,
        );
    }

    /// Runs the clock until one sample has come out, and gives it back.
    fn one_sample(sound: &mut Sound) -> StereoSample {
        sound.drain();
        while sound.output.is_empty() {
            sound.tick(64);
        }
        sound.drain()[0]
    }

    // ---- The queues --------------------------------------------------------

    /// Which timer each queue listens to is a bit apiece, and getting it wrong
    /// plays a tune at the wrong speed rather than not at all.
    #[test]
    fn each_queue_listens_to_the_timer_its_bit_names() {
        let mut sound = machine();
        // A on timer 0, B on timer 1.
        sound.write8(CONTROL + 1, (B_USES_TIMER_1 >> 8) as u8);
        post(&mut sound, 0, &[10]);
        post(&mut sound, 1, &[20]);

        sound.at_timers(1 << 0);
        assert_eq!(sound.playing(), [10, 0], "only A moved");

        sound.at_timers(1 << 1);
        assert_eq!(sound.playing(), [10, 20], "and now B");
    }

    /// A timer nobody is listening to drains nothing.
    #[test]
    fn a_timer_no_queue_names_drains_neither() {
        let mut sound = machine();
        post(&mut sound, 0, &[10]);
        sound.at_timers(1 << 2);
        assert_eq!(sound.playing(), [0, 0]);
    }

    /// Oldest out first. A queue that answered with the newest would play every
    /// tune backwards in blocks of sixteen.
    #[test]
    fn samples_come_out_in_the_order_they_went_in() {
        let mut sound = machine();
        post(&mut sound, 0, &[1, 2, 3]);
        for expected in [1, 2, 3] {
            sound.at_timers(1);
            assert_eq!(sound.playing()[0], expected);
        }
    }

    /// Samples are signed: a byte over 127 is a negative sample, not a loud
    /// one, and reading it unsigned turns the bottom half of every waveform
    /// inside out.
    #[test]
    fn a_sample_is_signed() {
        let mut sound = machine();
        post(&mut sound, 0, &[0xFF, 0x80, 0x7F]);
        let mut heard = Vec::new();
        for _ in 0..3 {
            sound.at_timers(1);
            heard.push(sound.playing()[0]);
        }
        assert_eq!(heard, [-1, -128, 127]);
    }

    /// An empty queue is silence, not the last sample held. A held sample is a
    /// click; silence is an honest gap.
    #[test]
    fn an_empty_queue_plays_silence_rather_than_the_last_sample() {
        let mut sound = machine();
        post(&mut sound, 0, &[42]);
        sound.at_timers(1);
        assert_eq!(sound.playing()[0], 42);

        sound.at_timers(1);
        assert_eq!(sound.playing()[0], 0, "nothing left to play");
    }

    /// The low-water mark. Asking at half full rather than at empty is what
    /// leaves the mover a millisecond to be late in.
    #[test]
    fn a_queue_asks_for_more_at_half_empty_and_not_before() {
        let mut sound = machine();
        post(&mut sound, 0, &[7; fifo::DEPTH]);
        assert_eq!(sound.hungry(), [false, true], "full A, empty B");

        // Down to the mark, one sample at a time.
        for _ in 0..(fifo::DEPTH - fifo::LOW_WATER - 1) {
            sound.at_timers(1);
            assert!(!sound.hungry()[0], "still above the mark");
        }
        sound.at_timers(1);
        assert!(sound.hungry()[0], "and here it asks");
    }

    /// A full queue drops what it cannot hold. There is nowhere to put it and
    /// nothing to tell.
    #[test]
    fn a_full_queue_drops_what_it_cannot_hold() {
        let mut sound = machine();
        let mut posted: Vec<u8> = (0..fifo::DEPTH as u8).collect();
        posted.push(99);
        post(&mut sound, 0, &posted);

        for expected in 0..fifo::DEPTH as i8 {
            sound.at_timers(1);
            assert_eq!(sound.playing()[0], expected);
        }
        sound.at_timers(1);
        assert_eq!(sound.playing()[0], 0, "the one past the end was never held");
    }

    /// Changing tune empties the queue: what is still in it belongs to the old
    /// one and must not be played over the new.
    #[test]
    fn the_reset_bit_empties_a_queue() {
        let mut sound = machine();
        post(&mut sound, 0, &[1, 2, 3]);
        post(&mut sound, 1, &[4, 5, 6]);

        // Both queues are on timer 0, so one time round drains both — which is
        // what makes this the test: the same tick shows A emptied and B not.
        sound.write8(CONTROL + 1, (A_RESET >> 8) as u8);
        sound.at_timers(1);
        assert_eq!(sound.playing(), [0, 4], "A was emptied and B was left alone");

        sound.at_timers(1);
        assert_eq!(sound.playing(), [0, 5], "and B carries on through its own");
    }

    /// The reset bits are an action and not a setting. A game reading one back
    /// would find an instruction sitting in a register.
    #[test]
    fn the_reset_bits_do_not_read_back() {
        let mut sound = machine();
        let written = A_RESET | B_RESET | B_USES_TIMER_1;
        sound.write8(CONTROL + 1, (written >> 8) as u8);
        let read = u16::from(sound.read8(CONTROL)) | (u16::from(sound.read8(CONTROL + 1)) << 8);
        assert_eq!(read & (A_RESET | B_RESET), 0, "the actions are gone");
        assert_ne!(read & B_USES_TIMER_1, 0, "the setting beside them stayed");
    }

    /// The queues do not read back. There is nothing at the mouth of a queue to
    /// read.
    #[test]
    fn the_queues_are_write_only() {
        let mut sound = machine();
        post(&mut sound, 0, &[0xAB]);
        for addr in FIFO_A..=FIFO_B {
            assert_eq!(sound.read8(addr), 0, "0x{addr:08X}");
        }
    }

    /// The two queues are separate all the way down: their own bytes, their own
    /// timer, their own mark.
    #[test]
    fn the_two_queues_are_independent() {
        let mut sound = machine();
        post(&mut sound, 0, &[1; fifo::DEPTH]);
        post(&mut sound, 1, &[2; fifo::DEPTH]);
        // Both on timer 0.
        sound.at_timers(1);
        assert_eq!(sound.playing(), [1, 2]);
        assert_eq!(sound.hungry(), [false, false]);
    }

    // ---- The mixer ---------------------------------------------------------

    /// Nothing sounds until the master switch is set, and that is the whole of
    /// what it does. A machine that ignored it would play the noise a game
    /// leaves in its queues while it is still setting them up.
    #[test]
    fn nothing_sounds_until_the_master_switch_is_set() {
        let mut sound = Sound::new();
        wide_open(&mut sound);
        post(&mut sound, 0, &[127]);
        sound.at_timers(1);
        assert_eq!(one_sample(&mut sound), StereoSample::default(), "switched off");

        sound.write8(ENABLE, MASTER as u8);
        assert!(one_sample(&mut sound).left > 0.0, "and now it is switched on");
    }

    /// Turning it off again silences a queue that is mid-tune, rather than
    /// leaving the last sample sitting there.
    #[test]
    fn the_master_switch_silences_what_is_already_playing() {
        let mut sound = machine();
        wide_open(&mut sound);
        post(&mut sound, 0, &[127]);
        sound.at_timers(1);
        assert!(one_sample(&mut sound).left > 0.0);

        sound.write8(ENABLE, 0);
        assert_eq!(sound.mix(), (0.0, 0.0));
    }

    /// A queue reaches the side its bit names and no other. A game panning an
    /// effect right expects nothing on the left.
    #[test]
    fn a_queue_only_reaches_the_side_its_bit_names() {
        let mut sound = machine();
        write16(&mut sound, CONTROL, A_FULL_VOLUME | A_RIGHT);
        post(&mut sound, 0, &[127]);
        sound.at_timers(1);

        let (left, right) = sound.mix();
        assert_eq!(left, 0.0, "nothing on the left");
        assert!(right > 0.0, "and the whole of it on the right");
    }

    /// Half volume is half. It is one bit, so getting it backwards is a tune at
    /// twice the loudness it was mixed for.
    #[test]
    fn the_volume_bit_halves_a_queue() {
        let mut sound = machine();
        write16(&mut sound, CONTROL, A_FULL_VOLUME | A_LEFT);
        post(&mut sound, 0, &[127, 127]);
        sound.at_timers(1);
        let loud = sound.mix().0;

        write16(&mut sound, CONTROL, A_LEFT);
        let quiet = sound.mix().0;
        assert!((loud - quiet * 2.0).abs() < 1e-6, "{loud} is twice {quiet}");
    }

    /// Both queues at full volume come to exactly full scale, which is the
    /// arrangement the levels were chosen around: music in one and effects in
    /// the other, and neither clipping the other off.
    #[test]
    fn both_queues_at_full_volume_reach_full_scale_and_no_further() {
        let mut sound = machine();
        wide_open(&mut sound);
        post(&mut sound, 0, &[128]);
        post(&mut sound, 1, &[128]);
        sound.at_timers(1);

        // -128 is as far down as a sample goes, and there are two of them.
        assert_eq!(sound.playing(), [-128, -128]);
        assert_eq!(sound.mix(), (-1.0, -1.0));
    }

    /// And past that it clips instead of wrapping. Wrapping would turn the
    /// loudest moment of a tune into its opposite, which is the worst possible
    /// noise to make.
    #[test]
    fn the_output_clips_rather_than_wrapping() {
        let mut sound = machine();
        wide_open(&mut sound);
        // The bias moves where clipping starts, which is how it is heard at
        // all: with the middle of the output at the very bottom there is no
        // headroom below it.
        write16(&mut sound, BIAS, 0);
        post(&mut sound, 0, &[128]);
        post(&mut sound, 1, &[128]);
        sound.at_timers(1);

        let (left, _) = sound.mix();
        assert!(left > -1.0, "clipped short of full scale, at {left}");
        assert_eq!(left, 0.0, "with the middle at the bottom, nothing below it fits");
    }

    /// `SOUNDBIAS` reads back as it was left, and it starts where the BIOS
    /// expects to find it: a register answering zero is a routine that never
    /// finishes.
    #[test]
    fn the_bias_starts_where_the_bios_looks_for_it() {
        let mut sound = Sound::new();
        assert_eq!(u16::from(sound.read8(BIAS + 1)) << 8 | u16::from(sound.read8(BIAS)), 0x0200);
        assert_eq!(sound.offset(), 0.0, "and the middle of the output is the middle");

        write16(&mut sound, BIAS, 0x0200 + 2);
        assert_eq!(u16::from(sound.read8(BIAS)), 0x02);
    }

    /// `SOUNDCNT_L` is kept even though nothing generates the channels it is
    /// about. A game sets it up early and reads it back, and a register that
    /// answered zero would look to it like the write had not happened.
    #[test]
    fn the_inherited_channels_volume_is_kept_though_nothing_makes_them_yet() {
        let mut sound = machine();
        write16(&mut sound, VOLUME, 0x7755);
        let read = u16::from(sound.read8(VOLUME)) | (u16::from(sound.read8(VOLUME + 1)) << 8);
        assert_eq!(read, 0x7755);
        assert_eq!(sound.psg(), (0.0, 0.0), "and it still makes no sound");
    }

    /// The master switch reads back. A game polls it after setting it.
    #[test]
    fn the_master_switch_reads_back() {
        let mut sound = Sound::new();
        assert_eq!(sound.read8(ENABLE) & MASTER as u8, 0);
        sound.write8(ENABLE, MASTER as u8);
        assert_ne!(sound.read8(ENABLE) & MASTER as u8, 0);
    }

    // ---- Sampling ----------------------------------------------------------

    /// The clock is what makes samples, and it makes them at the rate asked
    /// for. A machine that made them at the timer's rate instead would hand a
    /// sound card a third of what it needs and play everything at a third
    /// speed.
    #[test]
    fn samples_come_out_at_the_rate_that_was_asked_for() {
        let mut sound = machine();
        sound.tick(CLOCK_HZ);
        let made = sound.drain().len() as i64;
        assert!(
            (made - i64::from(DEFAULT_SAMPLE_RATE)).abs() <= 1,
            "a second of clock made {made} samples"
        );

        sound.set_sample_rate(22_050);
        sound.tick(CLOCK_HZ);
        let made = sound.drain().len() as i64;
        assert!((made - 22_050).abs() <= 1, "and at the other rate, {made}");
    }

    /// Draining takes what was made and leaves nothing behind, so that a
    /// frontend calling it every frame gets each sample once.
    #[test]
    fn draining_empties_what_it_hands_over() {
        let mut sound = machine();
        sound.tick(CLOCK_HZ / 60);
        assert!(!sound.drain().is_empty());
        assert!(sound.drain().is_empty(), "and nothing the second time");
    }

    /// Discarding is for a frontend with no sound card. Without it the samples
    /// would pile up for as long as the game ran.
    #[test]
    fn discarding_throws_them_away_without_handing_them_over() {
        let mut sound = machine();
        sound.tick(CLOCK_HZ / 60);
        sound.discard();
        assert!(sound.drain().is_empty());
    }

    /// A held sample is averaged over the cycles it was held for, not sampled
    /// once. Picking one cycle in every three hundred out of a signal that
    /// changes at 16 kHz is how aliasing gets in.
    #[test]
    fn what_happens_between_samples_is_averaged_and_not_dropped() {
        let mut sound = machine();
        wide_open(&mut sound);
        post(&mut sound, 0, &[100, 0]);

        // Half of one output sample at the top of the wave and half at zero.
        // Rounded up, because a sample is not a whole number of cycles and two
        // halves of the rounded-down one do not reach the end of it.
        let half = ((sound.cycles_per_sample >> 16) as u32 + 1).div_ceil(2);
        sound.drain();
        sound.at_timers(1);
        sound.tick(half);
        sound.at_timers(1);
        sound.tick(half);

        let heard = sound.drain();
        assert_eq!(heard.len(), 1, "one sample out of the two halves");
        let expected = (100.0 / 128.0) * QUEUE_AT_FULL_VOLUME / 2.0;
        assert!(
            (heard[0].left - expected).abs() < 0.01,
            "{} is the average of the two, not either of them",
            heard[0].left
        );
    }

    /// The high-pass takes the DC out. A queue sitting at a constant sample is
    /// an offset and not a sound, and without this it would be a thump every
    /// time a tune started.
    #[test]
    fn a_constant_sample_fades_to_nothing() {
        let mut sound = machine();
        wide_open(&mut sound);
        post(&mut sound, 0, &[100; fifo::DEPTH]);
        sound.at_timers(1);

        // The first sample carries the step; a second of it does not.
        let first = one_sample(&mut sound).left;
        assert!(first > 0.1, "the step itself is heard: {first}");

        for _ in 0..DEFAULT_SAMPLE_RATE {
            sound.tick(400);
        }
        let settled = sound.drain().pop().expect("a second of samples").left;
        assert!(settled.abs() < 0.01, "and then it has faded to {settled}");
    }
}
