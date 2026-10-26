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
// Named for what it is rather than imported plainly: `Pad` in this file is
// already the controls painted on the glass, and this is a button on the
// console they press.
use akebia_frontend::console::Pad as ConsoleButton;
use akebia_frontend::remote::{Remote, Role, Trouble};
use akebia_frontend::{net, roms};
use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions, Vec2};

use crate::java::Java;
use crate::pad::{Button, Cable, Pad, Touches, BUTTONS};

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
    down: [bool; 10],
    /// What the clock, the navigation bar and the camera's hole cover, in
    /// pixels. Asked of Android every frame because turning the telephone
    /// changes it.
    insets: [f32; 4],
    /// A connection being made, while the game carries on.
    connecting: Option<net::Pending>,
    /// Whether the cable's own dialog —wait here, or go somewhere— is up.
    linking: bool,
    /// Whether Java's address box is up and its answer still to be collected.
    ///
    /// The box is not this interface's; see [`Java::ask_for_address`]. What is
    /// kept here is only the fact that one is out there to be asked about, so
    /// that the crossing into Java happens on the few frames it can answer on
    /// and not on every frame of every game.
    asking_address: bool,
    /// Whether the way out has been pressed and is waiting to be meant.
    leaving: bool,
    /// What was typed there last. Retyping an address is a poor way to spend the
    /// moment before a trade, and worse on glass than on a keyboard.
    last_address: String,
    /// The last thing that happened to a link, kept over the game until
    /// something else happens to one.
    ///
    /// The list's warning is no use here: everything about a cable happens with
    /// a game running, and there was nowhere on this screen for any of it to be
    /// said. Pressing the cable button and seeing nothing but a colour is what
    /// that came to.
    notice: Option<String>,
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
            down: [false; 10],
            insets: [0.0; 4],
            connecting: None,
            linking: false,
            asking_address: false,
            leaving: false,
            last_address: String::new(),
            notice: None,
        }
    }

    /// Loads a cartridge and starts it, or leaves the reason in the list.
    ///
    /// Whichever machine the file is for: the list has been offering `.gba`
    /// cartridges since the scanner learnt to name them, and until this it
    /// handed every one of them to the Game Boy, which rejected it.
    fn play(&mut self, path: PathBuf) {
        match akebia_frontend::load_console(&path, &self.args) {
            Ok(console) => {
                let session = Session::new(console, path, &self.args, &self.settings);
                // An Advance with no BIOS runs and draws nothing, and the
                // session is the only thing that knows. Said here or not at
                // all: a black screen on a telephone has no window title to
                // explain itself in.
                self.notice = session.warning();
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
        self.notice = None;
        self.linking = false;
        // The box may still be up over there; Java forgets whatever it is
        // closed with the next time one is opened, so an answer nobody is
        // waiting for goes nowhere.
        self.asking_address = false;
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
        // Only the older machine has a port to disconnect. Nothing should have
        // plugged a cable into an Advance in the first place, and if something
        // did, there is nothing here to undo.
        if let Some(gb) = console.console_mut().gameboy_mut() {
            gb.set_link_connected(false);
        }
        self.screen = Screen::Playing(console);
        self.notice = None;
    }

    /// One line saying where the cable stands, or nothing when there is no cable
    /// and nothing has happened to one.
    ///
    /// Waiting has to say *where*: the other machine has to be told an address,
    /// and this is the end that knows it. Until this, the telephone knew it and
    /// said nothing, so the only way to link two of them was to already know
    /// what the telephone's address was.
    fn link_status(&self) -> Option<String> {
        if let Some(pending) = &self.connecting {
            return Some(match &pending.here {
                Some(here) => format!("{} — this telephone is {here}", pending.what),
                None => pending.what.clone(),
            });
        }
        if let Screen::Linked { remote, .. } = &self.screen {
            let peer = remote.peer();
            return Some(if remote.is_ready() {
                format!("Linked to {peer}")
            } else {
                format!("Connected to {peer} — waiting for it to answer")
            });
        }
        self.notice.clone()
    }

    /// What the cable button has to show.
    fn cable(&self) -> Cable {
        match &self.screen {
            Screen::Linked { remote, .. } if remote.is_ready() => Cable::In,
            Screen::Linked { .. } => Cable::Joining,
            _ if self.connecting.is_some() => Cable::Joining,
            _ => Cable::Out,
        }
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
                // Over the game and not in the list's warning: the game is still
                // running, and going back to the list to say a connection failed
                // would throw it away to deliver the news.
                self.notice = Some(failure.to_string());
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
        // The cable is the older machine's. An Advance has a port and nothing
        // here drives it, so the game carries on alone rather than the
        // connection taking it away — the same answer the desktop gives.
        let Some(gb) = console.console_mut().gameboy_mut() else {
            self.notice = Some("only a Game Boy has a link cable".to_owned());
            self.screen = Screen::Playing(console);
            return;
        };
        let remote = Box::new(Remote::new(wire, gb, role));
        self.screen = Screen::Linked { console, remote };
        self.warning = None;
        self.notice = None;
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

    /// The two ways to start a link, and neither of them is typed into.
    ///
    /// A dialog and not a screen of its own: the game stays where it is and
    /// cancelling costs nothing.
    ///
    /// The address used to be typed into an `egui` field right here, and on a
    /// telephone it could not be: the window is a `NativeActivity`, which has
    /// no editable view, so the keyboard came up over a field that never
    /// received a character. Pressing Connect on an empty field did nothing and
    /// said nothing, which from the outside was a button that did not work. So
    /// the typing is Java's now —see [`Java::ask_for_address`]— and what is
    /// left here is the choice of which end to be.
    fn link_dialog(&mut self, ctx: &egui::Context) {
        if !self.linking {
            return;
        }
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
            let width = ui.available_width();
            go |= ui
                .add_sized(Vec2::new(width, 52.0), egui::Button::new("Connect to..."))
                .clicked();
            ui.add_space(4.0);
            // The ellipsis is doing work: the button asks for an address, it
            // does not go to one. And the address it last went to is worth
            // showing, because the second link of an evening is with the same
            // machine as the first.
            let said = if self.last_address.is_empty() {
                "...a machine that is already waiting. It asks for its address.".to_owned()
            } else {
                format!("...a machine that is already waiting. Last time: {}", self.last_address)
            };
            ui.label(RichText::new(said).size(13.0).color(Color32::from_gray(0x8A)));

            ui.add_space(14.0);
            cancelled |= ui
                .add_sized(Vec2::new(ui.available_width(), 44.0), egui::Button::new("Cancel"))
                .clicked();
        });

        if response.should_close() {
            cancelled = true;
        }
        if wait {
            self.linking = false;
            self.connecting = Some(net::Pending::listen(akebia_core::link::bgb::DEFAULT_PORT));
        } else if go {
            // This one comes down while Java's is up: two dialogs over the same
            // game, one of them unreachable underneath the other, is not a thing
            // to offer anybody. It goes back up if the box is closed without an
            // address, so that cancelling one step undoes one step.
            self.linking = false;
            match self.java.ask_for_address(&self.last_address) {
                Ok(()) => {
                    self.asking_address = true;
                    // Whatever finger opened it is about to stop being this
                    // window's business: the box takes the touches, and the lift
                    // event for that finger is never coming here.
                    self.touches.clear();
                }
                Err(message) => {
                    log::error!("askForAddress: {message}");
                    self.notice = Some("the address box could not be opened".to_owned());
                }
            }
        } else if cancelled {
            self.linking = false;
        }
    }

    /// Picks up whatever the address box was closed with.
    ///
    /// Asked only while there is a box out there to answer, which is a handful
    /// of frames and not every one of them: the crossing into Java is cheap but
    /// it is not free, and there is nothing to collect the rest of the time.
    fn collect_address(&mut self) {
        if !self.asking_address {
            return;
        }
        let Some(typed) = self.java.typed_address() else {
            return;
        };
        self.asking_address = false;
        self.touches.clear();
        if typed.trim().is_empty() {
            // Closed without one. Back to the dialog it was opened from rather
            // than into the game: the way back out of that one is one more tap.
            self.linking = true;
            return;
        }
        self.last_address = typed.clone();
        self.connecting = Some(net::Pending::connect(typed));
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
        self.collect_address();
        self.collect_connection();
        if self.connecting.is_some() || self.asking_address {
            // Nothing else on this screen would ask for the repaint that looks
            // at the connection again — nor for the one that looks at the
            // address box, which is answered on a thread this one never hears
            // from.
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        self.touches.update(ctx);
        // Nothing is held down while a dialog is up. The controls are read
        // straight from the raw touches and know nothing of egui's layers, so
        // without this a finger reaching for a button in the middle of a dialog
        // would press whatever of the console's is underneath it — and both
        // dialogs are drawn right over the cross.
        let asking = self.leaving || self.linking || self.asking_address;
        self.down = match &self.pad {
            Some(pad) if !asking => pad.pressed(&self.touches),
            _ => [false; 10],
        };

        let mut failure = None;
        // A cable that ended while the game did not. Noted here and acted on
        // once the borrow on the screen is over.
        let mut cable_ended = None;
        // Before emulating, so the buttons are already set when the game reads
        // the joypad.
        let pressed = |session: &mut Session, down: [bool; 10]| {
            for (button, held) in BUTTONS.into_iter().zip(down) {
                session.press(console_button(button), held);
            }
        };
        let outcome = match &mut self.screen {
            Screen::Playing(session) => {
                pressed(session, self.down);
                session.advance(ctx, &mut self.texture).err()
            }
            Screen::Linked { console, remote } => {
                pressed(console, self.down);
                match console.advance_over(remote, ctx, &mut self.texture) {
                    Ok(()) => None,
                    // The console itself fell over: there is no game to keep.
                    Err(Trouble::Fault(fault)) => Some(akebia_frontend::describe_fault(fault)),
                    // The cable ended and the game did not. Going back to the
                    // list here threw a session away —and everything the game
                    // had not saved with it— because somebody else closed their
                    // window. The cable comes out instead.
                    Err(trouble) => {
                        cable_ended = Some(trouble.to_string());
                        None
                    }
                }
            }
            Screen::List => None,
        };
        if let Some(reason) = outcome {
            failure = Some(reason.lines().next().unwrap_or_default().to_owned());
        }
        if let Some(why) = cable_ended {
            self.unplug();
            self.notice = Some(why);
            return;
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
        pad.paint(ui.painter(), &self.down, ACCENT, self.cable());

        // Whatever the cable is doing, along the top row and to the left of the
        // two corner buttons, which is the only strip of the window that belongs
        // to neither the picture nor a thumb.
        if let Some(said) = self.link_status() {
            let strip = egui::Rect::from_min_max(
                egui::Pos2::new(safe.left() + 12.0, pad.link.top()),
                egui::Pos2::new(pad.link.left() - 8.0, pad.link.bottom()),
            );
            if strip.width() > 60.0 {
                ui.put(
                    strip,
                    egui::Label::new(RichText::new(said).size(13.0).color(ACCENT))
                        .truncate()
                        .halign(egui::Align::LEFT),
                );
            }
        }

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
                _ => self.linking = true,
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

/// What a button painted on the glass is to whichever console is running.
///
/// The two types exist for reasons that do not meet anywhere else. The pad is
/// drawn and measured in `pad`, which is built on a desktop too so that its
/// arithmetic can be tested away from a telephone — and on a desktop the
/// frontend's own button type is not there to be named, because this crate only
/// depends on the frontend when the target is Android. So the pad speaks the
/// older machine's `Button`, and the translation happens here, in the one file
/// that is never compiled for anything else.
///
/// The ten are the ten, and the two lists are written out against each other so
/// that a button added to either has to be answered for here.
fn console_button(button: Button) -> ConsoleButton {
    match button {
        Button::Up => ConsoleButton::Up,
        Button::Down => ConsoleButton::Down,
        Button::Left => ConsoleButton::Left,
        Button::Right => ConsoleButton::Right,
        Button::A => ConsoleButton::A,
        Button::B => ConsoleButton::B,
        Button::Start => ConsoleButton::Start,
        Button::Select => ConsoleButton::Select,
        Button::L => ConsoleButton::L,
        Button::R => ConsoleButton::R,
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
