//! The controller: what it is doing, and what its buttons are taken to mean.
//!
//! # Why the state is read and not accumulated
//!
//! `gilrs` delivers presses and releases as events, and the obvious thing to do
//! with them is to flip a flag on each. Obvious and wrong: an event that never
//! arrives —a controller unplugged mid-press, a queue drained while the machine
//! was somewhere else— leaves a flag up for ever, and the game walks off on its
//! own with a direction nobody is holding. The events are drained because the
//! library needs them drained to keep its own picture of the controller
//! current, and are then thrown away: what reaches the console is read afresh
//! from the controller every frame. A button nobody is touching cannot be
//! stuck.
//!
//! The exception is [`Gamepads::take_caught`], which is an event and has to be:
//! the remapping dialog is asking "which button was just pressed", and a
//! reading of what is held now cannot tell the button that went down this
//! instant from the one that has been down since the dialog opened.
//!
//! # Why the bindings are by name and not by number
//!
//! `gilrs` carries SDL's controller database, so a pad anybody owns arrives
//! with its buttons already named: the thing under the right thumb is `East`,
//! whatever the driver calls input 305. Storing `East` means the file written
//! on one machine still means something on another, and —more to the point—
//! that a controller which is not in the database still lands on the kernel's
//! own `BTN_EAST` and reads the same.
//!
//! The names are positions and not letters on purpose. What is printed on the
//! button under the thumb is `A` on one controller, `B` on another and `✕` on a
//! third; `South` is true of all three.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gilrs::{Axis, Button as Physical, EventType, Gilrs};

use crate::console::Pad;

/// How far the stick has to lean before it counts as a direction held, and how
/// far back before it lets go.
///
/// Two thresholds and not one because at a single one the reading sits astride
/// it: a thumb resting near the edge crosses it and back several times a
/// second, and a game reads that as somebody tapping the d-pad. Once a
/// direction is on, rather less keeps it on.
const LEAN: f32 = 0.55;
const RELEASE: f32 = 0.35;

/// What each console button is on a controller nobody has configured.
///
/// The d-pad on the d-pad and the shoulders on the shoulders need no defending.
/// A and B are `East` and `South` —a diagonal— rather than the two along the
/// bottom, for two reasons that agree: on a controller with Nintendo's labels
/// those are exactly the two that say A and B, and on any other they are the
/// pair the thumb rolls between from right to left, which is how the two sit on
/// the machine being emulated.
///
/// Select and Start land on the buttons of those names, which every controller
/// since has kept in some form even when it prints something else on them.
const DEFAULT: Mapping = Mapping {
    buttons: [
        Physical::DPadUp,
        Physical::DPadDown,
        Physical::DPadLeft,
        Physical::DPadRight,
        Physical::East,
        Physical::South,
        Physical::Start,
        Physical::Select,
        Physical::LeftTrigger,
        Physical::RightTrigger,
    ],
    stick: true,
};

/// Every physical button worth binding to, and what it is called on screen and
/// in the file.
///
/// `C` and `Z` are left out: they belong to controllers with six face buttons,
/// which is a shape no console emulated here ever had, and offering them would
/// be two more rows in a dialog nobody can use them from.
const NAMES: [(Physical, &str); 17] = [
    (Physical::South, "South"),
    (Physical::East, "East"),
    (Physical::North, "North"),
    (Physical::West, "West"),
    (Physical::LeftTrigger, "L1"),
    (Physical::LeftTrigger2, "L2"),
    (Physical::RightTrigger, "R1"),
    (Physical::RightTrigger2, "R2"),
    (Physical::Select, "Select"),
    (Physical::Start, "Start"),
    (Physical::Mode, "Home"),
    (Physical::LeftThumb, "Left stick"),
    (Physical::RightThumb, "Right stick"),
    (Physical::DPadUp, "D-pad up"),
    (Physical::DPadDown, "D-pad down"),
    (Physical::DPadLeft, "D-pad left"),
    (Physical::DPadRight, "D-pad right"),
];

/// What to call a physical button, on screen and in the file.
///
/// A button outside [`NAMES`] is named after nothing, and that is what it is
/// worth: it cannot be bound from the dialog and would not survive being
/// written down.
pub fn name_of(button: Physical) -> &'static str {
    NAMES.iter().find(|(b, _)| *b == button).map_or("—", |(_, name)| *name)
}

fn button_named(name: &str) -> Option<Physical> {
    NAMES.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)).map(|(b, _)| *b)
}

fn pad_named(name: &str) -> Option<Pad> {
    Pad::ALL.into_iter().find(|pad| pad.name().eq_ignore_ascii_case(name))
}

// ---- The bindings -----------------------------------------------------------

/// What one controller's buttons do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    /// One physical button per console button, in the order of [`Pad::ALL`].
    buttons: [Physical; Pad::ALL.len()],
    /// Whether the left stick steers the cross as well.
    stick: bool,
}

impl Default for Mapping {
    fn default() -> Self {
        DEFAULT
    }
}

impl Mapping {
    /// Which physical button presses this console one.
    pub fn of(&self, pad: Pad) -> Physical {
        self.buttons[pad.index()]
    }

    pub const fn stick(&self) -> bool {
        self.stick
    }

    /// Puts a console button on a physical one, giving whoever held that
    /// physical button whatever this one is letting go of.
    ///
    /// The swap is the point. Binding without it leaves two console buttons on
    /// the same physical one, which is not an error anybody meant and reads
    /// from the outside as the other binding having quietly stopped working —
    /// and there is no way back to it from a dialog that only ever assigns.
    /// Trading keeps the ten buttons on ten distinct ones, whatever order they
    /// are assigned in.
    pub fn bind(&mut self, pad: Pad, button: Physical) {
        let given_up = self.buttons[pad.index()];
        if let Some(other) = Pad::ALL.into_iter().find(|p| self.buttons[p.index()] == button) {
            self.buttons[other.index()] = given_up;
        }
        self.buttons[pad.index()] = button;
    }
}

// ---- The controllers --------------------------------------------------------

/// Every controller plugged in, and what they are all doing.
pub struct Gamepads {
    /// `None` when there are no controllers to be had at all: a platform
    /// without them, or a machine whose input devices cannot be opened. Either
    /// way the keyboard still works, which is why it is not an error.
    gilrs: Option<Gilrs>,
    /// The bindings, by the name of the controller they belong to. Two
    /// controllers of different makes do not have to be configured into each
    /// other's mapping, and two of the same make share one, which is what
    /// somebody with a pair of them would have written out twice by hand.
    mappings: HashMap<String, Mapping>,
    /// Which console buttons are held, in the order of [`Pad::ALL`].
    held: [bool; Pad::ALL.len()],
    /// The controller last used, which is the one the dialog configures.
    active: Option<String>,
    /// The button pressed this very frame, for the dialog to catch. It is
    /// cleared at the start of every poll, so what is here is always new.
    caught: Option<Physical>,
    /// Where the bindings are read from and written back to, if anywhere.
    path: Option<PathBuf>,
}

impl Gamepads {
    /// Opens whatever is plugged in, or carries on without it.
    ///
    /// Failing is not fatal and barely worth a line on stderr: this is the
    /// second way of pressing the buttons, and the first one —the keyboard— is
    /// still there. It is the same bargain [`crate::open_audio`] strikes with a
    /// machine that has no sound card.
    pub fn open() -> Self {
        let gilrs = match Gilrs::new() {
            Ok(gilrs) => Some(gilrs),
            // The platform has no controllers to offer at all, which on Android
            // is simply the truth: the buttons there are painted on the glass.
            // Nothing was lost, so nothing is said.
            Err(gilrs::Error::NotImplemented(_)) => None,
            Err(trouble) => {
                eprintln!("warning: no controller ({trouble})");
                None
            }
        };
        let path = config_path();
        let mappings = path.as_deref().map(load).unwrap_or_default();
        Self {
            gilrs,
            mappings,
            held: [false; Pad::ALL.len()],
            active: None,
            caught: None,
            path,
        }
    }

    /// Catches up with the controllers. Called once a frame, before the buttons
    /// are handed to the console.
    pub fn poll(&mut self) {
        // Cleared here and filled by the drain below, so that what is in it is
        // always a button pressed *this* frame. Left to accumulate, the dialog
        // would answer its first row with whatever was pressed before it
        // opened.
        self.caught = None;
        self.drain();
        self.read();
    }

    /// Empties the event queue, which is what moves `gilrs` on to the current
    /// state of every controller. An undrained queue would leave the readings
    /// in [`Gamepads::read`] frozen at whenever it was last emptied.
    ///
    /// Only one thing is kept out of the events themselves, and only because it
    /// cannot be read any other way: which button went down just now.
    fn drain(&mut self) {
        let Some(gilrs) = self.gilrs.as_mut() else {
            return;
        };
        while let Some(event) = gilrs.next_event() {
            if let EventType::ButtonPressed(button, _) = event.event {
                if button != Physical::Unknown {
                    self.caught = Some(button);
                    self.active = Some(gilrs.gamepad(event.id).name().to_owned());
                }
            }
        }
    }

    /// Asks every controller what it is holding, and works out what that means
    /// to the console.
    fn read(&mut self) {
        let Some(gilrs) = self.gilrs.as_ref() else {
            return;
        };
        let previous = self.held;
        let mut held = [false; Pad::ALL.len()];
        for (_, gamepad) in gilrs.gamepads() {
            let mapping = self.mappings.get(gamepad.name()).unwrap_or(&DEFAULT);
            for pad in Pad::ALL {
                // Every controller is asked, and the answers are ORed: a second
                // one plugged in is a second player's, or the same player's
                // other hand, and neither is a reason to ignore it.
                held[pad.index()] |= gamepad.is_pressed(mapping.of(pad));
            }
            if mapping.stick {
                let (x, y) = (gamepad.value(Axis::LeftStickX), gamepad.value(Axis::LeftStickY));
                // Up and right are positive, whichever way round the hardware
                // reports them: `gilrs` has already turned the ones that
                // disagree.
                for (pad, lean) in
                    [(Pad::Right, x), (Pad::Left, -x), (Pad::Up, y), (Pad::Down, -y)]
                {
                    held[pad.index()] |= leaning(lean, previous[pad.index()]);
                }
            }
        }

        // The controller the dialog configures is the one last used for as long
        // as it is plugged in, and otherwise whichever is there. A name that no
        // longer answers would leave the dialog remapping a controller that has
        // gone away.
        let plugged_in = |name: &String| gilrs.gamepads().any(|(_, pad)| pad.name() == name);
        if !self.active.as_ref().is_some_and(plugged_in) {
            self.active = gilrs.gamepads().next().map(|(_, pad)| pad.name().to_owned());
        }
        self.held = held;
    }

    /// Whether a console button is being held on any controller.
    pub fn down(&self, pad: Pad) -> bool {
        self.held[pad.index()]
    }

    /// The controller being configured, if one is plugged in.
    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// The bindings that controller is using — the defaults, until somebody
    /// changes one.
    pub fn mapping(&self) -> Mapping {
        self.active
            .as_ref()
            .and_then(|name| self.mappings.get(name))
            .cloned()
            .unwrap_or_default()
    }

    /// The button pressed since the last poll, taken so it is answered once.
    pub fn take_caught(&mut self) -> Option<Physical> {
        self.caught.take()
    }

    /// Moves a console button onto a physical one and writes it down.
    pub fn bind(&mut self, pad: Pad, button: Physical) {
        self.change(|mapping| mapping.bind(pad, button));
    }

    pub fn use_stick(&mut self, on: bool) {
        self.change(|mapping| mapping.stick = on);
    }

    /// Puts the active controller back to the bindings it came with.
    pub fn restore(&mut self) {
        self.change(|mapping| *mapping = Mapping::default());
    }

    /// Changes the active controller's bindings and saves the lot.
    ///
    /// Saved on every change rather than when the dialog closes: it is a few
    /// hundred bytes, and a dialog that has to be dismissed the right way to
    /// keep what it plainly already did is a dialog that loses somebody's work.
    fn change(&mut self, edit: impl FnOnce(&mut Mapping)) {
        let Some(name) = self.active.clone() else {
            return;
        };
        edit(self.mappings.entry(name).or_default());
        if let Some(path) = &self.path {
            save(path, &self.mappings);
        }
    }
}

/// Whether a stick leaned this far counts as the direction being held, given
/// whether it already was.
fn leaning(value: f32, already: bool) -> bool {
    value >= if already { RELEASE } else { LEAN }
}

// ---- Where the bindings are kept --------------------------------------------

/// File the bindings are written to.
///
/// This one goes in `XDG_CONFIG_HOME` and not beside the last folder used in
/// `XDG_STATE_HOME`, and the difference is the whole reason both exist:
/// remapping a controller is a preference somebody sat down and expressed,
/// whereas the folder is a trace of what they happened to do last. Deleting
/// this one loses work; deleting that one loses a convenience.
pub fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            if home.as_os_str().is_empty() {
                return None;
            }
            home.join(".config")
        }
    };
    Some(base.join("akebia/controllers"))
}

pub fn load(path: &Path) -> HashMap<String, Mapping> {
    std::fs::read_to_string(path).map(|text| parse(&text)).unwrap_or_default()
}

/// Writes them all out, silently. A failure here is not worth a dialog: the
/// bindings still work for as long as Akebia is open, and the one thing worse
/// than losing them at the end is being told about it in the middle of a game.
pub fn save(path: &Path, mappings: &HashMap<String, Mapping>) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = std::fs::write(path, unparse(mappings));
}

/// Reads the file. Anything it does not understand is left at the default
/// rather than throwing the file out: a line naming a button from a later
/// version costs that one binding, and the other nine are still somebody's
/// work.
fn parse(text: &str) -> HashMap<String, Mapping> {
    let mut mappings = HashMap::new();
    let mut current: Option<(String, Mapping)> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            if let Some((name, mapping)) = current.take() {
                mappings.insert(name, mapping);
            }
            current = Some((name.trim().to_owned(), Mapping::default()));
            continue;
        }
        // A binding before any controller has been named belongs to nobody.
        let Some((_, mapping)) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("stick") {
            mapping.stick = value.eq_ignore_ascii_case("yes");
            continue;
        }
        // Assigned and not bound: the file is taken at its word, duplicates and
        // all. `bind` swaps to keep the dialog honest, and swapping while
        // reading would shuffle the lines that come after into places nobody
        // wrote.
        if let (Some(pad), Some(button)) = (pad_named(key), button_named(value)) {
            mapping.buttons[pad.index()] = button;
        }
    }
    if let Some((name, mapping)) = current {
        mappings.insert(name, mapping);
    }
    mappings
}

fn unparse(mappings: &HashMap<String, Mapping>) -> String {
    let mut out = String::from(
        "# Akebia controllers. One block per controller, by the name it reports.\n\
         # Buttons are named by where they sit: South is the one under the thumb.\n",
    );
    // Sorted, so that saving a file that did not change gives back the same
    // file: a hash map hands its keys out in whatever order it likes, and a
    // configuration that reshuffles itself on every run is one nobody can keep
    // an eye on.
    let mut names: Vec<&String> = mappings.keys().collect();
    names.sort();

    for name in names {
        let mapping = &mappings[name];
        out.push_str(&format!("\n[{name}]\n"));
        for pad in Pad::ALL {
            out.push_str(&format!("{} = {}\n", pad.name().to_lowercase(), name_of(mapping.of(pad))));
        }
        out.push_str(&format!("stick = {}\n", if mapping.stick { "yes" } else { "no" }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opening the real controllers and polling them twice answers for the ten
    /// buttons and does not fall over.
    ///
    /// Everything else here is arithmetic on a mapping, which would go on
    /// passing with the library wired up wrong. This is the one test that
    /// touches the machine it runs on: it opens whatever is plugged in, drains
    /// its queue and reads it. It cannot say the bindings are *right* —that
    /// needs a thumb— but it does say the whole path runs, on a machine with a
    /// controller and on one without.
    #[test]
    fn the_real_controllers_can_be_opened_and_polled() {
        let mut pads = Gamepads::open();
        pads.poll();
        pads.poll();
        for pad in Pad::ALL {
            let _ = pads.down(pad);
        }
    }

    /// Every default lands on a button that can be written down and read back.
    /// One that could not would be a mapping which changes the first time it is
    /// saved.
    #[test]
    fn the_defaults_can_all_be_named() {
        for pad in Pad::ALL {
            let button = DEFAULT.of(pad);
            assert_eq!(
                button_named(name_of(button)),
                Some(button),
                "{pad:?} is on a button with no name"
            );
        }
    }

    /// Ten console buttons on ten distinct controller ones, before anybody has
    /// touched anything.
    #[test]
    fn no_two_console_buttons_start_on_the_same_one() {
        let mut seen = Vec::new();
        for pad in Pad::ALL {
            let button = DEFAULT.of(pad);
            assert!(!seen.contains(&button), "{pad:?} shares {}", name_of(button));
            seen.push(button);
        }
    }

    /// Binding onto a button somebody else holds trades with them instead of
    /// stranding them.
    #[test]
    fn binding_over_another_button_swaps_the_two() {
        let mut mapping = Mapping::default();
        let was_on_a = mapping.of(Pad::A);
        let was_on_b = mapping.of(Pad::B);

        mapping.bind(Pad::A, was_on_b);

        assert_eq!(mapping.of(Pad::A), was_on_b);
        assert_eq!(mapping.of(Pad::B), was_on_a, "B took what A was using");
    }

    /// Binding a button onto the one it already has changes nothing — the swap
    /// must not trade it with itself and leave the row empty.
    #[test]
    fn binding_a_button_onto_itself_is_no_change() {
        let mut mapping = Mapping::default();
        let before = mapping.clone();
        mapping.bind(Pad::A, mapping.of(Pad::A));
        assert_eq!(mapping, before);
    }

    #[test]
    fn a_saved_mapping_reads_back_the_same() {
        let mut mapping = Mapping::default();
        mapping.bind(Pad::A, Physical::South);
        mapping.stick = false;
        let mut mappings = HashMap::new();
        mappings.insert("Some Controller".to_owned(), mapping.clone());
        // A second one, to pin that the file keeps them apart.
        mappings.insert("Another Controller".to_owned(), Mapping::default());

        let read = parse(&unparse(&mappings));

        assert_eq!(read.get("Some Controller"), Some(&mapping));
        assert_eq!(read.get("Another Controller"), Some(&Mapping::default()));
    }

    /// A file written by a later version, or by hand: what is understood is
    /// kept and the rest stays at the default, rather than the whole file being
    /// thrown away over one line.
    #[test]
    fn what_cannot_be_understood_is_left_at_the_default() {
        let read = parse(
            "# a comment\n\
             [Pad]\n\
             a = West\n\
             b = Trackball\n\
             elbow = South\n\
             this line has no equals sign\n\
             stick = no\n",
        );

        let mapping = read.get("Pad").expect("the controller was named");
        assert_eq!(mapping.of(Pad::A), Physical::West, "the line that made sense");
        assert_eq!(mapping.of(Pad::B), DEFAULT.of(Pad::B), "the one that did not");
        assert!(!mapping.stick);
    }

    /// A binding written before any controller was named belongs to nobody, and
    /// must not be quietly filed under the next one.
    #[test]
    fn a_binding_with_no_controller_above_it_is_dropped() {
        let read = parse("a = West\n[Pad]\nb = North\n");
        let mapping = read.get("Pad").expect("the controller was named");
        assert_eq!(mapping.of(Pad::A), DEFAULT.of(Pad::A));
        assert_eq!(mapping.of(Pad::B), Physical::North);
    }

    /// The stick has to be leant on harder to press a direction than to keep
    /// it pressed, or a thumb resting near the edge taps the d-pad.
    #[test]
    fn a_direction_is_harder_to_start_than_to_hold() {
        let between = (LEAN + RELEASE) / 2.0;
        assert!(!leaning(between, false), "not enough to start it");
        assert!(leaning(between, true), "but enough to keep it");
        assert!(!leaning(RELEASE - 0.01, true), "and let go below the lower one");
        assert!(leaning(LEAN + 0.01, false));
    }

    /// Saving is not asked to invent a folder that is not there, and a file
    /// that cannot be read is no bindings rather than a crash.
    #[test]
    fn the_file_survives_a_folder_that_does_not_exist_yet() {
        let dir = std::env::temp_dir().join(format!("akebia-pads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("deeper/controllers");
        assert!(load(&path).is_empty(), "nothing written is nothing read");

        let mut mappings = HashMap::new();
        mappings.insert("Pad".to_owned(), Mapping::default());
        save(&path, &mappings);

        assert_eq!(load(&path), mappings);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
