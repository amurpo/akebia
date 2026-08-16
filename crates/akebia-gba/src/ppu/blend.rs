//! Mixing what is on top with what is underneath it.
//!
//! # Why this needed the renderer rebuilt underneath it
//!
//! Everything else the picture unit does can be drawn back to front and
//! forgotten: each layer paints over what is already there, skips its own
//! transparent pixels, and the nearest thing with something to say is simply
//! the last to write. Priority comes out of that for free, and nothing ever has
//! to look at what it covered.
//!
//! Blending is the one effect that has to. A window fading to black is the top
//! layer mixed with **the layer below it**, and by the time the top layer is
//! drawn that one has already been painted over and lost. So the line is no
//! longer built in the picture itself; it is built as two pixels deep — what is
//! on top and what is directly behind it — and resolved into colours once, at
//! the end. See [`Pixel`].
//!
//! Two deep and no more, because that is all the hardware keeps. Three
//! half-transparent layers do not accumulate.
//!
//! # Three effects out of one register
//!
//! `BLDCNT` names a set of layers as the **first target** and another set as
//! the **second**, and a mode:
//!
//! - **Alpha**: where a first target lies directly over a second, the two are
//!   mixed by the two coefficients in `BLDALPHA`. This is the one that needs
//!   both, and the only one that can fail for want of the right thing
//!   underneath.
//! - **Brighter** and **darker**: a first target is faded towards white or
//!   towards black by `BLDY`, with nothing underneath involved. This is how
//!   nearly every fade between screens in every game is done — which is why
//!   `BLDY` is written more than any other register here, tens of thousands of
//!   times in a few seconds of play.
//!
//! # The sprite that blends whatever the register says
//!
//! A sprite can declare itself semi-transparent in its own entry, and then it
//! is **always** a first target and **always** alpha-blended, whatever mode
//! `BLDCNT` is in and whether or not it names sprites at all. It is how a game
//! gets one ghost or one pane of glass without giving up the fade it is running
//! on everything else.
//!
//! If there is no second target under such a sprite there is nothing to mix it
//! with, and it falls back to whatever `BLDCNT` was going to do anyway.
//!
//! # Where it does not happen
//!
//! Inside a window that says not to. Each region of the screen carries a bit
//! for the colour effects, which is how a game fades the world to black and
//! leaves its text box unfaded — so this is asked before anything else here is
//! decided. See [`window`](super::window).

use super::Ppu;

/// The registers, none of which existed in this map until now.
pub const BLDCNT: u32 = 0x0400_0050;
pub const BLDALPHA: u32 = 0x0400_0052;
pub const BLDY: u32 = 0x0400_0054;

/// Which layers are the first target, which the second, and what to do where
/// they meet.
const FIRST_TARGET: u16 = 0x003F;
const MODE: u16 = 0x00C0;
const SECOND_TARGET: u16 = 0x3F00;
const SECOND_SHIFT: u32 = 8;

const MODE_OFF: u16 = 0 << 6;
const MODE_ALPHA: u16 = 1 << 6;
const MODE_BRIGHTER: u16 = 2 << 6;
const MODE_DARKER: u16 = 3 << 6;

/// A coefficient is five bits and everything above sixteen means sixteen — a
/// whole one. Sixteen is the unit here, not thirty-one, so the mixing shifts by
/// four.
const WHOLE: u16 = 16;
const COEFFICIENT: u16 = 0x1F;

/// The brightest a channel goes. Five bits a channel, three channels.
const FULL: u16 = 31;

/// Which layer a pixel came from, as the bit that names it in `BLDCNT`.
///
/// It is kept as the bit rather than as a number because that is the only thing
/// it is ever used for: asking whether this layer is one of the targets is then
/// an `and`, and no table has to agree with the register's order.
pub type Layer = u16;

pub const BG0: Layer = 1 << 0;
pub const BG1: Layer = 1 << 1;
pub const BG2: Layer = 1 << 2;
pub const BG3: Layer = 1 << 3;
pub const OBJ: Layer = 1 << 4;
pub const BACKDROP: Layer = 1 << 5;

/// Which background is which layer. Ordered so that the index is the number.
pub const BACKGROUNDS: [Layer; 4] = [BG0, BG1, BG2, BG3];

/// One pixel of a line being built, and where it came from.
///
/// The colour alone is not enough for any of this: whether a pixel is blended
/// depends on which layer drew it, and a colour has no memory of that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pixel {
    pub colour: u16,
    pub layer: Layer,
    /// A sprite that declared itself semi-transparent. Only ever true of a
    /// sprite, and it overrides the mode in `BLDCNT`.
    pub translucent: bool,
}

impl Pixel {
    /// Nothing at all — not even the backdrop.
    ///
    /// Its layer is no layer, so it can never be anybody's second target. That
    /// is the point of it: underneath the backdrop there is nothing to mix
    /// with, and a pixel of black claiming to be a layer would blend the
    /// backdrop into itself and darken a screen nobody asked to darken.
    pub const NONE: Self = Self { colour: 0, layer: 0, translucent: false };

    pub const fn new(colour: u16, layer: Layer) -> Self {
        Self { colour, layer, translucent: false }
    }
}

impl Ppu {
    /// Puts a pixel down, remembering what it covered.
    ///
    /// A layer writing over itself does **not** push the old pixel down: two
    /// sprites overlapping are one layer as far as the hardware is concerned,
    /// and one blended with the other is not an effect that exists.
    pub(super) fn put(&mut self, x: usize, pixel: Pixel) {
        // A layer a window does not allow here never happened. Dropping it
        // rather than covering it is the whole of what a window is, and doing
        // it in this one place is what makes every layer obey without any of
        // them knowing windows exist.
        if !self.allows(x, pixel.layer) {
            return;
        }
        if self.top[x].layer != pixel.layer {
            self.below[x] = self.top[x];
        }
        self.top[x] = pixel;
    }

    /// What a pixel finally is, once what is over it and what is under it are
    /// both known.
    pub(super) fn blended(&self, x: usize) -> u16 {
        // A region can switch the effects off, which is how a game fades the
        // world to black and leaves its text box unfaded. It covers the
        // declared sprite as well: that one ignores the mode, not the window.
        if !self.effects_at(x) {
            return self.top[x].colour;
        }

        let (top, below) = (self.top[x], self.below[x]);
        // The second set is the same six bits eight places up, so it has to
        // come down before a layer's bit means anything against it.
        let second = (self.bldcnt & SECOND_TARGET) >> SECOND_SHIFT;
        let backed = second & below.layer != 0;

        // Switched off is the ordinary case — most games blend for a moment
        // between screens and not at all the rest of the time — and a sprite
        // that declared itself is the one thing that still happens when it is.
        if self.bldcnt & MODE == MODE_OFF && !top.translucent {
            return top.colour;
        }

        // A semi-transparent sprite blends whatever the mode says, and is a
        // first target whether or not the register names sprites. It needs
        // something to mix with, though, and if there is nothing it falls
        // through to the ordinary rules below.
        if top.translucent && backed {
            return self.alpha(top.colour, below.colour);
        }
        if self.bldcnt & FIRST_TARGET & top.layer == 0 {
            return top.colour;
        }
        match self.bldcnt & MODE {
            MODE_ALPHA if backed => self.alpha(top.colour, below.colour),
            MODE_BRIGHTER => fade(top.colour, self.evy(), FULL),
            MODE_DARKER => fade(top.colour, self.evy(), 0),
            // Either the mode is off, or it is alpha and there was nothing of
            // the right kind underneath. Both leave the pixel alone.
            _ => top.colour,
        }
    }

    /// The two coefficients mixed, channel by channel.
    ///
    /// They do not have to add up to one. A game can set both to a whole and
    /// get a colour brighter than either, which is what makes a highlight, and
    /// that is why the sum is clamped rather than assumed.
    fn alpha(&self, first: u16, second: u16) -> u16 {
        let eva = (self.bldalpha & COEFFICIENT).min(WHOLE);
        let evb = ((self.bldalpha >> 8) & COEFFICIENT).min(WHOLE);
        channels(first, second, |a, b| (((a * eva) + (b * evb)) >> 4).min(FULL))
    }

    fn evy(&self) -> u16 {
        (self.bldy & COEFFICIENT).min(WHOLE)
    }

    pub(super) fn read_blend8(&self, addr: u32) -> u8 {
        let half = |value: u16| (value >> ((addr & 1) * 8)) as u8;
        match addr & !1 {
            BLDCNT => half(self.bldcnt),
            BLDALPHA => half(self.bldalpha),
            // How far a fade has got is write-only, like the scroll positions.
            _ => 0,
        }
    }

    pub(super) fn write_blend8(&mut self, addr: u32, value: u8) {
        let widened = |existing: u16| -> u16 {
            let shift = (addr & 1) * 8;
            (existing & !(0xFFu16 << shift)) | (u16::from(value) << shift)
        };
        match addr & !1 {
            BLDCNT => self.bldcnt = widened(self.bldcnt),
            BLDALPHA => self.bldalpha = widened(self.bldalpha),
            BLDY => self.bldy = widened(self.bldy),
            _ => {}
        }
    }
}

/// A colour moved part of the way towards another one, the same distance in
/// every channel.
///
/// Both brightness effects are this: towards white for one and towards black
/// for the other. Writing them as one says what they have in common, which is
/// that neither mixes anything — the layer underneath is not consulted, and a
/// fade to black covers a screen with nothing behind it just as well.
fn fade(colour: u16, evy: u16, towards: u16) -> u16 {
    channels(colour, colour, |a, _| {
        if towards > a { a + (((towards - a) * evy) >> 4) } else { a - (((a - towards) * evy) >> 4) }
    })
}

/// Applies a rule to each of the three five-bit channels of two colours.
fn channels(first: u16, second: u16, mut rule: impl FnMut(u16, u16) -> u16) -> u16 {
    let mut out = 0;
    for shift in [0, 5, 10] {
        let channel = rule((first >> shift) & FULL, (second >> shift) & FULL);
        out |= (channel & FULL) << shift;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rgb(r: u16, g: u16, b: u16) -> u16 {
        r | (g << 5) | (b << 10)
    }

    /// A machine with a line already two pixels deep, so that only the
    /// registers are left to vary.
    fn over(top: Pixel, below: Pixel) -> Ppu {
        let mut ppu = Ppu::new();
        ppu.top[0] = top;
        ppu.below[0] = below;
        ppu
    }

    const RED: u16 = rgb(31, 0, 0);
    const BLUE: u16 = rgb(0, 0, 31);
    const GREY: u16 = rgb(16, 16, 16);

    #[test]
    fn with_the_mode_off_a_pixel_is_left_alone() {
        let mut ppu = over(Pixel::new(RED, BG0), Pixel::new(BLUE, BG1));
        ppu.bldcnt = BG0 | (BG1 << 8);
        assert_eq!(ppu.blended(0), RED, "named as a target, but nothing to do");
    }

    /// Half of one and half of the other is the middle of the two.
    #[test]
    fn alpha_mixes_the_two_by_their_coefficients() {
        let mut ppu = over(Pixel::new(RED, BG0), Pixel::new(BLUE, BG1));
        ppu.bldcnt = BG0 | MODE_ALPHA | (BG1 << 8);
        ppu.bldalpha = 8 | (8 << 8);
        assert_eq!(ppu.blended(0), rgb(15, 0, 15), "half each, rounded down");
    }

    /// The coefficients do not have to add up to a whole. Both at full is how a
    /// game makes a highlight, and the channels have to clamp rather than wrap.
    #[test]
    fn a_sum_brighter_than_the_screen_is_clamped_and_not_wrapped() {
        let mut ppu = over(Pixel::new(GREY, BG0), Pixel::new(GREY, BG1));
        ppu.bldcnt = BG0 | MODE_ALPHA | (BG1 << 8);
        ppu.bldalpha = 16 | (16 << 8);
        assert_eq!(ppu.blended(0), rgb(31, 31, 31), "clamped at the top, not rolled over");
    }

    /// Anything above sixteen is sixteen: five bits of register and only
    /// seventeen meanings.
    #[test]
    fn a_coefficient_past_a_whole_is_a_whole() {
        let mut ppu = over(Pixel::new(RED, BG0), Pixel::new(BLUE, BG1));
        ppu.bldcnt = BG0 | MODE_ALPHA | (BG1 << 8);
        ppu.bldalpha = 31; // and none of the second
        assert_eq!(ppu.blended(0), RED, "a whole of the first and none of the second");
    }

    /// Alpha is the one effect that needs both. Over the wrong layer it does
    /// nothing at all rather than mixing with whatever happens to be there.
    #[test]
    fn alpha_over_a_layer_that_is_not_a_second_target_does_nothing() {
        let mut ppu = over(Pixel::new(RED, BG0), Pixel::new(BLUE, BG2));
        ppu.bldcnt = BG0 | MODE_ALPHA | (BG1 << 8);
        assert_eq!(ppu.blended(0), RED);
    }

    #[test]
    fn a_full_fade_reaches_white_and_black() {
        for (mode, expected) in [(MODE_BRIGHTER, rgb(31, 31, 31)), (MODE_DARKER, 0)] {
            let mut ppu = over(Pixel::new(GREY, BG0), Pixel::new(BLUE, BG1));
            ppu.bldcnt = BG0 | mode;
            ppu.bldy = 16;
            assert_eq!(ppu.blended(0), expected);
        }
    }

    #[test]
    fn a_fade_of_nothing_changes_nothing() {
        for mode in [MODE_BRIGHTER, MODE_DARKER] {
            let mut ppu = over(Pixel::new(GREY, BG0), Pixel::new(BLUE, BG1));
            ppu.bldcnt = BG0 | mode;
            ppu.bldy = 0;
            assert_eq!(ppu.blended(0), GREY);
        }
    }

    /// Halfway is halfway, and the two directions are not the same sum: towards
    /// white a channel gains a share of what is left above it, towards black it
    /// loses a share of itself.
    #[test]
    fn a_half_fade_goes_half_the_remaining_distance() {
        let mut ppu = over(Pixel::new(GREY, BG0), Pixel::NONE);
        ppu.bldcnt = BG0 | MODE_BRIGHTER;
        ppu.bldy = 8;
        assert_eq!(ppu.blended(0), rgb(23, 23, 23), "16 plus half of the 15 above it");

        ppu.bldcnt = BG0 | MODE_DARKER;
        assert_eq!(ppu.blended(0), rgb(8, 8, 8), "16 less half of itself");
    }

    /// A fade needs nothing underneath it. This is the difference from alpha
    /// that matters most in practice: a screen fading to black fades the whole
    /// screen, backdrop included.
    #[test]
    fn a_fade_works_with_nothing_underneath() {
        let mut ppu = over(Pixel::new(GREY, BACKDROP), Pixel::NONE);
        ppu.bldcnt = BACKDROP | MODE_DARKER;
        ppu.bldy = 16;
        assert_eq!(ppu.blended(0), 0);
    }

    #[test]
    fn a_layer_that_is_not_a_first_target_is_left_alone() {
        let mut ppu = over(Pixel::new(RED, BG1), Pixel::new(BLUE, BG2));
        ppu.bldcnt = BG0 | MODE_DARKER;
        ppu.bldy = 16;
        assert_eq!(ppu.blended(0), RED, "the register names BG0, and this is BG1");
    }

    /// A semi-transparent sprite blends against the register, not with it: the
    /// mode is off and sprites are not named a first target, and it blends
    /// anyway.
    #[test]
    fn a_semi_transparent_sprite_blends_whatever_the_register_says() {
        let mut top = Pixel::new(RED, OBJ);
        top.translucent = true;
        let mut ppu = over(top, Pixel::new(BLUE, BG1));
        ppu.bldcnt = MODE_OFF | (BG1 << 8);
        ppu.bldalpha = 8 | (8 << 8);
        assert_eq!(ppu.blended(0), rgb(15, 0, 15));
    }

    /// With nothing of the right kind underneath there is nothing to mix, and
    /// it falls back to whatever the register was going to do anyway.
    #[test]
    fn a_semi_transparent_sprite_over_nothing_falls_back_to_the_register() {
        let mut top = Pixel::new(GREY, OBJ);
        top.translucent = true;
        let mut ppu = over(top, Pixel::new(BLUE, BG3));
        ppu.bldcnt = OBJ | MODE_DARKER | (BG1 << 8);
        ppu.bldy = 16;
        assert_eq!(ppu.blended(0), 0, "no second target under it, so the fade applies");

        // And with sprites not named at all, nothing happens to it.
        ppu.bldcnt = MODE_DARKER | (BG1 << 8);
        assert_eq!(ppu.blended(0), GREY);
    }

    /// Two sprites overlapping are one layer. Pushing one under the other would
    /// let a game blend a sprite with a sprite, which is not an effect the
    /// hardware has.
    #[test]
    fn a_layer_drawn_over_itself_does_not_become_its_own_backing() {
        let mut ppu = Ppu::new();
        ppu.top[0] = Pixel::new(BLUE, BG1);
        ppu.put(0, Pixel::new(GREY, OBJ));
        ppu.put(0, Pixel::new(RED, OBJ));
        assert_eq!(ppu.top[0], Pixel::new(RED, OBJ));
        assert_eq!(ppu.below[0], Pixel::new(BLUE, BG1), "still the background, not the sprite");
    }

    #[test]
    fn a_new_layer_pushes_the_old_one_underneath() {
        let mut ppu = Ppu::new();
        ppu.put(0, Pixel::new(BLUE, BG3));
        ppu.put(0, Pixel::new(RED, BG1));
        assert_eq!(ppu.top[0], Pixel::new(RED, BG1));
        assert_eq!(ppu.below[0], Pixel::new(BLUE, BG3));
    }

    /// How far a fade has got cannot be read back, like the scroll positions.
    /// The other two can.
    #[test]
    fn the_registers_read_back_what_was_written_except_the_fade() {
        let mut ppu = Ppu::new();
        ppu.write_blend8(BLDCNT, 0x41);
        ppu.write_blend8(BLDCNT + 1, 0x3F);
        assert_eq!(ppu.read_blend8(BLDCNT), 0x41);
        assert_eq!(ppu.read_blend8(BLDCNT + 1), 0x3F);

        ppu.write_blend8(BLDALPHA, 0x08);
        assert_eq!(ppu.read_blend8(BLDALPHA), 0x08);

        ppu.write_blend8(BLDY, 0x10);
        assert_eq!(ppu.read_blend8(BLDY), 0, "write-only");
        assert_eq!(ppu.bldy, 0x10, "but it went in");
    }
}
