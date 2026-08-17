//! How fast the emulator is actually going, and how much of the time it is
//! spending to go that fast.
//!
//! # Why two numbers and not one
//!
//! Because "it feels slow" has two different causes and they want opposite
//! answers, and a frame rate alone cannot tell them apart.
//!
//! The **rate** is how many of the console's frames were produced in a second
//! of real time. It should be the machine's own rate — a bit under sixty for
//! both consoles — and anything less is a game running slower than it did on
//! hardware, which is the thing a player notices.
//!
//! The **load** is what share of that second went into producing them. Below a
//! whole there is time to spare and the rate is being held down deliberately,
//! which is right; at a whole the emulator is flat out, and the rate falling is
//! the emulator failing to keep up rather than choosing not to.
//!
//! A rate of fifty with a load of forty per cent is a pacing problem. A rate of
//! fifty with a load of a hundred per cent is a speed problem. They are not
//! fixed in the same place, and telling them apart is the whole reason this
//! exists.
//!
//! # Why it takes the clock rather than reading one
//!
//! So that it can be tested. Nothing here calls `Instant::now`: the caller
//! measures and hands over what it measured, which means the behaviour at a
//! window boundary can be checked without waiting half a second for it.

use std::time::Duration;

/// How long a reading covers.
///
/// Short enough that a stall of half a second shows up rather than being
/// averaged away, and long enough that the number is readable instead of
/// flickering through every value between forty and sixty.
const WINDOW: Duration = Duration::from_millis(500);

/// What the meter has to say.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// Console frames produced per second of real time.
    pub fps: f64,
    /// The share of real time spent producing them, where 1.0 is flat out.
    pub load: f64,
}

impl Reading {
    /// As it appears on the menu bar.
    pub fn to_line(self) -> String {
        format!("{:.0} fps · {:.0}%", self.fps, self.load * 100.0)
    }

    /// Whether the emulator has run out of room. The threshold is under a whole
    /// on purpose: a loop that spends nine tenths of every second emulating has
    /// nothing left for the tenth that is slower than the others, and is
    /// already dropping frames by the time it reads as full.
    pub fn flat_out(self) -> bool {
        self.load >= 0.9
    }
}

/// A rolling measurement of both.
#[derive(Debug, Default)]
pub struct Rate {
    /// Real time gone by in the window so far.
    elapsed: Duration,
    /// Of that, how much was spent inside the emulator.
    spent: Duration,
    frames: u32,
    reading: Option<Reading>,
}

impl Rate {
    /// One pass of the frontend's loop: how long since the last pass, how many
    /// console frames were produced in it, and how much of that time went into
    /// producing them.
    ///
    /// It is told about passes that produced nothing, and it has to be: those
    /// are the waiting, and leaving them out would measure how fast frames come
    /// when they come rather than how many come at all.
    pub fn pass(&mut self, elapsed: Duration, frames: u32, spent: Duration) {
        self.elapsed += elapsed;
        self.spent += spent;
        self.frames += frames;
        if self.elapsed < WINDOW {
            return;
        }
        let seconds = self.elapsed.as_secs_f64();
        self.reading = Some(Reading {
            fps: f64::from(self.frames) / seconds,
            load: self.spent.as_secs_f64() / seconds,
        });
        self.elapsed = Duration::ZERO;
        self.spent = Duration::ZERO;
        self.frames = 0;
    }

    /// The last completed reading, or nothing until the first window closes.
    ///
    /// Nothing rather than a guess: a number made from a tenth of a second of
    /// evidence swings between forty and eighty and would be read as a fault
    /// that is not there.
    pub fn reading(&self) -> Option<Reading> {
        self.reading
    }

    /// Forgets what it has gathered, without dropping the reading on show.
    ///
    /// For the moments when real time passed and the emulator was deliberately
    /// not running — a dialog open, a game paused, a menu being read. Counted,
    /// they would report a machine that had slowed to nothing.
    pub fn interrupted(&mut self) {
        self.elapsed = Duration::ZERO;
        self.spent = Duration::ZERO;
        self.frames = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Sixty frames in a second, half the time spent making them.
    #[test]
    fn it_reports_the_rate_and_the_share_of_time_it_took() {
        let mut rate = Rate::default();
        assert_eq!(rate.reading(), None, "nothing until a window has closed");

        // Ten passes of 60 ms, one frame each, taking 30 ms of it.
        for _ in 0..10 {
            rate.pass(ms(60), 1, ms(30));
        }
        let reading = rate.reading().expect("the window closed");
        assert!((reading.fps - 16.7).abs() < 0.5, "{reading:?}");
        assert!((reading.load - 0.5).abs() < 0.01, "{reading:?}");
        assert!(!reading.flat_out());
    }

    /// The measurement that matters: frames are being missed and the emulator
    /// is the reason. Flat out and below the console's rate at the same time is
    /// what says so.
    #[test]
    fn an_emulator_that_cannot_keep_up_reads_as_flat_out() {
        let mut rate = Rate::default();
        // Forty frames in a second, and every millisecond of it spent.
        for _ in 0..40 {
            rate.pass(ms(25), 1, ms(25));
        }
        let reading = rate.reading().unwrap();
        assert!((reading.fps - 40.0).abs() < 0.5, "{reading:?}");
        assert!(reading.flat_out(), "{reading:?}");
    }

    /// And a slow rate with time to spare is a different fault, which must not
    /// read the same way.
    #[test]
    fn a_slow_rate_with_room_to_spare_is_not_flat_out() {
        let mut rate = Rate::default();
        for _ in 0..40 {
            rate.pass(ms(25), 1, ms(2));
        }
        assert!(!rate.reading().unwrap().flat_out());
    }

    /// Passes that produced no frame are still time gone by. Leaving them out
    /// would measure how fast a frame arrives when one does, which is always
    /// fast, instead of how many arrive.
    #[test]
    fn the_waiting_counts_against_the_rate() {
        let mut rate = Rate::default();
        for _ in 0..30 {
            rate.pass(ms(10), 1, ms(1));
            rate.pass(ms(10), 0, Duration::ZERO);
        }
        let reading = rate.reading().unwrap();
        assert!((reading.fps - 50.0).abs() < 1.0, "{reading:?}");
    }

    /// A reading stands until the next one is ready, rather than blinking out
    /// every time a window closes.
    #[test]
    fn a_reading_stays_until_the_next_one_replaces_it() {
        let mut rate = Rate::default();
        for _ in 0..10 {
            rate.pass(ms(60), 6, ms(10));
        }
        let first = rate.reading().unwrap();
        rate.pass(ms(10), 1, ms(1));
        assert_eq!(rate.reading(), Some(first), "still the last completed one");
    }

    /// Time the emulator was deliberately not running is thrown away rather
    /// than counted as a machine that stopped.
    #[test]
    fn a_pause_does_not_read_as_a_collapse() {
        let mut rate = Rate::default();
        for _ in 0..10 {
            rate.pass(ms(60), 6, ms(10));
        }
        let before = rate.reading().unwrap();

        // Five seconds of a dialog being read, and then play resumes.
        rate.pass(ms(5000), 0, Duration::ZERO);
        rate.interrupted();
        for _ in 0..9 {
            rate.pass(ms(60), 6, ms(10));
        }
        assert_eq!(rate.reading(), Some(before), "the pause closed no window");

        rate.pass(ms(60), 6, ms(10));
        let after = rate.reading().unwrap();
        assert!((after.fps - 100.0).abs() < 1.0, "{after:?}");
    }
}
