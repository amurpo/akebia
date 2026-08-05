//! Two consoles joined by a link cable, both in this same process.
//!
//! This is the shortest cable there is: no sockets, no latency, no protocol.
//! Both consoles advance in **lockstep**, one instruction at a time, and the
//! byte one of them clocks out reaches the other before either has moved on.
//! Timing-wise it is the closest thing to two Game Boys on a table there can be.
//!
//! # Why it is worth having on its own
//!
//! Because it separates two problems that are very easy to confuse, and
//! debugging them together is miserable. A trade that fails over a network can
//! fail because the serial port is wrong or because the synchronisation is; here
//! there is no synchronisation to blame. Whatever works over this cable is a
//! problem of the emulated hardware, and whatever breaks once a socket is in the
//! middle is a problem of the transport.
//!
//! It is also the only cable that needs no second machine to try.
//!
//! # Keeping the two clocks together
//!
//! [`step`] always advances **whichever console is behind** in T-cycles. That
//! leaves them apart by at most one instruction —some twenty T-cycles— against
//! the 512 a single bit lasts, so neither can get so much as a bit ahead of the
//! other. That is the whole synchronisation, and it is why nothing here has to
//! stall anybody.
//!
//! # What comes after this
//!
//! Everything a transport needs is in the three calls [`exchange`] makes:
//! [`GameBoy::link_pending`], [`GameBoy::link_complete`] and
//! [`GameBoy::link_clock_in`]. Over a socket the shape stays the same; what
//! changes is that the answer no longer arrives in the same instant, and *that*
//! is where a real cycle-count protocol becomes necessary.

pub mod bgb;
pub mod session;

use crate::cpu::Fault;
use crate::gameboy::T_CYCLES_PER_FRAME;
use crate::ports::VideoOutput;
use crate::GameBoy;

/// Which of the two consoles is being spoken about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

/// A CPU fault, together with the console it happened on.
///
/// With two machines running, "the emulator stopped" is not enough to act on:
/// one of the two ROMs is at fault and the other one is fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkFault {
    pub side: Side,
    pub fault: Fault,
}

/// How far apart the two consoles are switched on. See [`connect`].
///
/// A third of a frame: far more than the 4096 T-cycles a byte takes, so no probe
/// from one end can land inside the other's, and well under a frame, so neither
/// game is ever a whole frame of reactions behind the other.
const SWITCH_ON_STAGGER: u64 = T_CYCLES_PER_FRAME as u64 / 3;

/// Plugs the cable into both consoles.
///
/// From here on neither of them will resolve a transfer by itself: they will
/// wait for the other, which is what a cable is. Advancing them with anything
/// other than [`step`] or [`run_frame`] will hang the first game that tries to
/// trade.
///
/// # Why the two clocks are pushed apart
///
/// Because otherwise they would be *identical*, and no game can talk over that.
///
/// A Game Boy at the Cable Club listens first: it arms a transfer as slave and
/// waits to be clocked. If nobody clocks it in time, it gives up waiting, takes
/// the clock itself and sends a byte. Whoever gets tired first leads, and that
/// is the entire negotiation —both cartridges are running the same code, so
/// there is nothing else to break the tie with.
///
/// Two consoles stepped in lockstep from zero have lived the very same number of
/// cycles, which means the same `DIV`, the same VBlank, the same countdown, and
/// they get tired **on the same T-cycle**. Both take the clock, neither is
/// listening, and the games sit there until they call it inactivity. On a table
/// this cannot happen: nobody switches two consoles on at the same instant.
///
/// So the second one is declared to have been switched on [a fraction of a
/// frame](SWITCH_ON_STAGGER) later. That is the tie-break real hardware gets for
/// free.
pub fn connect(a: &mut GameBoy, b: &mut GameBoy) {
    a.set_link_connected(true);
    b.set_link_connected(true);

    // Both are put on one clock, reading the same instant. Without this, joining
    // a console just switched on to one that has been played for an hour would
    // make the first linked frame emulate that whole hour to catch it up.
    let now = a.t_cycles().max(b.t_cycles());
    // Which of the two is the late one is arbitrary —only the gap matters— and
    // it falls on `a` so that whoever ends up waiting as slave has been
    // listening for a while before the other takes the clock.
    a.offset_clock(now - a.t_cycles() + SWITCH_ON_STAGGER);
    b.offset_clock(now - b.t_cycles());
}

/// Runs a console on its own for [a fraction of a frame](SWITCH_ON_STAGGER),
/// so that two which would otherwise be in perfect step are not.
///
/// It is the same tie-break [`connect`] arranges, for a cable that does not go
/// between two consoles in one process. There, one clock can simply be declared
/// to read later than the other and the stepping does the rest; over a network
/// there is no shared clock to declare anything about, and the only way to move
/// a console's `DIV`, its VBlank and every countdown a game derives from them is
/// to actually live those cycles.
///
/// Exactly one of the two ends should call it — the one that dialled, say, since
/// exactly one of them did. Both calling it puts them right back in step.
///
/// A fault here is not reported: the console is about to be run properly, and
/// whatever it fell over on it will fall over on again, where the caller is
/// looking.
pub fn stagger(gb: &mut GameBoy) {
    let until = gb.t_cycles() + SWITCH_ON_STAGGER;
    while gb.t_cycles() < until {
        if gb.step().is_err() {
            return;
        }
    }
}

/// Pulls the cable out of both.
///
/// A transfer left half-way is answered `0xFF` on the next tick, so neither game
/// is left waiting for an interrupt that is not coming.
pub fn disconnect(a: &mut GameBoy, b: &mut GameBoy) {
    a.set_link_connected(false);
    b.set_link_connected(false);
}

/// Advances the pair by one instruction and resolves whatever the cable owes.
///
/// The instruction goes to whichever console is behind, which is what keeps the
/// two clocks from drifting apart.
pub fn step(a: &mut GameBoy, b: &mut GameBoy) -> Result<(), LinkFault> {
    if a.t_cycles() <= b.t_cycles() {
        a.step().map_err(|fault| LinkFault { side: Side::A, fault })?;
    } else {
        b.step().map_err(|fault| LinkFault { side: Side::B, fault })?;
    }
    exchange(a, b);
    Ok(())
}

/// Advances both consoles until each has delivered a frame.
///
/// It is the linked counterpart of [`GameBoy::run_frame`], and the difference is
/// that it cannot return at the first frame: the two consoles reach the end of
/// theirs a few instructions apart, and returning early would leave one of them
/// a whole frame behind for good.
pub fn run_frame(
    a: &mut GameBoy,
    video_a: &mut impl VideoOutput,
    b: &mut GameBoy,
    video_b: &mut impl VideoOutput,
) -> Result<(), LinkFault> {
    // The same safety bound as the single-console version: a game that turns the
    // LCD off produces no frames, and without this the loop would not end.
    let limit = a.t_cycles().max(b.t_cycles()) + u64::from(T_CYCLES_PER_FRAME) * 2;
    let (mut done_a, mut done_b) = (false, false);

    while !(done_a && done_b) {
        if a.t_cycles().min(b.t_cycles()) >= limit {
            break;
        }
        step(a, b)?;
        // Both are asked every time and not just the one that moved: the flag is
        // consumed when read, and skipping a console here would drop its frame.
        done_a |= a.present_if_ready(video_a);
        done_b |= b.present_if_ready(video_b);
    }
    Ok(())
}

/// Moves whatever byte is in flight from one console to the other.
///
/// Called after every instruction. Almost always there is nothing to do, which
/// is why it is two `Option`s and not a queue.
pub fn exchange(a: &mut GameBoy, b: &mut GameBoy) {
    match (a.link_pending(), b.link_pending()) {
        // Both drove their own clock. On real hardware that is two consoles
        // fighting over one wire and what comes out is garbage; games never do
        // it, because the menu settles who leads before any byte moves. Swapping
        // is the reading that keeps both of them running.
        (Some(from_a), Some(from_b)) => {
            a.link_complete(from_b);
            b.link_complete(from_a);
        }
        // One end drove the clock and the other only has to answer. It is not
        // asked until it has actually lived through the moment the eighth bit
        // arrived: the console that is behind still has instructions left to run
        // inside that window, and one of them may be the very write that loads
        // the byte it means to send. Asking it early —which is what stepping the
        // one behind leaves you doing— reads the register one instruction before
        // the game filled it.
        (Some(from_a), None) if b.t_cycles() >= a.t_cycles() => {
            // A console that armed nothing gives up no byte, and the line it
            // leaves alone rests high.
            let from_b = b.link_clock_in(from_a).unwrap_or(crate::serial::IDLE_LINE);
            a.link_complete(from_b);
        }
        (None, Some(from_b)) if a.t_cycles() >= b.t_cycles() => {
            let from_a = a.link_clock_in(from_b).unwrap_or(crate::serial::IDLE_LINE);
            b.link_complete(from_a);
        }
        // Either nothing is in flight, or the answering end has not caught up
        // yet and will be asked on one of the next few steps.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::NullOutput;

    /// A ROM that puts `byte` in `SB`, arms a transfer and stays there.
    ///
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

    /// Runs off the head start [`connect`] gives one of the two, after which
    /// both clocks read the same instant and anything else can be measured
    /// against them.
    fn level(a: &mut GameBoy, b: &mut GameBoy) {
        while a.t_cycles().abs_diff(b.t_cycles()) > 512 {
            step(a, b).unwrap();
        }
    }

    /// Enough instructions for a byte —4096 T-cycles— to go through several
    /// times over. The idle loop is a `JR` of 12 T-cycles.
    fn run(a: &mut GameBoy, b: &mut GameBoy) {
        level(a, b);
        for _ in 0..4000 {
            step(a, b).unwrap();
        }
    }

    #[test]
    fn the_two_consoles_swap_their_bytes() {
        let mut a = GameBoy::new(sender(0x42, 0x81)).unwrap(); // drives the clock
        let mut b = GameBoy::new(sender(0x99, 0x80)).unwrap(); // follows it
        connect(&mut a, &mut b);
        run(&mut a, &mut b);

        assert_eq!(a.peek(0xFF01), 0x99, "the master ends up with the slave's byte");
        assert_eq!(b.peek(0xFF01), 0x42, "and the slave with the master's");
    }

    /// The point of the whole exercise: it is not that one sends and the other
    /// receives, it is that both do at once. Neither may keep its own byte.
    #[test]
    fn neither_keeps_the_byte_it_was_sending() {
        let mut a = GameBoy::new(sender(0x42, 0x81)).unwrap();
        let mut b = GameBoy::new(sender(0x99, 0x80)).unwrap();
        connect(&mut a, &mut b);
        run(&mut a, &mut b);

        assert_ne!(a.peek(0xFF01), 0x42);
        assert_ne!(b.peek(0xFF01), 0x99);
    }

    #[test]
    fn both_transfers_are_reported_as_finished() {
        let mut a = GameBoy::new(sender(0x42, 0x81)).unwrap();
        let mut b = GameBoy::new(sender(0x99, 0x80)).unwrap();
        connect(&mut a, &mut b);
        run(&mut a, &mut b);

        // The ROM arms the transfer once and never again, so a cleared start bit
        // can only mean the exchange went through.
        assert_eq!(a.peek(0xFF02) & 0x80, 0, "the master's start bit cleared");
        assert_eq!(b.peek(0xFF02) & 0x80, 0, "and so did the slave's");
    }

    /// Nobody drives the clock: on hardware the two would sit there forever, and
    /// so must they here. The wrong thing would be to invent an exchange.
    #[test]
    fn two_slaves_never_exchange_anything() {
        let mut a = GameBoy::new(sender(0x42, 0x80)).unwrap();
        let mut b = GameBoy::new(sender(0x99, 0x80)).unwrap();
        connect(&mut a, &mut b);
        run(&mut a, &mut b);

        assert_eq!(a.peek(0xFF01), 0x42, "it is still holding its own byte");
        assert_eq!(b.peek(0xFF01), 0x99);
        assert_eq!(a.peek(0xFF02) & 0x80, 0x80, "and still waiting for a clock");
    }

    #[test]
    fn without_the_cable_the_master_receives_an_empty_line() {
        let mut a = GameBoy::new(sender(0x42, 0x81)).unwrap();
        let mut b = GameBoy::new(sender(0x99, 0x80)).unwrap();
        // Deliberately not connected.
        run(&mut a, &mut b);

        assert_eq!(a.peek(0xFF01), 0xFF, "nothing at the other end: the line rests high");
        assert_eq!(b.peek(0xFF01), 0x99, "and the slave was never clocked");
    }

    /// If the two clocks drifted by more than a bit period the exchange would
    /// stop being simultaneous, which is the one thing a cable has to be.
    #[test]
    fn the_two_clocks_never_drift_apart_by_a_whole_bit() {
        let mut a = GameBoy::new(sender(0x42, 0x81)).unwrap();
        let mut b = GameBoy::new(sender(0x99, 0x80)).unwrap();
        connect(&mut a, &mut b);
        level(&mut a, &mut b);

        for _ in 0..4000 {
            step(&mut a, &mut b).unwrap();
            let drift = a.t_cycles().abs_diff(b.t_cycles());
            assert!(drift < 512, "the consoles drifted {drift} T-cycles apart");
        }
    }

    /// A partner that has not armed anything must read as *absent*, not as an
    /// echo.
    ///
    /// This is the shape of the bug that kept a trade from ever starting. The
    /// two players are never ready at the same instant: one reaches the Cable
    /// Club first and its game sends `0x01` at a console still walking around
    /// somewhere. If that console gives up its register anyway, the second
    /// attempt comes back carrying the first attempt's byte, and a game that
    /// reads back what it just sent takes it for an answer and moves on to a
    /// stage the other one knows nothing about.
    #[test]
    fn a_console_that_armed_nothing_never_echoes_the_byte_it_was_sent() {
        let mut a = GameBoy::new(sender(0x01, 0x81)).unwrap(); // drives the clock
        // `SC` without the start bit: the game is not listening to the cable.
        let mut b = GameBoy::new(sender(0x99, 0x00)).unwrap();
        connect(&mut a, &mut b);
        run(&mut a, &mut b);

        assert_eq!(a.peek(0xFF01), 0xFF, "nobody is answering: the line rests high");
        assert_eq!(b.peek(0xFF01), 0x99, "and its register was never touched");
    }

    /// The one thing two consoles must not have in common. Both cartridges run
    /// the same code, so the only thing that can settle which of them leads is
    /// that one gets tired of listening before the other; in perfect step they
    /// get tired on the same T-cycle and no game ever connects. Same ROM and
    /// same role here on purpose: it is the symmetric worst case.
    #[test]
    fn the_two_consoles_never_share_a_clock_phase() {
        let mut a = GameBoy::new(sender(0x42, 0x81)).unwrap();
        let mut b = GameBoy::new(sender(0x42, 0x81)).unwrap();
        connect(&mut a, &mut b);
        level(&mut a, &mut b);

        for _ in 0..4000 {
            step(&mut a, &mut b).unwrap();
            // DIV is the clock every one of those countdowns is counted with.
            assert_ne!(a.peek(0xFF04), b.peek(0xFF04), "the two consoles share a DIV");
        }
    }

    #[test]
    fn a_linked_frame_advances_both_consoles() {
        let mut a = GameBoy::new(sender(0x42, 0x81)).unwrap();
        let mut b = GameBoy::new(sender(0x99, 0x80)).unwrap();
        connect(&mut a, &mut b);

        run_frame(&mut a, &mut NullOutput, &mut b, &mut NullOutput).unwrap();

        let frame = u64::from(T_CYCLES_PER_FRAME);
        assert!(a.t_cycles() >= frame / 2, "console A barely moved: {}", a.t_cycles());
        assert!(b.t_cycles() >= frame / 2, "console B barely moved: {}", b.t_cycles());
    }
}
