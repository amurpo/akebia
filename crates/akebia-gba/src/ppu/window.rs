//! The rectangles that decide which layers exist where.
//!
//! # What a window is, and what it is not
//!
//! It is not something drawn. A window is a region of the screen with its own
//! answer to the question every layer is asked at every pixel: *are you allowed
//! here?* Inside window 0 a game may show two backgrounds and no sprites;
//! outside it, everything; inside window 1, only the layer holding its status
//! bar. Nothing about the layers themselves changes — the same background is
//! drawn from the same map with the same scroll — it is only permitted in some
//! columns and not in others.
//!
//! Which makes leaving them out a particular kind of wrong. A machine without
//! windows does not lose an effect; it **shows things that were meant to be
//! hidden**, over the whole screen, in whatever state the game left them.
//! That is the reported fault: a menu with a band of scrambled tiles across the
//! top, which is a layer the game had restricted to a rectangle somewhere else
//! and never expected to see.
//!
//! # Three regions, and the order they win in
//!
//! Window 0 first, then window 1, then the region cut out by sprites, then
//! everything else. Where two overlap the earlier one decides, and that order
//! is fixed in hardware — it is not a priority a game can set.
//!
//! Each region carries six bits: one per layer, and one saying whether the
//! colour effects happen there. That last one is why this module and
//! [`blend`](super::blend) meet: a game fades the world to black and leaves its
//! text box unfaded by putting the text box in a window with that bit clear.
//!
//! # The bit that means two different things
//!
//! Bit 5 of a region is *effects*. Bit 5 of a [`Layer`](super::blend::Layer) is
//! the *backdrop*. They are the same bit position in two different registers
//! and they have nothing to do with each other, so nothing here may test one
//! against the other — see [`Ppu::allows`], which masks the layer down to the
//! five a window can actually hide before comparing anything.
//!
//! # The far edge, and the pairs that make no sense
//!
//! A window is given its near edge and its far edge in one halfword, and the
//! far edge is exclusive. Games write pairs that describe nothing — a right
//! edge past the screen, or a right edge left of the left one — and the
//! hardware reads both of those as "to the edge of the screen" rather than as
//! an empty window or a wrapped one. Guessing otherwise makes a window vanish
//! where hardware shows one.

use super::blend::Layer;
use super::Ppu;
use crate::{SCREEN_HEIGHT, SCREEN_WIDTH};

/// The two windows' edges, and the two registers saying what is in each region.
pub const WIN0H: u32 = 0x0400_0040;
const WIN1H: u32 = 0x0400_0042;
const WIN0V: u32 = 0x0400_0044;
const WIN1V: u32 = 0x0400_0046;
const WININ: u32 = 0x0400_0048;
pub const WINOUT: u32 = 0x0400_004A;

/// A region's six bits: the five layers it may show, and whether the colour
/// effects happen in it.
const CONTENTS: u16 = 0x003F;
/// The five layers a window can hide. The backdrop is not among them: there is
/// no bit for it, and it shows wherever nothing else does.
const CLIPPABLE: u16 = 0x001F;
const EFFECTS: u16 = 1 << 5;

/// Every layer shown and the effects on, which is what every pixel gets when no
/// window is switched on at all.
///
/// It is also what a machine starts with. The mask is only meaningful once a
/// line has been prepared, and a machine that began with it clear would answer
/// "nothing is allowed anywhere" to anything that asked before the first line —
/// which is not a state the hardware has.
pub(super) const EVERYTHING: u16 = CONTENTS;

/// `DISPCNT`'s three switches. **If none of them is set there are no windows**,
/// and the outside register does not apply to anything — a machine that let it
/// apply anyway would hide layers on every game that configured these registers
/// at startup and then never used them, which is most of them.
const WIN0_ON: u16 = 1 << 13;
const WIN1_ON: u16 = 1 << 14;
const OBJ_WIN_ON: u16 = 1 << 15;

/// Where a sprite pass writes what it draws.
///
/// The sprites that cut the third region are drawn by the same code as every
/// other sprite — same shapes, same mirroring, same rotation — and the only
/// difference is that a covered pixel marks a mask instead of becoming a
/// colour. Two copies of that geometry would be two places for it to go wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Picture,
    WindowMask,
}

impl Ppu {
    /// Whether any window is switched on.
    pub(super) fn windows_on(&self) -> bool {
        self.dispcnt & (WIN0_ON | WIN1_ON | OBJ_WIN_ON) != 0
    }

    /// Works out, for every column of this line, which layers may be drawn.
    ///
    /// Done once before the line rather than asked per layer per pixel: the
    /// regions are the same for all of them, and the sprites that cut the third
    /// one have to be drawn before anything else is, since they decide where
    /// the others are allowed.
    pub(super) fn prepare_windows(&mut self, line: usize) {
        if !self.windows_on() {
            self.allowed.fill(EVERYTHING);
            return;
        }

        let by_sprites = self.dispcnt & OBJ_WIN_ON != 0;
        if by_sprites {
            self.obj_window.fill(false);
            self.mark_window_sprites(line);
        }

        let first = (self.dispcnt & WIN0_ON != 0).then(|| self.columns(0, line)).flatten();
        let second = (self.dispcnt & WIN1_ON != 0).then(|| self.columns(1, line)).flatten();
        let inside = [self.winin & CONTENTS, (self.winin >> 8) & CONTENTS];
        let cut = (self.winout >> 8) & CONTENTS;
        let outside = self.winout & CONTENTS;

        let mut allowed = [outside; SCREEN_WIDTH];
        for (x, region) in allowed.iter_mut().enumerate() {
            *region = if first.as_ref().is_some_and(|edges| edges.contains(&x)) {
                inside[0]
            } else if second.as_ref().is_some_and(|edges| edges.contains(&x)) {
                inside[1]
            } else if by_sprites && self.obj_window[x] {
                cut
            } else {
                outside
            };
        }
        *self.allowed = allowed;
    }

    /// Which columns of this line a window covers, or nothing if it does not
    /// reach this line at all.
    fn columns(&self, index: usize, line: usize) -> Option<std::ops::Range<usize>> {
        let (top, bottom) = edges(self.win_v[index]);
        if line < top || line >= far(top, bottom, SCREEN_HEIGHT) {
            return None;
        }
        let (left, right) = edges(self.win_h[index]);
        let right = far(left, right, SCREEN_WIDTH);
        (left < right).then_some(left..right)
    }

    /// Whether a layer may be drawn at a column.
    ///
    /// The backdrop always may. It is not one of the six bits — see the note at
    /// the top of this module about bit 5 meaning two different things — and
    /// masking it down is what keeps a game with the effects bit clear from
    /// losing its backdrop.
    pub(super) fn allows(&self, x: usize, layer: Layer) -> bool {
        let layer = layer & CLIPPABLE;
        layer == 0 || self.allowed[x] & layer != 0
    }

    /// Whether the colour effects happen at a column.
    pub(super) fn effects_at(&self, x: usize) -> bool {
        self.allowed[x] & EFFECTS != 0
    }

    pub(super) fn read_window8(&self, addr: u32) -> u8 {
        let half = |value: u16| (value >> ((addr & 1) * 8)) as u8;
        match addr & !1 {
            // The edges are write-only, like the scroll positions and the fade.
            WININ => half(self.winin),
            WINOUT => half(self.winout),
            _ => 0,
        }
    }

    pub(super) fn write_window8(&mut self, addr: u32, value: u8) {
        let widened = |existing: u16| -> u16 {
            let shift = (addr & 1) * 8;
            (existing & !(0xFFu16 << shift)) | (u16::from(value) << shift)
        };
        match addr & !1 {
            WIN0H => self.win_h[0] = widened(self.win_h[0]),
            WIN1H => self.win_h[1] = widened(self.win_h[1]),
            WIN0V => self.win_v[0] = widened(self.win_v[0]),
            WIN1V => self.win_v[1] = widened(self.win_v[1]),
            WININ => self.winin = widened(self.winin),
            WINOUT => self.winout = widened(self.winout),
            _ => {}
        }
    }
}

/// The two edges packed into one halfword: the near one on top.
fn edges(register: u16) -> (usize, usize) {
    ((register >> 8) as usize, (register & 0xFF) as usize)
}

/// The far edge as the hardware reads it.
///
/// Past the screen, or before the near edge, both mean the screen's own edge.
/// Games write both — a window closing by walking its right edge left ends up
/// there — and reading either as an empty window loses a window that hardware
/// still shows.
fn far(near: usize, far: usize, edge: usize) -> usize {
    if far > edge || near > far { edge } else { far }
}

#[cfg(test)]
mod tests {
    use super::super::blend;
    use super::*;

    /// A machine with one window covering a square in the top-left corner, and
    /// nothing else switched on.
    fn windowed() -> Ppu {
        let mut ppu = Ppu::new();
        ppu.dispcnt = WIN0_ON;
        ppu.win_h[0] = (10 << 8) | 20; // columns 10..20
        ppu.win_v[0] = 5; // lines 0..5
        ppu.winin = blend::BG0 | EFFECTS;
        ppu.winout = blend::BG1;
        ppu
    }

    #[test]
    fn a_layer_named_inside_is_allowed_only_inside() {
        let mut ppu = windowed();
        ppu.prepare_windows(0);
        assert!(ppu.allows(15, blend::BG0), "inside, where BG0 is named");
        assert!(!ppu.allows(15, blend::BG1), "and BG1 is not");
        assert!(ppu.allows(100, blend::BG1), "outside, where BG1 is");
        assert!(!ppu.allows(100, blend::BG0));
    }

    /// A window is a rectangle and not a band. A line below it is outside even
    /// in the columns the window covers.
    #[test]
    fn a_line_past_the_bottom_edge_is_outside_everywhere() {
        let mut ppu = windowed();
        ppu.prepare_windows(9);
        assert!(!ppu.allows(15, blend::BG0), "past the bottom edge");
        assert!(ppu.allows(15, blend::BG1));
    }

    /// Both far edges are exclusive: the column and the line they name are the
    /// first ones outside.
    #[test]
    fn the_far_edges_are_the_first_pixel_outside() {
        let mut ppu = windowed();
        ppu.prepare_windows(4);
        assert!(ppu.allows(19, blend::BG0), "the last column inside");
        assert!(!ppu.allows(20, blend::BG0), "and the first outside");

        ppu.prepare_windows(5);
        assert!(!ppu.allows(15, blend::BG0), "the line the bottom edge names");
    }

    /// With nothing switched on, the outside register applies to nothing. Games
    /// configure these at startup and then never use them, so letting it apply
    /// would hide layers across most of the library.
    #[test]
    fn with_no_window_switched_on_nothing_is_hidden() {
        let mut ppu = windowed();
        ppu.dispcnt = 0;
        ppu.winout = 0; // hides everything, and must not be consulted
        ppu.prepare_windows(0);
        for layer in [blend::BG0, blend::BG1, blend::BG2, blend::BG3, blend::OBJ] {
            assert!(ppu.allows(15, layer), "{layer:#X}");
        }
        assert!(ppu.effects_at(15));
    }

    /// The backdrop has no bit of its own and is never hidden. Bit 5 of a
    /// region means *effects*, and testing the backdrop against it would take
    /// the backdrop away from every game that clears that bit.
    #[test]
    fn the_backdrop_is_not_a_windows_to_hide() {
        let mut ppu = windowed();
        ppu.winin = 0;
        ppu.winout = 0;
        ppu.prepare_windows(0);
        assert!(ppu.allows(15, blend::BACKDROP), "inside a window that shows nothing");
        assert!(ppu.allows(100, blend::BACKDROP), "and outside it");
        assert!(!ppu.effects_at(15), "while the effects really are off");
    }

    /// The effects bit is what lets a game fade the world and leave a text box
    /// alone.
    #[test]
    fn the_effects_bit_is_per_region() {
        let mut ppu = windowed();
        ppu.prepare_windows(0);
        assert!(ppu.effects_at(15), "inside, where it is set");
        assert!(!ppu.effects_at(100), "outside, where it is not");
    }

    /// Window 0 wins where the two overlap, and it is the hardware's order and
    /// not a choice a game makes.
    #[test]
    fn the_first_window_wins_where_they_overlap() {
        let mut ppu = windowed();
        ppu.dispcnt = WIN0_ON | WIN1_ON;
        ppu.win_h[1] = 240;
        ppu.win_v[1] = 160;
        ppu.winin = blend::BG0 | (blend::BG2 << 8);

        ppu.prepare_windows(0);
        assert!(ppu.allows(15, blend::BG0), "inside both, and the first decides");
        assert!(!ppu.allows(15, blend::BG2));
        assert!(ppu.allows(50, blend::BG2), "inside the second alone");
    }

    /// A right edge left of the left one, or past the screen, means the edge of
    /// the screen. It is what a window closing by walking one edge produces,
    /// and reading it as an empty window loses a window hardware still shows.
    #[test]
    fn a_far_edge_that_makes_no_sense_reaches_the_screens_edge() {
        assert_eq!(far(10, 20, 240), 20, "an ordinary pair");
        assert_eq!(far(200, 10, 240), 240, "the far edge before the near one");
        assert_eq!(far(10, 250, 240), 240, "and past the screen");

        let mut ppu = windowed();
        ppu.win_h[0] = (200 << 8) | 10;
        ppu.prepare_windows(0);
        assert!(ppu.allows(230, blend::BG0), "so the window runs to the right-hand edge");
        assert!(!ppu.allows(100, blend::BG0), "and starts where it said");
    }

    /// A window whose near edge is off the screen covers nothing, which is
    /// different from covering everything.
    #[test]
    fn a_window_that_starts_past_the_screen_covers_nothing() {
        let mut ppu = windowed();
        ppu.win_h[0] = (250 << 8) | 255;
        ppu.prepare_windows(0);
        assert!(!ppu.allows(0, blend::BG0));
        assert!(ppu.allows(0, blend::BG1), "it is all outside");
    }

    #[test]
    fn the_regions_read_back_and_the_edges_do_not() {
        let mut ppu = Ppu::new();
        ppu.write_window8(WININ, 0x3F);
        ppu.write_window8(WININ + 1, 0x1F);
        assert_eq!(ppu.read_window8(WININ), 0x3F);
        assert_eq!(ppu.read_window8(WININ + 1), 0x1F);

        ppu.write_window8(WINOUT, 0x21);
        assert_eq!(ppu.read_window8(WINOUT), 0x21);

        ppu.write_window8(WIN0H, 0xF0);
        assert_eq!(ppu.read_window8(WIN0H), 0, "write-only, like the scroll");
        assert_eq!(ppu.win_h[0], 0x00F0, "but it went in");
    }
}
