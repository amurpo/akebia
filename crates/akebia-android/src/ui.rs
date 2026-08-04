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
use akebia_frontend::roms;
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
}

enum Screen {
    List,
    /// Boxed because a session holds a whole console inside.
    Playing(Box<Session>),
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
        let games = java.roms_dir().map(|dir| roms::scan(&dir)).unwrap_or_default();
        Self {
            java,
            args,
            settings,
            texture,
            screen: Screen::List,
            games,
            warning: None,
            touches: Touches::default(),
            pad: None,
            down: [false; 8],
            insets: [0.0; 4],
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
        if let Screen::Playing(session) = &mut self.screen {
            session.close();
        }
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
        self.insets = self.java.insets();

        self.touches.update(ctx);
        self.down = match &self.pad {
            Some(pad) => pad.pressed(&self.touches),
            None => [false; 8],
        };

        let mut failure = None;
        if let Screen::Playing(session) = &mut self.screen {
            // Before emulating, so the buttons are already set when the game
            // reads the joypad.
            for (button, held) in BUTTONS.into_iter().zip(self.down) {
                session.press(button, held);
            }
            if let Err(reason) = session.advance(ctx, &mut self.texture) {
                failure = Some(reason.lines().next().unwrap_or_default().to_owned());
            }
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

        let playing = matches!(self.screen, Screen::Playing(_));
        if !playing {
            if let Some(path) = self.list(ui) {
                self.play(path);
            }
            return;
        }

        let pad = Pad::lay_out(safe);
        if let Screen::Playing(session) = &self.screen {
            let mut inside = ui.new_child(egui::UiBuilder::new().max_rect(pad.screen));
            session.ui(&mut inside, &self.texture);
        }
        pad.paint(ui.painter(), &self.down, ACCENT);

        // The way out is a widget and not another touch region: it is a tap like
        // any other, and egui already knows how to tell a tap from a slide.
        let menu = ui.interact(pad.menu, egui::Id::new("back"), egui::Sense::click());
        self.pad = Some(pad);
        if menu.clicked() {
            self.back_to_list(None);
        }
    }

    /// Being closed has to save the game just like leaving for the list.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Screen::Playing(session) = &mut self.screen {
            session.close();
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
