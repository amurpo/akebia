//! The wire format two emulators use to be one cable: **BGB's link protocol**.
//!
//! # Credit
//!
//! The protocol is not ours. It was designed and documented by **beware**, the
//! author of [BGB](https://bgb.bircd.org/), and the specification followed here
//! is <https://bgb.bircd.org/bgblink.html>. Nothing of BGB's code is used; what
//! is used is the shape of its packets, and that is deliberate: a protocol only
//! one program speaks is not a protocol. Speaking BGB's means Akebia can link
//! against BGB itself and against every emulator that already talks to it, and
//! —while this is being written— that it can be tested against an implementation
//! already known to be right instead of only against itself.
//!
//! # Why a protocol is needed at all
//!
//! Because of one number. A byte on the cable takes 4096 T-cycles, near enough a
//! millisecond, and nothing answers that fast: a local network takes one to five,
//! Bluetooth twenty to fifty, the internet more. Sending a byte and waiting for
//! the reply would run the emulation at the speed of the link.
//!
//! What makes it work is that **each side says what time it is**. Every packet
//! carries a timestamp, and there is a packet whose only content is one
//! ([`Packet::Reached`]). Knowing the other end has already lived to a given
//! instant, this one may run freely up to that instant without asking anybody:
//! whatever arrives later is dated *after* the point already reached, so it
//! cannot contradict what has been emulated. The wait is not per byte, it is per
//! how far ahead one end gets.
//!
//! # This module has no sockets
//!
//! It turns packets into bytes and bytes into packets, and it counts time. Who
//! carries them —TCP, Bluetooth, a relay somewhere— belongs to the frontend,
//! like every other piece of I/O. That is also what makes the format testable
//! without a network and without a second machine.

/// Bytes in a packet. Every command is this long, including the ones that carry
/// nothing.
pub const PACKET_LEN: usize = 8;

/// The port BGB listens on by default.
///
/// It is not in the specification —it is what the program does— but an emulator
/// that picks another number by default is one that needs explaining to every
/// person who tries to connect.
pub const DEFAULT_PORT: u16 = 8765;

/// T-cycles in one unit of the protocol's clock.
///
/// The timestamps count at 2 MiHz, 2^21 per second, and the console's clock runs
/// at 2^22. So a unit is two T-cycles exactly, with no rounding to argue about.
const T_CYCLES_PER_UNIT: u64 = 2;

/// Bits a timestamp really has. The highest one is always zero.
const STAMP_BITS: u32 = 31;
const STAMP_MASK: u32 = (1 << STAMP_BITS) - 1;
/// Half the range. A distance beyond this is read as the other side of the wrap.
const STAMP_HALF: u32 = 1 << (STAMP_BITS - 1);

/// The version both ends announce. See [`Packet::Version`].
const VERSION: (u8, u8) = (1, 4);

/// An instant on the clock the two ends share.
///
/// # Why it is a type and not a `u32`
///
/// Because it **wraps**, and the wrap is not far off: thirty-one bits at 2 MiHz
/// come round every 1024 seconds, so a session of any length crosses it. Asking
/// whether one timestamp is later than another with `<` is right for seventeen
/// minutes and then silently wrong, and what it would be wrong about is whether
/// a console may keep running. The comparison here is the one TCP uses for its
/// sequence numbers: the *difference* is what has meaning, read as a signed
/// number, and it is correct as long as the two ends are not half a wrap apart —
/// which would be eight minutes of one waiting for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp(u32);

impl Stamp {
    /// The instant a console that has lived `t_cycles` is at.
    pub const fn from_t_cycles(t_cycles: u64) -> Self {
        Self(((t_cycles / T_CYCLES_PER_UNIT) as u32) & STAMP_MASK)
    }

    /// As it travels: thirty-one bits, the highest one clear.
    pub const fn wire(self) -> u32 {
        self.0
    }

    /// As it arrives. The unused bit is dropped rather than trusted.
    pub const fn from_wire(raw: u32) -> Self {
        Self(raw & STAMP_MASK)
    }

    /// How far this instant is past `other`, negative if it is short of it.
    ///
    /// This is the whole point of the type; see the note above about the wrap.
    pub const fn since(self, other: Self) -> i32 {
        let forward = self.0.wrapping_sub(other.0) & STAMP_MASK;
        if forward >= STAMP_HALF {
            // Past the halfway mark the short way round is backwards.
            (forward as i64 - (1i64 << STAMP_BITS)) as i32
        } else {
            forward as i32
        }
    }

    /// Whether this instant is later than `other`.
    pub const fn is_after(self, other: Self) -> bool {
        self.since(other) > 0
    }

    /// This instant moved on by `t_cycles` of console time.
    pub const fn plus_t_cycles(self, t_cycles: u64) -> Self {
        Self(self.0.wrapping_add((t_cycles / T_CYCLES_PER_UNIT) as u32) & STAMP_MASK)
    }
}

/// What a console reports about the clock it is driving, packed the way `sync1`
/// wants it.
///
/// It is `SC` with one bit added: the protocol has to distinguish the CGB's fast
/// shift clock from the CGB running at double speed, because they are different
/// things that both make a byte take less time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control(u8);

impl Control {
    /// Bit 0. Always set in a `sync1`: only the end driving the clock sends one.
    const INTERNAL: u8 = 0x01;
    /// Bit 1: the CGB's 262144 Hz shift clock, bit 1 of `SC`.
    const FAST: u8 = 0x02;
    /// Bit 2: the console is in double speed mode.
    const DOUBLE_SPEED: u8 = 0x04;
    /// Bit 7. Set for the same reason the start bit of `SC` is.
    const START: u8 = 0x80;

    pub const fn new(fast_clock: bool, double_speed: bool) -> Self {
        let mut bits = Self::INTERNAL | Self::START;
        if fast_clock {
            bits |= Self::FAST;
        }
        if double_speed {
            bits |= Self::DOUBLE_SPEED;
        }
        Self(bits)
    }

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn fast_clock(self) -> bool {
        self.0 & Self::FAST != 0
    }

    pub const fn double_speed(self) -> bool {
        self.0 & Self::DOUBLE_SPEED != 0
    }
}

/// One packet, already understood.
///
/// The command numbers are BGB's and are given on each variant, because that is
/// what a reader with the specification open needs to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet {
    /// `1`. Sent by both ends the moment the connection opens, and checked.
    Version { major: u8, minor: u8 },
    /// `101`. Somebody pressed a button on the other end.
    ///
    /// BGB uses it to let two people play one console. Akebia does not, but it
    /// arrives whether or not it is wanted and has to be read to stay in step
    /// with the stream.
    Joypad { button: u8, pressed: bool },
    /// `104` — `sync1`. The end driving the clock has clocked out a byte.
    Master { data: u8, control: Control, at: Stamp },
    /// `105` — `sync2`. The answer of an end that **was** armed and listening.
    Slave { data: u8 },
    /// `106` with `b2 = 1` — `sync3`. Clocked while nothing was armed.
    ///
    /// It is a different answer from [`Packet::Slave`] and not a detail: a
    /// console that has not armed a transfer gives up nothing, and the one
    /// driving the clock reads the empty line. Answering with a stale byte
    /// instead is what makes a game read back what it just sent and believe it
    /// is connected.
    NotListening,
    /// `106` with `b2 = 0` — `sync3`. "I have lived this far, with nothing to
    /// send." The packet that lets the other end run without asking.
    Reached(Stamp),
    /// `108`. What the other emulator is doing.
    Status { running: bool, paused: bool, reconnect: bool },
    /// `109`. The person at the other end closed the link on purpose, as opposed
    /// to a cable that fell out.
    WantDisconnect,
    /// A command this does not know.
    ///
    /// Kept and passed on rather than refused: the protocol's compatibility rule
    /// is that an end which does not understand a packet ignores it, and a newer
    /// BGB adding a command must not bring a session down.
    Unknown { command: u8 },
}

impl Packet {
    /// The version both ends announce, ready to send.
    pub const fn version() -> Self {
        Self::Version { major: VERSION.0, minor: VERSION.1 }
    }

    /// Whether this is a version packet this end can work with.
    ///
    /// Only the major number is looked at. That is what "compatible with older
    /// versions" has to mean in practice: a minor that is not ours is a BGB of
    /// another age, and refusing it would break the very compatibility the
    /// number exists to keep.
    pub const fn is_compatible_version(&self) -> bool {
        matches!(self, Self::Version { major, .. } if *major == VERSION.0)
    }

    /// The eight bytes as they travel.
    pub const fn encode(&self) -> [u8; PACKET_LEN] {
        let (command, b2, b3, stamp) = match *self {
            Self::Version { major, minor } => (1, major, minor, 0),
            Self::Joypad { button, pressed } => {
                (101, (button & 0x07) | if pressed { 0x08 } else { 0 }, 0, 0)
            }
            Self::Master { data, control, at } => (104, data, control.bits(), at.wire()),
            // The control value of a `sync2` is the bare start bit: the end that
            // answers is not driving anything.
            Self::Slave { data } => (105, data, Control::START, 0),
            Self::NotListening => (106, 1, 0, 0),
            Self::Reached(at) => (106, 0, 0, at.wire()),
            Self::Status { running, paused, reconnect } => {
                let flags = (running as u8) | ((paused as u8) << 1) | ((reconnect as u8) << 2);
                (108, flags, 0, 0)
            }
            Self::WantDisconnect => (109, 0, 0, 0),
            // There is nothing sensible to put in the payload of a command we
            // did not understand, and echoing one back is not something this
            // ever does. It exists so that the round trip is total.
            Self::Unknown { command } => (command, 0, 0, 0),
        };
        let [t1, t2, t3, t4] = stamp.to_le_bytes();
        [command, b2, b3, 0, t1, t2, t3, t4]
    }

    /// What arrived, if it is anything this end knows.
    ///
    /// It does not fail. Every eight bytes are *some* packet, even if it is
    /// [`Packet::Unknown`], and a stream that cannot be misread is one less thing
    /// to get wrong while a link is being brought up.
    pub const fn decode(raw: [u8; PACKET_LEN]) -> Self {
        let [command, b2, b3, _b4, t1, t2, t3, t4] = raw;
        let stamp = Stamp::from_wire(u32::from_le_bytes([t1, t2, t3, t4]));

        match command {
            1 => Self::Version { major: b2, minor: b3 },
            101 => Self::Joypad { button: b2 & 0x07, pressed: b2 & 0x08 != 0 },
            104 => Self::Master { data: b2, control: Control::from_bits(b3), at: stamp },
            105 => Self::Slave { data: b2 },
            // Anything other than zero means "nothing was armed here": the
            // specification gives 1, and reading only 1 would turn a value from
            // some other implementation into a byte that never came.
            106 if b2 != 0 => Self::NotListening,
            106 => Self::Reached(stamp),
            108 => Self::Status {
                running: b2 & 0x01 != 0,
                paused: b2 & 0x02 != 0,
                reconnect: b2 & 0x04 != 0,
            },
            109 => Self::WantDisconnect,
            command => Self::Unknown { command },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anything sent has to come back the same, or a session is at the mercy of
    /// which packets happen to be exercised.
    #[test]
    fn every_packet_survives_the_round_trip() {
        let stamp = Stamp::from_t_cycles(1_234_568);
        let packets = [
            Packet::version(),
            Packet::Joypad { button: 5, pressed: true },
            Packet::Joypad { button: 0, pressed: false },
            Packet::Master { data: 0x60, control: Control::new(false, false), at: stamp },
            Packet::Master { data: 0xD4, control: Control::new(true, true), at: stamp },
            Packet::Slave { data: 0x02 },
            Packet::NotListening,
            Packet::Reached(stamp),
            Packet::Status { running: true, paused: false, reconnect: true },
            Packet::WantDisconnect,
            Packet::Unknown { command: 200 },
        ];
        for packet in packets {
            assert_eq!(Packet::decode(packet.encode()), packet, "{packet:?}");
        }
    }

    #[test]
    fn a_packet_is_eight_bytes_and_the_stamp_is_little_endian() {
        let packet = Packet::Master {
            data: 0x60,
            control: Control::new(false, false),
            at: Stamp::from_wire(0x0201_0403),
        };
        assert_eq!(packet.encode(), [104, 0x60, 0x81, 0, 0x03, 0x04, 0x01, 0x02]);
    }

    /// The two answers to being clocked are different packets, and confusing
    /// them is the whole bug this project already had once: a console that was
    /// not listening must not seem to have answered.
    #[test]
    fn being_clocked_while_idle_is_not_the_same_as_answering() {
        assert_ne!(Packet::NotListening, Packet::Slave { data: 0xFF });
        assert_eq!(Packet::decode([106, 1, 0, 0, 0, 0, 0, 0]), Packet::NotListening);
        assert_eq!(Packet::decode([106, 0, 0, 0, 0, 0, 0, 0]), Packet::Reached(Stamp::from_wire(0)));
    }

    #[test]
    fn a_command_from_a_newer_bgb_is_kept_and_not_refused() {
        assert_eq!(Packet::decode([250, 9, 9, 9, 1, 0, 0, 0]), Packet::Unknown { command: 250 });
    }

    #[test]
    fn a_version_of_another_major_is_not_worked_with() {
        assert!(Packet::version().is_compatible_version());
        assert!(Packet::Version { major: 1, minor: 9 }.is_compatible_version());
        assert!(!Packet::Version { major: 2, minor: 4 }.is_compatible_version());
    }

    #[test]
    fn the_stamp_counts_one_unit_every_two_t_cycles() {
        assert_eq!(Stamp::from_t_cycles(0).wire(), 0);
        assert_eq!(Stamp::from_t_cycles(2).wire(), 1);
        // A whole frame, which is what the pacing is really measured in.
        assert_eq!(Stamp::from_t_cycles(70_224).wire(), 35_112);
    }

    #[test]
    fn the_highest_bit_never_travels() {
        let stamp = Stamp::from_wire(u32::MAX);
        assert_eq!(stamp.wire(), STAMP_MASK);
        assert_eq!(stamp.wire() >> 31, 0);
    }

    #[test]
    fn a_later_instant_reads_as_later() {
        let early = Stamp::from_wire(1000);
        let late = Stamp::from_wire(1500);
        assert_eq!(late.since(early), 500);
        assert_eq!(early.since(late), -500);
        assert!(late.is_after(early));
        assert!(!early.is_after(late));
        assert!(!early.is_after(early));
    }

    /// Seventeen minutes into a session the counter comes round, and a plain
    /// comparison would say the console that just moved on is now behind — which
    /// is the one question the answer to which decides whether it runs.
    #[test]
    fn it_is_still_later_across_the_wrap() {
        let before = Stamp::from_wire(STAMP_MASK - 10);
        let after = before.plus_t_cycles(100); // 50 units, past the end
        assert_eq!(after.wire(), 39, "it came round");
        assert!(after.is_after(before), "and it is still the later of the two");
        assert_eq!(after.since(before), 50);
        assert_eq!(before.since(after), -50);
    }

    #[test]
    fn the_control_byte_says_which_clock_the_master_is_driving() {
        let plain = Control::new(false, false);
        assert_eq!(plain.bits(), 0x81, "start bit and internal clock");
        assert!(!plain.fast_clock() && !plain.double_speed());

        let quick = Control::new(true, true);
        assert_eq!(quick.bits(), 0x87);
        assert!(quick.fast_clock() && quick.double_speed());
    }
}
