//! The eight buttons, painted on the glass and listened to.
//!
//! # Why the fingers are tracked by hand
//!
//! egui hands out one pointer, because a mouse is one. A console needs at least
//! two at once —nobody runs and jumps otherwise— so the raw touch events are
//! read instead: each finger arrives with an identifier that lasts from the
//! moment it lands to the moment it lifts, and what this module keeps is the set
//! of positions currently on the glass. Which buttons those positions fall on is
//! then a question of geometry, and a finger that slides from one button onto
//! the next presses the next one, which is what a thumb on real plastic does.

use std::collections::HashMap;

use akebia_core::Button;
use egui::{
    Align2, Color32, Context, Event, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, TouchPhase,
    Vec2,
};

/// The order everything in this module counts in.
pub const BUTTONS: [Button; 8] = [
    Button::Up,
    Button::Down,
    Button::Left,
    Button::Right,
    Button::A,
    Button::B,
    Button::Start,
    Button::Select,
];

/// How far from the middle of the cross a finger has to be for the direction to
/// count, as a fraction of its arm.
///
/// Without a dead zone the exact centre would fire two opposite directions at
/// once, and a thumb resting in the middle would jitter between them.
const DEAD_ZONE: f32 = 0.22;

/// Room left between a cluster of buttons and the edge of the glass.
const MARGIN: f32 = 16.0;

/// The least the picture is ever squeezed to: two whole screen pixels for each
/// Game Boy one.
///
/// It is a floor and not a preference. The scaling is by whole numbers —a Game
/// Boy pixel has to come out an exact square— so the step below two is one, and
/// at one the console's screen is a postage stamp in the middle of a telephone.
/// The controls give up their share before that happens.
const SCREEN_MIN: f32 = 144.0 * 2.0;

/// Colours. They are the desktop's, so that the two do not look like different
/// programs.
const FACE: Color32 = Color32::from_rgb(0x26, 0x26, 0x2C);
const EDGE: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x42);
const LABEL: Color32 = Color32::from_rgb(0x9A, 0x9A, 0xA6);

/// Where every control ended up, once the space left over was divided.
pub struct Pad {
    /// What is left for the console's screen.
    pub screen: Rect,
    /// The cross, as a square. Its arms are a third of the side.
    cross: Rect,
    a: Rect,
    b: Rect,
    start: Rect,
    select: Rect,
    /// The way back to the list.
    pub menu: Rect,
    /// The cable: waiting for another machine, or going to one.
    ///
    /// Next to the way out and as quiet, for the same reason. A telephone has no
    /// menu bar to hide it in, and the two things one reaches for while a game
    /// is running are leaving it and joining somebody.
    pub link: Rect,
}

impl Pad {
    /// Divides the window between the screen and the controls.
    ///
    /// Upright the controls go underneath, which is where a thumb reaches.
    /// Turned there is no room underneath —a landscape telephone is all width
    /// and no height— so they go on either side, and the screen keeps the middle.
    pub fn lay_out(area: Rect) -> Self {
        if area.height() >= area.width() {
            Self::upright(area)
        } else {
            Self::turned(area)
        }
    }

    fn upright(area: Rect) -> Self {
        // The controls are given a share of the bottom and the screen keeps
        // everything above, **centred in it**. Sizing the screen first and
        // handing the leftovers to the thumbs is what put the picture up under
        // the clock with a hand's width of nothing underneath it: the leftovers
        // of a telephone this tall are enormous.
        let band_height =
            (area.height() * 0.46).clamp(240.0, 470.0).min(area.height() - SCREEN_MIN);
        let (screen, band) = split_top(area, area.height() - band_height);

        // The row of small buttons is taken off the bottom first: it is the one
        // thing that must not move when the rest grows.
        let pills_row = (band.height() * 0.16).clamp(40.0, 58.0);
        let play = Rect::from_min_max(
            band.min,
            Pos2::new(band.right(), band.bottom() - pills_row - band.height() * 0.06),
        );
        // Both clusters hang off their own edge instead of being centred in half
        // the width each. Centred, they grow towards each other: on a telephone
        // 393 points wide the cross and the B button end up overlapping by a few
        // points, and a thumb landing in between presses both.
        let half = play.width() * 0.5;
        let cross_side = (half * 0.80).min(play.height() * 0.80).clamp(120.0, 240.0);
        let button = (half * 0.36).min(play.height() * 0.34).clamp(62.0, 104.0);

        let cross =
            square(Pos2::new(play.left() + MARGIN + cross_side * 0.5, play.center().y), cross_side);
        let (a, b) = action_buttons(
            Pos2::new(play.right() - MARGIN - action_reach(button), play.center().y),
            button,
        );
        let (start, select) = pills(band, pills_row);
        Self { screen, cross, a, b, start, select, menu: menu_corner(area, 0), link: menu_corner(area, 1) }
    }

    fn turned(area: Rect) -> Self {
        // Turned there is no room underneath —a landscape telephone is all width
        // and no height— so the controls go on either side and the screen keeps
        // the middle. The picture is 160×144: even after giving away a third of
        // the width it has more than it can grow into.
        let side = (area.width() * 0.30).clamp(150.0, 320.0);
        let (left, rest) = split_left(area, side);
        let (screen, right) = split_left(rest, rest.width() - side);

        let cross_side = (side * 0.80).min(area.height() * 0.62).clamp(120.0, 250.0);
        let button = (side * 0.36).min(area.height() * 0.30).clamp(62.0, 108.0);
        let height = area.center().y - side * 0.06;

        let cross = square(Pos2::new(left.left() + MARGIN + cross_side * 0.5, height), cross_side);
        let (a, b) = action_buttons(
            Pos2::new(right.right() - MARGIN - action_reach(button), height),
            button,
        );

        // Underneath the picture, which is the only strip of nothing left.
        let pills_row = (area.height() * 0.12).clamp(38.0, 54.0);
        let (start, select) = pills(screen, pills_row);
        Self { screen, cross, a, b, start, select, menu: menu_corner(area, 0), link: menu_corner(area, 1) }
    }

    /// Which of the eight are held down by the fingers currently on the glass.
    pub fn pressed(&self, touches: &Touches) -> [bool; 8] {
        let mut down = [false; 8];
        for point in touches.positions() {
            if self.cross.contains(point) {
                let arm = self.cross.width() * 0.5;
                let offset = point - self.cross.center();
                down[0] |= offset.y < -arm * DEAD_ZONE;
                down[1] |= offset.y > arm * DEAD_ZONE;
                down[2] |= offset.x < -arm * DEAD_ZONE;
                down[3] |= offset.x > arm * DEAD_ZONE;
            }
            down[4] |= inside_circle(self.a, point);
            down[5] |= inside_circle(self.b, point);
            down[6] |= self.start.contains(point);
            down[7] |= self.select.contains(point);
        }
        down
    }

    /// Draws them, lit where a finger is.
    pub fn paint(&self, painter: &Painter, down: &[bool; 8], accent: Color32, cable: Cable) {
        let arm = self.cross.width() / 3.0;
        let centre = self.cross.center();
        let lit = |on: bool| if on { accent } else { FACE };

        // The cross is four arms and not one shape so that each can light up on
        // its own: pressing up must not look like pressing the whole thing.
        for (index, offset) in [
            (0usize, Vec2::new(0.0, -arm)),
            (1, Vec2::new(0.0, arm)),
            (2, Vec2::new(-arm, 0.0)),
            (3, Vec2::new(arm, 0.0)),
        ] {
            let rect = Rect::from_center_size(centre + offset, Vec2::splat(arm));
            painter.rect_filled(rect, 3.0, lit(down[index]));
        }
        painter.rect_filled(Rect::from_center_size(centre, Vec2::splat(arm)), 3.0, FACE);
        painter.rect_stroke(self.cross, 6.0, Stroke::new(1.0, EDGE), StrokeKind::Inside);

        circle(painter, self.a, "A", down[4], accent);
        circle(painter, self.b, "B", down[5], accent);
        pill(painter, self.start, "START", down[6], accent);
        pill(painter, self.select, "SELECT", down[7], accent);

        // The way out and the cable, kept quiet: neither is something one reaches
        // for while playing, and bright buttons there would be pressed by
        // accident.
        painter.rect_filled(self.menu, 6.0, FACE);
        strokes(painter, &menu_icon(self.menu), self.menu, LABEL);

        // Three states and not two. Filled the moment the button was pressed, it
        // said "connecting" and "trading with somebody" in exactly the same red,
        // and a link that never came up looked from the outside like one that
        // had: the button went red at the start and stayed red for ever.
        let (face, icon) = match cable {
            Cable::Out => (FACE, LABEL),
            Cable::Joining => (FACE, accent),
            Cable::In => (accent, FACE),
        };
        painter.rect_filled(self.link, 6.0, face);
        strokes(painter, &link_icon(self.link), self.link, icon);
    }
}

/// What the cable button has to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cable {
    /// Nothing plugged in.
    Out,
    /// A connection being made, or made and not yet answered. Either way the
    /// other console is not trading anything yet.
    Joining,
    /// Joined, greeted, and carrying bytes.
    In,
}

/// The fingers currently on the glass.
#[derive(Default)]
pub struct Touches(HashMap<u64, Pos2>);

impl Touches {
    /// Takes in what happened since the last frame.
    pub fn update(&mut self, ctx: &Context) {
        ctx.input(|input| {
            for event in &input.events {
                let Event::Touch { id, phase, pos, .. } = event else {
                    continue;
                };
                match phase {
                    TouchPhase::Start | TouchPhase::Move => {
                        self.0.insert(id.0, *pos);
                    }
                    TouchPhase::End | TouchPhase::Cancel => {
                        self.0.remove(&id.0);
                    }
                }
            }
        });
    }

    fn positions(&self) -> impl Iterator<Item = Pos2> + '_ {
        self.0.values().copied()
    }

    /// Forgets every finger. Called on leaving the game, so that a button held
    /// down while the screen changed does not stay held for ever.
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

fn split_top(area: Rect, height: f32) -> (Rect, Rect) {
    let cut = area.top() + height;
    (
        Rect::from_min_max(area.min, Pos2::new(area.right(), cut)),
        Rect::from_min_max(Pos2::new(area.left(), cut), area.max),
    )
}

fn split_left(area: Rect, width: f32) -> (Rect, Rect) {
    let cut = area.left() + width;
    (
        Rect::from_min_max(area.min, Pos2::new(cut, area.bottom())),
        Rect::from_min_max(Pos2::new(cut, area.top()), area.max),
    )
}

fn square(centre: Pos2, side: f32) -> Rect {
    Rect::from_center_size(centre, Vec2::splat(side))
}

/// A above and to the right of B, the way they sit on the console itself: the
/// thumb rolls between them instead of reaching across.
fn action_buttons(centre: Pos2, diameter: f32) -> (Rect, Rect) {
    let reach = diameter * 0.62;
    (
        square(centre + Vec2::new(reach, -reach * 0.66), diameter),
        square(centre + Vec2::new(-reach, reach * 0.66), diameter),
    )
}

/// Half the width the two of them take together, which is what has to be kept
/// clear of the edge.
fn action_reach(diameter: f32) -> f32 {
    diameter * 0.62 + diameter * 0.5
}

/// START and SELECT, side by side along the bottom edge of whatever they are
/// given: small, because they are pressed between games and not during them.
fn pills(band: Rect, height: f32) -> (Rect, Rect) {
    let size = Vec2::new((band.width() * 0.30).clamp(96.0, 190.0), height);
    let y = band.bottom() - size.y * 0.5;
    let gap = size.x * 0.58;
    (
        Rect::from_center_size(Pos2::new(band.center().x + gap, y), size),
        Rect::from_center_size(Pos2::new(band.center().x - gap, y), size),
    )
}

/// The `index`-th small button along the top right corner, counting inwards.
fn menu_corner(area: Rect, index: usize) -> Rect {
    let side = 44.0;
    let gap = 6.0;
    let offset = (side + gap) * index as f32;
    Rect::from_min_size(Pos2::new(area.right() - side - offset, area.top()), Vec2::splat(side))
}

/// The three lines of a menu.
///
/// Drawn and not written, and that is the whole point of these two. What was
/// here before was `≡` and `⇄`, one character each and no code at all — and both
/// came out as the empty box a font puts in place of a character it does not
/// carry. egui brings its own fonts and neither of those two symbols is in any
/// of them; Android's are never asked. Lines and arrowheads are there on every
/// telephone because they are drawn by hand.
fn menu_icon(rect: Rect) -> Vec<[Pos2; 2]> {
    let centre = rect.center();
    let half = rect.width() * 0.22;
    let gap = rect.height() * 0.15;
    [-1.0, 0.0, 1.0]
        .into_iter()
        .map(|step| {
            let y = centre.y + gap * step;
            [Pos2::new(centre.x - half, y), Pos2::new(centre.x + half, y)]
        })
        .collect()
}

/// Two arrows passing each other, which is what a cable between two consoles is
/// for: something goes each way.
fn link_icon(rect: Rect) -> Vec<[Pos2; 2]> {
    let centre = rect.center();
    let half = rect.width() * 0.22;
    let gap = rect.height() * 0.12;
    let head = rect.width() * 0.10;

    let mut lines = Vec::with_capacity(6);
    // The upper one points right and the lower one left, mirrored through the
    // middle: `towards` is which way each is going.
    for (step, towards) in [(-1.0, 1.0), (1.0, -1.0)] {
        let y = centre.y + gap * step;
        let tip = Pos2::new(centre.x + half * towards, y);
        lines.push([Pos2::new(centre.x - half * towards, y), tip]);
        // The head as two strokes off the tip and not as a filled triangle: at
        // this size a triangle is a handful of pixels and comes out a blob.
        for slant in [-1.0, 1.0] {
            lines.push([tip, Pos2::new(tip.x - head * towards, tip.y + head * slant)]);
        }
    }
    lines
}

/// Draws an icon, with a line thick enough to be seen on glass.
fn strokes(painter: &Painter, lines: &[[Pos2; 2]], rect: Rect, colour: Color32) {
    let stroke = Stroke::new((rect.height() * 0.055).max(1.5), colour);
    for line in lines {
        painter.line_segment(*line, stroke);
    }
}

fn inside_circle(rect: Rect, point: Pos2) -> bool {
    rect.center().distance(point) <= rect.width() * 0.5
}

fn circle(painter: &Painter, rect: Rect, text: &str, down: bool, accent: Color32) {
    let radius = rect.width() * 0.5;
    painter.circle_filled(rect.center(), radius, if down { accent } else { FACE });
    painter.circle_stroke(rect.center(), radius, Stroke::new(1.0, EDGE));
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(radius * 0.8),
        if down { Color32::WHITE } else { LABEL },
    );
}

fn pill(painter: &Painter, rect: Rect, text: &str, down: bool, accent: Color32) {
    painter.rect_filled(rect, rect.height() * 0.5, if down { accent } else { FACE });
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(rect.height() * 0.52),
        if down { Color32::WHITE } else { LABEL },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Telephones to lay the controls out on, in points —not pixels—: what a
    /// finger cares about is the physical size, and points already carry the
    /// density. Every one of them minus the room the clock and the navigation
    /// bar take, because that is what the interface actually gets.
    ///
    /// The first is the one this was found wrong on.
    ///
    /// They are the shapes Akebia claims to lay out, and a landscape telephone
    /// narrower than about seven hundred points is not among them: two reachable
    /// clusters of buttons and a picture at two whole pixels do not fit across
    /// it, and something would have to be given up that is not ours to choose.
    const SCREENS: [(&str, f32, f32); 6] = [
        ("Mi 10T upright", 393.0, 797.0),
        ("Mi 10T turned", 845.0, 365.0),
        ("small telephone", 320.0, 500.0),
        ("tall telephone", 412.0, 900.0),
        ("tablet", 800.0, 1180.0),
        ("tablet turned", 1180.0, 780.0),
    ];

    fn area(width: f32, height: f32) -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 28.0), Vec2::new(width, height))
    }

    /// Every control, as a rectangle, with the name to complain about.
    fn controls(pad: &Pad) -> [(&'static str, Rect); 7] {
        [
            ("cross", pad.cross),
            ("A", pad.a),
            ("B", pad.b),
            ("start", pad.start),
            ("select", pad.select),
            ("menu", pad.menu),
            ("link", pad.link),
        ]
    }

    #[test]
    fn no_control_falls_outside_the_window() {
        for (name, width, height) in SCREENS {
            let area = area(width, height);
            let pad = Pad::lay_out(area);
            for (control, rect) in controls(&pad) {
                assert!(
                    area.contains_rect(rect),
                    "{name}: {control} at {rect:?} sticks out of {area:?}"
                );
            }
        }
    }

    #[test]
    fn no_two_controls_overlap() {
        for (name, width, height) in SCREENS {
            let pad = Pad::lay_out(area(width, height));
            let all = controls(&pad);
            for (i, (one, first)) in all.iter().enumerate() {
                for (other, second) in all.iter().skip(i + 1) {
                    assert!(
                        !first.intersects(*second),
                        "{name}: {one} and {other} share {:?}",
                        first.intersect(*second)
                    );
                }
            }
        }
    }

    /// A finger is about nine millimetres across, which on any of these screens
    /// is somewhere near forty points. Below that the misses start, and a
    /// control that cannot be hit is worse than one that is not there.
    #[test]
    fn every_control_is_big_enough_for_a_thumb() {
        for (name, width, height) in SCREENS {
            let pad = Pad::lay_out(area(width, height));
            for (control, rect) in controls(&pad) {
                let smallest = rect.width().min(rect.height());
                assert!(smallest >= 40.0, "{name}: {control} is only {smallest} points across");
            }
        }
    }

    /// The picture must not be squeezed into a sliver by the controls: below two
    /// whole pixels per Game Boy pixel it stops being worth looking at.
    #[test]
    fn the_screen_keeps_room_for_a_whole_factor_of_two() {
        for (name, width, height) in SCREENS {
            let pad = Pad::lay_out(area(width, height));
            let scale = (pad.screen.width() / 160.0).min(pad.screen.height() / 144.0);
            assert!(scale >= 2.0, "{name}: the screen only has room for {scale:.2}×");
        }
    }

    /// The cross answers with one direction in each quarter and two on the
    /// diagonals, and with nothing at all in the middle.
    #[test]
    fn the_cross_reads_directions_and_diagonals() {
        let pad = Pad::lay_out(area(393.0, 797.0));
        let centre = pad.cross.center();
        let arm = pad.cross.width() * 0.5;

        let press = |offset: Vec2| {
            let mut touches = Touches::default();
            touches.0.insert(1, centre + offset);
            pad.pressed(&touches)
        };

        // up, down, left, right, in that order.
        assert_eq!(press(Vec2::new(0.0, -arm * 0.7))[..4], [true, false, false, false]);
        assert_eq!(press(Vec2::new(0.0, arm * 0.7))[..4], [false, true, false, false]);
        assert_eq!(press(Vec2::new(-arm * 0.7, 0.0))[..4], [false, false, true, false]);
        assert_eq!(press(Vec2::new(arm * 0.7, 0.0))[..4], [false, false, false, true]);

        assert_eq!(
            press(Vec2::new(arm * 0.6, -arm * 0.6))[..4],
            [true, false, false, true],
            "up and right at once"
        );
        assert_eq!(
            press(Vec2::splat(0.0))[..4],
            [false; 4],
            "a thumb resting in the middle presses nothing"
        );
    }

    /// The two corner icons are drawn and not written, which turns them into
    /// arithmetic — and arithmetic is what this module is here to check.
    #[test]
    fn the_corner_icons_stay_inside_their_buttons() {
        for (name, width, height) in SCREENS {
            let pad = Pad::lay_out(area(width, height));
            for (control, rect, lines) in [
                ("menu", pad.menu, menu_icon(pad.menu)),
                ("link", pad.link, link_icon(pad.link)),
            ] {
                assert!(!lines.is_empty(), "{name}: {control} came out with no icon at all");
                for [from, to] in lines {
                    assert!(
                        rect.contains(from) && rect.contains(to),
                        "{name}: the {control} icon runs {from:?} to {to:?}, outside {rect:?}"
                    );
                }
            }
        }
    }

    /// Two fingers at once, which is the whole reason the touches are tracked by
    /// hand instead of through egui's single pointer.
    #[test]
    fn the_cross_and_a_button_can_be_held_together() {
        let pad = Pad::lay_out(area(393.0, 797.0));
        let mut touches = Touches::default();
        touches.0.insert(1, pad.cross.center() + Vec2::new(pad.cross.width() * 0.35, 0.0));
        touches.0.insert(2, pad.a.center());

        let down = pad.pressed(&touches);
        assert!(down[3], "right");
        assert!(down[4], "A");
    }
}
