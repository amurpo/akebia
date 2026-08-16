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
//! The picture unit sweeps and draws: the three bitmap modes, both kinds of
//! tiled background — the ones that scroll and the two that rotate and scale —
//! and the sprites over them. The sweep came first on purpose: it is what gives
//! a game a sense of time, and without it a cartridge stops at the first thing
//! it waits for. What is left there is effects — windows, mosaic, blending.
//! See [`ppu`].
//!
//! The four memory movers work, which is what carries a game's own code and
//! graphics from the cartridge into the memory it runs them out of. See
//! [`dma`].
//!
//! The buttons are read, which is less obvious a requirement than it sounds:
//! the register reports a held button as a zero, so a machine that answers zero
//! for addresses it does not know is a machine reporting every button held for
//! ever. A cartridge sat on its title screen over it. See [`keypad`].
//!
//! Nothing else exists yet: no sound bar the one register the BIOS insists on
//! reading back, no timers, and no timing worth the name — a step of the
//! processor charges one cycle, which is a floor and not a measurement.

pub mod bus;
pub mod console;
pub mod cpu;
pub mod dma;
pub mod interrupts;
pub mod keypad;
pub mod ppu;
pub mod sound;
pub mod timers;

pub use console::Gba;
pub use cpu::{Mode, Registers};
pub use keypad::Button;

/// Frequency of the master clock, in Hz: four times the Game Boy's.
pub const CLOCK_HZ: u32 = 16_777_216;

/// The screen, in pixels. Half again as wide as the older machine's and a
/// little taller.
pub const SCREEN_WIDTH: usize = 240;
pub const SCREEN_HEIGHT: usize = 160;
