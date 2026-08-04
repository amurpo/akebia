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
use akebia_core::{Button, FrameBuffer, GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};
use eframe::egui::{
    self, Align, Color32, ColorImage, Key, Label, Layout, RichText, Sense, TextureHandle,
    TextureOptions, UiBuilder, Vec2, ViewportCommand,
};

use crate::args::Args;
use crate::{audio, debug, roms, save};

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
const KEYS: [(Key, Button); 8] = [
    (Key::ArrowUp, Button::Up),
    (Key::ArrowDown, Button::Down),
    (Key::ArrowLeft, Button::Left),
    (Key::ArrowRight, Button::Right),
    (Key::Z, Button::A),
    (Key::X, Button::B),
    (Key::Enter, Button::Start),
    (Key::Backspace, Button::Select),
];

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
            let gb = crate::load(path, &args)?;
            crate::remember_dir(path);
            Some(Session::new(gb, path.clone(), &args, &settings))
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
}

impl Settings {
    pub fn from_args(args: &Args) -> Self {
        Self {
            sound: !args.mute,
            speaker_filter: !args.raw_audio,
            grayscale: args.grayscale,
            scale: args.scale,
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

/// The window that leaves the console's screen at exactly `scale`.
fn window_size(scale: f32) -> Vec2 {
    Vec2::new(SCREEN_WIDTH as f32 * scale, SCREEN_HEIGHT as f32 * scale + MENU_BAR_ROOM)
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
    /// A console set aside while its player picks the game for the other end of
    /// the cable. It is paused, not closed: its saved game is still open and
    /// cancelling puts it straight back on screen.
    pending_link: Option<Box<Session>>,
}

enum Screen {
    List(List),
    /// Boxed because a `Session` holds the whole console inside, and without the
    /// box the enum would take up that much while sitting in the list.
    Playing(Box<Session>),
    /// Two consoles joined by a link cable.
    Linked(Box<Pair>),
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
            pending_link: None,
        }
    }

    /// Loads the chosen ROM and starts playing, or leaves the warning in the
    /// list.
    fn play(&mut self, ctx: &egui::Context, path: PathBuf) {
        match crate::load(&path, &self.args) {
            Ok(gb) => {
                crate::remember_dir(&path);
                if let Some(dir) = path.parent() {
                    self.folder = dir.to_owned();
                }
                let session = Session::new(gb, path, &self.args, &self.settings);
                // A console was left waiting for a partner: this is the partner,
                // and picking it is what plugs the cable in.
                self.screen = match self.pending_link.take() {
                    Some(first) => {
                        let pair = Pair::new(*first, session, &self.settings);
                        ctx.send_viewport_cmd(ViewportCommand::Title(linked_title(&pair)));
                        Screen::Linked(Box::new(pair))
                    }
                    None => {
                        ctx.send_viewport_cmd(ViewportCommand::Title(title(Some(&session))));
                        Screen::Playing(Box::new(session))
                    }
                };
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
        let mut open_dialog = false;
        let mut back_to_list = false;
        let mut connect = false;
        let mut swap = false;
        let mut unplug = false;
        let mut settings = self.settings.clone();

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
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
                ui.menu_button("Link", |ui| {
                    connect = ui
                        .add_enabled(playing, egui::Button::new("Second console…"))
                        .on_hover_text("Pick another game and join the two with a link cable")
                        .clicked();
                    swap = ui
                        .add_enabled(linked, egui::Button::new("Swap keyboard\tTab"))
                        .on_hover_text("Hand the keyboard to the other console")
                        .clicked();
                    ui.separator();
                    unplug = ui
                        .add_enabled(linked, egui::Button::new("Unplug the cable"))
                        .on_hover_text("Keep playing on the console that has the keyboard")
                        .clicked();
                });
            });
        });

        if settings != self.settings {
            self.apply(&settings, ctx);
        }
        if open_dialog {
            self.open_dialog();
        }
        if back_to_list {
            self.back_to_list(ctx, None);
        }
        if connect {
            self.begin_link();
        }
        if swap {
            if let Screen::Linked(pair) = &mut self.screen {
                pair.swap_keyboard(&self.settings);
            }
        }
        if unplug {
            self.unplug(ctx);
        }
    }

    /// Sets the running game aside and shows the list so its partner can be
    /// picked. The console is paused, not closed: its saved game stays open, and
    /// Escape puts it back on screen with nothing lost.
    fn begin_link(&mut self) {
        if !matches!(self.screen, Screen::Playing(_)) {
            return;
        }
        let mut list = List::new(self.folder.clone());
        // The list's one line of prose is the warning slot, and an instruction is
        // what belongs there now: without it, coming back to the list right after
        // asking for a second console looks like the menu did nothing.
        list.warning = Some("Pick the game for the second console".to_owned());

        if let Screen::Playing(session) = std::mem::replace(&mut self.screen, Screen::List(list)) {
            self.pending_link = Some(session);
        }
    }

    /// Calls off a link that was never made and resumes the console that was
    /// waiting for it.
    fn cancel_link(&mut self, ctx: &egui::Context) {
        let Some(session) = self.pending_link.take() else {
            return;
        };
        ctx.send_viewport_cmd(ViewportCommand::Title(title(Some(&session))));
        self.screen = Screen::Playing(session);
    }

    /// Pulls the cable out, keeping whichever console has the keyboard and
    /// saving the other one on its way out.
    fn unplug(&mut self, ctx: &egui::Context) {
        if !matches!(self.screen, Screen::Linked(_)) {
            return;
        }
        let list = Screen::List(List::new(self.folder.clone()));
        if let Screen::Linked(pair) = std::mem::replace(&mut self.screen, list) {
            let session = pair.split(&self.settings);
            ctx.send_viewport_cmd(ViewportCommand::Title(title(Some(&session))));
            self.screen = Screen::Playing(Box::new(session));
        }
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
            Screen::List(_) => {}
        }
        // The console waiting for a partner has to take the change too, or it
        // would come back with the palette and the sound it was set aside with.
        if let Some(pending) = self.pending_link.as_mut() {
            pending.apply(&self.settings);
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
            Screen::List(_) => {}
        }
        if let Some(mut pending) = self.pending_link.take() {
            pending.close();
        }
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
        let dialog = self.dialog.is_some();

        if matches!(self.screen, Screen::List(_)) {
            // Escape over the list, with a console set aside waiting for its
            // partner, calls the link off instead of doing nothing.
            if !dialog && self.pending_link.is_some() && ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.cancel_link(ctx);
            }
            return;
        }

        // With the folder chooser open the game is paused. It has to happen
        // before anything else is read: `logic` runs before `ui`, so without
        // this the Escape that closes the dialog would first be taken here as
        // "back to the list", and the path being typed would drive the joypad.
        if dialog {
            match &mut self.screen {
                Screen::Playing(session) => session.pause(),
                Screen::Linked(pair) => pair.pause(),
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

        // The keyboard is read here and not inside the session because the
        // session no longer knows what a keyboard is. Right before emulating, so
        // that the buttons are set when the game reads the joypad.
        let (result, frames) = match &mut self.screen {
            Screen::Playing(session) => {
                ctx.input(|i| {
                    for (key, button) in KEYS {
                        session.press(button, i.key_down(key));
                    }
                });
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
                ctx.input(|i| {
                    for (key, button) in KEYS {
                        pair.active().press(button, i.key_down(key));
                    }
                });
                (pair.advance(ctx, &mut self.textures), pair.frames())
            }
            Screen::List(_) => return,
        };

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
            Screen::List(_) => {}
        }
        if let Some(pending) = self.pending_link.as_mut() {
            pending.close();
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
    /// Index **within what is visible**, not within `entries`.
    selection: usize,
    /// Ask for the chosen row to be brought into the visible part of the scroll.
    follow: bool,
    warning: Option<String>,
}

impl List {
    fn new(dir: PathBuf) -> Self {
        let entries = roms::scan(&dir);
        Self { dir, entries, filter: String::new(), selection: 0, follow: false, warning: None }
    }

    /// Indices of `entries` that pass the typed filter.
    fn visible(&self) -> Vec<usize> {
        if self.filter.trim().is_empty() {
            return (0..self.entries.len()).collect();
        }
        let wanted = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name.to_lowercase().contains(&wanted))
            .map(|(i, _)| i)
            .collect()
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
            ui.add_space(10.0);
        });
        change_folder
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
            } else {
                ui.label(RichText::new("No game matches the search").color(Color32::GRAY));
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
    gb: GameBoy,
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
    pub fn new(mut gb: GameBoy, rom: PathBuf, args: &Args, settings: &Settings) -> Self {
        gb.set_trace_enabled(args.debug);
        gb.set_write_log_enabled(args.debug);

        let save = if args.no_save {
            None
        } else {
            let path = args.save.clone().unwrap_or_else(|| save::default_path(&rom));
            save::SaveFile::open(&mut gb, path)
        };
        let title = gb.header().title.clone();

        let mut session = Self {
            gb,
            title,
            rom,
            save,
            audio: None,
            pixels: vec![Color32::BLACK; SCREEN_WIDTH * SCREEN_HEIGHT],
            next: Instant::now(),
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

    /// Makes sure this console is not autosaving over the same file as `other`.
    ///
    /// It happens the moment somebody links a game to itself, which is the
    /// obvious thing to try first: both consoles derive their `.sav` from the
    /// same ROM path, and from then on two autosaves a second apart take turns
    /// overwriting one real saved game. This one moves aside; see
    /// [`save::linked_path`].
    fn unshare_save_with(&mut self, other: &Self) {
        let (Some(mine), Some(theirs)) = (self.save.as_ref(), other.save.as_ref()) else {
            return;
        };
        if mine.path() != theirs.path() {
            return;
        }
        let moved = save::linked_path(&self.rom);
        eprintln!("the second console saves to {} so as not to share one file", moved.display());
        if let Some(save) = self.save.as_mut() {
            save.redirect(moved);
        }
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
        self.gb.set_dmg_shades(settings.palette().shades);
        self.gb.set_speaker_filter(settings.speaker_filter);
        match (settings.sound && !self.muted, self.audio.is_some()) {
            // Dropping the output stops the stream, and from then on the frame's
            // samples are discarded instead of piling up.
            (false, true) => self.audio = None,
            (true, false) => self.audio = crate::open_audio(&mut self.gb, true),
            _ => {}
        }
    }

    /// Stops the clock while a dialog is up, so that closing it does not leave
    /// the session owing every frame the user spent reading.
    pub fn pause(&mut self) {
        self.next = Instant::now();
    }

    /// Presses or releases a joypad button.
    ///
    /// It is the caller that says which ones are down, and this is not a detail:
    /// it is the whole difference between the two frontends. On the desktop a
    /// keyboard answers, on a telephone eight painted circles do, and the
    /// console is not to know which. Whoever calls this must do so **before**
    /// [`Session::advance`], so the buttons are already set when the game reads
    /// 0xFF00 during VBlank.
    pub fn press(&mut self, button: Button, down: bool) {
        self.gb.set_button(button, down);
    }

    /// Emulates whatever is due and uploads the result to the texture.
    pub fn advance(
        &mut self,
        ctx: &egui::Context,
        texture: &mut TextureHandle,
    ) -> Result<(), String> {
        let period = Duration::from_secs_f64(1.0 / FRAMES_PER_SECOND);
        let now = Instant::now();
        let mut emulated = 0;
        let mut failure = None;
        while self.next <= now && emulated < MAX_CATCH_UP {
            if let Err(f) = self.frame() {
                failure = Some(f);
                break;
            }
            self.next += period;
            emulated += 1;
        }
        if emulated == MAX_CATCH_UP {
            // We are running really late: the debt is dropped instead of running
            // faster over the next few seconds.
            self.next = Instant::now();
        }

        if emulated > 0 {
            texture.set(
                ColorImage::new([SCREEN_WIDTH, SCREEN_HEIGHT], self.pixels.clone()),
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

    fn frame(&mut self) -> Result<(), String> {
        // `gb` and `pixels` are distinct fields, so both borrows coexist; the
        // sink only exists during this call.
        let result = self.gb.run_frame(&mut FrameSink { pixels: &mut self.pixels });
        self.after_frame();
        result.map_err(crate::describe_fault)
    }

    /// One frame of a linked pair.
    ///
    /// There is a single call for both consoles because there is no other way to
    /// do it: with a cable in between, each one's next instruction can depend on
    /// what the other did on its last, so they cannot be advanced one after the
    /// other. [`link::run_frame`] interleaves them instruction by instruction.
    fn linked_frame(a: &mut Self, b: &mut Self) -> Result<(), String> {
        let result = link::run_frame(
            &mut a.gb,
            &mut FrameSink { pixels: &mut a.pixels },
            &mut b.gb,
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
        crate::drain_audio(&mut self.gb, self.audio.as_ref());
        crate::drain_serial(&mut self.gb, self.serial);
        if let Some(save) = self.save.as_mut() {
            save.tick(&self.gb);
        }
        // It has to be drained every frame even if nothing is printed: otherwise
        // the core's capture would pile up 144 lines per frame without end.
        if let Some(trace) = self.trace.as_mut() {
            trace.frame(self.gb.take_frame_trace());
        }
        self.frames += 1;
    }

    /// Dumps the frame and the VRAM as images, with `--debug`.
    fn capture(&mut self) {
        if self.trace.is_none() {
            return;
        }
        self.captures += 1;
        // The captures go to the current directory, not next to the ROM.
        match debug::snapshot(&self.gb, Path::new("."), self.captures) {
            Ok(path) => eprintln!("capture written to {}", path.display()),
            Err(e) => eprintln!("warning: {e}"),
        }
    }

    /// Writes the saved game. Called on leaving for the list and on closing the
    /// window.
    pub fn close(&mut self) {
        if let Some(save) = self.save.as_mut() {
            save.flush_final(&self.gb);
        }
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
        let scale = (area.width() / SCREEN_WIDTH as f32)
            .min(area.height() / SCREEN_HEIGHT as f32)
            .floor()
            .max(1.0);
        let size = Vec2::new(SCREEN_WIDTH as f32 * scale, SCREEN_HEIGHT as f32 * scale);
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
        link::connect(&mut a.gb, &mut b.gb);
        // Only one console is heard; see `Session::muted`.
        b.set_muted(true, settings);
        b.unshare_save_with(&a);
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
        for (_, button) in KEYS {
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
        kept.gb.set_link_connected(false);
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

        while self.next <= now && emulated < MAX_CATCH_UP {
            let [a, b] = &mut self.consoles;
            if let Err(f) = Session::linked_frame(a, b) {
                failure = Some(f);
                break;
            }
            self.next += period;
            emulated += 1;
        }
        if emulated == MAX_CATCH_UP {
            self.next = Instant::now();
        }

        if emulated > 0 {
            for (console, texture) in self.consoles.iter().zip(textures.iter_mut()) {
                texture.set(
                    ColorImage::new([SCREEN_WIDTH, SCREEN_HEIGHT], console.pixels.clone()),
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
        /// Gap between the two screens. Without it the two pictures read as one
        /// wide one, which with the same game running twice is genuinely
        /// confusing.
        const GUTTER: f32 = 16.0;

        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::BLACK);

        let width = ((area.width() - GUTTER) / 2.0).max(1.0);
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
                })
                .collect(),
            filter: String::new(),
            selection: 0,
            follow: false,
            warning: None,
        }
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
