//! The emulator's output ports.
//!
//! **Hexagonal architecture.** These traits are the contract between the core
//! and the outside world. The core *uses* them; the frontend *implements* them.
//! Thanks to that, `akebia-core` depends on no graphics or audio library, and
//! the same core serves the terminal, a GUI or WebAssembly.
//!
//! Input (buttons) needs no port: it is a one-off push from the frontend with
//! [`GameBoy::set_button`](crate::GameBoy::set_button), and modelling it as a
//! *pull* trait would only add indirection.

use crate::ppu::{FrameBuffer, Rgb555};

/// Receives a finished frame.
///
/// It is invoked exactly once per frame, when the PPU enters VBlank.
pub trait VideoOutput {
    fn present(&mut self, frame: &FrameBuffer);
}

/// Receives stereo audio samples normalised to `-1.0..=1.0`.
///
/// Nobody uses it yet: it exists to pin down the shape the APU will have so that
/// adding it does not force any signature to change.
pub trait AudioOutput {
    fn queue(&mut self, left: f32, right: f32);

    /// Sample rate the core should generate samples at.
    fn sample_rate(&self) -> u32;
}

/// Null implementation, useful for tests and for running without a window.
pub struct NullOutput;

impl VideoOutput for NullOutput {
    fn present(&mut self, _frame: &FrameBuffer) {}
}

impl AudioOutput for NullOutput {
    fn queue(&mut self, _left: f32, _right: f32) {}
    fn sample_rate(&self) -> u32 {
        48_000
    }
}

/// The four shades used by **DMG mode**, from lightest to darkest.
///
/// In CGB mode it plays no part: the game writes the colours into palette RAM
/// and nobody else has any right to reinterpret them. It is handed to the core
/// with [`GameBoy::set_dmg_shades`](crate::GameBoy::set_dmg_shades) instead of
/// being applied in the frontend, because that way there is a single pixel
/// format for both consoles.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub shades: [Rgb555; 4],
}

impl Palette {
    /// The original Game Boy greens.
    pub const DMG: Self = Self {
        shades: [
            Rgb555::from_rgb888(0x9B, 0xBC, 0x0F),
            Rgb555::from_rgb888(0x8B, 0xAC, 0x0F),
            Rgb555::from_rgb888(0x30, 0x62, 0x30),
            Rgb555::from_rgb888(0x0F, 0x38, 0x0F),
        ],
    };

    /// Greyscale, more comfortable for debugging.
    pub const GRAYSCALE: Self = Self {
        shades: [
            Rgb555::from_rgb888(0xFF, 0xFF, 0xFF),
            Rgb555::from_rgb888(0xAA, 0xAA, 0xAA),
            Rgb555::from_rgb888(0x55, 0x55, 0x55),
            Rgb555::from_rgb888(0x00, 0x00, 0x00),
        ],
    };
}
