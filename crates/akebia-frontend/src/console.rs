//! Which machine a session is running.
//!
//! # An enum and not a trait object
//!
//! There are two consoles and there will not be a third by accident. An enum
//! says that: every place that has to care is a `match` the compiler checks,
//! and adding a machine is a compiler error at each of them rather than a
//! silent fallthrough. A `dyn` trait would buy extensibility nobody wants and
//! pay for it by making every difference between the two machines invisible at
//! the call site.
//!
//! And the differences are real. One console is 160×144 and the other 240×160.
//! One has eight buttons and the other ten. One has a link port and a mapper;
//! the other has neither, and keeps its saved game in one of three chips that
//! are nothing like a mapper's battery-backed RAM. Hiding those behind a
//! uniform interface would mean inventing answers — a link cable for a machine
//! with no link port — where saying "this machine does not do that" is both
//! true and shorter.
//!
//! # The buttons are the frontend's, not either core's
//!
//! Neither core's button type covers both machines: the Game Boy has no
//! shoulders, and a shared type would have to carry two the older machine
//! cannot press. So the frontend has its own, and each console takes what it
//! recognises. A Game Boy handed `L` does nothing, which is exactly what a Game
//! Boy does when you press a button it has not got.

use akebia_core::GameBoy;
use akebia_gba::Gba;

/// Every button either machine has.
///
/// The four directions and the four face buttons are common; the shoulders are
/// the Advance's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    Up,
    Down,
    Left,
    Right,
    A,
    B,
    Start,
    Select,
    L,
    R,
}

impl Pad {
    /// Every button, in the order everything that keeps one flag per button
    /// counts in.
    ///
    /// The order is the declaration's, and [`Pad::index`] relies on that. The
    /// test below pins the two together: a button inserted in the middle of the
    /// enum without being inserted here as well would silently give the
    /// controller somebody else's d-pad.
    pub const ALL: [Pad; 10] = [
        Pad::Up,
        Pad::Down,
        Pad::Left,
        Pad::Right,
        Pad::A,
        Pad::B,
        Pad::Start,
        Pad::Select,
        Pad::L,
        Pad::R,
    ];

    /// Where this button sits in [`Pad::ALL`], for whoever keeps an array of
    /// them.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// What it is called on the machine, which is what a person remapping it
    /// reads.
    pub const fn name(self) -> &'static str {
        match self {
            Pad::Up => "Up",
            Pad::Down => "Down",
            Pad::Left => "Left",
            Pad::Right => "Right",
            Pad::A => "A",
            Pad::B => "B",
            Pad::Start => "Start",
            Pad::Select => "Select",
            Pad::L => "L",
            Pad::R => "R",
        }
    }

    /// What this means to a Game Boy, if anything.
    fn on_gameboy(self) -> Option<akebia_core::Button> {
        use akebia_core::Button as Gb;
        Some(match self {
            Pad::Up => Gb::Up,
            Pad::Down => Gb::Down,
            Pad::Left => Gb::Left,
            Pad::Right => Gb::Right,
            Pad::A => Gb::A,
            Pad::B => Gb::B,
            Pad::Start => Gb::Start,
            Pad::Select => Gb::Select,
            // The older machine has no shoulders. Pressing one is not an error,
            // it is simply nothing — as it would be on the hardware.
            Pad::L | Pad::R => return None,
        })
    }

    fn on_advance(self) -> akebia_gba::Button {
        use akebia_gba::Button as Gba;
        match self {
            Pad::Up => Gba::Up,
            Pad::Down => Gba::Down,
            Pad::Left => Gba::Left,
            Pad::Right => Gba::Right,
            Pad::A => Gba::A,
            Pad::B => Gba::B,
            Pad::Start => Gba::Start,
            Pad::Select => Gba::Select,
            Pad::L => Gba::L,
            Pad::R => Gba::R,
        }
    }
}

/// One of the two machines, with a game in it.
///
/// Both are boxed, and neither for the reason one would guess. An enum is as
/// large as its largest variant, and a Game Boy is 800 bytes of registers and
/// mapper state sitting inline where the Advance is already a handful of
/// pointers to memory it allocates. Unboxed, every `Console` in existence would
/// be the size of the older machine — so they are boxed together, and the enum
/// is a tag and a pointer.
pub enum Console {
    Gb(Box<GameBoy>),
    Gba(Box<Gba>),
}

impl Console {
    /// How big this machine's screen is, in pixels.
    pub fn screen_size(&self) -> (usize, usize) {
        match self {
            Self::Gb(_) => (akebia_core::SCREEN_WIDTH, akebia_core::SCREEN_HEIGHT),
            Self::Gba(gba) => gba.screen_size(),
        }
    }

    /// How many frames a second this machine draws.
    ///
    /// They are not the same and the difference is audible before it is
    /// visible: running an Advance at the Game Boy's rate would play every tune
    /// slightly flat.
    pub fn frames_per_second(&self) -> f64 {
        match self {
            Self::Gb(_) => akebia_core::gameboy::FRAMES_PER_SECOND,
            // 16.78 MHz over 280,896 cycles a frame.
            Self::Gba(_) => 59.7275,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Gb(gb) => gb.header().title.clone(),
            Self::Gba(gba) => gba.title(),
        }
    }

    pub fn press(&mut self, button: Pad, down: bool) {
        match self {
            Self::Gb(gb) => {
                if let Some(button) = button.on_gameboy() {
                    gb.set_button(button, down);
                }
            }
            Self::Gba(gba) => gba.set_button(button.on_advance(), down),
        }
    }

    /// The samples this machine has made since the last call, in the shape the
    /// frontend speaks.
    ///
    /// Both machines make audio and neither makes it in the other's type: they
    /// are separate cores and the Advance's has no business importing the older
    /// one's. They agree on what a stereo sample *is*, though — two numbers
    /// between -1 and 1 — so the translation is here, in the one place that has
    /// to know about both.
    pub fn take_audio(&mut self) -> Vec<akebia_core::StereoSample> {
        match self {
            Self::Gb(gb) => gb.take_audio(),
            Self::Gba(gba) => gba
                .take_audio()
                .into_iter()
                .map(|s| akebia_core::StereoSample { left: s.left, right: s.right })
                .collect(),
        }
    }

    /// Throws them away instead. Whoever runs frames must do one or the other:
    /// samples are made whether or not anybody is listening, and uncollected
    /// they would pile up for as long as the game ran.
    pub fn discard_audio(&mut self) {
        match self {
            Self::Gb(gb) => gb.discard_audio(),
            Self::Gba(gba) => gba.discard_audio(),
        }
    }

    /// Tells the machine what rate the sound card wants.
    pub fn set_sample_rate(&mut self, rate: u32) {
        match self {
            Self::Gb(gb) => gb.set_sample_rate(rate),
            Self::Gba(gba) => gba.set_sample_rate(rate),
        }
    }

    /// Whether this machine is running without something it needs to run at
    /// all.
    ///
    /// Only the Advance can be, and only for one reason: no BIOS. It is asked
    /// after the machine is built rather than reported by whatever built it,
    /// because the machine is the thing that knows — and because a session
    /// created any other way (a copy, a test) is answered just as truthfully.
    pub fn missing_bios(&self) -> bool {
        matches!(self, Self::Gba(gba) if !gba.has_bios())
    }

    /// Whether this machine has a link port at all.
    ///
    /// The Advance has one and nothing here drives it, so the answer is no for
    /// both reasons at once — and the menu greys the cable out rather than
    /// offering something that would not work.
    pub const fn links(&self) -> bool {
        matches!(self, Self::Gb(_))
    }

    /// The Game Boy inside, for everything that is the older machine's alone:
    /// the link cable, the serial port, the debug captures.
    pub fn gameboy(&self) -> Option<&GameBoy> {
        match self {
            Self::Gb(gb) => Some(gb),
            Self::Gba(_) => None,
        }
    }

    pub fn gameboy_mut(&mut self) -> Option<&mut GameBoy> {
        match self {
            Self::Gb(gb) => Some(gb),
            Self::Gba(_) => None,
        }
    }
}

/// Whether a path names an Advance cartridge.
///
/// By extension, which is what a file manager and a person both go by. The
/// header has a logo that could be checked instead, but a mis-named file is a
/// mistake worth reporting rather than quietly working around.
pub fn is_advance(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gba"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// [`Pad::index`] is the enum's own discriminant, so the list and the
    /// declaration have to say the same thing. They are two places and nothing
    /// but this holds them together: an array of ten flags indexed by a button
    /// that moved would answer for the button beside it.
    #[test]
    fn a_button_knows_where_it_sits_in_the_list() {
        for (i, pad) in Pad::ALL.into_iter().enumerate() {
            assert_eq!(pad.index(), i, "{pad:?} is not where the list has it");
        }
    }

    #[test]
    fn the_shoulders_are_the_advances_alone() {
        assert_eq!(Pad::L.on_gameboy(), None);
        assert_eq!(Pad::R.on_gameboy(), None);
        assert_eq!(Pad::A.on_gameboy(), Some(akebia_core::Button::A));
    }

    /// Every button means something on the Advance, which is the machine the
    /// list was drawn from.
    #[test]
    fn every_button_reaches_the_advance() {
        let mut seen = Vec::new();
        for pad in Pad::ALL {
            let button = pad.on_advance();
            assert!(!seen.contains(&button), "{pad:?} collides with something");
            seen.push(button);
        }
        assert_eq!(seen.len(), akebia_gba::keypad::Button::ALL.len());
    }

    #[test]
    fn a_cartridge_is_told_by_its_extension() {
        assert!(is_advance(Path::new("game.gba")));
        assert!(is_advance(Path::new("game.GBA")), "however it is spelled");
        assert!(!is_advance(Path::new("game.gb")));
        assert!(!is_advance(Path::new("game.gbc")));
        assert!(!is_advance(Path::new("game")));
    }

    /// The two screens are different sizes, which is the difference that
    /// reaches furthest into the interface.
    #[test]
    fn the_two_machines_have_different_screens() {
        let gba = Console::Gba(Box::new(Gba::new()));
        assert_eq!(gba.screen_size(), (240, 160));
        assert!(!gba.links(), "and the Advance's cable is not driven");
    }

    /// A machine handed no BIOS says so, and one that was handed one stops
    /// saying it. It is only ever asked of the Advance: the older machine's
    /// boot ROM is not needed to run a cartridge.
    #[test]
    fn an_advance_says_whether_it_has_its_bios() {
        let mut gba = Gba::new();
        assert!(Console::Gba(Box::new(Gba::new())).missing_bios());

        gba.load_bios(&[0u8; 16 * 1024]);
        assert!(!Console::Gba(Box::new(gba)).missing_bios(), "and now it has one");
    }
}
