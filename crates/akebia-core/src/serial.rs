//! The serial port (`SB` 0xFF01, `SC` 0xFF02).
//!
//! For playing it is useless until there is a link cable. It is implemented
//! first for another reason: **Blargg's test suites write their results through
//! it**. With the serial working the whole CPU can be validated before there is
//! a single pixel on screen, which is the right order in which to build an
//! emulator.
//!
//! With nothing connected at the other end, the hardware receives `0xFF`. Here
//! the transfer is simulated as completing immediately: the sent byte is stored,
//! the start bit is cleared and the interrupt is requested.

use crate::cpu::{Interrupt, InterruptController};

pub struct Serial {
    /// Serial Byte: the data to transmit or the one received.
    sb: u8,
    /// Serial Control. Bit 7 = transfer in progress, bit 0 = internal clock.
    sc: u8,
    /// Transmitted bytes the frontend has not collected yet.
    output: Vec<u8>,
}

impl Serial {
    pub const fn new() -> Self {
        Self { sb: 0, sc: 0x7E, output: Vec::new() }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF01 => self.sb,
            // Bits 6-1 do not exist and read as 1.
            0xFF02 => self.sc | 0x7E,
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8, ic: &mut InterruptController) {
        match addr {
            0xFF01 => self.sb = value,
            0xFF02 => {
                self.sc = value;
                // Bit 7 (start) + bit 0 (internal clock) = transmit now.
                if value & 0x81 == 0x81 {
                    self.output.push(self.sb);
                    // Nobody answers on the other side: the line is at 1.
                    self.sb = 0xFF;
                    self.sc &= !0x80;
                    ic.request(Interrupt::Serial);
                }
            }
            _ => {}
        }
    }

    /// Empties and returns what was transmitted since the last call.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.output)
    }
}

impl Default for Serial {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_transmits_when_the_start_bit_is_written() {
        let mut s = Serial::new();
        let mut ic = InterruptController::new();
        ic.write_enable(0xFF);

        s.write(0xFF01, b'O', &mut ic);
        s.write(0xFF02, 0x81, &mut ic);
        s.write(0xFF01, b'K', &mut ic);
        s.write(0xFF02, 0x81, &mut ic);

        assert_eq!(s.take_output(), b"OK");
        assert!(s.take_output().is_empty(), "the output is emptied when collected");
        assert_eq!(ic.pending(), Some(Interrupt::Serial));
    }

    #[test]
    fn with_an_external_clock_it_does_not_transmit() {
        let mut s = Serial::new();
        let mut ic = InterruptController::new();
        s.write(0xFF01, b'X', &mut ic);
        s.write(0xFF02, 0x80, &mut ic); // start, but waiting for an external clock
        assert!(s.take_output().is_empty());
    }
}
