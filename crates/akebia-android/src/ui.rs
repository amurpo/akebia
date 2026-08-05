//! The telephone's two screens.
//!
//! They are not the desktop's. A window has a menu bar, rows the height of a
//! mouse pointer and a folder browser, and none of the three survives contact
//! with a telephone: there is no keyboard to walk a list with, no room for a
//! menu bar over a screen this small, and Android does not let anyone walk the
//! storage anyway. What is shared is everything below the interface —the
//! console, the saved game, the sound— through [`Session`].

use std::path::PathBuf;

use akebia_core::{SCREEN_HEIGHT, SCREEN_WIDTH};
use akebia_frontend::app::{Session, Settings, ACCENT, BACKGROUND};
use akebia_frontend::args::Args;
use akebia_frontend::remote::{Remote, Role};
use akebia_frontend::{net, roms};
use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions, Vec2};

use crate::java::Java;
use crate::pad::{Pad, Touches, BUTTONS};

/// Height of a row in the list. A finger is not a mouse pointer: below about
/// fifty points the misses start, and this list is touched with a thumb.
const ROW_HEIGHT: f32 = 64.0;

pub struct Phone {
    java: Java,
    args: Args,
    settings: Settings,
    /// The console's screen, uploaded once per emulated frame.
    texture: TextureHandle,
    screen: Screen,
    /// The cartridges already imported.
    games: Vec<roms::Entry>,
    /// Whether the cartridges are kept outside the application's own folder,
    /// which is the same as asking whether a saved game survives uninstalling
    /// Akebia. False until Android is asked and says so.
    storage: bool,
    /// Whatever went wrong last, shown over the list.
    warning: Option<String>,
    touches: Touches,
    /// Where the controls ended up the last time they were drawn, and which of
    /// them the fingers are on.
    ///
    /// The geometry is worked out while drawing and used while emulating, which
    /// puts the two a frame apart. It does not matter: the shape only changes
    /// when the telephone is turned, and one frame of the old one goes by
    /// unseen. Before the first drawing there is no geometry at all, which is
    /// what the `None` is for.
    pad: Option<Pad>,
    down: [bool; 8],
    /// What the clock, the navigation bar and the camera's hole cover, in
    /// pixels. Asked of Android every frame because turning the telephone
    /// changes it.
    insets: [f32; 4],
    /// A connection being made, while the game carries on.
    connecting: Option<net::Pending>,
    /// The address box, open with whatever has been typed into it.
    address: Option<String>,
    /// Whether the way out has been pressed and is waiting to be meant.
    leaving: bool,
    /// What was typed there last. Retyping an address is a poor way to spend the
    /// moment before a trade, and worse on glass than on a keyboard.
    last_address: String,
}

enum Screen {
    List,
    /// Boxed because a session holds a whole console inside.
    Playing(Box<Session>),
    /// The same console, with the other end of its cable on another machine.
    ///
    /// There is no second screen and no second keyboard: a telephone has room
    /// for neither, and over a network it needs neither, because the other
    /// console is somebody else's and is drawing itself over there.
    Linked { console: Box<Session>, remote: Box<Remote> },
}

impl Phone {
    pub fn new(cc: &eframe::CreationContext<'_>, java: Java, args: Args) -> Self {
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

        let settings = Settings::from_args(&args);
        // In this order: where the cartridges are depends on the answer, and
        // asking afterwards would list the wrong folder for one frame.
        let storage = java.storage_granted();
        let games = java.roms_dir().map(|dir| roms::scan(&dir)).unwrap_or_default();
        Self {
            java,
            args,
            settings,
            texture,
            screen: Screen::List,
            games,
            storage,
            warning: None,
            touches: Touches::default(),
            pad: None,
            down: [false; 8],
            insets: [0.0; 4],
            connecting: None,
            address: None,
            leaving: false,
            last_address: String::new(),
        }
    }

    /// Loads a cartridge and starts it, or leaves the reason in the list.
    fn play(&mut self, path: PathBuf) {
        match akebia_frontend::load(&path, &self.args) {
            Ok(gb) => {
                let session = Session::new(gb, path, &self.args, &self.settings);
                self.screen = Screen::Playing(Box::new(session));
                self.warning = None;
                self.touches.clear();
            }
            Err(failure) => {
                self.warning = Some(failure.lines().next().unwrap_or_default().to_owned());
            }
        }
    }

    /// Saves the game and goes back to the list.
    fn back_to_list(&mut self, warning: Option<String>) {
        match &mut self.screen {
            Screen::Playing(session) => session.close(),
            Screen::Linked { console, .. } => console.close(),
            Screen::List => {}
        }
        // A connection half made has nobody left to hand a console to.
        self.connecting = None;
        self.address = None;
        // Whether the question was answered or the game ended on its own, there
        // is no longer a game to be asked about leaving.
        self.leaving = false;
        self.screen = Screen::List;
        self.warning = warning;
        // A finger held down when the screen changed would otherwise stay held
        // for ever: no lift event is coming for it any more.
        self.touches.clear();
        self.rescan();
    }

    fn rescan(&mut self) {
        if let Some(dir) = self.java.roms_dir() {
            self.games = roms::scan(&dir);
        }
    }

    /// Pulls the cable out, keeping the console.
    fn unplug(&mut self) {
        let Screen::Linked { console, remote } = std::mem::replace(&mut self.screen, Screen::List)
        else {
            return;
        };
        remote.close();
        let mut console = console;
        console.console_mut().set_link_connected(false);
        self.screen = Screen::Playing(console);
    }

    /// Picks up a connection once it is made, and plugs the cable in.
    fn collect_connection(&mut self) {
        let Some(pending) = self.connecting.as_mut() else {
            return;
        };
        let dialled = pending.dialled;
        let Some(result) = pending.poll() else {
            return;
        };
        self.connecting = None;

        let wire = match result {
            Ok(wire) => wire,
            Err(failure) => {
                self.warning = Some(failure.to_string());
                return;
            }
        };
        // The console it was for may be gone: the game was left for the list
        // while the connection was being made. Taking the screen apart anyway
        // would drop a session without saving it.
        let Screen::Playing(session) = std::mem::replace(&mut self.screen, Screen::List) else {
            return;
        };
        let mut console = session;
        let role = if dialled { Role::Dialled } else { Role::Waited };
        let remote = Box::new(Remote::new(wire, console.console_mut(), role));
        self.screen = Screen::Linked { console, remote };
        self.warning = None;
    }

    /// The question the way out asks before it is taken.
    ///
    /// The button sits in a corner of the glass, which is where a thumb goes
    /// when it reaches past the screen; pressed by accident it used to end the
    /// game there and then. What that costs is not the saved game —that is
    /// written on the way out— but everything since the last time the game
    /// itself saved, and on a Game Boy that can be an hour ago.
    fn leave_dialog(&mut self, ctx: &egui::Context) {
        if !self.leaving {
            return;
        }
        let mut leave = false;
        let mut stay = false;

        let response = egui::Modal::new(egui::Id::new("leave")).show(ctx, |ui| {
            ui.set_width(ui.available_width().min(420.0));
            ui.label(RichText::new("Leave the game?").size(20.0).strong());
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "The cartridge is saved on the way out. What the game itself \
                     has not saved is what is lost.",
                )
                .size(13.0)
                .color(Color32::from_gray(0x8A)),
            );

            ui.add_space(16.0);
            stay |= ui
                .add_sized(
                    Vec2::new(ui.available_width(), 52.0),
                    egui::Button::new("Keep playing"),
                )
                .clicked();
            ui.add_space(6.0);
            leave |= ui
                .add_sized(
                    Vec2::new(ui.available_width(), 52.0),
                    egui::Button::new("Leave for the list"),
                )
                .clicked();
        });

        // Tapping outside it is one more way of saying no, and the likeliest one
        // if the button was hit by mistake in the first place.
        if response.should_close() {
            stay = true;
        }
        if leave {
            self.back_to_list(None);
        } else if stay {
            self.leaving = false;
        }
    }

    /// The box the address is typed into, and the two ways to start a link.
    ///
    /// A dialog and not a screen of its own: the game stays where it is and
    /// cancelling costs nothing. On glass it is also the only sensible place for
    /// a text field, which is a thing this interface otherwise does not have.
    fn link_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut typed) = self.address.take() else {
            return;
        };
        let mut go = false;
        let mut wait = false;
        let mut cancelled = false;

        let response = egui::Modal::new(egui::Id::new("link")).show(ctx, |ui| {
            // A modal is offered the whole window, so the width has to be said
            // rather than taken: 420 points is about a thumb's reach across.
            ui.set_width(ui.available_width().min(420.0));
            ui.label(RichText::new("Link cable").size(20.0).strong());
            ui.add_space(10.0);

            wait |= ui
                .add_sized(Vec2::new(ui.available_width(), 52.0), egui::Button::new("Wait here"))
                .clicked();
            ui.add_space(4.0);
            ui.label(
                RichText::new("...and let the other machine connect to this one.")
                    .size(13.0)
                    .color(Color32::from_gray(0x8A)),
            );

            ui.add_space(14.0);
            ui.add(
                egui::TextEdit::singleline(&mut typed)
                    .hint_text("192.168.1.20")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);
            go |= ui
                .add_sized(Vec2::new(ui.available_width(), 52.0), egui::Button::new("Connect"))
                .clicked();

            ui.add_space(14.0);
            cancelled |= ui
                .add_sized(Vec2::new(ui.available_width(), 44.0), egui::Button::new("Cancel"))
                .clicked();
        });

        if response.should_close() {
            cancelled = true;
        }
        if wait {
            self.connecting = Some(net::Pending::listen(akebia_core::link::bgb::DEFAULT_PORT));
        } else if go && !typed.trim().is_empty() {
            self.last_address = typed.clone();
            self.connecting = Some(net::Pending::connect(typed));
        } else if !cancelled {
            self.address = Some(typed);
        }
    }

    /// The window minus whatever the system draws on top of it.
    ///
    /// Android hands a native activity the entire display, so without this the
    /// top row of the interface comes out underneath the clock and the bottom
    /// one underneath the navigation bar. The insets arrive in pixels and
    /// everything here is in points, which is what the division is for.
    fn safe_area(&self, ui: &egui::Ui) -> egui::Rect {
        let area = ui.available_rect_before_wrap();
        let scale = ui.ctx().pixels_per_point().max(0.1);
        let [left, top, right, bottom] = self.insets.map(|edge| edge / scale);
        let safe = egui::Rect::from_min_max(
            egui::Pos2::new(area.left() + left, area.top() + top),
            egui::Pos2::new(area.right() - right, area.bottom() - bottom),
        );
        // A window smaller than its own furniture is not a real answer; better
        // to draw over the clock than to draw nothing at all.
        if safe.width() > 80.0 && safe.height() > 80.0 {
            safe
        } else {
            area
        }
    }

    /// What is standing between the saved games and the next uninstall, while
    /// there is anything standing there at all.
    ///
    /// It is worth the room it takes on the list: a saved game is the one thing
    /// in the telephone that cannot be downloaded again, and the day it is found
    /// missing is the day it is too late. Once the folder is out of Android's
    /// reach the notice goes away for good.
    fn storage_notice(&mut self, ui: &mut egui::Ui) {
        if self.storage {
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Uninstalling Akebia erases the saved games with it.")
                        .size(14.0)
                        .color(ACCENT),
                );
                ui.add_space(6.0);
                let move_out = ui.add_sized(
                    Vec2::new(220.0, 44.0),
                    egui::Button::new(RichText::new("Keep them outside…").size(16.0)),
                );
                if move_out.clicked() {
                    // Android grants this in its own settings and nowhere else,
                    // so the button leaves for another application entirely. The
                    // answer is seen on the way back, in `logic`.
                    if let Err(message) = self.java.request_storage() {
                        self.warning = Some(message);
                    }
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "...in /sdcard/Akebia, along with the cartridges.\n\
                         Android asks for this in its settings.",
                    )
                    .size(13.0)
                    .color(Color32::from_gray(0x8A)),
                );
            });
        });
        ui.add_space(12.0);
    }

    /// The list of cartridges. Answers with the one that was tapped.
    fn list(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        let mut picked = None;

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(RichText::new("Akebia").size(26.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(14.0);
                let open = ui.add_sized(
                    Vec2::new(150.0, 46.0),
                    egui::Button::new(RichText::new("Open ROM…").size(17.0)),
                );
                if open.clicked() {
                    // The answer does not come back here. It arrives frames
                    // later, whenever the user is done looking around.
                    if let Err(message) = self.java.open_picker() {
                        self.warning = Some(message);
                    }
                }
            });
        });
        ui.add_space(10.0);

        if let Some(warning) = &self.warning {
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(RichText::new(warning).size(15.0).color(ACCENT));
            });
            ui.add_space(6.0);
        }

        self.storage_notice(ui);

        if self.games.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No cartridges yet").size(19.0));
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Open ROM brings one in from wherever it is\non the telephone.")
                        .size(15.0)
                        .color(Color32::from_gray(0x8A)),
                );
            });
            return None;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in &self.games {
                let row = ui.add_sized(
                    Vec2::new(ui.available_width(), ROW_HEIGHT),
                    egui::Button::new(RichText::new(&entry.name).size(18.0))
                        .wrap_mode(egui::TextWrapMode::Truncate),
                );
                if row.clicked() {
                    picked = Some(entry.path.clone());
                }
                ui.add_space(4.0);
            }
        });
        picked
    }
}

impl eframe::App for Phone {
    /// Black around the screen, like a console's bezel.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BACKGROUND.to_normalized_gamma_f32()
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A cartridge the user picked out of the system's file browser lands
        // here, several frames after the button that asked for it.
        if let Some(path) = self.java.imported_rom() {
            self.rescan();
            self.play(path);
        }
        // Coming back from the settings the access may be there now, and with it
        // another folder to list: granting it moves what was inside across.
        //
        // Only from the list, and that is deliberate rather than thrift. The
        // move happens the first time the folder is asked for, and a game
        // running while its `.sav` was carried elsewhere would go on autosaving
        // to the file left behind. From the list there is no game to strand.
        if !self.storage && matches!(self.screen, Screen::List) {
            self.storage = self.java.storage_granted();
            if self.storage {
                self.rescan();
            }
        }
        self.insets = self.java.insets();
        self.collect_connection();
        if self.connecting.is_some() {
            // Nothing else on this screen would ask for the repaint that looks
            // at the connection again.
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        self.touches.update(ctx);
        // Nothing is held down while a dialog is up. The controls are read
        // straight from the raw touches and know nothing of egui's layers, so
        // without this a finger reaching for a button in the middle of a dialog
        // would press whatever of the console's is underneath it — and both
        // dialogs are drawn right over the cross.
        let asking = self.leaving || self.address.is_some();
        self.down = match &self.pad {
            Some(pad) if !asking => pad.pressed(&self.touches),
            _ => [false; 8],
        };

        let mut failure = None;
        // Before emulating, so the buttons are already set when the game reads
        // the joypad.
        let pressed = |session: &mut Session, down: [bool; 8]| {
            for (button, held) in BUTTONS.into_iter().zip(down) {
                session.press(button, held);
            }
        };
        let outcome = match &mut self.screen {
            Screen::Playing(session) => {
                pressed(session, self.down);
                Some(session.advance(ctx, &mut self.texture))
            }
            Screen::Linked { console, remote } => {
                pressed(console, self.down);
                Some(console.advance_over(remote, ctx, &mut self.texture))
            }
            Screen::List => None,
        };
        if let Some(Err(reason)) = outcome {
            failure = Some(reason.lines().next().unwrap_or_default().to_owned());
        }
        if failure.is_some() {
            self.back_to_list(failure);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Everything is drawn inside what the system leaves free, both screens
        // alike: the list's own title was coming out under the clock too.
        let safe = self.safe_area(ui);
        let ui = &mut ui.new_child(egui::UiBuilder::new().max_rect(safe));

        let console = match &self.screen {
            Screen::Playing(session) => session,
            Screen::Linked { console, .. } => console,
            Screen::List => {
                if let Some(path) = self.list(ui) {
                    self.play(path);
                }
                return;
            }
        };

        let pad = Pad::lay_out(safe);
        let mut inside = ui.new_child(egui::UiBuilder::new().max_rect(pad.screen));
        console.ui(&mut inside, &self.texture);
        let linked = matches!(self.screen, Screen::Linked { .. }) || self.connecting.is_some();
        pad.paint(ui.painter(), &self.down, ACCENT, linked);

        // The two corner buttons are widgets and not touch regions: they are
        // taps like any other, and egui already knows how to tell a tap from a
        // slide. The eight that are not are the ones a thumb slides between.
        let menu = ui.interact(pad.menu, egui::Id::new("back"), egui::Sense::click());
        let cable = ui.interact(pad.link, egui::Id::new("link"), egui::Sense::click());
        self.pad = Some(pad);

        if menu.clicked() {
            self.leaving = true;
        }
        if cable.clicked() {
            match &self.screen {
                // Already joined, or joining: the button is the way out of it.
                Screen::Linked { .. } => self.unplug(),
                _ if self.connecting.is_some() => self.connecting = None,
                _ => self.address = Some(self.last_address.clone()),
            }
        }
        self.link_dialog(&ui.ctx().clone());
        self.leave_dialog(&ui.ctx().clone());
    }

    /// Being closed has to save the game just like leaving for the list.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        match &mut self.screen {
            Screen::Playing(session) => session.close(),
            Screen::Linked { console, .. } => console.close(),
            Screen::List => {}
        }
    }
}

/// Colours and sizes. The dark theme is fixed instead of following the system:
/// around a stretched 160×144 screen, a light background is blinding.
fn theme(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = BACKGROUND;
    visuals.selection.bg_fill = ACCENT;
    ctx.set_visuals(visuals);

    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(14.0, 10.0);
        style.interaction.selectable_labels = false;
    });
}
