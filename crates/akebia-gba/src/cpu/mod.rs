//! The ARM7TDMI: two instruction sets, seven modes and a three-stage pipeline.

pub mod bus;
pub mod registers;

pub use bus::Bus;
pub use registers::{Mode, Registers};
