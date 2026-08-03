//! Game Boy (DMG) and Game Boy Color (CGB) emulation core.
//!
//! # Architecture
//!
//! This crate does no I/O: it opens no windows, reads no files, touches no
//! system audio. It takes the bytes of a ROM and exposes a framebuffer, audio
//! samples and a button state. The frontend (console, GUI, WASM) is an *adapter*
//! that implements the ports in [`ports`].
//!
//! ```text
//!            ┌──────────────────────────────────────────┐
//!            │              GameBoy (Facade)            │
//!            │                                          │
//!            │   ┌─────┐   reads/writes  ┌───────────┐  │
//!            │   │ Cpu │ ──────────────► │ SystemBus │  │
//!            │   └─────┘   (each access  └─────┬─────┘  │
//!            │             advances 1 M-cycle) │        │
//!            │                                 ▼        │
//!            │            ┌────────┬────────┬───────┐   │
//!            │            │  Ppu   │ Timer  │ Apu   │   │
//!            │            └────────┴────────┴───────┘   │
//!            │                     │                    │
//!            │              ┌──────▼───────┐            │
//!            │              │ dyn Mapper   │  Strategy  │
//!            │              └──────────────┘            │
//!            └──────────────────────────────────────────┘
//!                            │           ▲
//!                  VideoSink │           │ buttons
//!                  AudioSink ▼           │
//!                       ┌───────────────────┐
//!                       │      Frontend     │
//!                       └───────────────────┘
//! ```
//!
//! # Patterns applied
//!
//! - **Facade**: [`GameBoy`] is the frontend's single entry point.
//! - **Strategy**: [`cartridge::Mapper`] abstracts the different MBCs.
//! - **Abstract Factory**: [`cartridge::Cartridge::load`] picks the mapper from
//!   the ROM header.
//! - **Dependency Inversion**: the CPU depends on the [`cpu::Bus`] trait, not on
//!   the concrete [`bus::SystemBus`]; that allows testing it against a flat
//!   64 KiB memory.
//! - **Ports & Adapters**: [`ports`] defines the outputs; the frontend
//!   implements them.
//! - **State pattern** (lightweight): the PPU is a state machine over
//!   [`ppu::Mode`].

pub mod apu;
pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod debug;
pub mod gameboy;
pub mod joypad;
pub mod model;
pub mod ports;
pub mod ppu;
pub mod serial;
pub mod timer;

pub use apu::StereoSample;
pub use gameboy::GameBoy;
pub use joypad::Button;
pub use model::Model;
pub use ppu::{FrameBuffer, Rgb555, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Frequency of the DMG's master clock, in Hz.
pub const CLOCK_HZ: u32 = 4_194_304;

/// One "M-cycle" (machine cycle) equals 4 clock cycles. All internal timing is
/// counted in T-cycles, but the bus advances 4 at a time.
pub const T_CYCLES_PER_M_CYCLE: u32 = 4;

/// Errors that loading a ROM can return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The file is smaller than the cartridge header (0x150 bytes).
    RomTooSmall { len: usize },
    /// Byte 0x0147 indicates a mapper that is not implemented yet.
    UnsupportedMapper { code: u8, name: &'static str },
    /// The size declared in the header does not match the file.
    RomSizeMismatch { declared: usize, actual: usize },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RomTooSmall { len } => {
                write!(f, "ROM too small: {len} bytes (minimum 0x150)")
            }
            Self::UnsupportedMapper { code, name } => {
                write!(f, "unsupported mapper: {name} (code 0x{code:02X})")
            }
            Self::RomSizeMismatch { declared, actual } => {
                write!(f, "the header declares {declared} bytes but the file has {actual}")
            }
        }
    }
}

impl core::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
