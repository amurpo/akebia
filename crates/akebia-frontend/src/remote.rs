//! One console with the other end of its cable somewhere else.
//!
//! This is where the three pieces meet: a [`GameBoy`], the
//! [`Session`](akebia_core::link::session::Session) that decides when it may
//! run, and a [`Wire`] that carries the packets. Each of the three knows nothing
//! of the other two, which is the point — the console does not know it is on a
//! network, the protocol does not know what a socket is, and the socket does not
//! know what a Game Boy is.
//!
//! Its opposite number is [`akebia_core::link`], the same job with the other
//! console in this very process. What that one does in an instant, this one does
//! across a network, and the difference is entirely in the waiting.

use std::time::{Duration, Instant};

use akebia_core::cpu::Fault;
use akebia_core::link::session::{Incoming, Session};
use akebia_core::ports::VideoOutput;
use akebia_core::GameBoy;

use crate::net::Wire;

/// How long a call to [`Remote::run_frame`] may spend before coming back
/// empty-handed.
///
/// The interface has to be drawn and the buttons read, so the emulator cannot be
/// left in a loop for as long as the other console feels like taking. Two thirds
/// of a frame leaves room for both and is long enough that a link answering in a
/// millisecond is never the reason a frame was missed.
const BUDGET: Duration = Duration::from_micros(11_000);

/// How long to wait on the wire while the console is stalled.
///
/// Short, and it is not impatience: it is the difference between noticing the
/// packet that lets the console run and going back to the interface to be asked
/// again a whole frame later.
const SIP: Duration = Duration::from_millis(1);

/// Why a linked console stopped.
#[derive(Debug)]
pub enum Trouble {
    /// The emulated CPU fell over. Nothing to do with the link.
    Fault(Fault),
    /// The other end went away.
    Gone,
    /// The other end closed the link on purpose.
    Closed,
    /// The other end speaks a version of the protocol this does not.
    Incompatible,
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fault(fault) => write!(f, "{}", crate::describe_fault(*fault)),
            Self::Gone => write!(f, "the other console is not there any more"),
            Self::Closed => write!(f, "the other console unplugged the cable"),
            Self::Incompatible => write!(f, "the other end speaks another link protocol"),
        }
    }
}

/// Which end of the connection this is.
///
/// The only thing it decides is who steps aside so that two consoles are not in
/// perfect step; see [`akebia_core::link::stagger`]. Exactly one end dialled, so
/// it is the tie-break that is going spare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// This end waited for somebody to arrive.
    Waited,
    /// This end went looking.
    Dialled,
}

/// A console joined to another over a socket.
pub struct Remote {
    session: Session,
    wire: Wire,
}

impl Remote {
    /// Takes over a connection that is already made.
    ///
    /// The console's clock is asked for here and not later: the protocol counts
    /// from the moment the two meet, so this is the moment.
    pub fn new(wire: Wire, gb: &mut GameBoy, role: Role) -> Self {
        // Two copies of one saved game, playing the same moves, decide to take
        // the clock on the very same frame and neither is left listening. On a
        // table that cannot happen; here it has to be arranged, and the end that
        // dialled is the one that does the arranging because exactly one did.
        if role == Role::Dialled {
            akebia_core::link::stagger(gb);
        }
        Self { session: Session::new(gb.t_cycles()), wire }
    }

    /// Who is at the other end.
    pub fn peer(&self) -> &str {
        &self.wire.peer
    }

    /// Whether the greeting went through and bytes may start moving.
    pub fn is_ready(&self) -> bool {
        self.session.is_ready()
    }

    /// Tells the other end this console is going, and lets the connection close.
    pub fn close(mut self) {
        self.session.close();
        self.flush();
        // The wire is dropped with this, which shuts the socket down and takes
        // its two threads with it. Whether the last packet made it out is not
        // worth waiting to find out: the other end treats a cable that went
        // quiet the same way, only with a worse message for the player.
    }

    /// Advances the console as far as the link allows, up to a frame.
    ///
    /// Answers whether a frame came out. `false` is not a failure: it is a
    /// console waiting for the other one, and the caller should draw what it has
    /// and come back.
    pub fn run_frame(
        &mut self,
        gb: &mut GameBoy,
        video: &mut impl VideoOutput,
    ) -> Result<bool, Trouble> {
        let deadline = Instant::now() + BUDGET;

        loop {
            self.pump(gb)?;
            self.session.advance_to(gb.t_cycles());
            self.flush();

            if !self.wire.is_up() {
                return Err(Trouble::Gone);
            }
            if self.session.is_refused() {
                return Err(Trouble::Incompatible);
            }

            if !self.session.may_run() {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                // Nothing to do but wait for the word that lets us move. Waiting
                // *here* rather than back in the interface is what keeps a link
                // that answers in a millisecond from costing sixteen.
                if let Some(packet) = self.wire.recv_timeout(SIP) {
                    self.deliver(packet, gb)?;
                }
                continue;
            }

            // How far the other end has vouched for. Running to there in one go
            // and not instruction by instruction is what keeps the cost of the
            // link off the emulation.
            let target = gb.t_cycles() + self.session.allowance_t_cycles();
            while gb.t_cycles() <= target {
                gb.step().map_err(Trouble::Fault)?;

                // Eight bits are out and the other console has to say what came
                // back. Nothing else may happen until it does.
                if let Some(byte) = gb.link_pending() {
                    self.session.send(byte, gb.link_control());
                    break;
                }
                if gb.present_if_ready(video) {
                    self.session.advance_to(gb.t_cycles());
                    self.flush();
                    return Ok(true);
                }
            }

            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }

    /// Reads whatever has arrived and acts on it.
    fn pump(&mut self, gb: &mut GameBoy) -> Result<(), Trouble> {
        while let Some(packet) = self.wire.try_recv() {
            self.deliver(packet, gb)?;
        }
        Ok(())
    }

    /// One packet, from the wire to the console.
    fn deliver(&mut self, packet: akebia_core::link::bgb::Packet, gb: &mut GameBoy) -> Result<(), Trouble> {
        match self.session.receive(packet) {
            Incoming::Nothing => {}
            Incoming::Clocked { data, .. } => {
                // `None` is a console that had armed nothing, and that is a
                // packet of its own rather than a byte of ones: the difference
                // is what keeps a game from reading back what it just sent.
                let answer = gb.link_clock_in(data);
                self.session.answer(answer);
            }
            Incoming::Answered(byte) => gb.link_complete(byte),
            Incoming::Closed => return Err(Trouble::Closed),
        }
        Ok(())
    }

    /// Puts everything the session has queued on the wire.
    fn flush(&mut self) {
        for packet in self.session.take_outgoing() {
            self.wire.send(packet);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akebia_core::ports::NullOutput;
    use std::net::TcpListener;

    /// A ROM that puts `byte` in `SB`, arms a transfer and stays there.
    /// `sc` picks the role: `0x81` drives the clock, `0x80` waits for it.
    fn sender(byte: u8, sc: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 32 * 1024];
        let program: &[u8] = &[
            0x3E, byte, // LD A,byte
            0xE0, 0x01, // LDH (SB),A
            0x3E, sc,   // LD A,sc
            0xE0, 0x02, // LDH (SC),A
            0x18, 0xFE, // JR -2
        ];
        rom[0x0100..0x0100 + program.len()].copy_from_slice(program);
        rom
    }

    /// Two consoles on a real loopback socket, each with its own thread, run
    /// until both have said what they had to say.
    ///
    /// It is the whole stack at once —serial port, protocol, pacing, sockets—
    /// and it is worth having as one test because every piece under it is
    /// already covered on its own: what this can still catch is the seam.
    fn trade(master: Vec<u8>, slave: Vec<u8>) -> (u8, u8) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let listening = std::thread::spawn(move || {
            let mut gb = GameBoy::new(master).unwrap();
            gb.set_link_connected(true);
            let wire = crate::net::Wire::accept_from(&listener).unwrap();
            let mut remote = Remote::new(wire, &mut gb, Role::Waited);
            for _ in 0..600 {
                if remote.run_frame(&mut gb, &mut NullOutput).is_err() {
                    break;
                }
            }
            gb.peek(0xFF01)
        });

        let calling = std::thread::spawn(move || {
            let mut gb = GameBoy::new(slave).unwrap();
            gb.set_link_connected(true);
            let wire = crate::net::Wire::dial(&format!("127.0.0.1:{port}")).unwrap();
            let mut remote = Remote::new(wire, &mut gb, Role::Dialled);
            for _ in 0..600 {
                if remote.run_frame(&mut gb, &mut NullOutput).is_err() {
                    break;
                }
            }
            gb.peek(0xFF01)
        });

        (listening.join().unwrap(), calling.join().unwrap())
    }

    /// The one that matters: two consoles on opposite ends of a socket swap
    /// their bytes, exactly as two on one table do.
    #[test]
    fn two_consoles_over_a_socket_swap_their_bytes() {
        let (master, slave) = trade(sender(0x42, 0x81), sender(0x99, 0x80));
        assert_eq!(master, 0x99, "the master ends up with the slave's byte");
        assert_eq!(slave, 0x42, "and the slave with the master's");
    }

    /// A console whose partner armed nothing must read the empty line, not a
    /// stale byte — over a socket the two are different packets, and this is the
    /// end of the stack where that has to still be true.
    #[test]
    fn a_partner_that_armed_nothing_reads_as_absent() {
        // `SC` without the start bit: the second console never listens.
        let (master, slave) = trade(sender(0x42, 0x81), sender(0x99, 0x00));
        assert_eq!(master, 0xFF, "nobody answering: the line rests high");
        assert_eq!(slave, 0x99, "and its register was never touched");
    }

    #[test]
    fn a_link_that_never_comes_up_does_not_run_the_console() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // Somebody connects and then says nothing at all, ever.
        let mute = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

        let mut gb = GameBoy::new(sender(0x42, 0x81)).unwrap();
        gb.set_link_connected(true);
        let wire = crate::net::Wire::accept_from(&listener).unwrap();
        let mut remote = Remote::new(wire, &mut gb, Role::Waited);

        assert!(!remote.run_frame(&mut gb, &mut NullOutput).unwrap());
        assert_eq!(gb.t_cycles(), 0, "not one instruction without a greeting");
        assert!(!remote.is_ready());
        drop(mute);
    }
}
