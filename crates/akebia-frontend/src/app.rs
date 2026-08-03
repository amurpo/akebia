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
use akebia_core::ports::{Palette, VideoOutput};
use akebia_core::{Button, FrameBuffer, GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};
use eframe::egui::{
    self, Align, Color32, ColorImage, Key, Label, Layout, RichText, Sense, TextureHandle,
    TextureOptions, UiBuilder, Vec2, ViewportCommand,
};

use crate::args::Args;
use crate::{audio, debug, roms, save};

/// The red of the Akebia logo. It is the colour of everything selected.
const ACCENT: Color32 = Color32::from_rgb(0xCF, 0x1A, 0x30);

/// Application background, a very dark grey but not black: black is reserved for
/// the screen's frame, and that keeps the two apart.
const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x15, 0x18);

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
        ..Default::default()
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
struct Settings {
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
    fn from_args(args: &Args) -> Self {
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
    /// The console's screen, uploaded to the GPU once per emulated frame.
    texture: TextureHandle,
}

enum Screen {
    List(List),
    /// Boxed because a `Session` holds the whole console inside, and without the
    /// box the enum would take up that much while sitting in the list.
    Playing(Box<Session>),
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

        let texture = cc.egui_ctx.load_texture(
            "screen",
            ColorImage::new(
                [SCREEN_WIDTH, SCREEN_HEIGHT],
                vec![Color32::BLACK; SCREEN_WIDTH * SCREEN_HEIGHT],
            ),
            // Nearest neighbour: a Game Boy pixel has to come out as an exact
            // square, not as an interpolated smudge.
            TextureOptions::NEAREST,
        );

        let folder = roms::initial_dir(args.roms_dir.as_deref(), roms::state_path().as_deref());
        let screen = match initial {
            Some(session) => Screen::Playing(Box::new(session)),
            None => Screen::List(List::new(folder.clone())),
        };

        Self { args, limit, folder, settings, screen, dialog: None, texture }
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
                ctx.send_viewport_cmd(ViewportCommand::Title(title(Some(&session))));
                self.screen = Screen::Playing(Box::new(session));
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
        let mut open_dialog = false;
        let mut back_to_list = false;
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
        if let Screen::Playing(session) = &mut self.screen {
            session.apply(&self.settings);
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
        if let Screen::Playing(session) = &mut self.screen {
            session.close();
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
        let Screen::Playing(session) = &mut self.screen else {
            return;
        };

        // With the folder chooser open the game is paused. It has to happen
        // before anything else is read: `logic` runs before `ui`, so without
        // this the Escape that closes the dialog would first be taken here as
        // "back to the list", and the path being typed would drive the joypad.
        if dialog {
            session.pause();
            return;
        }

        // Escape leaves the game for the list instead of closing the program:
        // with a real window, closing already has its own button.
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.back_to_list(ctx, None);
            return;
        }

        let result = session.advance(ctx, &mut self.texture);
        let frames = session.frames;

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
                session.ui(ui, &self.texture);
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
        if let Screen::Playing(session) = &mut self.screen {
            session.close();
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
struct Session {
    gb: GameBoy,
    title: String,
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
}

impl Session {
    fn new(mut gb: GameBoy, rom: PathBuf, args: &Args, settings: &Settings) -> Self {
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
            save,
            audio: None,
            pixels: vec![Color32::BLACK; SCREEN_WIDTH * SCREEN_HEIGHT],
            next: Instant::now(),
            frames: 0,
            trace: args.debug.then(debug::LiveTrace::new),
            captures: 0,
            serial: args.serial,
        };
        // The settings and not the arguments: a game started after the menu was
        // touched has to come up the way the menu left things.
        session.apply(settings);
        session
    }

    /// Pushes the menu's settings into the running console.
    fn apply(&mut self, settings: &Settings) {
        self.gb.set_dmg_shades(settings.palette().shades);
        self.gb.set_speaker_filter(settings.speaker_filter);
        match (settings.sound, self.audio.is_some()) {
            // Dropping the output stops the stream, and from then on the frame's
            // samples are discarded instead of piling up.
            (false, true) => self.audio = None,
            (true, false) => self.audio = crate::open_audio(&mut self.gb, true),
            _ => {}
        }
    }

    /// Stops the clock while a dialog is up, so that closing it does not leave
    /// the session owing every frame the user spent reading.
    fn pause(&mut self) {
        self.next = Instant::now();
    }

    /// Emulates whatever is due and uploads the result to the texture.
    fn advance(&mut self, ctx: &egui::Context, texture: &mut TextureHandle) -> Result<(), String> {
        let period = Duration::from_secs_f64(1.0 / FRAMES_PER_SECOND);

        // The keyboard is polled before emulating so that the buttons are set
        // when the game reads 0xFF00 during VBlank.
        ctx.input(|i| {
            for (key, button) in KEYS {
                self.gb.set_button(button, i.key_down(key));
            }
        });

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
        if ctx.input(|i| i.key_pressed(Key::D)) {
            self.capture();
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
        result.map_err(crate::describe_fault)
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
    fn close(&mut self) {
        if let Some(save) = self.save.as_mut() {
            save.flush_final(&self.gb);
        }
    }

    /// The screen, centred and with integer scaling.
    fn ui(&self, ui: &mut egui::Ui, texture: &TextureHandle) {
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::BLACK);

        // The factor is rounded down so that a Game Boy pixel is an exact N×N
        // square. With a fractional factor, some rows of pixels would come out
        // one thickness and others another.
        let scale = (area.width() / SCREEN_WIDTH as f32)
            .min(area.height() / SCREEN_HEIGHT as f32)
            .floor()
            .max(1.0);
        let size = Vec2::new(SCREEN_WIDTH as f32 * scale, SCREEN_HEIGHT as f32 * scale);

        ui.painter().image(
            texture.id(),
            egui::Rect::from_center_size(area.center(), size),
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
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
