//! Game Boy Advance emulation core.
//!
//! # Why this is a crate of its own
//!
//! Because it shares almost nothing with the Game Boy. A different processor
//! with a different instruction set, a bus twice as wide, a picture unit with
//! six modes where the older machine has one. What the two do have in common is
//! four sound channels — the GBA carries the older machine's square, wave and
//! noise generators unchanged — and even those are reached through different
//! registers.
//!
//! So [`akebia_core`](https://docs.rs/akebia-core) is not a dependency and must
//! not become one. A core that served both would be a core that serves neither
//! well, and the seam between them would end up in the middle of every
//! instruction. What the two are expected to share, once there is enough here
//! to share it with, is the frontend's side: the traits a frame and a sample
//! come out through, and the 15-bit pixel both machines happen to use.
//!
//! # Where this is
//!
//! The processor runs ARM code: the register file with its banking, the memory
//! map, and every instruction of the first of the two sets. THUMB says it is
//! not written rather than decoding a halfword as if it were a word.
//!
//! Nothing else exists yet — no picture, no sound, no cartridge, no timing —
//! and nothing loads a ROM. The next thing worth doing is measuring what is
//! here against a test ROM, because everything above this depends on the
//! processor being right and none of it is worth writing on top of a wrong one.

pub mod bus;
pub mod cpu;

pub use cpu::{Mode, Registers};

/// Frequency of the master clock, in Hz: four times the Game Boy's.
pub const CLOCK_HZ: u32 = 16_777_216;

/// The screen, in pixels. Half again as wide as the older machine's and a
/// little taller.
pub const SCREEN_WIDTH: usize = 240;
pub const SCREEN_HEIGHT: usize = 160;
