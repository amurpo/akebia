//! The queue digital sound is played out of, one of two.
//!
//! # Why the queue is 32 bytes and the mover posts 16
//!
//! It is a queue with a low-water mark, which is the standard answer to a
//! consumer that must never wait. Refilling when it is *empty* would mean a gap
//! whenever the mover is late; refilling at half leaves sixteen samples in hand
//! — about a millisecond and a half — for it to be late in.

/// How much a queue holds, and the mark at which it asks for more.
pub const DEPTH: usize = 32;
pub const LOW_WATER: usize = DEPTH / 2;

/// A ring of bytes, oldest out first.
#[derive(Clone, Copy)]
pub struct Fifo {
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
    pub fn push(&mut self, value: u8) {
        if self.len == DEPTH {
            return;
        }
        self.bytes[(self.head + self.len) % DEPTH] = value;
        self.len += 1;
    }

    /// Takes the oldest sample. An empty queue answers with silence rather than
    /// with the last sample over again — a held sample is a click, and silence
    /// is at least an honest one.
    pub fn pop(&mut self) -> i8 {
        if self.len == 0 {
            return 0;
        }
        let value = self.bytes[self.head];
        self.head = (self.head + 1) % DEPTH;
        self.len -= 1;
        value as i8
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Whether it has fallen to the mark where it wants refilling.
    pub fn hungry(&self) -> bool {
        self.len <= LOW_WATER
    }
}
