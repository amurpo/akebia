//! The half of the cable that decides **when a console may run**.
//!
//! [`bgb`](super::bgb) says what the packets look like; this says what to do
//! with them. Between the two there are no sockets: a [`Session`] is fed packets
//! and hands back packets, and whoever carries them is the frontend's business.
//!
//! # The rule the whole thing rests on
//!
//! *A console may run up to the last instant the other end said it had reached,
//! and not one cycle further.*
//!
//! That is the entire synchronisation, and it is worth seeing why it is enough.
//! Every packet carries a timestamp, and [`Packet::Reached`] carries nothing
//! else — it is one end saying "I have lived this far with nothing to send". Any
//! byte that end sends afterwards is therefore dated *after* that instant, so a
//! console that stops there can never be handed a byte for a moment it has
//! already emulated past. Nothing has to be undone, and nothing arrives late,
//! because "late" is measured on the console's clock and not on the wall's.
//!
//! It also explains why this is not slow. The wait is not one round trip per
//! byte —that would put a millisecond of cable behind fifty of Bluetooth— it is
//! one wait per *how far ahead* an end gets, and how far ahead it gets is set by
//! how often the other one announces itself. Announcing once a frame buys about
//! sixteen milliseconds of tolerance; announcing every few frames buys more, at
//! the cost of the other console's byte being that much staler. Neither costs
//! anything per byte.
//!
//! # The two ends are not symmetrical while a byte is in flight
//!
//! When this console drives the clock it stops dead until the answer comes. It
//! has to: the transfer is over eight bit periods and the game is entitled to its
//! interrupt at the end of them, so running on while nobody has said what came
//! back would be emulating a transfer that has not finished. That is the one
//! place a round trip is really paid, and games pay it a few hundred times in a
//! trade, not a few hundred thousand.

use std::collections::VecDeque;

use super::bgb::{Control, Packet, Stamp};

/// How far this console runs past its last announcement before making another.
///
/// One frame. It is the natural unit —the frontend already thinks in frames— and
/// it is what decides how much delay the link absorbs without either side
/// waiting: the other end may always run to wherever we last said we were, so
/// announcing every frame lets it run about sixteen milliseconds ahead. Below a
/// frame the packets multiply for nothing; far above it the byte one console
/// hands the other starts being visibly stale.
pub const ANNOUNCE_EVERY_T_CYCLES: u64 = crate::gameboy::T_CYCLES_PER_FRAME as u64;

/// How far past the other end's last word a console is allowed to get.
///
/// # Why there is a lead at all
///
/// Because without one nothing moves. "Run up to what the other end vouched
/// for" sounds like the safe rule and it is a deadlock: a console cannot run a
/// frame until its partner says it has, and its partner is waiting on exactly
/// the same thing. Both sit at the instant they met.
///
/// So a console is allowed a frame's head start over what it has been told, and
/// that is what turns the two of them over: each runs a frame, says so, and the
/// saying so is what buys the other its next frame. It also decides how much
/// delay the link swallows without either console slowing down — a round trip
/// shorter than a frame is free, which covers a local network and most of
/// Bluetooth.
///
/// # What it costs
///
/// Honesty about a byte's placing. A byte the other end clocks may be dated up
/// to this far before the instant this console has already reached, and there is
/// nothing to be done about that but apply it where we are. It is the one
/// approximation in the whole cable, it is the same one BGB makes, and a frame
/// of it is nothing against protocols whose own bytes are a millisecond apart
/// and padded besides.
pub const LEAD_T_CYCLES: u64 = crate::gameboy::T_CYCLES_PER_FRAME as u64;

/// What the caller has to do about a packet it just handed over.
///
/// Everything else —version checks, status, timestamps, packets from a newer BGB
/// than this— is dealt with inside and comes back as [`Incoming::Nothing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incoming {
    /// Nothing for the console to do.
    Nothing,
    /// The other end drove the clock. Give the byte to the serial port and send
    /// back whatever it had in its register with [`Session::answer`].
    Clocked { data: u8, control: Control },
    /// The transfer this console started is over: this is what came back.
    Answered(u8),
    /// The person at the other end closed the link, as opposed to a cable that
    /// fell out. Worth telling apart when deciding whether to try again.
    Closed,
}

/// What this end is doing about the greeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Greeting {
    /// The version has gone out and no answer has come.
    Waiting,
    /// A version arrived and it is one we can work with.
    Agreed,
    /// A version arrived and it is not. Nothing more is sent.
    Refused,
}

/// One end of a cable that goes somewhere else.
pub struct Session {
    greeting: Greeting,
    /// Where this console's clock stood when the connection opened.
    ///
    /// Everything on the wire is counted from here and not from the console's
    /// switching on, and that is not tidiness. A timestamp has thirty-one bits
    /// at 2 MiHz, so the whole range is 1024 seconds and *distances only mean
    /// anything up to half of it*. Two consoles that had been running for
    /// different lengths of time —one for a minute, one for an hour, which is
    /// the ordinary case— would be further apart than the arithmetic can read,
    /// and the difference between them would come out with the wrong sign.
    /// Starting both at zero when they meet leaves them within a round trip of
    /// each other, and eight minutes of slack for whatever happens next.
    origin: u64,
    /// How far this console has lived since then.
    ours: Stamp,
    /// The furthest the other end has said it has reached, **read on this
    /// console's clock**. It may run up to here and no further.
    theirs: Stamp,
    /// The distance between the two clocks, worked out the first time the other
    /// end says what time it is.
    ///
    /// Each console counts from its own switching on, so the two timestamps are
    /// not numbers on the same scale: comparing them raw would leave any pair of
    /// unequal age stuck for good, one of them permanently "ahead" of a clock it
    /// has nothing to do with. What has meaning is the difference, and the
    /// difference holds, because both clocks run at the same rate. `None` until
    /// the first timestamp arrives, and nothing runs before then.
    offset: Option<i32>,
    /// Where this end was when it last announced itself.
    announced: Stamp,
    /// Set while a byte this console clocked out is on its way and the answer
    /// has not come back. Nothing may advance meanwhile.
    awaiting_answer: bool,
    /// Whether the other emulator says it is running. A paused one is not
    /// broken, but it will not be announcing itself either.
    peer_running: bool,
    outgoing: VecDeque<Packet>,
}

impl Session {
    /// A session on a connection that has just opened, with the console's clock
    /// wherever it stands.
    ///
    /// The version packet is queued straight away: both ends send theirs without
    /// waiting to be asked, and whichever arrives first is the one that gets
    /// checked.
    pub fn new(started_at: u64) -> Self {
        let mut outgoing = VecDeque::new();
        outgoing.push_back(Packet::version());
        Self {
            greeting: Greeting::Waiting,
            origin: started_at,
            ours: Stamp::from_t_cycles(0),
            theirs: Stamp::from_t_cycles(0),
            offset: None,
            announced: Stamp::from_t_cycles(0),
            awaiting_answer: false,
            peer_running: false,
            outgoing,
        }
    }

    /// Packets to put on the wire, in order. They are gone once taken.
    pub fn take_outgoing(&mut self) -> Vec<Packet> {
        self.outgoing.drain(..).collect()
    }

    /// Whether the greeting went through and bytes may start moving.
    pub fn is_ready(&self) -> bool {
        self.greeting == Greeting::Agreed
    }

    /// Whether the other end announced a version this one cannot work with.
    pub fn is_refused(&self) -> bool {
        self.greeting == Greeting::Refused
    }

    /// Whether the other emulator says it is running rather than paused.
    pub fn peer_running(&self) -> bool {
        self.peer_running
    }

    /// May the console emulate the next instruction?
    ///
    /// This is the whole of the pacing, and it says no for two different
    /// reasons: because a byte of ours is out there unanswered, or because we
    /// have caught up with the last instant the other end vouched for.
    pub fn may_run(&self) -> bool {
        if !self.is_ready() || self.awaiting_answer || self.offset.is_none() {
            return false;
        }
        !self.ours.is_after(self.theirs.plus_t_cycles(LEAD_T_CYCLES))
    }

    /// How many T-cycles the console may still run before it has to wait.
    ///
    /// [`Session::may_run`] answers whether; this answers how far, which is what
    /// lets a caller emulate a stretch at a time instead of asking again after
    /// every instruction. Zero means the two are level — the caller should still
    /// run one instruction, or two consoles that meet exactly would each sit
    /// waiting for the other to move first.
    pub fn allowance_t_cycles(&self) -> u64 {
        if !self.may_run() {
            return 0;
        }
        let limit = self.theirs.plus_t_cycles(LEAD_T_CYCLES);
        limit.since(self.ours).max(0) as u64 * super::bgb::T_CYCLES_PER_UNIT
    }

    /// This console has now lived to `t_cycles`.
    ///
    /// Called as it runs. Every so often it puts a [`Packet::Reached`] on the
    /// wire, which is what keeps the other end moving: without it both would run
    /// to each other's last word and stop there for good.
    pub fn advance_to(&mut self, t_cycles: u64) {
        self.ours = Stamp::from_t_cycles(t_cycles.saturating_sub(self.origin));
        if !self.is_ready() {
            return;
        }
        // `since` is in the protocol's units and the interval is in T-cycles,
        // which is what the conversion is for.
        if self.ours.since(self.announced.plus_t_cycles(ANNOUNCE_EVERY_T_CYCLES)) >= 0 {
            self.announce();
        }
    }

    /// Says where this end is, right now, whatever the interval says.
    ///
    /// Worth doing on purpose after something that took a while —a dialog, a
    /// pause— so the other end is not left waiting on an announcement that the
    /// ordinary running of the emulator would have made.
    pub fn announce(&mut self) {
        self.announced = self.ours;
        self.outgoing.push_back(Packet::Reached(self.ours));
    }

    /// This console drove the clock and has eight bits out.
    ///
    /// From here nothing advances until the answer arrives; see the note at the
    /// top about the one round trip that really is paid.
    pub fn send(&mut self, data: u8, control: Control) {
        self.awaiting_answer = true;
        self.announced = self.ours;
        self.outgoing.push_back(Packet::Master { data, control, at: self.ours });
    }

    /// The answer to having been clocked by the other end.
    ///
    /// `None` is not the same as `Some(0xFF)` and the difference is the one this
    /// project already got wrong once: a console that had armed nothing gives up
    /// no byte at all, and saying so is a packet of its own.
    pub fn answer(&mut self, data: Option<u8>) {
        self.outgoing.push_back(match data {
            Some(data) => Packet::Slave { data },
            None => Packet::NotListening,
        });
    }

    /// Tells the other end this console is closing on purpose.
    pub fn close(&mut self) {
        self.outgoing.push_back(Packet::WantDisconnect);
    }

    /// Takes a packet in and says what the console has to do about it.
    pub fn receive(&mut self, packet: Packet) -> Incoming {
        match packet {
            Packet::Version { .. } => {
                self.greeting = if packet.is_compatible_version() {
                    // The status is the answer to a version, and it is what
                    // finishes the greeting from this side.
                    self.outgoing.push_back(Packet::Status {
                        running: true,
                        paused: false,
                        reconnect: false,
                    });
                    Greeting::Agreed
                } else {
                    Greeting::Refused
                };
                if self.greeting == Greeting::Agreed {
                    // Somebody has to say what time it is first, and neither end
                    // may run before it has heard the other's clock. Both saying
                    // so on greeting is what gets the pair moving; leaving it to
                    // the ordinary announcements would leave two consoles each
                    // waiting for the other to take the first step.
                    self.announce();
                }
                Incoming::Nothing
            }

            Packet::Master { data, control, at } => {
                // The instant the byte was clocked is a point the other end has
                // plainly lived to, so it moves the mark forward like any
                // announcement would.
                self.observe(at);
                Incoming::Clocked { data, control }
            }

            // The two answers to our own byte. Both end the wait; only one of
            // them carries anything.
            Packet::Slave { data } => {
                self.awaiting_answer = false;
                Incoming::Answered(data)
            }
            Packet::NotListening => {
                self.awaiting_answer = false;
                // Nobody was driving the line at the other end, and a line
                // nobody drives rests high.
                Incoming::Answered(0xFF)
            }

            Packet::Reached(at) => {
                self.observe(at);
                Incoming::Nothing
            }

            Packet::Status { running, paused, .. } => {
                self.peer_running = running && !paused;
                Incoming::Nothing
            }

            Packet::WantDisconnect => Incoming::Closed,

            // Read to stay in step with the stream and then dropped: Akebia has
            // one player per console, and a command from a newer BGB than this
            // must not bring a session down.
            Packet::Joypad { .. } | Packet::Unknown { .. } => Incoming::Nothing,
        }
    }

    /// Moves the mark this console is allowed to run to.
    ///
    /// It only ever goes forward. Out-of-order delivery is not a thing TCP does,
    /// but a relay in the middle might, and letting the mark go backwards would
    /// stall a console that had already been given leave to pass that point.
    fn observe(&mut self, at: Stamp) {
        // The first word sets the two clocks against each other. It arrives a
        // transport's delay late, so the other end is taken to be that much
        // behind — which is true, as far as this console can ever know.
        let offset = *self.offset.get_or_insert_with(|| at.since(self.ours));
        let here = at.shifted(-offset);
        if here.is_after(self.theirs) {
            self.theirs = here;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with the greeting done and the two clocks set against each
    /// other, both at zero. From there the numbers in a test mean the same on
    /// either end.
    fn greeted() -> Session {
        let mut session = Session::new(0);
        assert_eq!(session.take_outgoing(), vec![Packet::version()]);
        assert_eq!(session.receive(Packet::version()), Incoming::Nothing);
        assert!(session.is_ready());
        session.receive(Packet::Reached(Stamp::from_t_cycles(0)));
        session.take_outgoing();
        session
    }

    #[test]
    fn the_version_goes_out_before_anything_is_asked_of_it() {
        let mut session = Session::new(0);
        assert_eq!(session.take_outgoing(), vec![Packet::version()]);
        assert!(!session.is_ready(), "nothing has answered yet");
        assert!(!session.may_run(), "and nothing runs until it does");
    }

    /// Both come out of agreeing: the status because the protocol asks for it,
    /// and the timestamp because neither end may run before it has heard the
    /// other's clock, so somebody has to speak first.
    #[test]
    fn agreeing_answers_with_a_status_and_says_what_time_it_is() {
        let mut session = Session::new(0);
        session.take_outgoing();
        session.receive(Packet::version());

        assert_eq!(
            session.take_outgoing(),
            vec![
                Packet::Status { running: true, paused: false, reconnect: false },
                Packet::Reached(Stamp::from_t_cycles(0)),
            ]
        );
    }

    /// Nothing runs on a clock it has not heard. Two ends each waiting for the
    /// other's first word would be a link that greets and then never moves.
    #[test]
    fn it_does_not_run_before_it_has_heard_the_other_clock() {
        let mut session = Session::new(0);
        session.take_outgoing();
        session.receive(Packet::version());
        assert!(session.is_ready());
        assert!(!session.may_run(), "agreed, but with no idea what time it is there");

        session.receive(Packet::Reached(Stamp::from_t_cycles(0)));
        assert!(session.may_run());
    }

    /// The ordinary case, and the one a plain comparison gets wrong: one console
    /// has been played for an hour and the other has just been switched on.
    /// Thirty-one bits at 2 MiHz only tell distances apart up to eight minutes,
    /// so the two ages must not be what is compared.
    #[test]
    fn two_consoles_of_very_different_ages_still_link() {
        const HOUR: u64 = 4_194_304 * 3_600;
        let mut session = Session::new(HOUR);
        session.take_outgoing();
        session.receive(Packet::version());
        session.advance_to(HOUR);

        // The other end has just been switched on: its own clock is near zero.
        session.receive(Packet::Reached(Stamp::from_t_cycles(0)));
        assert!(session.may_run(), "an hour of age is no reason to stall");

        session.advance_to(HOUR + 10_000 + LEAD_T_CYCLES);
        assert!(!session.may_run(), "and it still stops where the other end stopped");

        session.receive(Packet::Reached(Stamp::from_t_cycles(30_000)));
        assert!(session.may_run());
    }

    #[test]
    fn a_version_from_another_major_stops_the_session() {
        let mut session = Session::new(0);
        session.take_outgoing();
        session.receive(Packet::Version { major: 2, minor: 0 });

        assert!(session.is_refused());
        assert!(!session.is_ready());
        assert!(!session.may_run());
        assert!(session.take_outgoing().is_empty(), "nothing more is said to it");
    }

    /// The rule, in one test: a console runs to the last instant vouched for,
    /// plus the head start that keeps the two of them turning over, and stops.
    #[test]
    fn it_runs_a_lead_past_what_the_other_end_vouched_for_and_no_further() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(10_000)));

        session.advance_to(10_000 + LEAD_T_CYCLES - 2);
        assert!(session.may_run());

        session.advance_to(10_000 + LEAD_T_CYCLES);
        assert!(session.may_run(), "level with the limit is still allowed");

        session.advance_to(10_000 + LEAD_T_CYCLES + 2);
        assert!(!session.may_run(), "past it is not");
    }

    /// The reason there is a lead at all. With none, neither console could run
    /// the frame whose running is what would let the other one move.
    #[test]
    fn two_consoles_that_meet_exactly_do_not_both_sit_there() {
        let session = greeted();
        assert!(session.may_run(), "level at the start, and it still has a frame to run");
        assert!(session.allowance_t_cycles() >= LEAD_T_CYCLES);
    }

    #[test]
    fn how_far_it_may_run_is_the_distance_to_that_word() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(10_000)));

        session.advance_to(4_000);
        assert_eq!(session.allowance_t_cycles(), 6_000 + LEAD_T_CYCLES);

        session.advance_to(10_000 + LEAD_T_CYCLES);
        assert_eq!(session.allowance_t_cycles(), 0, "level: one instruction and no more");

        session.advance_to(10_000 + LEAD_T_CYCLES + 2_000);
        assert_eq!(session.allowance_t_cycles(), 0, "and past it, nothing at all");
    }

    #[test]
    fn a_further_word_lets_it_go_further() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(10_000)));
        session.advance_to(20_000 + LEAD_T_CYCLES);
        assert!(!session.may_run());

        session.receive(Packet::Reached(Stamp::from_t_cycles(30_000)));
        assert!(session.may_run());
    }

    /// A relay could reorder, and a mark that went backwards would stop a
    /// console that had already been let past that point.
    #[test]
    fn the_mark_never_goes_backwards() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(30_000)));
        session.receive(Packet::Reached(Stamp::from_t_cycles(10_000)));

        session.advance_to(29_000 + LEAD_T_CYCLES);
        assert!(session.may_run(), "the older word must not take the leave away");
    }

    #[test]
    fn a_byte_from_the_other_end_moves_the_mark_like_an_announcement_does() {
        let mut session = greeted();
        session.advance_to(50_000 + LEAD_T_CYCLES);
        assert!(!session.may_run());

        let control = Control::new(false, false);
        let incoming = session.receive(Packet::Master { data: 0x60, control, at: Stamp::from_t_cycles(60_000) });

        assert_eq!(incoming, Incoming::Clocked { data: 0x60, control });
        assert!(session.may_run(), "it plainly lived to the moment it clocked us");
    }

    #[test]
    fn nothing_moves_while_our_own_byte_is_unanswered() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(1_000_000)));
        session.advance_to(1_000);
        assert!(session.may_run());

        session.send(0x01, Control::new(false, false));
        assert!(!session.may_run(), "there is a byte out there");

        assert_eq!(session.receive(Packet::Slave { data: 0x02 }), Incoming::Answered(0x02));
        assert!(session.may_run());
    }

    /// The distinction the serial port had wrong: an end that armed nothing did
    /// not answer `0xFF`, it did not answer.
    #[test]
    fn an_end_that_was_not_listening_still_ends_the_wait() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(1_000_000)));
        session.send(0x01, Control::new(false, false));

        assert_eq!(session.receive(Packet::NotListening), Incoming::Answered(0xFF));
        assert!(session.may_run());
    }

    #[test]
    fn the_two_answers_we_give_are_different_packets() {
        let mut session = greeted();
        session.take_outgoing();

        session.answer(Some(0x02));
        session.answer(None);
        assert_eq!(session.take_outgoing(), vec![Packet::Slave { data: 0x02 }, Packet::NotListening]);
    }

    /// Without this both ends run to each other's last word and stop there for
    /// good, each waiting for the other to say something.
    #[test]
    fn running_on_makes_it_announce_itself() {
        let mut session = greeted();
        session.take_outgoing();

        session.advance_to(1_000);
        assert!(session.take_outgoing().is_empty(), "not for a thousand cycles it does not");

        session.advance_to(ANNOUNCE_EVERY_T_CYCLES);
        assert_eq!(
            session.take_outgoing(),
            vec![Packet::Reached(Stamp::from_t_cycles(ANNOUNCE_EVERY_T_CYCLES))]
        );
    }

    #[test]
    fn a_byte_counts_as_having_announced_itself() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(10_000_000)));
        session.advance_to(50_000);
        session.take_outgoing();

        session.send(0x60, Control::new(false, false));
        session.receive(Packet::Slave { data: 0x60 });
        session.take_outgoing();

        // The byte said where we were, so the interval starts again from there
        // instead of firing an announcement that would repeat it.
        session.advance_to(60_000);
        assert!(session.take_outgoing().is_empty());
    }

    #[test]
    fn nothing_is_announced_before_the_greeting_is_through() {
        let mut session = Session::new(0);
        session.take_outgoing();
        session.advance_to(ANNOUNCE_EVERY_T_CYCLES * 4);
        assert!(session.take_outgoing().is_empty());
    }

    #[test]
    fn a_paused_emulator_at_the_other_end_says_so() {
        let mut session = greeted();
        assert!(!session.peer_running(), "nothing has been said about it yet");

        session.receive(Packet::Status { running: true, paused: false, reconnect: false });
        assert!(session.peer_running());

        session.receive(Packet::Status { running: true, paused: true, reconnect: false });
        assert!(!session.peer_running());
    }

    #[test]
    fn a_deliberate_disconnect_is_told_apart_from_a_cable_falling_out() {
        let mut session = greeted();
        assert_eq!(session.receive(Packet::WantDisconnect), Incoming::Closed);

        session.take_outgoing();
        session.close();
        assert_eq!(session.take_outgoing(), vec![Packet::WantDisconnect]);
    }

    #[test]
    fn a_command_from_a_newer_bgb_changes_nothing() {
        let mut session = greeted();
        session.receive(Packet::Reached(Stamp::from_t_cycles(10_000)));
        session.take_outgoing();

        assert_eq!(session.receive(Packet::Unknown { command: 250 }), Incoming::Nothing);
        assert_eq!(session.receive(Packet::Joypad { button: 3, pressed: true }), Incoming::Nothing);
        assert!(session.take_outgoing().is_empty());
        assert!(session.may_run(), "and it does not take the leave away either");
    }
}
