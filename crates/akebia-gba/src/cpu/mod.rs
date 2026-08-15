//! The ARM7TDMI: two instruction sets, seven modes and a three-stage pipeline.

pub mod alu;
pub mod bus;
pub mod condition;
pub mod registers;
pub mod shift;

pub use bus::Bus;
pub use condition::Condition;
pub use registers::{Mode, Registers};
