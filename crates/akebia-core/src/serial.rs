//! The serial port (`SB` 0xFF01, `SC` 0xFF02) and the link cable.
//!
//! Two things live here, and it is worth telling them apart.
//!
//! The first is **the port as a way out for text**: Blargg's test suites report
//! their results by writing bytes to it. That needs no cable and no partner —the
//! bytes are collected with [`Serial::take_output`]— and it is why this file
//! existed before any of the rest.
//!
//! The second is **the cable**, which is what a trade between two games goes
//! through. There the port stops being a hole to drop bytes into and becomes
//! what the hardware really is: a **shift register with the clock coming from
//! one of the two ends**.
//!
//! # How the hardware exchanges a byte
//!
//! Both consoles hold a byte in `SB`. Whoever has bit 0 of `SC` set drives the
//! clock; the other one only follows. On each clock edge both registers shift
//! one bit: the master's most significant bit travels down one wire while the
//! slave's travels back down the other. Eight edges later the two bytes have
//! **swapped**, and each end gets its interrupt.
//!
//! Two consequences follow, and neither is optional if a trade is to work:
//!
//! - The exchange is **simultaneous**. There is no sender and no receiver: the
//!   byte you give and the byte you get move on the same edges.
//! - It **takes time**: 512 T-cycles per bit, 4096 for a byte, about a
//!   millisecond. A port that completes instantly is not a faster cable, it is
//!   one that hands the game an answer before the other console has had the
//!   chance to say anything.
//!
//! # What this module does not decide
//!
//! Who the other end is. [`Serial`] gets as far as knowing it has eight bits
//! ready and pausing —[`Serial::pending_out`]— until somebody says what came
//! back. Whether that somebody is another console in this same process, a socket
//! or a Bluetooth link is not its business; see [`crate::link`] for the first of
//! those.

use crate::cpu::{Interrupt, InterruptController};
use crate::model::Model;

/// T-cycles a bit lasts at the standard shift clock: 4194304 / 8192 Hz.
const BIT_T_CYCLES: u32 = 512;
/// And at the CGB's fast clock, 262144 Hz: thirty-two times less.
const FAST_BIT_T_CYCLES: u32 = 16;
/// Bits in a transfer. The register is eight wide and it does not stop halfway.
const BITS_PER_TRANSFER: u8 = 8;

/// Bit 7 of `SC`: a transfer is in progress.
const START: u8 = 0x80;
/// Bit 1 of `SC`, **CGB only**: pick the fast clock.
const FAST: u8 = 0x02;
/// Bit 0 of `SC`: this console drives the clock.
const INTERNAL_CLOCK: u8 = 0x01;

pub struct Serial {
    /// Serial Byte: what is going out, and once the transfer ends, what came in.
    sb: u8,
    /// Serial Control. See the three constants above.
    sc: u8,
    /// Which console this is. It decides whether bit 1 of `SC` exists.
    model: Model,

    /// T-cycles left before clocking the next bit. Only counts while this end
    /// drives the clock.
    countdown: u32,
    /// Bits already clocked out in the transfer in progress.
    bits: u8,
    /// Eight bits are out and the other end has not said what it was sending.
    ///
    /// This is where the emulation of a *linked* console differs from every
    /// other peripheral: it is the one point where advancing depends on somebody
    /// outside. See [`Serial::pending_out`].
    pending: bool,
    /// Whether there is anything at the other end of the cable.
    connected: bool,

    /// Transmitted bytes the frontend has not collected yet.
    output: Vec<u8>,
}

impl Serial {
    pub const fn new(model: Model) -> Self {
        Self {
            sb: 0,
            sc: 0x7E,
            model,
            countdown: 0,
            bits: 0,
            pending: false,
            connected: false,
            output: Vec::new(),
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF01 => self.sb,
            0xFF02 => self.sc | self.unused_sc_bits(),
            _ => 0xFF,
        }
    }

    /// Unlike the other peripherals this one asks for no interrupt controller:
    /// writing to `SC` only *arms* the transfer. What finishes it is the clock,
    /// in [`Serial::tick`], or the other console, in [`Serial::clock_in`].
    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0xFF01 => self.sb = value,
            0xFF02 => {
                let starting = value & START != 0;
                self.sc = value;
                self.bits = 0;
                self.pending = false;

                if starting {
                    self.countdown = self.bit_period();
                    // The byte is recorded here and not when the transfer ends,
                    // and that is deliberate: this log is Blargg's way out for
                    // text, and those tests write characters faster than a real
                    // cable could carry them without ever waiting for the
                    // interrupt. Charging them the millisecond would swallow
                    // most of the message. What goes out is decided the moment
                    // the game orders the transfer; how long the wire takes is
                    // another matter, and it is settled elsewhere.
                    if value & INTERNAL_CLOCK != 0 {
                        self.output.push(self.sb);
                    }
                }
            }
            _ => {}
        }
    }

    /// Advances the clock this end drives.
    ///
    /// A slave counts nothing: its bits arrive when the other console decides,
    /// through [`Serial::clock_in`].
    pub fn tick(&mut self, t_cycles: u32, ic: &mut InterruptController) {
        if self.pending {
            // Unplugged while waiting for the answer. Better to hand the game
            // the 0xFF of an empty line than to leave it waiting forever for an
            // interrupt that is no longer coming.
            if !self.connected {
                self.complete(0xFF, ic);
            }
            return;
        }
        if !self.is_master() {
            return;
        }

        let period = self.bit_period();
        let mut left = t_cycles;
        while left >= self.countdown {
            left -= self.countdown;
            self.countdown = period;
            self.bits += 1;

            if self.bits == BITS_PER_TRANSFER {
                if self.connected {
                    // The eight bits are out. Now somebody has to say what came
                    // back down the other wire.
                    self.pending = true;
                } else {
                    self.complete(0xFF, ic);
                }
                return;
            }
        }
        self.countdown -= left;
    }

    /// The byte this end has finished clocking out and is holding on to until
    /// the other one answers. `None` while there is nothing to resolve.
    ///
    /// This is the whole interface a transport needs on the master's side: take
    /// the byte, get it to the other console however you can, and hand back what
    /// it returns with [`Serial::complete`].
    pub fn pending_out(&self) -> Option<u8> {
        self.pending.then_some(self.sb)
    }

    /// Hands over the byte the other end was sending, ending the transfer.
    pub fn complete(&mut self, received: u8, ic: &mut InterruptController) {
        self.sb = received;
        self.sc &= !START;
        self.bits = 0;
        self.pending = false;
        ic.request(Interrupt::Serial);
    }

    /// Clocks a whole byte in from the other console, which is the one driving
    /// the clock. Returns what this end had in its register, which is exactly
    /// what the other one receives.
    ///
    /// # Why it answers even without an armed transfer
    ///
    /// The shift register is wired to the cable all the time; the start bit only
    /// says whether the interrupt fires when the eight bits are through. A
    /// console whose game has not armed the next transfer yet still gives up
    /// whatever `SB` held and still takes in what arrives. It is faithful, and
    /// it is also what makes the games' resynchronisation work: they count on
    /// reading stale bytes while the other side catches up, and that is why
    /// their protocols are full of padding bytes.
    pub fn clock_in(&mut self, incoming: u8, ic: &mut InterruptController) -> u8 {
        let outgoing = self.sb;
        self.sb = incoming;

        if self.sc & START != 0 && self.sc & INTERNAL_CLOCK == 0 {
            self.output.push(outgoing);
            self.sc &= !START;
            self.bits = 0;
            ic.request(Interrupt::Serial);
        }
        outgoing
    }

    /// Plugs the cable in or pulls it out. With nothing plugged in, transfers
    /// still take their time but always come back 0xFF.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Whether this end is driving the clock of a transfer in progress.
    pub fn is_master(&self) -> bool {
        self.sc & START != 0 && self.sc & INTERNAL_CLOCK != 0
    }

    /// T-cycles per bit. The fast clock is a CGB register; on a DMG that bit
    /// does not exist and reads as 1, so it cannot be consulted.
    fn bit_period(&self) -> u32 {
        if self.model == Model::Cgb && self.sc & FAST != 0 {
            FAST_BIT_T_CYCLES
        } else {
            BIT_T_CYCLES
        }
    }

    /// Bits of `SC` that do not exist and read as 1. On the CGB bit 1 does
    /// exist, so it is not among them.
    const fn unused_sc_bits(&self) -> u8 {
        match self.model {
            Model::Cgb => 0x7C,
            Model::Dmg => 0x7E,
        }
    }

    /// Empties and returns what was transmitted since the last call.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.output)
    }
}

impl Default for Serial {
    fn default() -> Self {
        Self::new(Model::Dmg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advances the port far enough for a whole byte to be clocked out.
    fn tick_a_byte(s: &mut Serial, ic: &mut InterruptController) {
        for _ in 0..BITS_PER_TRANSFER * 2 {
            s.tick(BIT_T_CYCLES, ic);
        }
    }

    fn dmg() -> (Serial, InterruptController) {
        let mut ic = InterruptController::new();
        ic.write_enable(0xFF);
        (Serial::new(Model::Dmg), ic)
    }

    #[test]
    fn it_transmits_when_the_start_bit_is_written() {
        let (mut s, mut ic) = dmg();

        s.write(0xFF01, b'O');
        s.write(0xFF02, 0x81);
        tick_a_byte(&mut s, &mut ic);
        s.write(0xFF01, b'K');
        s.write(0xFF02, 0x81);
        tick_a_byte(&mut s, &mut ic);

        assert_eq!(s.take_output(), b"OK");
        assert!(s.take_output().is_empty(), "the output is emptied when collected");
        assert_eq!(ic.pending(), Some(Interrupt::Serial));
    }

    /// Blargg's tests write characters without ever waiting for the interrupt.
    /// Charging them the millisecond a real cable takes would lose the message,
    /// which is the whole reason the port was implemented first.
    #[test]
    fn text_written_faster_than_the_cable_is_not_lost() {
        let (mut s, mut ic) = dmg();
        for c in b"Passed" {
            s.write(0xFF01, *c);
            s.write(0xFF02, 0x81);
            s.tick(4, &mut ic); // a single M-cycle between characters
        }
        assert_eq!(s.take_output(), b"Passed");
    }

    #[test]
    fn with_an_external_clock_it_does_not_transmit_by_itself() {
        let (mut s, mut ic) = dmg();
        s.write(0xFF01, b'X');
        s.write(0xFF02, 0x80); // start, but waiting for an external clock
        tick_a_byte(&mut s, &mut ic);

        assert!(s.take_output().is_empty());
        assert_eq!(ic.pending(), None, "nobody clocked it: there is nothing to report");
        assert_eq!(s.read(0xFF02) & START, START, "the transfer is still armed");
    }

    #[test]
    fn a_byte_costs_eight_bit_periods() {
        let (mut s, mut ic) = dmg();
        s.write(0xFF02, 0x81);

        // One T-cycle short of the eight bits, nothing has happened yet.
        s.tick(BIT_T_CYCLES * u32::from(BITS_PER_TRANSFER) - 1, &mut ic);
        assert_eq!(ic.pending(), None);
        assert_eq!(s.read(0xFF02) & START, START);

        s.tick(1, &mut ic);
        assert_eq!(ic.pending(), Some(Interrupt::Serial));
        assert_eq!(s.read(0xFF02) & START, 0, "the start bit clears on its own");
    }

    #[test]
    fn with_nothing_plugged_in_it_receives_0xff() {
        let (mut s, mut ic) = dmg();
        s.write(0xFF01, 0x42);
        s.write(0xFF02, 0x81);
        tick_a_byte(&mut s, &mut ic);
        assert_eq!(s.read(0xFF01), 0xFF, "an empty line rests high");
    }

    #[test]
    fn plugged_in_it_waits_instead_of_inventing_an_answer() {
        let (mut s, mut ic) = dmg();
        s.set_connected(true);
        s.write(0xFF01, 0x42);
        s.write(0xFF02, 0x81);
        tick_a_byte(&mut s, &mut ic);

        assert_eq!(s.pending_out(), Some(0x42), "it holds the byte until it is answered");
        assert_eq!(ic.pending(), None, "and does not report a transfer that is not finished");

        s.complete(0x99, &mut ic);
        assert_eq!(s.read(0xFF01), 0x99);
        assert_eq!(ic.pending(), Some(Interrupt::Serial));
        assert_eq!(s.pending_out(), None);
    }

    /// Pulling the cable out mid-transfer must not hang the game.
    #[test]
    fn unplugging_it_while_waiting_answers_0xff() {
        let (mut s, mut ic) = dmg();
        s.set_connected(true);
        s.write(0xFF02, 0x81);
        tick_a_byte(&mut s, &mut ic);
        assert!(s.pending_out().is_some());

        s.set_connected(false);
        s.tick(4, &mut ic);
        assert_eq!(s.read(0xFF01), 0xFF);
        assert_eq!(ic.pending(), Some(Interrupt::Serial));
    }

    #[test]
    fn the_slave_swaps_its_byte_and_gets_the_interrupt() {
        let (mut s, mut ic) = dmg();
        s.write(0xFF01, 0xAA);
        s.write(0xFF02, 0x80); // armed, external clock

        let sent = s.clock_in(0xBB, &mut ic);
        assert_eq!(sent, 0xAA, "the other end receives what this one was holding");
        assert_eq!(s.read(0xFF01), 0xBB);
        assert_eq!(s.read(0xFF02) & START, 0);
        assert_eq!(ic.pending(), Some(Interrupt::Serial));
        assert_eq!(s.take_output(), vec![0xAA]);
    }

    /// The register is wired to the cable whether the game armed a transfer or
    /// not; only the interrupt depends on the start bit.
    #[test]
    fn an_unarmed_slave_still_answers_but_is_not_interrupted() {
        let (mut s, mut ic) = dmg();
        s.write(0xFF01, 0xAA);

        let sent = s.clock_in(0xBB, &mut ic);
        assert_eq!(sent, 0xAA);
        assert_eq!(s.read(0xFF01), 0xBB);
        assert_eq!(ic.pending(), None, "nothing was armed: the game is not told");
    }

    #[test]
    fn the_cgb_fast_clock_is_thirty_two_times_shorter() {
        let mut ic = InterruptController::new();
        ic.write_enable(0xFF);
        let mut s = Serial::new(Model::Cgb);
        s.write(0xFF02, 0x83); // start + fast + internal

        s.tick(FAST_BIT_T_CYCLES * u32::from(BITS_PER_TRANSFER), &mut ic);
        assert_eq!(ic.pending(), Some(Interrupt::Serial));
    }

    /// On a DMG that bit does not exist, so setting it must change nothing.
    #[test]
    fn on_a_dmg_the_fast_bit_is_ignored() {
        let (mut s, mut ic) = dmg();
        s.write(0xFF02, 0x83);

        s.tick(FAST_BIT_T_CYCLES * u32::from(BITS_PER_TRANSFER), &mut ic);
        assert_eq!(ic.pending(), None, "it still runs at the slow clock");
    }

    #[test]
    fn the_unused_bits_of_sc_read_as_one() {
        let mut s = Serial::new(Model::Dmg);
        s.write(0xFF02, 0x00);
        assert_eq!(s.read(0xFF02), 0x7E, "on a DMG bit 1 does not exist either");

        let mut cgb = Serial::new(Model::Cgb);
        cgb.write(0xFF02, 0x00);
        assert_eq!(cgb.read(0xFF02), 0x7C, "on a CGB bit 1 is the clock speed");
    }
}
