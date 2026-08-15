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
//! The processor is finished: both instruction sets, the register file with its
//! banking by mode, the whole memory map, and interrupts. A cartridge runs, and
//! a real BIOS can be handed in and boots one.
//!
//! The picture unit sweeps but does not draw. That order is deliberate — the
//! sweep is what gives a game a sense of time, and without it a cartridge stops
//! at the first thing it waits for, which is usually the second thing it does.
//! See [`ppu`].
//!
//! Nothing else exists yet: no sound, no direct memory access, no timers, and
//! no timing worth the name — a step of the processor charges one cycle, which
//! is a floor and not a measurement.

pub mod bus;
pub mod cpu;
pub mod interrupts;
pub mod ppu;

pub use cpu::{Mode, Registers};

/// Frequency of the master clock, in Hz: four times the Game Boy's.
pub const CLOCK_HZ: u32 = 16_777_216;

/// The screen, in pixels. Half again as wide as the older machine's and a
/// little taller.
pub const SCREEN_WIDTH: usize = 240;
pub const SCREEN_HEIGHT: usize = 160;
