//! The two queues digital sound is played out of.
//!
//! # What is here, and what is not
//!
//! Not sound. There is no mixer, no output, and none of the four channels this
//! machine inherited from the older one. What is here is the pair of queues the
//! GBA added on top of those — and they are here **because the memory movers
//! need them**, not because anything listens yet.
//!
//! The arrangement is worth stating plainly, because it is the reason a queue
//! belongs in a machine with no sound. A game does not hand the hardware a
//! sample at a time; it hands it a queue, and three separate pieces keep that
//! queue full:
//!
//! 1. A **timer** comes round at the sample rate — eleven thousand times a
//!    second, say — and one sample leaves the queue each time.
//! 2. When the queue is half empty, it asks for more.
//! 3. A **memory mover** in its special mode answers, and posts sixteen bytes.
//!
//! So the queue is the thing in the middle, and without it the mover has
//! nothing to be triggered by and the timer has nothing to drain. A cartridge
//! sets all three up in its first second and none of it happens. That is why
//! this exists now: it is the wiring, and the sound can be hung off the far end
//! of it later, where the sample leaves.
//!
//! # Why the queue is 32 bytes and the mover posts 16
//!
//! It is a queue with a low-water mark, which is the standard answer to a
//! consumer that must never wait. Refilling when it is *empty* would mean a gap
//! whenever the mover is late; refilling at half leaves sixteen samples in hand
//! — about a millisecond and a half — for it to be late in.

use crate::timers::COUNT as TIMERS;

/// Where a game posts samples. Two addresses, four bytes each, and write-only:
/// they are the mouth of a queue, and there is nothing there to read.
pub const FIFO_A: u32 = 0x0400_00A0;
pub const FIFO_B: u32 = 0x0400_00A7;

/// The register that says how the two queues are driven.
pub const CONTROL: u32 = 0x0400_0082;

/// How much a queue holds, and the mark at which it asks for more.
const DEPTH: usize = 32;
const LOW_WATER: usize = DEPTH / 2;

/// `SOUNDCNT_H`: which timer drives each queue, and the bit that empties one.
///
/// Only two timers can do it. A sample rate is a fast, plain count, which is
/// what timers 0 and 1 are usually left free for.
const A_USES_TIMER_1: u16 = 1 << 10;
const A_RESET: u16 = 1 << 11;
const B_USES_TIMER_1: u16 = 1 << 14;
const B_RESET: u16 = 1 << 15;
/// The two reset bits are not stored: they are an action, and a game that read
/// one back would find a queue-emptying instruction sitting in a register.
const CONTROL_KEPT: u16 = !(A_RESET | B_RESET);

/// One queue: a ring of bytes, oldest out first.
#[derive(Clone, Copy)]
struct Fifo {
    bytes: [u8; DEPTH],
    /// Where the oldest byte is, and how many there are.
    head: usize,
    len: usize,
}

impl Default for Fifo {
    fn default() -> Self {
        Self { bytes: [0; DEPTH], head: 0, len: 0 }
    }
}

impl Fifo {
    /// Posts one byte. A full queue drops it, which is what the hardware does:
    /// there is nowhere to put it and nothing to tell.
    fn push(&mut self, value: u8) {
        if self.len == DEPTH {
            return;
        }
        self.bytes[(self.head + self.len) % DEPTH] = value;
        self.len += 1;
    }

    /// Takes the oldest sample. An empty queue answers with silence rather than
    /// with the last sample over again — a held sample is a click, and silence
    /// is at least an honest one.
    fn pop(&mut self) -> i8 {
        if self.len == 0 {
            return 0;
        }
        let value = self.bytes[self.head];
        self.head = (self.head + 1) % DEPTH;
        self.len -= 1;
        value as i8
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Whether it has fallen to the mark where it wants refilling.
    fn hungry(&self) -> bool {
        self.len <= LOW_WATER
    }
}

/// Both of them, and the register that drives them.
#[derive(Clone, Copy, Default)]
pub struct Sound {
    queues: [Fifo; 2],
    control: u16,
    /// The sample most recently taken from each queue.
    ///
    /// Nothing reads these yet. They are the far end of the wiring described
    /// above — the point where a mixer will pick the sample up — and keeping
    /// them is what makes the queue's output observable at all, including to a
    /// test.
    playing: [i8; 2],
}

impl Sound {
    pub fn new() -> Self {
        Self::default()
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

    /// The sample each queue is on. For whatever ends up listening.
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
        match addr & !1 {
            CONTROL => (self.control >> ((addr & 1) * 8)) as u8,
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
        let written = (self.control & !(0xFFu16 << shift)) | (u16::from(value) << shift);
        // Emptying a queue is what a game does when it changes tune: whatever
        // is still queued belongs to the old one and must not be played over
        // the new.
        if written & A_RESET != 0 {
            self.queues[0].clear();
        }
        if written & B_RESET != 0 {
            self.queues[1].clear();
        }
        self.control = written & CONTROL_KEPT;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timer 0 drives both queues, which is the resting state of the register.
    fn machine() -> Sound {
        Sound::new()
    }

    fn post(sound: &mut Sound, which: u32, bytes: &[u8]) {
        let at = FIFO_A + which * 4;
        for byte in bytes {
            sound.write8(at, *byte);
        }
    }

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
        post(&mut sound, 0, &[7; DEPTH]);
        assert_eq!(sound.hungry(), [false, true], "full A, empty B");

        // Down to the mark, one sample at a time.
        for _ in 0..(DEPTH - LOW_WATER - 1) {
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
        let mut posted: Vec<u8> = (0..DEPTH as u8).collect();
        posted.push(99);
        post(&mut sound, 0, &posted);

        for expected in 0..DEPTH as i8 {
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
        post(&mut sound, 0, &[1; DEPTH]);
        post(&mut sound, 1, &[2; DEPTH]);
        // Both on timer 0.
        sound.at_timers(1);
        assert_eq!(sound.playing(), [1, 2]);
        assert_eq!(sound.hungry(), [false, false]);
    }
}
