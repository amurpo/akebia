//! The desktop application: ROM list and play screen.
//!
//! # Why `eframe` and not a system toolkit
//!
//! An emulator needs two things from its interface: stretching a 160×144 texture
//! sixty times per second and checking the state of eight keys. GTK or Qt give
//! far more than that, and in exchange demand being installed on the machine
//! that runs the program —with everything that implies for the RPM and for
//! Windows—. `eframe` is pure Rust: a standalone binary still comes out, and the
//! same interface compiles on Linux, Windows and macOS.
//!
//! # Why the emulation runs here and not on another thread
//!
//! It looked like a thread of its own would be needed, because `eframe` has its
//! own event loop and must not be blocked. It is not needed: emulating a frame
//! costs less than a millisecond —the console runs at about 17 times real
//! time—, so they fit comfortably inside [`App::logic`]. What is needed is
//! **decoupling from the screen refresh**: `logic` is called at the monitor's
//! rate, which may be 60, 120 or 144 Hz, and the Game Boy runs at 59.73. That is
//! why it does not emulate "one frame per call" but as many as fit in the
//! elapsed time. A separate thread would only add synchronisation around the
//! saved game and the debug captures.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use akebia_core::cartridge::CgbSupport;
use akebia_core::gameboy::FRAMES_PER_SECOND;
use akebia_core::link::{self, LinkFault, Side};
use akebia_core::ports::{Palette, VideoOutput};
use akebia_core::{FrameBuffer, GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::console::{Console, Pad};
use eframe::egui::{
    self, Align, Color32, ColorImage, Key, Label, Layout, RichText, Sense, TextureHandle,
    TextureOptions, UiBuilder, Vec2, ViewportCommand,
};

use crate::args::Args;
use crate::gamepad::Gamepads;
use crate::remote::{Remote, Trouble};
use crate::rate::Rate;
use crate::{audio, debug, gamepad, net, rate, recent, remote, roms, save};

/// The red of the Akebia logo. It is the colour of everything selected.
pub const ACCENT: Color32 = Color32::from_rgb(0xCF, 0x1A, 0x30);

/// Application background, a very dark grey but not black: black is reserved for
/// the screen's frame, and that keeps the two apart.
pub const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x15, 0x18);

/// The Game Boy Color brand violet, for the badge on the exclusives.
const VIOLET: Color32 = Color32::from_rgb(0x6B, 0x4F, 0xC9);

/// Height of each row in the list.
const ROW_HEIGHT: f32 = 32.0;

/// How much of the path is shown in the header before clipping it.
const MAX_PATH: usize = 46;

/// Room left for the menu bar whenever a window size is asked for.
///
/// It is a reservation and not a measurement because the first size is decided
/// before there is any interface to measure: `--scale` acts as the window is
/// created. Erring high is the safe direction — a few spare pixels end up as
/// black around the screen, whereas a few short drop the picture to the next
/// whole factor down, which is very visible.
const MENU_BAR_ROOM: f32 = 28.0;

/// How many frames are emulated at most in one go to catch up.
///
/// With no cap, coming back from a window minimised for a minute would try to
/// emulate three thousand six hundred frames at once and would hang the
/// interface. Past the cap the debt is abandoned, which is the same thing the
/// terminal loop does: running late is forgiven, speeding up afterwards is not.
const MAX_CATCH_UP: u32 = 4;

/// Joypad keys. The layout is the usual one in emulators: the d-pad on the
/// arrows and the action buttons under the left hand.
///
/// The two shoulders are here for the Advance and do nothing on a Game Boy,
/// which is what a Game Boy does when a button it has not got is pressed. One
/// list for both machines is the point: a person does not want the keys to move
/// when the cartridge does.
const KEYS: [(Key, Pad); 10] = [
    (Key::ArrowUp, Pad::Up),
    (Key::ArrowDown, Pad::Down),
    (Key::ArrowLeft, Pad::Left),
    (Key::ArrowRight, Pad::Right),
    (Key::Z, Pad::A),
    (Key::X, Pad::B),
    (Key::Enter, Pad::Start),
    (Key::Backspace, Pad::Select),
    (Key::A, Pad::L),
    (Key::S, Pad::R),
];

/// What is being held down, on the keyboard and on any controller at once.
///
/// The two are ORed rather than chosen between. A controller does not take the
/// keyboard away: whoever is holding one still has Escape, the menu and the
/// capture key under their other hand, and a second player on the keyboard is
/// nobody's mistake to correct.
fn held(ctx: &egui::Context, pads: &Gamepads) -> [(Pad, bool); KEYS.len()] {
    ctx.input(|i| KEYS.map(|(key, button)| (button, i.key_down(key) || pads.down(button))))
}

/// Opens the window and does not return until it is closed.
pub fn run(args: Args, limit: Option<u64>) -> Result<(), String> {
    run_with(args, limit, eframe::NativeOptions::default())
}

/// The same, on top of options the platform has already filled in.
///
/// Android builds its event loop out of an `AndroidApp` handed to
/// `android_main`, and that value cannot be manufactured here: it only exists
/// inside that call. The Android entry point puts it in `base` and this is the
/// only reason the seam exists; the desktop passes the defaults.
pub fn run_with(args: Args, limit: Option<u64>, base: eframe::NativeOptions) -> Result<(), String> {
    let settings = Settings::from_args(&args);

    // A ROM given on the command line is loaded **before** opening anything: if
    // it is broken, the error has to come out on stderr and with an exit code,
    // like that of any other command, not inside a window.
    let initial = match &args.rom {
        Some(path) => {
            let console = crate::load_console(path, &args)?;
            crate::remember_dir(path);
            Some(Session::new(console, path.clone(), &args, &settings))
        }
        None => None,
    };

    let scale = if args.scale == 0 { 4 } else { args.scale } as f32;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title(initial.as_ref()))
            // The identifier has to match the name of the `.desktop` file, or
            // the desktop does not know which icon to give the window.
            .with_app_id("akebia")
            .with_inner_size(window_size(scale))
            .with_min_inner_size(window_size(2.0))
            .with_maximized(args.scale == 0),
        ..base
    };

    eframe::run_native(
        "Akebia",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, args, limit, initial, settings)))),
    )
    .map_err(|e| format!("could not open the window: {e}"))
}

// ---- Settings ---------------------------------------------------------------

/// Everything the menu changes.
///
/// It lives in [`App`] and not in [`Session`] because it has to outlive a game:
/// turning the sound off, going back to the list and starting another game must
/// not turn it back on. The command line only seeds it; from then on the menu
/// is what says how Akebia is set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    sound: bool,
    /// The low-pass that stands in for the amplifier and the speaker. Off, the
    /// treble comes through whole and the square waves sound harsher than the
    /// console did.
    speaker_filter: bool,
    /// Greys instead of the DMG greens. It only shows on a monochrome game: a
    /// Game Boy Color one supplies its own colours.
    grayscale: bool,
    /// Window size as a multiple of 160×144, or `0` to fill the screen.
    scale: u32,
    /// Show how fast the emulator is going. Off by default: it answers a
    /// question nobody has while the game is running properly.
    fps: bool,
}

impl Settings {
    pub fn from_args(args: &Args) -> Self {
        Self {
            sound: !args.mute,
            speaker_filter: !args.raw_audio,
            grayscale: args.grayscale,
            scale: args.scale,
            fps: false,
        }
    }

    fn palette(&self) -> Palette {
        if self.grayscale {
            Palette::GRAYSCALE
        } else {
            Palette::DMG
        }
    }
}

/// The window that leaves a screen of that size at exactly `scale`.
///
/// The size is passed in rather than assumed: the two machines are 160×144 and
/// 240×160, and a window built for the wrong one either wastes half its width
/// or shrinks the picture to fit.
fn window_size_for(screen: (usize, usize), scale: f32) -> Vec2 {
    let (width, height) = screen;
    Vec2::new(width as f32 * scale, height as f32 * scale + MENU_BAR_ROOM)
}

/// The window for the older machine, which is what the list and an empty
/// window are sized to.
fn window_size(scale: f32) -> Vec2 {
    window_size_for((SCREEN_WIDTH, SCREEN_HEIGHT), scale)
}

/// Gap left between two linked screens. Without it the two pictures read as one
/// wide one, which with the same game running twice is genuinely confusing.
const LINK_GUTTER: f32 = 16.0;

/// The window two linked consoles need: both screens at the size the single one
/// had, with the gutter between them.
///
/// The window grows rather than the screens shrinking, and that is the point. A
/// second console squeezed into the window the first one had would halve both of
/// them, and a Game Boy screen at half the size it was is not two consoles: it is
/// two unreadable ones.
fn linked_window_size(scale: f32) -> Vec2 {
    let single = window_size(scale);
    Vec2::new(single.x * 2.0 + LINK_GUTTER, single.y)
}

fn title(session: Option<&Session>) -> String {
    match session {
        Some(s) => format!("Akebia — {}", s.title),
        None => "Akebia".to_owned(),
    }
}

/// The window's name while two consoles are joined by a cable.
fn linked_title(pair: &Pair) -> String {
    format!("Akebia — {} ↔ {}", pair.consoles[0].title, pair.consoles[1].title)
}

/// Colours and spacing. The dark theme is fixed instead of following the system:
/// around a stretched 160×144 screen, a light background is blinding.
fn theme(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BACKGROUND;
    v.window_fill = BACKGROUND;
    v.extreme_bg_color = Color32::from_rgb(0x0D, 0x0D, 0x0F);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke.color = Color32::WHITE;
    ctx.set_visuals(v);

    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(10.0, 6.0);
        // egui labels are selectable text by default, and to be so they capture
        // clicks and drags. On top of a list row that swallows the click right
        // where the name is —which is where one clicks— and the row never finds
        // out. There is no text here worth copying, so it is switched off for
        // the whole application.
        s.interaction.selectable_labels = false;
    });
}

struct App {
    args: Args,
    /// `--frames` cap, which closes the window when reached.
    limit: Option<u64>,
    /// The list's folder, remembered so as to return to it when leaving a game.
    folder: PathBuf,
    settings: Settings,
    screen: Screen,
    /// The folder chooser, floating over whatever screen is behind. It is a
    /// dialog and not a screen of its own so that it can be opened from the menu
    /// in the middle of a game without throwing the game away.
    dialog: Option<Box<Browser>>,
    /// The consoles' screens, uploaded to the GPU once per emulated frame. The
    /// second one is only painted while a cable is plugged in, but it costs
    /// 160×144 pixels to keep and saves creating a texture mid-game.
    textures: [TextureHandle; 2],
    /// A connection being made, while the game carries on. Both waiting and
    /// dialling take as long as they take, so neither stops the window.
    connecting: Option<net::Pending>,
    /// The address box, open with whatever is in it.
    address: Option<String>,
    /// The controllers, polled once a frame whatever is on screen.
    pads: Gamepads,
    /// The controller dialog, open, and which console button it is waiting to
    /// hear a controller button for.
    controls: Option<Wait>,
    /// What was typed there last. Going back to the same machine is far and away
    /// the commonest thing to want, and retyping an address is a poor way to
    /// spend the moment before a trade.
    last_address: String,
    /// The games opened lately, as the menu offers them.
    ///
    /// Held rather than read from disk each time the menu is opened: it is
    /// drawn every frame that the mouse is over it, and a file read per frame
    /// to show ten lines that change once a session is not a trade worth
    /// making.
    recent: Vec<PathBuf>,
    /// The last thing worth saying about the session that is not the picture
    /// itself, kept on the menu bar until something replaces it.
    ///
    /// Two kinds of thing end up here, and they have the same shape. A link
    /// ends for reasons that are nobody's mistake —the other player put their
    /// telephone down— and the game carries on. An Advance started without a
    /// BIOS runs and draws nothing, which needs saying more than most things
    /// do. Neither is a question, so neither belongs in a dialog, and there is
    /// no list left to leave them on: the end of the menu bar is where the
    /// session already speaks from.
    notice: Option<String>,
}

/// The controller dialog while it is open: nothing, until a row is clicked and
/// it is listening for the button that row is to be moved onto.
#[derive(Default)]
struct Wait {
    button: Option<Pad>,
}

enum Screen {
    List(List),
    /// Boxed because a `Session` holds the whole console inside, and without the
    /// box the enum would take up that much while sitting in the list.
    Playing(Box<Session>),
    /// Two consoles joined by a link cable.
    Linked(Box<Pair>),
    /// One console, with the other end of the cable on another machine.
    Networked(Box<Wired>),
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        args: Args,
        limit: Option<u64>,
        initial: Option<Session>,
        settings: Settings,
    ) -> Self {
        theme(&cc.egui_ctx);

        let screen_texture = |name: &str| {
            cc.egui_ctx.load_texture(
                name,
                ColorImage::new(
                    [SCREEN_WIDTH, SCREEN_HEIGHT],
                    vec![Color32::BLACK; SCREEN_WIDTH * SCREEN_HEIGHT],
                ),
                // Nearest neighbour: a Game Boy pixel has to come out as an exact
                // square, not as an interpolated smudge.
                TextureOptions::NEAREST,
            )
        };
        let textures = [screen_texture("screen"), screen_texture("screen-linked")];

        let folder = roms::initial_dir(args.roms_dir.as_deref(), roms::state_path().as_deref());
        let notice = initial.as_ref().and_then(Session::warning);
        let screen = match initial {
            Some(session) => Screen::Playing(Box::new(session)),
            None => Screen::List(List::new(folder.clone())),
        };

        Self {
            args,
            limit,
            folder,
            settings,
            screen,
            dialog: None,
            textures,
            connecting: None,
            address: None,
            pads: Gamepads::open(),
            controls: None,
            last_address: String::new(),
            recent: recent::path().map(|list| recent::load(&list)).unwrap_or_default(),
            notice,
        }
    }

    /// Loads the chosen ROM and starts playing, or leaves the warning in the
    /// list.
    fn play(&mut self, ctx: &egui::Context, path: PathBuf) {
        match crate::load_console(&path, &self.args) {
            Ok(console) => {
                crate::remember_dir(&path);
                // Re-read rather than pushed onto the front here: what was
                // written is what the menu should show, including the pruning
                // of anything that has since gone away.
                self.recent = recent::path().map(|list| recent::load(&list)).unwrap_or_default();
                if let Some(dir) = path.parent() {
                    self.folder = dir.to_owned();
                }
                let session = Session::new(console, path, &self.args, &self.settings);
                ctx.send_viewport_cmd(ViewportCommand::Title(title(Some(&session))));
                // Whatever a previous game or a previous cable left there is
                // gone; what this one has to say about itself takes its place.
                self.notice = session.warning();
                self.screen = Screen::Playing(Box::new(session));
                // The window follows the machine. Starting an Advance in a Game
                // Boy-shaped window would letterbox a picture that has a window
                // of its own to be shown in.
                self.resize_for(ctx, false);
            }
            // Loading can fail because of a mapper not implemented yet or a file
            // that is not a ROM. It is shown in the list and not on stderr:
            // whoever opened Akebia from the desktop launcher has no terminal to
            // read it in.
            Err(failure) => {
                if let Screen::List(list) = &mut self.screen {
                    list.warning = Some(first_line(&failure));
                }
            }
        }
    }

    /// The menu bar, always on top of whichever screen is showing.
    ///
    /// It is where anything that is not "pick a game" or "play" belongs. A
    /// setting reached from here does not have to invent a place to live in
    /// either of the two screens, which is what keeps adding one from
    /// disfiguring them: filters, models or key remapping are new entries in a
    /// menu, not new buttons wedged into a list.
    fn menu_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let playing = matches!(self.screen, Screen::Playing(_));
        let linked = matches!(self.screen, Screen::Linked(_));
        let networked = matches!(self.screen, Screen::Networked(_));
        let waiting = self.connecting.is_some();
        let mut open_dialog = false;
        let mut back_to_list = false;
        let mut connect = false;
        let mut swap = false;
        let mut unplug = false;
        let mut listen = false;
        let mut dial = false;
        let mut give_up = false;
        let mut configure = false;
        // A game chosen off the recent menu, and whether the menu was emptied.
        let mut replay: Option<PathBuf> = None;
        let mut forget = false;
        let mut settings = self.settings.clone();
        // Read out here because the menu is drawn inside a closure that already
        // holds the whole of `self`.
        let pad = self.pads.active().map(str::to_owned);

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    // The games lately played, above the way of finding one
                    // that is not in the list: going back to a game is far
                    // commoner than going looking for a new one.
                    ui.menu_button("Recent games", |ui| {
                        let paths = self.recent.clone();
                        if paths.is_empty() {
                            ui.add_enabled(false, egui::Button::new("Nothing yet"));
                            return;
                        }
                        for (path, label) in paths.iter().zip(recent::labels(&paths)) {
                            if ui.button(label).clicked() {
                                replay = Some(path.clone());
                            }
                        }
                        ui.separator();
                        forget = ui.button("Clear the list").clicked();
                    });
                    open_dialog = ui.button("ROM folder…").clicked();
                    ui.separator();
                    back_to_list =
                        ui.add_enabled(playing, egui::Button::new("Back to the list")).clicked();
                    if ui.button("Quit").clicked() {
                        ui.send_viewport_cmd(ViewportCommand::Close);
                    }
                });
                ui.menu_button("Video", |ui| {
                    ui.menu_button("Window size", |ui| {
                        for factor in [2, 3, 4, 6] {
                            ui.radio_value(&mut settings.scale, factor, format!("×{factor}"));
                        }
                        ui.radio_value(&mut settings.scale, 0, "Fill the screen");
                    });
                    ui.menu_button("Monochrome palette", |ui| {
                        // Both entries say what they do to a Game Boy game, not
                        // what they are called inside: `--grayscale` is a flag,
                        // "Greys" is a picture.
                        ui.radio_value(&mut settings.grayscale, false, "Game Boy green");
                        ui.radio_value(&mut settings.grayscale, true, "Greys");
                    });
                    ui.separator();
                    ui.checkbox(&mut settings.fps, "Frame rate").on_hover_text(
                        "How many of the console's frames a second are being produced, \
                         and the share of the time it takes to produce them",
                    );
                });
                ui.menu_button("Audio", |ui| {
                    ui.checkbox(&mut settings.sound, "Sound");
                    // Named after what it imitates and not after what it is:
                    // "Speaker filter" says more than "8 kHz low-pass" to
                    // somebody deciding whether they want it on.
                    ui.add_enabled(
                        settings.sound,
                        egui::Checkbox::new(&mut settings.speaker_filter, "Speaker filter"),
                    )
                    .on_hover_text("Rolls off the treble the way the console's speaker did");
                });
                ui.menu_button("Controls", |ui| {
                    // Which controller is being listened to, said before
                    // anything can be done to it: half of "the controller does
                    // nothing" is Akebia never having seen one, and that is not
                    // a question a dialog full of bindings answers.
                    match &pad {
                        Some(name) => {
                            ui.add_enabled(false, egui::Button::new(name));
                        }
                        None => {
                            ui.add_enabled(false, egui::Button::new("No controller"))
                                .on_disabled_hover_text(
                                    "Plug one in and it is picked up without restarting",
                                );
                        }
                    }
                    ui.separator();
                    configure = ui.button("Configure…").clicked();
                });
                ui.menu_button("Link", |ui| {
                    connect = ui
                        .add_enabled(playing && !waiting, egui::Button::new("Second console"))
                        .on_hover_text("Copy this console and join the two with a link cable")
                        .clicked();
                    swap = ui
                        .add_enabled(linked, egui::Button::new("Swap keyboard\tTab"))
                        .on_hover_text("Hand the keyboard to the other console")
                        .clicked();
                    ui.separator();
                    // Over a network the second console is somebody else's, so
                    // there is nothing to copy and nothing to swap to: each
                    // machine shows its own screen and keeps its own keyboard.
                    listen = ui
                        .add_enabled(playing && !waiting, egui::Button::new("Wait for a console…"))
                        .on_hover_text("Let another machine connect to this one")
                        .clicked();
                    dial = ui
                        .add_enabled(playing && !waiting, egui::Button::new("Connect to a console…"))
                        .on_hover_text("Go to another machine that is already waiting")
                        .clicked();
                    give_up = ui
                        .add_enabled(waiting, egui::Button::new("Stop waiting"))
                        .clicked();
                    ui.separator();
                    unplug = ui
                        .add_enabled(linked || networked, egui::Button::new("Unplug the cable"))
                        .on_hover_text("Keep playing on this console alone")
                        .clicked();
                });

                // Whatever the cable is doing says so here, at the far end of the
                // same row. Until this, the only sign that "Wait for a console"
                // had done anything at all was the window's title, which is
                // covered by a full screen, cut short by some window managers
                // and not looked at by anybody in the middle of a game: what it
                // came to was choosing it and seeing nothing happen.
                let said = self.status();
                let speed = self.settings.fps.then(|| self.speed()).flatten();
                if said.is_some() || speed.is_some() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        if let Some(said) = said {
                            ui.label(RichText::new(said).color(ACCENT));
                        }
                        // The rate goes left of whatever the session is saying,
                        // and turns the colour of a warning once the emulator
                        // has no room left — which is the moment it stops being
                        // a curiosity and becomes the answer to "why is this
                        // slow".
                        if let Some(speed) = speed {
                            let colour =
                                if speed.flat_out() { ACCENT } else { Color32::GRAY };
                            ui.label(RichText::new(speed.to_line()).small().color(colour))
                                .on_hover_text(
                                    "Console frames a second, and the share of real time \
                                     spent producing them",
                                );
                            ui.add_space(8.0);
                        }
                    });
                }
            });
        });

        if settings != self.settings {
            self.apply(&settings, ctx);
        }
        if configure {
            self.controls = Some(Wait::default());
        }
        if open_dialog {
            self.open_dialog();
        }
        if let Some(path) = replay {
            // Straight into the game, whatever is on screen now. A game chosen
            // from the menu is a game asked for, and dropping back to the list
            // first would make the menu a slower way of doing what the list
            // already does.
            self.play(ctx, path);
        }
        if forget {
            if let Some(list) = recent::path() {
                recent::clear(&list);
            }
            self.recent.clear();
        }
        if back_to_list {
            self.back_to_list(ctx, None);
        }
        if connect {
            self.plug_in_a_second_console(ctx);
        }
        if swap {
            if let Screen::Linked(pair) = &mut self.screen {
                pair.swap_keyboard(&self.settings);
            }
        }
        if listen {
            self.start_waiting(ctx);
        }
        if dial {
            // Whatever was typed last: reconnecting to the same machine is far
            // and away the commonest thing to want.
            self.address = Some(self.last_address.clone());
        }
        if give_up {
            self.connecting = None;
            self.notice = None;
            self.retitle(ctx);
        }
        if unplug {
            self.unplug(ctx);
        }
    }

    /// One line about the session, or nothing when there is nothing to say.
    ///
    /// A live cable outranks whatever [`App::notice`] is holding, because it is
    /// happening now and the notice is something that happened. A connection
    /// has four states worth telling apart and the interface used to show one
    /// of them: "connecting" and "connected but the other end has not answered
    /// yet" look identical from a chair —the game sits there in both— and the
    /// difference is exactly what a player needs to know before deciding the
    /// thing is broken.
    /// How fast whichever session is running is going, if it has been going
    /// long enough to say. Nothing on the list, where there is nothing being
    /// emulated to measure.
    fn speed(&self) -> Option<rate::Reading> {
        match &self.screen {
            Screen::Playing(session) => session.rate(),
            Screen::Linked(pair) => pair.consoles[0].rate(),
            Screen::Networked(wired) => wired.console.rate(),
            Screen::List(_) => None,
        }
    }

    fn status(&self) -> Option<String> {
        // Waiting has to say *where*, because the other machine has to be told
        // an address and this is the end that knows it.
        if let Some(pending) = &self.connecting {
            return Some(match &pending.here {
                Some(here) => format!("{} — this machine is {here}", pending.what),
                None => pending.what.clone(),
            });
        }
        if let Screen::Networked(wired) = &self.screen {
            let peer = wired.remote.peer();
            return Some(if wired.remote.is_ready() {
                format!("Linked to {peer}")
            } else {
                format!("Connected to {peer} — waiting for it to answer")
            });
        }
        self.notice.clone()
    }

    /// Starts waiting for another machine to connect to this one.
    fn start_waiting(&mut self, ctx: &egui::Context) {
        if !matches!(self.screen, Screen::Playing(_)) {
            return;
        }
        self.notice = None;
        self.connecting = Some(net::Pending::listen(akebia_core::link::bgb::DEFAULT_PORT));
        self.retitle(ctx);
    }

    /// Goes looking for a machine that is already waiting.
    fn start_dialling(&mut self, ctx: &egui::Context, address: String) {
        if !matches!(self.screen, Screen::Playing(_)) || address.trim().is_empty() {
            return;
        }
        self.notice = None;
        self.last_address = address.clone();
        self.connecting = Some(net::Pending::connect(address));
        self.retitle(ctx);
    }

    /// Picks up a connection once it has been made, and plugs the cable in.
    fn collect_connection(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.connecting.as_mut() else {
            return;
        };
        let dialled = pending.dialled;
        let Some(result) = pending.poll() else {
            return;
        };
        self.connecting = None;
        let role = if dialled { remote::Role::Dialled } else { remote::Role::Waited };

        let wire = match result {
            Ok(wire) => wire,
            Err(failure) => {
                // The game is not thrown away to say a connection failed: it
                // goes on the menu bar, next to where the attempt was announced.
                self.notice = Some(first_line(&failure.to_string()));
                self.retitle(ctx);
                return;
            }
        };

        // Whatever the cable was for is no longer on screen: a second console
        // was copied in the meantime, or the game was left for the list. Taking
        // the screen apart here regardless would drop a session without saving
        // it, which is a great deal worse than a connection nobody uses.
        if !matches!(self.screen, Screen::Playing(_)) {
            return;
        }
        let aside = Screen::List(List::new(self.folder.clone()));
        if let Screen::Playing(mut session) = std::mem::replace(&mut self.screen, aside) {
            // The cable is the older machine's. Nothing should have offered it
            // for anything else, and if something did, the game carries on
            // rather than the connection taking it away.
            let Some(gb) = session.gb_mut() else {
                self.notice = Some("only a Game Boy has a link cable".to_string());
                self.screen = Screen::Playing(session);
                return;
            };
            let remote = Remote::new(wire, gb, role);
            self.screen = Screen::Networked(Box::new(Wired { console: *session, remote }));
            self.retitle(ctx);
        }
    }

    /// Puts on the title bar whatever the window is doing.
    fn retitle(&mut self, ctx: &egui::Context) {
        let name = match (&self.screen, &self.connecting) {
            (_, Some(pending)) => format!("Akebia — {}", pending.what),
            (Screen::Networked(wired), None) => {
                format!("Akebia — {} ↔ {}", wired.console.title, wired.remote.peer())
            }
            (Screen::Linked(pair), None) => linked_title(pair),
            (Screen::Playing(session), None) => title(Some(session)),
            (Screen::List(_), None) => title(None),
        };
        ctx.send_viewport_cmd(ViewportCommand::Title(name));
    }

    /// The box the address is typed into.
    ///
    /// A dialog and not a screen of its own, like the folder chooser: whatever
    /// is being played stays where it is, and cancelling costs nothing.
    fn address_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut typed) = self.address.take() else {
            return;
        };
        let mut go = false;
        let mut cancelled = false;

        let response = egui::Modal::new(egui::Id::new("address")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new("Connect to a console").size(18.0).strong());
            ui.add_space(6.0);
            ui.label(
                RichText::new("The address of the machine that is waiting. Without a port it\nuses the usual one for a link cable.")
                    .size(13.0)
                    .color(Color32::from_gray(0x9A)),
            );
            ui.add_space(10.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut typed)
                    .hint_text("192.168.1.20")
                    .desired_width(f32::INFINITY),
            );
            field.request_focus();
            go |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                go |= ui.button("Connect").clicked();
                cancelled |= ui.button("Cancel").clicked();
            });
        });

        if response.should_close() {
            cancelled = true;
        }
        if go {
            self.start_dialling(ctx, typed);
        } else if !cancelled {
            // Still being typed into.
            self.address = Some(typed);
        }
    }

    /// The controller's ten buttons, and what each one is on it.
    ///
    /// A dialog like the other two, and for the same reason: it opens over a
    /// game without ending it, so a button that turned out to be the wrong one
    /// can be moved where it was noticed rather than back at the list.
    ///
    /// Every change is kept the moment it is made — there is no accepting or
    /// cancelling. A row shows what it is on; clicking it makes the row listen,
    /// and the next button pressed on the controller is what it becomes.
    fn controls_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut wait) = self.controls.take() else {
            return;
        };

        // A button pressed while a row was listening is the answer that row was
        // waiting for. Taken before the dialog is drawn so the new binding is
        // what gets shown, rather than a frame of the old one.
        if let (Some(button), Some(pressed)) = (wait.button, self.pads.take_caught()) {
            self.pads.bind(button, pressed);
            wait.button = None;
        }

        let mapping = self.pads.mapping();
        let pad = self.pads.active().map(str::to_owned);
        let mut stick = mapping.stick();
        let mut listen_for = None;
        let mut restore = false;
        let mut done = false;

        let response = egui::Modal::new(egui::Id::new("controls")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(RichText::new("Controller").size(18.0).strong());
            ui.add_space(6.0);
            let said = match &pad {
                Some(name) => name.clone(),
                None => "Nothing plugged in — these are what it would use".to_owned(),
            };
            ui.label(RichText::new(said).size(13.0).color(Color32::from_gray(0x9A)));
            ui.add_space(10.0);

            // Nothing can be changed with no controller plugged in, and the
            // rows say so by being dead rather than by accepting a click and
            // then listening for ever to a controller that is not there.
            ui.add_enabled_ui(pad.is_some(), |ui| {
                egui::Grid::new("bindings").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    for button in Pad::ALL {
                        ui.label(button.name());
                        let listening = wait.button == Some(button);
                        let label = if listening {
                            RichText::new("press a button…").color(ACCENT)
                        } else {
                            RichText::new(gamepad::name_of(mapping.of(button)))
                        };
                        let row = egui::Button::new(label).min_size(Vec2::new(150.0, 0.0));
                        if ui.add(row).clicked() {
                            listen_for = Some(button);
                        }
                        ui.end_row();
                    }
                });

                ui.add_space(10.0);
                // Named after what it does and not after the hardware: whoever
                // has to decide whether they want it is thinking about the
                // cross, not about an axis pair.
                ui.checkbox(&mut stick, "The left stick steers the cross too");
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "The face buttons are named by where they sit, because what is\n\
                     printed on them is not the same on two controllers: South is\n\
                     the one under the thumb, East the one to the right of it.",
                )
                .size(12.0)
                .color(Color32::from_gray(0x8A)),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                done = ui.button("Done").clicked();
                restore =
                    ui.add_enabled(pad.is_some(), egui::Button::new("Defaults")).clicked();
            });
        });

        if let Some(button) = listen_for {
            // Clicking the row that is already listening stops it listening,
            // which is the only way out of a row bound to a button that has
            // since been unplugged.
            wait.button = (wait.button != Some(button)).then_some(button);
        }
        if restore {
            self.pads.restore();
            wait.button = None;
        }
        if stick != mapping.stick() {
            self.pads.use_stick(stick);
        }
        // While a row is listening nothing on screen changes until the
        // controller is touched, and a controller is not something egui wakes up
        // for. Without this the dialog would sit there until the mouse moved.
        if wait.button.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if !done && !response.should_close() {
            self.controls = Some(wait);
        }
    }

    /// Copies the running console and joins the two with a cable.
    ///
    /// Nothing is asked for, and no game is opened: the second console is this
    /// one, in the state it is in. See [`Session::duplicate`].
    fn plug_in_a_second_console(&mut self, ctx: &egui::Context) {
        if !matches!(self.screen, Screen::Playing(_)) {
            return;
        }
        let list = Screen::List(List::new(self.folder.clone()));
        if let Screen::Playing(first) = std::mem::replace(&mut self.screen, list) {
            let second = first.duplicate(&self.args, &self.settings);
            let pair = Pair::new(*first, second, &self.settings);

            ctx.send_viewport_cmd(ViewportCommand::Title(linked_title(&pair)));
            self.resize_for(ctx, true);
            self.screen = Screen::Linked(Box::new(pair));
        }
    }

    /// Pulls the cable out, keeping whichever console has the keyboard and
    /// saving the other one on its way out.
    fn unplug(&mut self, ctx: &egui::Context) {
        if !matches!(self.screen, Screen::Linked(_) | Screen::Networked(_)) {
            return;
        }
        self.notice = None;
        let list = Screen::List(List::new(self.folder.clone()));
        match std::mem::replace(&mut self.screen, list) {
            Screen::Linked(pair) => {
                let session = pair.split(&self.settings);
                ctx.send_viewport_cmd(ViewportCommand::Title(title(Some(&session))));
                self.resize_for(ctx, false);
                self.screen = Screen::Playing(Box::new(session));
            }
            Screen::Networked(wired) => {
                // No resizing: a networked pair never took a second screen's
                // worth of window in the first place.
                let session = wired.split();
                ctx.send_viewport_cmd(ViewportCommand::Title(title(Some(&session))));
                self.screen = Screen::Playing(Box::new(session));
            }
            other => self.screen = other,
        }
    }

    /// Grows the window for a second screen, or gives back the width when the
    /// cable comes out.
    ///
    /// A maximised window is left alone: it is already as big as it gets, and
    /// unmaximising it to fit a number would be answering a question nobody
    /// asked.
    fn resize_for(&self, ctx: &egui::Context, linked: bool) {
        let scale = self.settings.scale;
        if scale == 0 {
            return;
        }
        // A linked pair is two Game Boys, so its size is the older machine's
        // twice over. Otherwise the window is sized to whatever is playing —
        // and to the older machine when nothing is.
        let size = if linked {
            linked_window_size(scale as f32)
        } else {
            let screen = match &self.screen {
                Screen::Playing(session) => session.console().screen_size(),
                Screen::Networked(wired) => wired.console.console().screen_size(),
                _ => (SCREEN_WIDTH, SCREEN_HEIGHT),
            };
            window_size_for(screen, scale as f32)
        };
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
    }

    /// Takes on what the menu just changed.
    fn apply(&mut self, settings: &Settings, ctx: &egui::Context) {
        if settings.scale != self.settings.scale {
            match settings.scale {
                0 => ctx.send_viewport_cmd(ViewportCommand::Maximized(true)),
                factor => {
                    // Unmaximised first, or the size asked for would be ignored.
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(window_size(factor as f32)));
                }
            }
        }
        self.settings = settings.clone();
        match &mut self.screen {
            Screen::Playing(session) => session.apply(&self.settings),
            Screen::Linked(pair) => pair.apply(&self.settings),
            Screen::Networked(wired) => wired.console.apply(&self.settings),
            Screen::List(_) => {}
        }
    }

    /// Opens the folder chooser at whichever folder the list is on.
    fn open_dialog(&mut self) {
        self.dialog = Some(Box::new(Browser::at(self.folder.clone())));
    }

    /// Runs the folder chooser. Returns the folder if one was accepted.
    fn folder_dialog(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        let browser = self.dialog.as_mut()?;
        let response = egui::Modal::new(egui::Id::new("folder")).show(ctx, |ui| browser.ui(ui));

        // `should_close` covers the backdrop and Escape, and **consumes** the
        // key so it does not also reach the game underneath.
        let pick = if response.should_close() { Pick::Cancelled } else { response.inner };
        match pick {
            Pick::Browsing => None,
            Pick::Cancelled => {
                self.dialog = None;
                None
            }
            Pick::Chosen(dir) => {
                self.dialog = None;
                Some(dir)
            }
        }
    }

    /// Points the list at another folder and remembers it for the next session.
    ///
    /// Remembering it here, and not only when a game is opened, is the whole
    /// point of the chooser: a folder guessed wrong left no way out, because
    /// with an empty list there was no game to open and nothing to learn from.
    fn change_folder(&mut self, dir: PathBuf) {
        crate::remember_folder(&dir);
        self.folder = dir.clone();
        // In the middle of a game the list is only noted down, not rebuilt.
        // Swapping the screen here would drop the session —and with it whatever
        // the autosave had not written yet— and nobody asked to stop playing:
        // the new folder is what `Escape` will land on.
        if matches!(self.screen, Screen::List(_)) {
            self.screen = Screen::List(List::new(dir));
        }
    }

    /// Saves the game and goes back to the list.
    fn back_to_list(&mut self, ctx: &egui::Context, warning: Option<String>) {
        match &mut self.screen {
            Screen::Playing(session) => session.close(),
            Screen::Linked(pair) => pair.close(),
            Screen::Networked(wired) => wired.console.close(),
            Screen::List(_) => {}
        }
        // A connection half made has nobody left to hand a console to.
        self.connecting = None;
        self.notice = None;
        let mut list = List::new(self.folder.clone());
        list.warning = warning;
        self.screen = Screen::List(list);
        ctx.send_viewport_cmd(ViewportCommand::Title(title(None)));
    }
}

impl eframe::App for App {
    /// Background of the gap left around the screen when it is not an exact
    /// multiple: black, like a console's bezel.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::BLACK.to_normalized_gamma_f32()
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.collect_connection(ctx);
        // Before anything returns early: the dialog is asking which button was
        // just pressed, and it can be open over the list as well as over a
        // game.
        self.pads.poll();
        // A connection being made has to be looked at again soon, and nothing on
        // the list screen would otherwise ask for a repaint.
        if self.connecting.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let dialog = self.dialog.is_some() || self.address.is_some() || self.controls.is_some();

        if matches!(self.screen, Screen::List(_)) {
            return;
        }

        // With the folder chooser open the game is paused. It has to happen
        // before anything else is read: `logic` runs before `ui`, so without
        // this the Escape that closes the dialog would first be taken here as
        // "back to the list", and the path being typed would drive the joypad.
        // The controller dialog counts for the same reason twice over: the
        // buttons being pressed at it are being pressed *at it*, and a game
        // left running underneath would take every one of them as well.
        if dialog {
            match &mut self.screen {
                Screen::Playing(session) => session.pause(),
                Screen::Linked(pair) => pair.pause(),
                // Not paused: the other machine is still running, and a console
                // that stops while a dialog is open is one that stops answering
                // its partner. Pausing here would look to them like a link that
                // died because somebody opened a menu.
                Screen::Networked(_) => {}
                Screen::List(_) => {}
            }
            return;
        }

        // Escape leaves the game for the list instead of closing the program:
        // with a real window, closing already has its own button.
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.back_to_list(ctx, None);
            return;
        }

        // A cable that ended while the game did not. Noted here and acted on
        // below, once the borrow on the screen is over.
        let mut cable_ended = None;

        // The keyboard and the controller are read here and not inside the
        // session because the session no longer knows what either of them is:
        // it is handed ten buttons and told which are down. Right before
        // emulating, so that they are set when the game reads the joypad.
        let pressed = held(ctx, &self.pads);
        let (result, frames) = match &mut self.screen {
            Screen::Playing(session) => {
                for (button, down) in pressed {
                    session.press(button, down);
                }
                if ctx.input(|i| i.key_pressed(Key::D)) {
                    session.capture();
                }
                (session.advance(ctx, &mut self.textures[0]), session.frames)
            }
            Screen::Linked(pair) => {
                if ctx.input(|i| i.key_pressed(Key::Tab)) {
                    pair.swap_keyboard(&self.settings);
                }
                // egui reads Tab as "move to the next widget" before this runs,
                // and it cannot be talked out of it: by the time `logic` is
                // called the direction is already set. Left alone, one Tab would
                // land the focus on a menu button and the next Enter —which is
                // the console's Start— would open that menu instead of pressing
                // Start. Nothing on this screen has any business holding the
                // keyboard, so the focus is handed back every frame.
                ctx.memory_mut(|memory| {
                    if let Some(id) = memory.focused() {
                        memory.surrender_focus(id);
                    }
                });
                // Only the console holding the keyboard is told about the keys.
                // The other one is not merely ignored, it is told nothing is
                // pressed, which is not the same thing: see `swap_keyboard`.
                // The controller goes wherever the keyboard is: there is one of
                // each and one player, and splitting them would leave whoever
                // swapped playing two consoles at once.
                for (button, down) in pressed {
                    pair.active().press(button, down);
                }
                (pair.advance(ctx, &mut self.textures), pair.frames())
            }
            Screen::Networked(wired) => {
                for (button, down) in pressed {
                    wired.console.press(button, down);
                }
                if ctx.input(|i| i.key_pressed(Key::D)) {
                    wired.console.capture();
                }
                let Wired { console, remote } = &mut **wired;
                let outcome = match console.advance_over(remote, ctx, &mut self.textures[0]) {
                    Ok(()) => Ok(()),
                    // The console itself fell over: there is no game to keep.
                    Err(Trouble::Fault(fault)) => Err(crate::describe_fault(fault)),
                    // The cable ended and the game did not. Throwing the session
                    // away here is what sent a player back to the list of ROMs
                    // because somebody else closed their window — and with it
                    // went everything the game had not saved. The cable comes
                    // out instead, and the reason is put where it can be read.
                    Err(trouble) => {
                        cable_ended = Some(trouble.to_string());
                        Ok(())
                    }
                };
                (outcome, console.frames)
            }
            Screen::List(_) => return,
        };

        if let Some(why) = cable_ended {
            self.unplug(ctx);
            self.notice = Some(why);
            return;
        }
        if let Err(failure) = result {
            self.back_to_list(ctx, Some(first_line(&failure)));
            return;
        }
        if self.limit.is_some_and(|max| frames >= max) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.menu_bar(ui, &ctx);
        if let Some(dir) = self.folder_dialog(&ctx) {
            self.change_folder(dir);
        }
        self.address_dialog(&ctx);
        self.controls_dialog(&ctx);

        let request = match &mut self.screen {
            Screen::List(list) => list.ui(ui),
            Screen::Playing(session) => {
                session.ui(ui, &self.textures[0]);
                None
            }
            Screen::Linked(pair) => {
                pair.ui(ui, &self.textures);
                None
            }
            Screen::Networked(wired) => {
                wired.console.ui(ui, &self.textures[0]);
                None
            }
        };
        match request {
            Some(Request::Play(path)) => self.play(&ctx, path),
            Some(Request::ChooseFolder) => self.open_dialog(),
            None => {}
        }
    }

    /// Closing the window has to save the game just like leaving for the menu.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        match &mut self.screen {
            Screen::Playing(session) => session.close(),
            Screen::Linked(pair) => pair.close(),
            Screen::Networked(wired) => wired.console.close(),
            Screen::List(_) => {}
        }
    }
}

/// The core's error messages carry hints on the following lines, useful in a
/// terminal but too much for a single-line label.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or_default().to_owned()
}

/// Keeps the last `max` characters, warning with an ellipsis.
///
/// The clipping egui does on its own eats the end, and in a path the end is
/// exactly what identifies it: the beginning is the same `/home/whoever/` in all
/// of them.
fn tail(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_owned();
    }
    std::iter::once('…').chain(text.chars().skip(length + 1 - max)).collect()
}

/// "N games", or "no games" when there are none.
fn games(how_many: usize) -> String {
    match how_many {
        0 => "no games".to_owned(),
        1 => "1 game".to_owned(),
        n => format!("{n} games"),
    }
}

/// The button that carries the screen forward, in the accent colour.
fn accept(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(ACCENT))
}

/// Paints a row's background and returns the colours its text takes.
///
/// The list and the folder browser share it so that a row means the same thing
/// on both screens: the accent bar is where the keyboard is, the grey one is
/// where the mouse is.
fn row_frame(
    ui: &egui::Ui,
    rect: egui::Rect,
    response: &egui::Response,
    selected: bool,
) -> (Color32, Color32) {
    // The painted strip shrinks by a couple of pixels so that two consecutive
    // rows do not end up glued together; what can be clicked is still the whole
    // row.
    let strip = rect.shrink2(Vec2::new(0.0, 2.0));
    if selected {
        ui.painter().rect_filled(strip, 6.0, ACCENT);
    } else if response.hovered() {
        ui.painter().rect_filled(strip, 6.0, Color32::from_rgb(0x25, 0x25, 0x2A));
    }

    if selected {
        (Color32::WHITE, Color32::from_rgb(0xF0, 0xC0, 0xC6))
    } else {
        (Color32::from_gray(0xE0), Color32::GRAY)
    }
}

/// A row's inner area, laid out from the right so the badges sit at that edge.
fn row_contents(ui: &mut egui::Ui, rect: egui::Rect) -> egui::Ui {
    ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(12.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    )
}

// ---- The ROM list -----------------------------------------------------------

/// What the list is asking [`App`] to do.
enum Request {
    Play(PathBuf),
    ChooseFolder,
}

struct List {
    dir: PathBuf,
    entries: Vec<roms::Entry>,
    filter: String,
    /// Which machines' games to show. All three at once is the resting state
    /// and the one a collection is normally looked at through — the toggles are
    /// for the moment somebody wants only one of them, not a setting to be
    /// configured before the list is usable.
    showing: [bool; roms::Kind::ALL.len()],
    /// Index **within what is visible**, not within `entries`.
    selection: usize,
    /// Ask for the chosen row to be brought into the visible part of the scroll.
    follow: bool,
    warning: Option<String>,
}

impl List {
    fn new(dir: PathBuf) -> Self {
        let entries = roms::scan(&dir);
        Self {
            dir,
            entries,
            filter: String::new(),
            showing: [true; roms::Kind::ALL.len()],
            selection: 0,
            follow: false,
            warning: None,
        }
    }

    /// Indices of `entries` that pass the typed filter and the machine
    /// toggles.
    fn visible(&self) -> Vec<usize> {
        let wanted = self.filter.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.shows(e.kind))
            .filter(|(_, e)| wanted.is_empty() || e.name.to_lowercase().contains(&wanted))
            .map(|(i, _)| i)
            .collect()
    }

    fn shows(&self, kind: roms::Kind) -> bool {
        roms::Kind::ALL.iter().position(|k| *k == kind).is_some_and(|at| self.showing[at])
    }

    /// How many games of each machine the folder holds.
    ///
    /// Counted over everything and not over what is visible, so the number on a
    /// switched-off button says how many would come back — a zero there is the
    /// answer to "where are my Advance games", and a count that fell to zero
    /// because the button is off would not be.
    fn totals(&self) -> [usize; roms::Kind::ALL.len()] {
        let mut totals = [0; roms::Kind::ALL.len()];
        for entry in &self.entries {
            if let Some(at) = roms::Kind::ALL.iter().position(|k| *k == entry.kind) {
                totals[at] += 1;
            }
        }
        totals
    }

    /// Draws the whole screen. Returns what it is asking for, if anything.
    fn ui(&mut self, ui: &mut egui::Ui) -> Option<Request> {
        let visible = self.visible();
        self.selection = self.selection.min(visible.len().saturating_sub(1));

        let mut change_folder = self.header(ui);
        self.footer(ui, visible.len());
        self.keyboard(ui, visible.len());

        let chosen = egui::CentralPanel::default()
            .show(ui, |ui| {
                if visible.is_empty() {
                    change_folder |= self.empty(ui);
                    return None;
                }
                self.rows(ui, &visible)
            })
            .inner;

        if change_folder {
            return Some(Request::ChooseFolder);
        }
        chosen.map(Request::Play)
    }

    /// Returns whether the folder is to be changed.
    fn header(&mut self, ui: &mut egui::Ui) -> bool {
        let mut change_folder = false;
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Akebia").size(20.0).strong().color(ACCENT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // The path is the button that changes it. A label beside a
                    // separate button would say the same thing twice, and where
                    // the list is looking is exactly what one wants to click on
                    // when it is looking in the wrong place.
                    let path = tail(&roms::shorten(&self.dir), MAX_PATH);
                    let response = ui.button(RichText::new(path).color(Color32::GRAY));
                    change_folder = response.on_hover_text("Choose another folder").clicked();
                });
            });
            ui.add_space(8.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Search")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);
            self.machines(ui);
            ui.add_space(10.0);
        });
        change_folder
    }

    /// The three machine toggles, each with how many games of its kind the
    /// folder holds.
    ///
    /// The counts are half the point. A column of names sorted alphabetically
    /// says nothing about what is in the folder, and "GBA 6" beside the button
    /// answers the question somebody is usually asking when they reach for a
    /// filter at all.
    ///
    /// A machine the folder has none of is shown greyed rather than hidden: a
    /// row of buttons that changes shape from folder to folder is a row nobody
    /// can learn, and "GBA 0" is a useful thing to be told.
    fn machines(&mut self, ui: &mut egui::Ui) {
        let totals = self.totals();
        ui.horizontal(|ui| {
            for (at, kind) in roms::Kind::ALL.into_iter().enumerate() {
                let label = format!("{}  {}", kind.label(), totals[at]);
                let toggle = ui.add_enabled_ui(totals[at] > 0, |ui| {
                    ui.toggle_value(&mut self.showing[at], label)
                });
                toggle.inner.on_hover_text(kind.machine());
            }
            // Turning the last one off would leave an empty list explained by
            // nothing on screen, so the last one on cannot be turned off. It is
            // not a rule to be learnt: with one showing, that button simply
            // does not respond, and every other one does.
            if self.showing.iter().filter(|on| **on).count() == 0 {
                self.showing = [true; roms::Kind::ALL.len()];
            }
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, how_many: usize) {
        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(8.0);
            match &self.warning {
                Some(warning) => {
                    ui.add(Label::new(RichText::new(warning).color(ACCENT)).truncate());
                }
                None => {
                    ui.horizontal(|ui| {
                        // No "↑↓" arrows: the font egui ships does not have
                        // those glyphs and they come out as empty boxes.
                        ui.label(
                            RichText::new(
                                "Choose: arrows or mouse  ·  Play: Enter or double click",
                            )
                            .small()
                            .color(Color32::GRAY),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(games(how_many)).small().color(Color32::GRAY));
                        });
                    });
                }
            }
            ui.add_space(8.0);
        });
    }

    /// Arrows, Home and End. They are read even when the cursor is in the search
    /// box: they are keys a single-line field does not use, and that way one can
    /// type and choose without letting go of the keyboard.
    fn keyboard(&mut self, ui: &mut egui::Ui, how_many: usize) {
        if how_many == 0 {
            return;
        }
        let last = how_many - 1;
        let move_to = |target: usize, follow: &mut bool, selection: &mut usize| {
            *selection = target.min(last);
            *follow = true;
        };
        ui.input(|i| {
            if i.key_pressed(Key::ArrowDown) {
                move_to(self.selection + 1, &mut self.follow, &mut self.selection);
            }
            if i.key_pressed(Key::ArrowUp) {
                move_to(self.selection.saturating_sub(1), &mut self.follow, &mut self.selection);
            }
            if i.key_pressed(Key::Home) {
                move_to(0, &mut self.follow, &mut self.selection);
            }
            if i.key_pressed(Key::End) {
                move_to(last, &mut self.follow, &mut self.selection);
            }
        });
    }

    /// Returns whether the folder is to be changed.
    fn empty(&self, ui: &mut egui::Ui) -> bool {
        let mut change_folder = false;
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            if self.entries.is_empty() {
                ui.label(RichText::new("There are no ROMs in this folder").size(15.0));
                ui.add_space(8.0);
                let path = tail(&roms::shorten(&self.dir), MAX_PATH);
                ui.label(RichText::new(path).small().color(Color32::GRAY));
                ui.add_space(16.0);
                // The way out goes here and not in a hint about `--roms`:
                // whoever opened Akebia from the desktop launcher has no
                // command line to add a flag to, and this screen is precisely
                // the one they are looking at when the guess went wrong.
                change_folder = accept(ui, "Choose the folder with the games").clicked();
            } else if self.showing.iter().all(|on| *on) {
                ui.label(RichText::new("No game matches the search").color(Color32::GRAY));
            } else {
                // Saying which one is off matters: the search box is visible
                // and its contents obvious, and a button pressed some minutes
                // ago is neither.
                ui.label(
                    RichText::new("No game matches the search on the machines shown")
                        .color(Color32::GRAY),
                );
            }
        });
        change_folder
    }

    /// The scrollable list. Returns the ROM chosen with Enter or a double click.
    fn rows(&mut self, ui: &mut egui::Ui, visible: &[usize]) -> Option<PathBuf> {
        let mut chosen = None;
        let play_pressed = ui.input(|i| i.key_pressed(Key::Enter));

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // The rows touch each other. With the normal spacing there would be
            // six pixels between every two belonging to neither, and a click
            // there does nothing: the list would feel broken without being so.
            // The air separating one row from the next comes from its own
            // height.
            ui.spacing_mut().item_spacing.y = 0.0;

            for (row, &index) in visible.iter().enumerate() {
                let selected = row == self.selection;
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), ROW_HEIGHT),
                    Sense::click(),
                );

                if ui.is_rect_visible(rect) {
                    self.row(ui, rect, &response, index, selected);
                }
                if selected && std::mem::take(&mut self.follow) {
                    response.scroll_to_me(Some(Align::Center));
                }
                if response.clicked() {
                    self.selection = row;
                }
                if response.double_clicked() {
                    chosen = Some(self.entries[index].path.clone());
                }
            }
        });

        if play_pressed {
            if let Some(&index) = visible.get(self.selection) {
                chosen = Some(self.entries[index].path.clone());
            }
        }
        chosen
    }

    fn row(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        index: usize,
        selected: bool,
    ) {
        let entry = &self.entries[index];
        let (name, muted) = row_frame(ui, rect, response, selected);

        let mut child = row_contents(ui, rect);
        child.label(RichText::new(&entry.mapper).small().color(muted));
        if entry.cgb != CgbSupport::None {
            child
                .label(RichText::new("CGB").small().color(Color32::WHITE).background_color(VIOLET));
        }
        child.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add(Label::new(RichText::new(&entry.name).size(15.0).color(name)).truncate());
        });
    }
}

// ---- Choosing the folder ----------------------------------------------------

/// What the folder browser came back with.
enum Pick {
    /// Still open.
    Browsing,
    /// Closed without changing anything.
    Cancelled,
    Chosen(PathBuf),
}

/// How wide the dialog is drawn, and how much of it the folders get.
const DIALOG_WIDTH: f32 = 460.0;
const DIALOG_ROWS: f32 = 300.0;

/// Browser for the folder the list reads, shown as a dialog by
/// [`App::folder_dialog`].
///
/// It is Akebia's own and not the system's file dialog on purpose. `rfd`, the
/// usual crate for that, wants GTK on Linux to compile, or the desktop portal
/// and an async runtime to work; either of them undoes what `eframe` was chosen
/// for, which is a binary that runs wherever it is copied. Walking a directory
/// is `read_dir`, and that already lives in [`crate::roms`].
struct Browser {
    dir: PathBuf,
    /// The path box. It matches `dir` until somebody edits it, and it is the way
    /// into a hidden folder or a mounted drive that the rows below never offer.
    typed: String,
    folders: Vec<roms::Folder>,
    /// ROMs in `dir` itself: what the accept button promises.
    here: usize,
    /// Highlighted row, and **`None` until an arrow is pressed**. A row lit up
    /// on arrival would read as the answer to "which folder is it going to
    /// use?", and that answer is the path box, not a row.
    selection: Option<usize>,
    follow: bool,
    warning: Option<String>,
}

impl Browser {
    fn at(dir: PathBuf) -> Self {
        Self {
            typed: roms::shorten(&dir),
            folders: roms::subfolders(&dir),
            here: roms::count(&dir),
            dir,
            selection: None,
            follow: false,
            warning: None,
        }
    }

    /// Moves to another folder, keeping the box, the rows and the count in step.
    fn go(&mut self, dir: PathBuf) {
        *self = Self::at(dir);
    }

    fn parent(&self) -> Option<PathBuf> {
        self.dir.parent().map(Path::to_owned)
    }

    /// Draws the dialog and says what came of it.
    ///
    /// It lays itself out top to bottom instead of using panels: inside a modal
    /// there is no window to hang a top or a bottom panel off, and at this size
    /// there is nothing to gain from one.
    fn ui(&mut self, ui: &mut egui::Ui) -> Pick {
        let mut pick = Pick::Browsing;
        ui.set_width(DIALOG_WIDTH);

        let typing = self.header(ui);
        self.keyboard(ui, typing);

        ui.add_space(8.0);
        if let Some(dir) = self.rows(ui) {
            self.go(dir);
        }
        ui.add_space(8.0);
        self.buttons(ui, &mut pick);
        pick
    }

    /// Title, the way up and the path box.
    ///
    /// Returns whether the box is holding the keyboard: while it is, `Enter` and
    /// `Backspace` belong to the text being typed and not to the rows below.
    fn header(&mut self, ui: &mut egui::Ui) -> bool {
        let mut go_to = None;

        ui.horizontal(|ui| {
            ui.label(RichText::new("ROM folder").size(16.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let parent = self.parent();
                if ui.add_enabled(parent.is_some(), egui::Button::new("Up")).clicked() {
                    go_to = parent;
                }
            });
        });
        ui.add_space(8.0);

        let box_ = ui.add(
            egui::TextEdit::singleline(&mut self.typed)
                .hint_text("Path to a folder")
                .desired_width(f32::INFINITY),
        );
        // `lost_focus` counts as typing too: it is true on the very frame
        // `Enter` is pressed inside the box, and without it that same key would
        // also be read below as "go into the highlighted folder".
        let typing = box_.has_focus() || box_.lost_focus();

        if box_.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let wanted = roms::expand_user(&self.typed);
            if wanted.is_dir() {
                go_to = Some(wanted);
            } else {
                // Nothing moves and the box keeps what was typed: jumping to an
                // empty folder would hide the typo instead of showing it.
                self.warning = Some(format!("There is no folder at {}", wanted.display()));
            }
        }

        if let Some(dir) = go_to {
            self.go(dir);
        }
        typing
    }

    fn buttons(&mut self, ui: &mut egui::Ui, pick: &mut Pick) {
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                *pick = Pick::Cancelled;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // The count goes on the button itself: it answers "is this the
                // right folder?" without having to accept it and come back if it
                // was not.
                let label = format!("Use this folder · {}", games(self.here));
                if accept(ui, &label).clicked() {
                    *pick = Pick::Chosen(self.dir.clone());
                }
            });
        });
    }

    /// Arrows, `Home`, `End`, `Backspace` and `Enter`. Escape is not here:
    /// the modal consumes it to close itself.
    fn keyboard(&mut self, ui: &mut egui::Ui, typing: bool) {
        let mut go_to = None;
        ui.input(|i| {
            if let Some(last) = self.folders.len().checked_sub(1) {
                // The first press lights the first row up instead of moving off
                // it: with nothing highlighted there is nowhere to move from.
                let target = if i.key_pressed(Key::ArrowDown) {
                    Some(self.selection.map_or(0, |row| row + 1))
                } else if i.key_pressed(Key::ArrowUp) {
                    Some(self.selection.map_or(0, |row| row.saturating_sub(1)))
                } else if i.key_pressed(Key::Home) {
                    Some(0)
                } else if i.key_pressed(Key::End) {
                    Some(last)
                } else {
                    None
                };
                if let Some(target) = target {
                    self.selection = Some(target.min(last));
                    self.follow = true;
                }
            }
            // The arrows are read even with the cursor in the box, as in the
            // list: a single-line field has no use for up and down. The rest of
            // the keys do have one, so they stop here.
            if typing {
                return;
            }
            if i.key_pressed(Key::Backspace) {
                go_to = self.parent();
            }
            if i.key_pressed(Key::Enter) {
                go_to = self
                    .selection
                    .and_then(|row| self.folders.get(row))
                    .map(|folder| folder.path.clone());
            }
        });
        if let Some(dir) = go_to {
            self.go(dir);
        }
    }

    /// The subfolders. Returns the one to move into, if any.
    fn rows(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        if let Some(warning) = &self.warning {
            ui.add(Label::new(RichText::new(warning).color(ACCENT)).truncate());
            ui.add_space(4.0);
        }
        let mut entered = None;
        // A height of its own, not one that follows the contents: a folder with
        // two subfolders and one with two hundred have to give the same dialog,
        // or walking through them would make it jump about under the pointer.
        // That is also why the "nothing here" notice goes inside and not
        // instead.
        egui::ScrollArea::vertical().max_height(DIALOG_ROWS).auto_shrink([false, false]).show(
            ui,
            |ui| {
                if self.folders.is_empty() {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| {
                        let message = if self.dir.is_dir() {
                            "There are no more folders inside this one"
                        } else {
                            "This folder cannot be read"
                        };
                        ui.label(RichText::new(message).color(Color32::GRAY));
                    });
                    return;
                }
                ui.spacing_mut().item_spacing.y = 0.0;

                for (row, folder) in self.folders.iter().enumerate() {
                    let selected = self.selection == Some(row);
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), ROW_HEIGHT),
                        Sense::click(),
                    );

                    if ui.is_rect_visible(rect) {
                        let (name, muted) = row_frame(ui, rect, &response, selected);
                        let mut child = row_contents(ui, rect);
                        // Only the folders holding something say so, and in the
                        // accent colour: on a home folder that one line of red is
                        // the answer to the whole question this screen asks.
                        if folder.roms > 0 {
                            let count = if selected { muted } else { ACCENT };
                            child.label(RichText::new(games(folder.roms)).small().color(count));
                        }
                        child.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(
                                Label::new(RichText::new(&folder.name).size(15.0).color(name))
                                    .truncate(),
                            );
                        });
                    }
                    if selected && std::mem::take(&mut self.follow) {
                        response.scroll_to_me(Some(Align::Center));
                    }
                    // A single click, not a double one: there is nothing else to do
                    // with a subfolder here, and the list's double click is reserved
                    // for starting a game, which is not undone by pressing Escape.
                    if response.clicked() {
                        entered = Some(folder.path.clone());
                    }
                }
            },
        );
        entered
    }
}

// ---- The play session -------------------------------------------------------

/// Everything that lasts as long as a play session does.
pub struct Session {
    /// Which machine this is. Everything below that is the older machine's
    /// alone — the cable, the saved game, the sound — asks this for a Game Boy
    /// and does nothing when there is not one.
    console: Console,
    pub title: String,
    /// The file this console was loaded from. Kept because the saved game's name
    /// is derived from it, and a linked pair may have to derive a second one.
    rom: PathBuf,
    save: Option<save::SaveFile>,
    audio: Option<audio::AudioOutput>,
    /// The last frame, already converted to the colour the texture wants.
    pixels: Vec<Color32>,
    /// When the next frame is due to be emulated.
    next: Instant,
    /// How fast this is actually going. Measured always and shown only when
    /// asked: the cost is two clock readings a pass, and a meter that only ran
    /// while it was on screen would have nothing to say for the first half
    /// second after being switched on — which is exactly the half second
    /// somebody switching it on wants to see.
    rate: Rate,
    /// When the last pass through the loop was, for the meter to measure
    /// against.
    last_pass: Instant,
    frames: u64,
    trace: Option<debug::LiveTrace>,
    captures: u32,
    serial: bool,
    /// Silenced whatever the sound setting says, because something else has the
    /// speakers.
    ///
    /// Only a linked pair sets it. Two consoles running the same game a few
    /// frames apart do not sound twice as loud: they sound like one console
    /// through a flanger, because every note arrives twice a few milliseconds
    /// apart. It is worse than either of them alone, so only the console holding
    /// the keyboard is heard.
    muted: bool,
}

impl Session {
    pub fn new(mut console: Console, rom: PathBuf, args: &Args, settings: &Settings) -> Self {
        if let Some(gb) = console.gameboy_mut() {
            gb.set_trace_enabled(args.debug);
            gb.set_write_log_enabled(args.debug);
        }

        // Both machines keep a saved game, in memories that have nothing in
        // common; what they agree on is a file, which is all this needs. See
        // [`save::Battery`].
        let save = (!args.no_save)
            .then(|| {
                let path = args.save.clone().unwrap_or_else(|| save::default_path(&rom));
                save::SaveFile::open(&mut console, path)
            })
            .flatten();
        let title = console.title();
        let (width, height) = console.screen_size();

        let mut session = Self {
            console,
            title,
            rom,
            save,
            audio: None,
            pixels: vec![Color32::BLACK; width * height],
            next: Instant::now(),
            rate: Rate::default(),
            last_pass: Instant::now(),
            frames: 0,
            trace: args.debug.then(debug::LiveTrace::new),
            captures: 0,
            serial: args.serial,
            muted: false,
        };
        // The settings and not the arguments: a game started after the menu was
        // touched has to come up the way the menu left things.
        session.apply(settings);
        session
    }

    /// What is wrong with this session that the player should hear about
    /// before wondering, if anything.
    ///
    /// Only one thing so far: an Advance with no BIOS, which draws nothing and
    /// gives no other sign of why. It goes on the menu bar rather than into a
    /// dialog because it is a fact about the session and not a question — the
    /// game does start, and stopping it to be told so every time would be worse
    /// than the black screen it explains.
    pub fn warning(&self) -> Option<String> {
        self.console.missing_bios().then(|| crate::bios::ADVICE.to_string())
    }

    /// A second console in the state this one is in, down to the frame.
    ///
    /// This is what "second console" means here, and it is not a shortcut. A
    /// trade happens at the Cable Club, hours into a game; starting the second
    /// console at the title screen would mean playing those hours again before
    /// there were two consoles able to talk, and doing it with one keyboard
    /// shared between them. Copying the console that is already there gives two
    /// players standing in the same room, which is where the interesting part
    /// starts.
    ///
    /// The copy is complete —CPU, RAM, video, cartridge and its SRAM— so both
    /// games are the same save, with the same team in it. What it does not copy
    /// is where that save is written: see [`save::linked_path`].
    fn duplicate(&self, args: &Args, settings: &Settings) -> Self {
        // Only a Game Boy gets here: the cable is the older machine's alone and
        // the menu entry that leads here is greyed out for anything else.
        let gb = self.gb().expect("only a Game Boy has a link cable").clone();
        let save = self.save.as_ref().and_then(|_| {
            let path = save::linked_path(&self.rom);
            eprintln!("the second console saves to {}", path.display());
            save::SaveFile::copied(&gb, path)
        });

        let mut copy = Self {
            console: Console::Gb(Box::new(gb)),
            title: self.title.clone(),
            rom: self.rom.clone(),
            save,
            audio: None,
            pixels: self.pixels.clone(),
            next: Instant::now(),
            rate: Rate::default(),
            last_pass: Instant::now(),
            frames: self.frames,
            trace: args.debug.then(debug::LiveTrace::new),
            captures: 0,
            serial: args.serial,
            // Silent from the start rather than opened and closed a moment later
            // by the pair: the console being copied is the one already playing,
            // and it is the one that keeps the speakers.
            muted: true,
        };
        copy.apply(settings);
        copy
    }

    /// Silences this console, or gives it the speakers back.
    ///
    /// The stream is genuinely closed and reopened rather than fed silence: an
    /// open output device that nobody is listening to still costs a callback
    /// every few milliseconds.
    pub fn set_muted(&mut self, muted: bool, settings: &Settings) {
        if self.muted != muted {
            self.muted = muted;
            self.apply(settings);
        }
    }

    /// Pushes the menu's settings into the running console.
    pub fn apply(&mut self, settings: &Settings) {
        let sound = settings.sound && !self.muted;
        match (sound, self.audio.is_some()) {
            // Dropping the output stops the stream, and from then on the frame's
            // samples are discarded instead of piling up.
            (false, true) => self.audio = None,
            (true, false) => {
                self.audio = crate::open_audio(true);
                if let Some(output) = &self.audio {
                    self.console.set_sample_rate(output.sample_rate());
                }
            }
            _ => {}
        }

        // The rest of the settings are the older machine's alone: the shades
        // are a DMG palette and the filter stands in for its speaker. The
        // Advance has neither, and takes none of this rather than being given a
        // stand-in for something it has not got.
        let (filter, shades) = (settings.speaker_filter, settings.palette().shades);
        if let Some(gb) = self.console.gameboy_mut() {
            gb.set_dmg_shades(shades);
            gb.set_speaker_filter(filter);
        }
    }

    /// The console itself, for whoever has to ask it the time or step it aside.
    pub fn console_mut(&mut self) -> &mut Console {
        &mut self.console
    }

    pub fn console(&self) -> &Console {
        &self.console
    }

    /// The Game Boy inside, if this is one.
    ///
    /// Everything that reaches through this is the older machine's alone: the
    /// link cable, the mapper's saved game, the sound, the debug captures. None
    /// of it exists on the Advance yet, and where it does not, the answer is
    /// nothing rather than a stand-in.
    fn gb(&self) -> Option<&GameBoy> {
        self.console.gameboy()
    }

    fn gb_mut(&mut self) -> Option<&mut GameBoy> {
        self.console.gameboy_mut()
    }

    /// Emulates whatever is due, with the far end of a cable saying how far.
    ///
    /// The wall clock still decides *when* a frame is owed —a link that answers
    /// instantly must not run the game at a thousand frames a second— and the
    /// link decides whether it can be delivered.
    ///
    /// The trouble comes back whole rather than as a sentence, because the two
    /// kinds are not the same news: a console that fell over has no game left to
    /// go back to, and a cable that ended has the same game it had a moment ago.
    /// Only the caller knows what to do about each.
    pub fn advance_over(
        &mut self,
        remote: &mut Remote,
        ctx: &egui::Context,
        texture: &mut TextureHandle,
    ) -> Result<(), Trouble> {
        let period = self.frame_period();
        let now = Instant::now();
        let mut emulated = 0;
        let mut stalled = false;
        let mut failure = None;

        let started = Instant::now();
        while self.next <= now && emulated < MAX_CATCH_UP {
            let Some(gb) = self.console.gameboy_mut() else {
                break;
            };
            match remote.run_frame(gb, &mut FrameSink { pixels: &mut self.pixels }) {
                Ok(true) => {
                    self.after_frame();
                    self.next += period;
                    emulated += 1;
                }
                // The other console has not got this far yet. The clock is put
                // back rather than left owing: a wait is not a debt, and running
                // it off at double speed afterwards would be a worse answer than
                // having waited.
                Ok(false) => {
                    self.next = Instant::now();
                    stalled = true;
                    break;
                }
                Err(trouble) => {
                    failure = Some(trouble);
                    break;
                }
            }
        }
        self.measure(emulated, started.elapsed());
        if emulated == MAX_CATCH_UP {
            self.next = Instant::now();
        }
        if emulated > 0 {
            texture.set(
                ColorImage::new(self.screen_size(), self.pixels.clone()),
                TextureOptions::NEAREST,
            );
        }
        // Stalled, come straight back: `run_frame` does its waiting on the wire
        // rather than spinning, so asking again at once is what turns a link
        // answering in a millisecond into a millisecond of waiting instead of a
        // frame of it.
        ctx.request_repaint_after(if stalled {
            Duration::ZERO
        } else {
            self.next.saturating_duration_since(Instant::now())
        });

        match failure {
            Some(f) => Err(f),
            None => Ok(()),
        }
    }

    /// Stops the clock while a dialog is up, so that closing it does not leave
    /// the session owing every frame the user spent reading.
    pub fn pause(&mut self) {
        self.next = Instant::now();
        // And the meter forgets the pause with it, or a dialog left open would
        // be reported as a machine that had slowed to nothing.
        self.last_pass = Instant::now();
        self.rate.interrupted();
    }

    /// Notes a pass of the loop for the meter.
    fn measure(&mut self, emulated: u32, spent: Duration) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_pass);
        self.last_pass = now;
        self.rate.pass(elapsed, emulated, spent);
    }

    /// How fast this session is going, once there has been long enough to say.
    pub fn rate(&self) -> Option<rate::Reading> {
        self.rate.reading()
    }

    /// Presses or releases a joypad button.
    ///
    /// It is the caller that says which ones are down, and this is not a detail:
    /// it is the whole difference between the two frontends. On the desktop a
    /// keyboard answers, on a telephone eight painted circles do, and the
    /// console is not to know which. Whoever calls this must do so **before**
    /// [`Session::advance`], so the buttons are already set when the game reads
    /// 0xFF00 during VBlank.
    pub fn press(&mut self, button: Pad, down: bool) {
        self.console.press(button, down);
    }

    /// Emulates whatever is due and uploads the result to the texture.
    pub fn advance(
        &mut self,
        ctx: &egui::Context,
        texture: &mut TextureHandle,
    ) -> Result<(), String> {
        let period = self.frame_period();
        let now = Instant::now();
        let mut emulated = 0;
        let mut failure = None;
        // What the emulating itself costs, measured around the loop and not
        // around the whole pass: the rest of a pass is egui drawing a window,
        // which is not what anybody is asking about.
        let started = Instant::now();
        while self.next <= now && emulated < MAX_CATCH_UP {
            if let Err(f) = self.frame() {
                failure = Some(f);
                break;
            }
            self.next += period;
            emulated += 1;
        }
        self.measure(emulated, started.elapsed());
        if emulated == MAX_CATCH_UP {
            // We are running really late: the debt is dropped instead of running
            // faster over the next few seconds.
            self.next = Instant::now();
        }

        if emulated > 0 {
            texture.set(
                ColorImage::new(self.screen_size(), self.pixels.clone()),
                TextureOptions::NEAREST,
            );
        }

        // Without this the interface would only repaint when the mouse moves:
        // nothing the game draws goes through egui's event system.
        ctx.request_repaint_after(self.next.saturating_duration_since(Instant::now()));

        match failure {
            Some(f) => Err(f),
            None => Ok(()),
        }
    }

    /// One frame of whichever machine this is.
    ///
    /// The two arrive at a picture differently — the older core pushes a
    /// finished frame into a sink, the Advance draws into a buffer of its own
    /// and hands it over — so this is where the difference lives and the only
    /// place it does.
    fn frame(&mut self) -> Result<(), String> {
        let result = match &mut self.console {
            // `console` and `pixels` are distinct fields, so both borrows
            // coexist; the sink only exists during this call.
            Console::Gb(gb) => gb
                .run_frame(&mut FrameSink { pixels: &mut self.pixels })
                .map_err(crate::describe_fault),
            Console::Gba(gba) => {
                let outcome = gba.run_frame().map_err(|stopped| stopped.to_string());
                for (target, &colour) in self.pixels.iter_mut().zip(gba.frame()) {
                    *target = from_rgb555(colour);
                }
                outcome
            }
        };
        self.after_frame();
        result
    }

    /// One frame of a linked pair.
    ///
    /// There is a single call for both consoles because there is no other way to
    /// do it: with a cable in between, each one's next instruction can depend on
    /// what the other did on its last, so they cannot be advanced one after the
    /// other. [`link::run_frame`] interleaves them instruction by instruction.
    fn linked_frame(a: &mut Self, b: &mut Self) -> Result<(), String> {
        // Both sides are Game Boys: the cable is theirs and the menu offers it
        // for nothing else.
        let (Some(_), Some(_)) = (a.gb(), b.gb()) else {
            return Err("only a Game Boy has a link cable".to_string());
        };
        let [ga, gb_console] = [&mut a.console, &mut b.console];
        let (Console::Gb(ga), Console::Gb(gbb)) = (ga, gb_console) else {
            unreachable!("checked just above")
        };
        let result = link::run_frame(
            ga,
            &mut FrameSink { pixels: &mut a.pixels },
            gbb,
            &mut FrameSink { pixels: &mut b.pixels },
        );
        a.after_frame();
        b.after_frame();

        result.map_err(|LinkFault { side, fault }| {
            // Which of the two stopped matters: the other one is fine, and the
            // message is all the player has to tell them apart.
            let which = match side {
                Side::A => "left",
                Side::B => "right",
            };
            format!("{which} console: {}", crate::describe_fault(fault))
        })
    }

    /// The housekeeping an emulated frame owes, however it was emulated: audio
    /// out, serial out, autosave and trace.
    fn after_frame(&mut self) {
        self.frames += 1;
        // Both machines make sound, so this comes first and out of the console
        // rather than out of a Game Boy. It has to happen whether or not there
        // is a sound card: uncollected samples pile up for the whole session.
        crate::drain_audio(&mut self.console, self.audio.as_ref());
        // Both machines keep a saved game, so this comes out of the console too
        // and not out of a Game Boy.
        if let Some(save) = self.save.as_mut() {
            save.tick(&self.console);
        }

        // The rest is the older machine's: the serial port and the debug
        // capture. The Advance has neither here yet, and doing nothing is the
        // honest answer.
        let serial = self.serial;
        let Some(gb) = self.console.gameboy_mut() else {
            return;
        };
        crate::drain_serial(gb, serial);
        // It has to be drained every frame even if nothing is printed: otherwise
        // the core's capture would pile up 144 lines per frame without end.
        if let Some(trace) = self.trace.as_mut() {
            trace.frame(gb.take_frame_trace());
        }
    }

    /// Dumps the frame and the VRAM as images, with `--debug`.
    fn capture(&mut self) {
        if self.trace.is_none() {
            return;
        }
        self.captures += 1;
        // The captures go to the current directory, not next to the ROM.
        let Some(gb) = self.gb() else {
            eprintln!("warning: captures are a Game Boy's, and this is not one");
            return;
        };
        match debug::snapshot(gb, Path::new("."), self.captures) {
            Ok(path) => eprintln!("capture written to {}", path.display()),
            Err(e) => eprintln!("warning: {e}"),
        }
    }

    /// Writes the saved game. Called on leaving for the list and on closing the
    /// window.
    pub fn close(&mut self) {
        if let Some(save) = self.save.as_mut() {
            save.flush_final(&self.console);
        }
    }

    /// How long one of this machine's frames is owed to last.
    ///
    /// Asked of the console rather than assumed, because the two rates are not
    /// the same. Running one machine at the other's rate is a game that is
    /// slightly the wrong speed all the time — which is harder to notice, and
    /// worse, than one that is obviously broken.
    fn frame_period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.console.frames_per_second())
    }

    /// How big this machine's screen is, as `egui` wants it.
    ///
    /// Asked rather than assumed: the two machines are 160×144 and 240×160, and
    /// a texture built to the wrong one either crops the picture or pads it
    /// with whatever was in memory.
    pub fn screen_size(&self) -> [usize; 2] {
        let (width, height) = self.console.screen_size();
        [width, height]
    }

    /// The screen, centred and with integer scaling.
    pub fn ui(&self, ui: &mut egui::Ui, texture: &TextureHandle) {
        let area = ui.available_rect_before_wrap();
        self.paint(ui, texture, area);
    }

    /// Draws the screen inside `area` and returns the rectangle the picture
    /// actually landed in, which is smaller than `area` whenever the scaling
    /// left a border. A caller that wants to draw *around* the screen —a frame
    /// marking which console the keyboard is on— needs that rectangle and not
    /// the area it offered.
    fn paint(&self, ui: &mut egui::Ui, texture: &TextureHandle, area: egui::Rect) -> egui::Rect {
        ui.painter().rect_filled(area, 0.0, Color32::BLACK);

        // The factor is rounded down so that a Game Boy pixel is an exact N×N
        // square. With a fractional factor, some rows of pixels would come out
        // one thickness and others another.
        let [width, height] = self.screen_size();
        let (width, height) = (width as f32, height as f32);
        let scale = (area.width() / width)
            .min(area.height() / height)
            .floor()
            .max(1.0);
        let size = Vec2::new(width * scale, height * scale);
        let picture = egui::Rect::from_center_size(area.center(), size);

        ui.painter().image(
            texture.id(),
            picture,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        picture
    }
}

/// A console whose partner is on another machine.
///
/// It is the network's answer to [`Pair`], and it is deliberately much less:
/// there is one screen, one keyboard and one saved game, because the other
/// console belongs to somebody else and has its own. Everything that made a
/// local pair awkward —whose sound, whose `.sav`, whose turn with the keys— is
/// simply not a question here.
pub struct Wired {
    console: Session,
    remote: Remote,
}

impl Wired {
    /// Pulls the cable out and gives the console back on its own.
    fn split(mut self) -> Session {
        self.remote.close();
        if let Some(gb) = self.console.gb_mut() {
            gb.set_link_connected(false);
        }
        self.console
    }
}

// ---- Two consoles on one cable ----------------------------------------------

/// Two consoles joined by a link cable, side by side in the same window.
///
/// It is the shortest cable there is —both consoles are in this process, and a
/// byte reaches the other end before either has moved on— and it exists so that
/// a trade can be tried at all without a second machine, a network or a protocol
/// in the middle. What works here is the emulated hardware working; what breaks
/// once a socket is involved will be the transport's fault and not this. See
/// [`akebia_core::link`].
pub struct Pair {
    consoles: [Session; 2],
    /// Which of the two the keyboard is driving.
    ///
    /// One player with one keyboard has to walk both games to the counter, and
    /// swapping between them is cheaper than learning a second set of eight keys.
    focus: usize,
    /// When the next frame is due. It is the pair's and not each console's: they
    /// advance together or not at all, so there is one clock for both.
    next: Instant,
}

impl Pair {
    fn new(mut a: Session, mut b: Session, settings: &Settings) -> Self {
        if let (Some(_), Some(_)) = (a.gb(), b.gb()) {
            let [ca, cb] = [&mut a.console, &mut b.console];
            if let (Console::Gb(ga), Console::Gb(gbb)) = (ca, cb) {
                link::connect(ga, gbb);
            }
        }
        // Only one console is heard; see `Session::muted`.
        b.set_muted(true, settings);
        Self { consoles: [a, b], focus: 0, next: Instant::now() }
    }

    /// The console the keyboard is driving.
    fn active(&mut self) -> &mut Session {
        &mut self.consoles[self.focus]
    }

    fn frames(&self) -> u64 {
        self.consoles[0].frames
    }

    /// Hands the keyboard to the other console.
    ///
    /// The one losing it is first told every button is up. Skipping that would
    /// leave whatever was held down pressed forever —a direction is the usual
    /// one— and the abandoned game would spend the rest of the session walking
    /// into a wall.
    fn swap_keyboard(&mut self, settings: &Settings) {
        for button in Pad::ALL {
            self.consoles[self.focus].press(button, false);
        }
        self.focus ^= 1;
        // The sound follows the keyboard: whoever is being played is the one
        // worth hearing.
        self.mute_all_but_the_active(settings);
    }

    fn mute_all_but_the_active(&mut self, settings: &Settings) {
        let focus = self.focus;
        for (index, console) in self.consoles.iter_mut().enumerate() {
            console.set_muted(index != focus, settings);
        }
    }

    fn pause(&mut self) {
        self.next = Instant::now();
    }

    fn apply(&mut self, settings: &Settings) {
        for console in &mut self.consoles {
            console.apply(settings);
        }
        // `apply` alone would hand the speakers back to both of them: it reads
        // the sound setting, which says nothing about which console is being
        // played.
        self.mute_all_but_the_active(settings);
    }

    fn close(&mut self) {
        for console in &mut self.consoles {
            console.close();
        }
    }

    /// Pulls the cable out and gives back the console that had the keyboard,
    /// saving the other one as it goes.
    fn split(self, settings: &Settings) -> Session {
        let focus = self.focus;
        let [a, b] = self.consoles;
        let (mut kept, mut left) = if focus == 0 { (a, b) } else { (b, a) };

        left.close();
        // Without this the console kept would sit waiting for an answer from an
        // end that no longer exists the next time its game tried to transfer.
        if let Some(gb) = kept.console.gameboy_mut() {
            gb.set_link_connected(false);
        }
        // On its own again there is nobody left to share the speakers with.
        kept.set_muted(false, settings);
        kept
    }

    /// Emulates whatever both consoles are due and uploads the two screens.
    fn advance(
        &mut self,
        ctx: &egui::Context,
        textures: &mut [TextureHandle; 2],
    ) -> Result<(), String> {
        let period = Duration::from_secs_f64(1.0 / FRAMES_PER_SECOND);
        let now = Instant::now();
        let mut emulated = 0;
        let mut failure = None;

        let started = Instant::now();
        while self.next <= now && emulated < MAX_CATCH_UP {
            let [a, b] = &mut self.consoles;
            if let Err(f) = Session::linked_frame(a, b) {
                failure = Some(f);
                break;
            }
            self.next += period;
            emulated += 1;
        }
        // A pair is one loop producing two consoles' frames, so the cost of
        // both is charged to the one that is being watched.
        let spent = started.elapsed();
        self.consoles[0].measure(emulated, spent);
        if emulated == MAX_CATCH_UP {
            self.next = Instant::now();
        }

        if emulated > 0 {
            for (console, texture) in self.consoles.iter().zip(textures.iter_mut()) {
                texture.set(
                    ColorImage::new(console.screen_size(), console.pixels.clone()),
                    TextureOptions::NEAREST,
                );
            }
        }
        ctx.request_repaint_after(self.next.saturating_duration_since(Instant::now()));

        match failure {
            Some(f) => Err(f),
            None => Ok(()),
        }
    }

    /// The two screens side by side, with the one holding the keyboard framed.
    fn ui(&self, ui: &mut egui::Ui, textures: &[TextureHandle; 2]) {
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::BLACK);

        let width = ((area.width() - LINK_GUTTER) / 2.0).max(1.0);
        let halves = [
            egui::Rect::from_min_size(area.min, Vec2::new(width, area.height())),
            egui::Rect::from_min_size(
                egui::pos2(area.max.x - width, area.min.y),
                Vec2::new(width, area.height()),
            ),
        ];

        for (index, half) in halves.into_iter().enumerate() {
            let picture = self.consoles[index].paint(ui, &textures[index], half);
            if index == self.focus {
                // Where the keyboard is has to be readable at a glance: with two
                // copies of the same game on screen, pressing a key into the
                // wrong one is the mistake waiting to happen.
                ui.painter().rect_stroke(
                    picture.expand(4.0),
                    2.0,
                    egui::Stroke::new(2.0, ACCENT),
                    egui::StrokeKind::Outside,
                );
            }
        }
    }
}

/// Collects the frame the core delivers and leaves it ready for the texture.
struct FrameSink<'a> {
    pixels: &'a mut Vec<Color32>,
}

/// One 15-bit colour, as the texture wants it.
///
/// Both machines draw in the same five-bits-a-channel format — the one thing
/// they do share — so this is the whole of the difference between what a core
/// produces and what the screen takes.
fn from_rgb555(colour: u16) -> Color32 {
    let (r, g, b) = (colour & 0x1F, (colour >> 5) & 0x1F, (colour >> 10) & 0x1F);
    // Five bits to eight by repeating the top three, so that 0x1F comes out as
    // 0xFF rather than 0xF8 and white is actually white.
    let widen = |c: u16| ((c << 3) | (c >> 2)) as u8;
    Color32::from_rgb(widen(r), widen(g), widen(b))
}

impl VideoOutput for FrameSink<'_> {
    fn present(&mut self, frame: &FrameBuffer) {
        for (target, &color) in self.pixels.iter_mut().zip(frame.as_slice()) {
            let [r, g, b] = color.to_rgb888();
            *target = Color32::from_rgb(r, g, b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Advance cartridge that paints one red pixel and stops.
    ///
    /// Hand-assembled, because the point is to check that a picture drawn by
    /// the Advance core arrives in the frontend's pixels, and any ROM that
    /// draws something known will do. Mode 3 is the one where a pixel is a
    /// colour at an address, with no tiles or palettes in between.
    fn red_pixel_rom() -> Vec<u8> {
        let program: [u32; 7] = [
            0xE3A0_0301, // MOV r0, #0x04000000   - the registers
            0xE3A0_1003, // MOV r1, #3            - video mode 3
            0xE381_1B01, // ORR r1, r1, #0x400    - and background 2 switched on
            0xE580_1000, // STR r1, [r0]          - into DISPCNT
            0xE3A0_0406, // MOV r0, #0x06000000   - video memory
            0xE3A0_101F, // MOV r1, #0x1F         - red, five bits a channel
            0xE1C0_10B0, // STRH r1, [r0]         - the very first pixel
        ];
        let mut rom: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
        // `B .` — a branch to itself, so the beam sweeps on with the picture
        // standing still.
        rom.extend_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom.resize(0x200, 0);
        rom
    }

    fn advance_session() -> Session {
        let gba = akebia_gba::Gba::with_rom(&red_pixel_rom());
        let mut args = match Args::parse(["game.gba".to_owned()]).unwrap() {
            crate::args::Parsed::Run(args) => *args,
            crate::args::Parsed::Help => unreachable!("a path is not --help"),
        };
        // Opening a session reads the saved game, so it is pointed somewhere
        // that is not the working directory: a test must not depend on what
        // happens to be lying next to it, nor leave anything there.
        args.save =
            Some(std::env::temp_dir().join(format!("akebia-advance-{}.sav", std::process::id())));
        Session::new(
            Console::Gba(Box::new(gba)),
            PathBuf::from("game.gba"),
            &args,
            &Settings::from_args(&args),
        )
    }

    /// The whole point of the frontend change, in one test: an Advance session
    /// holds a screenful of the Advance's size, and what the core drew arrives
    /// in it.
    #[test]
    fn an_advance_session_draws_the_advances_picture() {
        let mut session = advance_session();
        assert_eq!(session.screen_size(), [240, 160], "the Advance's screen");
        assert_eq!(session.pixels.len(), 240 * 160, "and a buffer to match");

        // Two frames: the first is whatever was already partly swept when the
        // machine started, the second is drawn with the picture in place.
        for _ in 0..2 {
            session.frame().expect("a cartridge that paints and stops");
        }

        assert_eq!(session.pixels[0], Color32::from_rgb(255, 0, 0), "the pixel it painted");
        assert_eq!(session.pixels[1], Color32::BLACK, "and nothing beside it");
    }

    /// An Advance keeps its saved game, in whichever of three chips its
    /// cartridge carries, and in the same `.sav` the older machine uses. This
    /// test used to assert the opposite, which was the honest answer while the
    /// chips did not exist — and the reason it was worth having is that a
    /// machine that quietly keeps nothing loses a player's afternoon.
    #[test]
    fn an_advance_session_keeps_its_saved_game() {
        let session = advance_session();
        assert!(session.save.is_some());
        assert!(session.gb().is_none(), "and there is still no Game Boy in it");
    }

    /// The two machines run at their own rates. One at the other's is a game
    /// that is subtly the wrong speed the whole time.
    #[test]
    fn each_machine_is_paced_at_its_own_rate() {
        let advance = advance_session().frame_period();
        assert!(advance.as_secs_f64() > 0.0 && advance.as_secs_f64() < 0.02, "{advance:?}");
    }

    fn list(names: &[&str]) -> List {
        List {
            dir: PathBuf::from("/roms"),
            entries: names
                .iter()
                .map(|n| roms::Entry {
                    path: PathBuf::from(format!("/roms/{n}.gb")),
                    name: (*n).to_owned(),
                    mapper: "MBC1".to_owned(),
                    cgb: CgbSupport::None,
                    kind: roms::Kind::GameBoy,
                })
                .collect(),
            filter: String::new(),
            showing: [true; roms::Kind::ALL.len()],
            selection: 0,
            follow: false,
            warning: None,
        }
    }

    /// A list holding one game of each machine, for the toggles.
    fn mixed() -> List {
        let mut list = list(&["alfa", "beta", "gamma", "delta"]);
        list.entries[1].kind = roms::Kind::Color;
        list.entries[2].kind = roms::Kind::Advance;
        list.entries[3].kind = roms::Kind::Advance;
        list
    }

    #[test]
    fn the_folder_is_counted_by_machine() {
        assert_eq!(mixed().totals(), [1, 1, 2]);
    }

    /// The whole point of the toggles: one machine's games, out of a folder
    /// that holds everybody's.
    #[test]
    fn switching_a_machine_off_takes_its_games_out_of_the_list() {
        let mut list = mixed();
        assert_eq!(list.visible().len(), 4, "everything, to begin with");

        list.showing = [false, false, true];
        let names: Vec<&str> =
            list.visible().iter().map(|&i| list.entries[i].name.as_str()).collect();
        assert_eq!(names, ["gamma", "delta"], "the two Advance games and nothing else");
    }

    /// And the counts do not move when a button is switched off. A zero beside
    /// "GBA" has to mean the folder has none, or it answers a different
    /// question from the one being asked.
    #[test]
    fn the_counts_are_of_the_folder_and_not_of_what_is_shown() {
        let mut list = mixed();
        list.showing = [true, false, false];
        assert_eq!(list.totals(), [1, 1, 2]);
    }

    /// The two filters are one filter. Typing a name while a machine is off
    /// must not bring that machine's games back.
    #[test]
    fn the_search_and_the_machines_narrow_together() {
        let mut list = mixed();
        list.showing = [true, false, false];
        list.filter = "a".to_owned();
        let names: Vec<&str> =
            list.visible().iter().map(|&i| list.entries[i].name.as_str()).collect();
        assert_eq!(names, ["alfa"], "gamma and delta match the search and are not shown");
    }

    #[test]
    fn the_filter_is_case_insensitive() {
        let mut l = list(&["Aurora", "Bravo", "AURORA 2"]);
        assert_eq!(l.visible().len(), 3, "with no filter they all show");

        l.filter = "aurora".to_owned();
        let seen: Vec<&str> = l.visible().iter().map(|&i| l.entries[i].name.as_str()).collect();
        assert_eq!(seen, ["Aurora", "AURORA 2"]);

        l.filter = "  ".to_owned();
        assert_eq!(l.visible().len(), 3, "whitespace filters nothing");

        l.filter = "nothing".to_owned();
        assert!(l.visible().is_empty());
    }

    /// The browser's navigation touches no `egui`, only the filesystem, so it
    /// gets tested the same way `roms` does: without opening a window.
    #[test]
    fn the_browser_walks_folders_and_counts_what_is_in_them() {
        let dir = std::env::temp_dir().join(format!("akebia-browser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Games")).unwrap();
        std::fs::write(dir.join("Games/one.gb"), [0u8; 1024]).unwrap();

        let mut browser = Browser::at(dir.clone());
        assert_eq!(browser.here, 0, "nothing loose in the folder itself");
        let names: Vec<&str> = browser.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Games"]);
        assert_eq!(browser.folders[0].roms, 1, "counted without going in");

        browser.go(dir.join("Games"));
        assert_eq!(browser.here, 1, "and the button now promises that game");
        assert_eq!(browser.typed, roms::shorten(&dir.join("Games")), "the box follows");

        browser.go(browser.parent().unwrap());
        assert_eq!(browser.dir, dir, "back where it started");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_long_path_keeps_its_end() {
        assert_eq!(tail("/roms", 10), "/roms");
        assert_eq!(tail("/one/two/three", 8), "…o/three");
        assert_eq!(tail("/one/two/three", 8).chars().count(), 8);
    }
}
