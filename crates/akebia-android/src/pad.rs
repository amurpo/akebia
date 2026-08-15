//! The eight buttons, painted on the glass and listened to.
//!
//! # Why the fingers are tracked by hand
//!
//! egui hands out one pointer, because a mouse is one. A console needs at least
//! two at once —nobody runs and jumps otherwise— so the raw touch events are
//! read instead: each finger arrives with an identifier that lasts from the
//! moment it lands to the moment it lifts, and what this module keeps is where
//! every finger on the glass came down and where it is now. Which buttons those
//! positions fall on is then a question of geometry, and a finger that slides
//! from one button onto the next presses the next one, which is what a thumb on
//! real plastic does.
//!
//! The cross is the exception, and it is why the landing is kept at all. A thumb
//! leaning on a direction drifts outwards —leaning is drifting— and a cross read
//! only by where the finger is this instant lets go the moment it passes the
//! edge: a walk that stops for no reason the player can see. Whichever finger
//! came down on the cross goes on steering it until it lifts, and presses
//! nothing else on its way.

use std::collections::HashMap;

use akebia_core::Button;
use egui::{
    Align2, Color32, Context, Event, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind,
    TouchPhase, Vec2,
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

/// How much further a finger already out along one arm has to lean before the
/// arm beside it comes in too, as a share of how far out it is.
///
/// At nought the cross is a plain board: a stem in x and a stem in y, and every
/// corner square answers both. At one it has no diagonals left at all —whichever
/// axis is further ahead silences the other— and in between the wedge narrows.
/// The point of it is that near the middle the cross measures a lean in
/// millimetres, as a board does, and away from the middle it measures one in
/// degrees, as a thumb means it: out at the end of an arm a hair of drift is
/// drift, not a diagonal.
///
/// The rule is EmuFramework's, out of `VControllerDPad::getInput` — GBC.emu and
/// the rest of Robert Broglia's emulators, GPL-3.0-or-later, which is this
/// program's own licence. The idea is theirs and the arithmetic below is written
/// afresh; what Akebia adds is drawing the wedge the rule carves, so that the
/// two go on saying the same thing at any setting.
const DIAGONAL_BIAS: f32 = 0.30;

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
/// The corners of the cross, a shade darker than its arms: they belong to the
/// pad and they light like it, but the plus is what the eye should find first.
const CORNER: Color32 = Color32::from_rgb(0x1D, 0x1D, 0x22);

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

    /// Half the width of an arm of the cross, which is the one number both the
    /// reading and the painting are made of.
    ///
    /// Further than this from the middle in x is left or right, further in y is
    /// up or down, and further in both is the two of them at once: nine squares
    /// of a noughts and crosses board, of which the middle one says nothing,
    /// four are directions and four are diagonals. `DIAGONAL_BIAS` then bends the
    /// four lines between them outwards, and `corner` bends the painting with
    /// them.
    ///
    /// The two used to disagree, and that is what made a diagonal so hard to
    /// mean. Each arm was painted a sixth of the cross wide to either side and
    /// read at a ninth, so the outer third of every arm answered with the
    /// diagonal beside it: aiming at the middle of the right arm and landing a
    /// little high walked the player up and to the right. And the diagonals, for
    /// their part, took three fifths of the whole cross and were painted nowhere
    /// at all — a player hunting for one had nothing to aim at but the gap
    /// between two arms.
    fn stem(&self) -> f32 {
        self.cross.width() / 6.0
    }

    /// Which of the eight are held down by the fingers currently on the glass.
    pub fn pressed(&self, touches: &Touches) -> [bool; 8] {
        let mut down = [false; 8];
        let stem = self.stem();
        for finger in touches.fingers() {
            // Either it came down on the cross —and then it is the cross's until
            // it lifts, wherever it has wandered to since— or it is over it now,
            // having slid there off something else.
            if self.cross.contains(finger.landed) || self.cross.contains(finger.at) {
                let offset = finger.at - self.cross.center();
                // Each axis waits out its own stem, and then a share of however
                // far past its stem the other axis has already gone: `lean` is
                // what x has to beat, given y, and the other way about.
                let lean = |other: f32| stem + (other.abs() - stem).max(0.0) * DIAGONAL_BIAS;
                down[0] |= offset.y < -lean(offset.x);
                down[1] |= offset.y > lean(offset.x);
                down[2] |= offset.x < -lean(offset.y);
                down[3] |= offset.x > lean(offset.y);
                // A thumb steering does not also press whatever it has drifted
                // over: the two clusters are a hand apart and reaching from one
                // to the other is not a thing a player does by accident.
                continue;
            }
            down[4] |= inside_circle(self.a, finger.at);
            down[5] |= inside_circle(self.b, finger.at);
            down[6] |= self.start.contains(finger.at);
            down[7] |= self.select.contains(finger.at);
        }
        down
    }

    /// Draws them, lit where a finger is.
    pub fn paint(&self, painter: &Painter, down: &[bool; 8], accent: Color32, cable: Cable) {
        let arm = self.cross.width() / 3.0;
        let centre = self.cross.center();
        let lit = |on: bool| if on { accent } else { FACE };

        // The corners go down first, so the arms lie over the ends of them. Each
        // bridges the two arms it sits between and lights when both of those do,
        // which is what turns a diagonal from a bare patch of glass into
        // something a thumb can be aimed at and watched to answer.
        for (one, other, towards) in [
            (0usize, 3usize, Vec2::new(1.0, -1.0)),
            (1, 3, Vec2::new(1.0, 1.0)),
            (1, 2, Vec2::new(-1.0, 1.0)),
            (0, 2, Vec2::new(-1.0, -1.0)),
        ] {
            let face = if down[one] && down[other] { accent } else { CORNER };
            painter.add(Shape::convex_polygon(corner(self.cross, towards), face, Stroke::NONE));
        }

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

/// One finger, from where it came down to where it is now.
#[derive(Clone, Copy)]
struct Touch {
    landed: Pos2,
    at: Pos2,
}

/// The fingers currently on the glass.
#[derive(Default)]
pub struct Touches(HashMap<u64, Touch>);

impl Touches {
    /// Takes in what happened since the last frame.
    pub fn update(&mut self, ctx: &Context) {
        ctx.input(|input| {
            for event in &input.events {
                let Event::Touch { id, phase, pos, .. } = event else {
                    continue;
                };
                match phase {
                    TouchPhase::Start => self.land(id.0, *pos),
                    TouchPhase::Move => self.slide(id.0, *pos),
                    TouchPhase::End | TouchPhase::Cancel => {
                        self.0.remove(&id.0);
                    }
                }
            }
        });
    }

    /// A finger coming down.
    fn land(&mut self, id: u64, at: Pos2) {
        self.0.insert(id, Touch { landed: at, at });
    }

    /// A finger already down, moved. One nobody saw land is taken to have landed
    /// where it is now: `clear` throws the fingers away in the middle of a
    /// gesture —that is what it is for— and the moves that follow are all that
    /// is left of it.
    fn slide(&mut self, id: u64, to: Pos2) {
        self.0.entry(id).or_insert(Touch { landed: to, at: to }).at = to;
    }

    fn fingers(&self) -> impl Iterator<Item = Touch> + '_ {
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

/// The wedge filling one corner of the cross, between the two arms it joins.
///
/// Its point is the corner of the middle square, where the reading stops calling
/// a finger one direction and starts calling it two; its outer edge chamfers the
/// corner of the frame, which leaves the whole an octagon: eight faces for the
/// eight things a cross can say. `towards` is which of the four corners, as a
/// pair of signs.
///
/// The two long sides are the very lines `pressed` draws, `DIAGONAL_BIAS` and
/// all: at nought they lie flat along the arms and the wedge is the widest it
/// gets, and as the bias rises they close in on each other and the wedge narrows
/// to match. Not a point of what is painted here falls outside what answers the
/// diagonal, at any setting.
fn corner(cross: Rect, towards: Vec2) -> Vec<Pos2> {
    let stem = cross.width() / 6.0;
    // How far the wedge would reach along an arm if the bias were nought, and
    // then where the two sides actually cut the chamfer instead.
    let span = cross.width() * 0.5 - stem;
    let long = span / (1.0 + DIAGONAL_BIAS);
    let short = long * DIAGONAL_BIAS;
    let at = |x: f32, y: f32| cross.center() + Vec2::new(x * towards.x, y * towards.y);
    vec![at(stem, stem), at(stem + long, stem + short), at(stem + short, stem + long)]
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

    /// What one finger, come down at `at` and gone nowhere since, is holding.
    fn press(pad: &Pad, at: Pos2) -> [bool; 8] {
        let mut touches = Touches::default();
        touches.land(1, at);
        pad.pressed(&touches)
    }

    /// The four arms of the cross, as `paint` lays them out. The reading and the
    /// painting have to be given the same numbers or the test proves nothing, so
    /// these are the numbers, in one place, and both sides are held to them.
    fn arms(pad: &Pad) -> [(usize, Rect); 4] {
        let arm = pad.cross.width() / 3.0;
        let centre = pad.cross.center();
        [
            (0usize, Vec2::new(0.0, -arm)),
            (1, Vec2::new(0.0, arm)),
            (2, Vec2::new(-arm, 0.0)),
            (3, Vec2::new(arm, 0.0)),
        ]
        .map(|(index, offset)| (index, Rect::from_center_size(centre + offset, Vec2::splat(arm))))
    }

    /// The four corners, likewise, each with the two directions it stands for.
    fn corners(pad: &Pad) -> [(usize, usize, Vec<Pos2>); 4] {
        [
            (0usize, 3usize, Vec2::new(1.0, -1.0)),
            (1, 3, Vec2::new(1.0, 1.0)),
            (1, 2, Vec2::new(-1.0, 1.0)),
            (0, 2, Vec2::new(-1.0, -1.0)),
        ]
        .map(|(one, other, towards)| (one, other, corner(pad.cross, towards)))
    }

    /// The cross answers with one direction in each quarter and two on the
    /// diagonals, and with nothing at all in the middle.
    #[test]
    fn the_cross_reads_directions_and_diagonals() {
        let pad = Pad::lay_out(area(393.0, 797.0));
        let centre = pad.cross.center();
        let arm = pad.cross.width() * 0.5;
        let press = |offset: Vec2| press(&pad, centre + offset);

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
        touches.land(1, pad.cross.center() + Vec2::new(pad.cross.width() * 0.35, 0.0));
        touches.land(2, pad.a.center());

        let down = pad.pressed(&touches);
        assert!(down[3], "right");
        assert!(down[4], "A");
    }

    /// Everywhere an arm is painted it answers with that direction and nothing
    /// else. The corners of the arm are the point of it: they are where the old
    /// reading gave the diagonal to a thumb that had asked for a direction.
    #[test]
    fn every_point_of_a_painted_arm_reads_one_direction_alone() {
        for (name, width, height) in SCREENS {
            let pad = Pad::lay_out(area(width, height));
            for (index, arm) in arms(&pad) {
                // A hair in from the edges, which is as close as a promise about
                // a painted shape can be held without arguing over the outline.
                let inside = arm.shrink(0.5);
                let mut wanted = [false; 4];
                wanted[index] = true;
                for at in [
                    inside.left_top(),
                    inside.right_top(),
                    inside.left_bottom(),
                    inside.right_bottom(),
                    inside.center(),
                ] {
                    assert_eq!(
                        press(&pad, at)[..4],
                        wanted,
                        "{name}: the arm painted at {arm:?} does not read {index} at {at:?}"
                    );
                }
            }
        }
    }

    /// And everywhere a corner is painted it answers with both of the directions
    /// beside it, which is the thing that could not be aimed at before because
    /// nothing was painted there.
    #[test]
    fn every_point_of_a_painted_corner_reads_both_its_directions() {
        for (name, width, height) in SCREENS {
            let pad = Pad::lay_out(area(width, height));
            for (one, other, triangle) in corners(&pad) {
                let middle = triangle.iter().fold(Vec2::ZERO, |sum, at| sum + at.to_vec2()) / 3.0;
                let mut wanted = [false; 4];
                wanted[one] = true;
                wanted[other] = true;
                // The middle of the triangle and a step in from each of its
                // points: a diagonal has to answer in the thin of it as well.
                for vertex in &triangle {
                    let at = *vertex + (middle - vertex.to_vec2()) * 0.1;
                    assert_eq!(
                        press(&pad, at)[..4],
                        wanted,
                        "{name}: the corner painted at {triangle:?} does not read {one} and \
                         {other} at {at:?}"
                    );
                    assert!(
                        pad.cross.contains(*vertex),
                        "{name}: the corner painted at {triangle:?} leaves the cross"
                    );
                }
            }
        }
    }

    /// Near the middle the cross measures a lean in points and away from it in
    /// degrees, which is what `DIAGONAL_BIAS` buys. The same lean, sideways, is
    /// a diagonal close in and mere drift out at the end of the arm.
    #[test]
    fn the_same_lean_counts_for_less_further_out_along_an_arm() {
        let pad = Pad::lay_out(area(393.0, 797.0));
        let stem = pad.stem();
        let lean = |along: f32| {
            press(&pad, pad.cross.center() + Vec2::new(stem * along, -stem * 1.2))[..4].to_vec()
        };

        assert_eq!(lean(1.2), [true, false, false, true], "a lean of the same size, close in");
        assert_eq!(lean(2.8), [false, false, false, true], "and out at the end of the arm");
    }

    /// A thumb leaning on a direction drifts off the edge, and used to take the
    /// direction with it. Whichever finger came down on the cross keeps it.
    #[test]
    fn a_finger_that_lands_on_the_cross_keeps_it_wherever_it_goes() {
        let pad = Pad::lay_out(area(393.0, 797.0));
        let mut touches = Touches::default();
        touches.land(1, pad.cross.center() + Vec2::new(pad.cross.width() * 0.4, 0.0));
        assert!(pad.pressed(&touches)[3], "right, to begin with");

        touches.slide(1, pad.cross.center() + Vec2::new(pad.cross.width(), 0.0));
        assert!(pad.pressed(&touches)[3], "right, a whole cross past the edge of it");

        touches.slide(1, pad.a.center());
        let down = pad.pressed(&touches);
        assert!(down[3], "right, all the way across the glass");
        assert!(!down[4], "and A, which the thumb is sitting on, is not pressed by it");
    }
}
