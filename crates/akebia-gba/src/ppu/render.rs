//! Turning what is in video memory into pixels.
//!
//! # A line at a time, and why not a frame
//!
//! Because a game changes things between lines and expects that to show. The
//! scroll registers, the palette, even the video mode can be written in the gap
//! at the end of one line and be different for the next, and effects are built
//! on exactly that — a sky that shifts as it goes down the screen is one
//! background drawn with a different offset in every one of its lines.
//!
//! Drawing a whole frame from the state at the end of it would collapse all of
//! that into whatever the last line happened to want. So a line is drawn as the
//! beam finishes it, out of the registers as they are at that moment, and is
//! never touched again.
//!
//! # The six modes are three ideas
//!
//! Modes 0 to 2 are **tiled**: video memory holds a small set of blocks and a
//! map saying where each one goes, so a screenful of graphics costs a few
//! kilobytes. That is how a game with more artwork than memory works, and it is
//! what nearly everything uses.
//!
//! Modes 3 to 5 are **bitmaps**: video memory holds the picture itself, a
//! colour or a palette index per pixel. Simple, and expensive — mode 3 spends
//! 75 KiB of the 96 available on a single screen, which leaves no room for a
//! second one to draw into while the first is shown. Mode 4 halves the cost by
//! storing palette indices instead of colours, and gets two screens out of it;
//! mode 5 keeps the colours and shrinks the picture instead.
//!
//! Games use the bitmap modes for still pictures and for effects that are
//! easier to compute than to tile. Test ROMs use them because a number drawn on
//! screen is the simplest possible way to report a result.
//!
//! # What is here
//!
//! The three bitmap modes, and the scrolling backgrounds of the tiled ones —
//! all four of mode 0's and the two of mode 1's, drawn in priority order with
//! the further ones showing through wherever the nearer have nothing.
//!
//! Not here: the backgrounds that rotate and scale, which are the other half of
//! modes 1 and 2; sprites, which sit on top of everything; and the windows,
//! mosaic and blending that the rest of the register block is for.
//!
//! An unwritten mode draws the backdrop rather than nothing, which is what the
//! hardware does and is also honest: a screen in the backdrop colour says the
//! picture is not being drawn, where leaving the previous frame's pixels would
//! say nothing at all.
//!
//! # Transparency is index zero, everywhere
//!
//! In a palette, entry zero is not a colour: it means nothing is there and
//! whatever is behind shows through. That one rule is what lets four
//! backgrounds share a screen, and it is why a game can draw over what is
//! already on screen without clearing it first. The backdrop — the first colour
//! of the palette — is what shows when nothing at all has anything to say.

use super::{Ppu, PIXELS};
use crate::{SCREEN_HEIGHT, SCREEN_WIDTH};

/// White, which is every one of the fifteen bits set.
const WHITE: u16 = 0x7FFF;

/// Where the second of the two pictures starts, in the modes that have one.
const SECOND_FRAME: usize = 0xA000;

/// `DISPCNT`'s bit for the second picture, and the four that switch the
/// backgrounds on. The bitmap modes are all drawn as background 2.
const FRAME_SELECT: u16 = 1 << 4;
const BG0_ENABLED: u16 = 1 << 8;
const BG2_ENABLED: u16 = 1 << 10;

/// A background control register: what it is drawn from, and where it sits.
const PRIORITY: u16 = 0x0003;
const TILE_BASE: u16 = 0x000C;
/// Sixteen colours per tile, or 256 for the whole background.
const FULL_COLOUR: u16 = 1 << 7;
const MAP_BASE: u16 = 0x1F00;
const SIZE: u16 = 0xC000;

/// A map entry: a tile, which way round it goes, and which palette it uses.
const TILE_NUMBER: u16 = 0x03FF;
const FLIP_ACROSS: u16 = 1 << 10;
const FLIP_DOWN: u16 = 1 << 11;

/// The blocks video memory is handed out in: 2 KiB for a map, 16 KiB for a set
/// of tiles.
const MAP_BLOCK: usize = 2 * 1024;
const TILE_BLOCK: usize = 16 * 1024;

/// How much of video memory the backgrounds have in the tiled modes. The rest
/// belongs to sprites.
const BACKGROUND_VRAM: usize = 0x1_0000;

/// How big a scrolling background's map is, in pixels. Both are powers of two,
/// which is what makes scrolling off an edge a mask rather than a comparison.
fn text_size(control: u16) -> (usize, usize) {
    match control & SIZE {
        0x0000 => (256, 256),
        0x4000 => (512, 256),
        0x8000 => (256, 512),
        _ => (512, 512),
    }
}

/// Mode 5's picture is smaller than the screen, and what is not covered by it
/// stays the backdrop colour.
const SMALL_WIDTH: usize = 160;
const SMALL_HEIGHT: usize = 128;

impl Ppu {
    /// Draws one line of the picture, out of the registers as they stand.
    pub(super) fn draw_line(&mut self, line: u16) {
        let line = line as usize;
        debug_assert!(line < SCREEN_HEIGHT);
        let at = line * SCREEN_WIDTH;

        // Held blank: the screen goes white, whatever is in memory and whatever
        // mode is set. A game does this while it rearranges video memory, so
        // drawing the half-rearranged contents instead would show exactly the
        // mess it is being used to hide.
        if self.forced_blank() {
            self.frame[at..at + SCREEN_WIDTH].fill(WHITE);
            return;
        }

        // Everything not covered by a background shows the first colour of the
        // palette. It is a real colour a game chooses, not a stand-in for
        // nothing.
        let backdrop = self.colour(0);
        self.frame[at..at + SCREEN_WIDTH].fill(backdrop);

        // All three bitmap modes are drawn as background 2, and a game can turn
        // that off — in which case the backdrop is the whole picture. This is
        // the bitmap modes' own condition and not a general one: the tiled
        // modes have four backgrounds with an enable bit each, and testing
        // background 2's for them would blank a screen drawn on any of the
        // other three.
        let bitmap_on = self.dispcnt & BG2_ENABLED != 0;

        match self.mode() {
            // The tiled modes, where a screenful of graphics is a small set of
            // blocks and a map saying where each goes.
            0..=2 => self.draw_tiled_line(line),
            3 if bitmap_on => self.draw_direct_line(line, 0, SCREEN_WIDTH, SCREEN_HEIGHT),
            4 if bitmap_on => self.draw_indexed_line(line),
            5 if bitmap_on => {
                self.draw_direct_line(line, self.picture_base(), SMALL_WIDTH, SMALL_HEIGHT)
            }
            // Six and seven are not modes: the hardware draws nothing for them
            // either.
            _ => {}
        }
    }

    /// A line of the tiled modes: every background that is switched on, drawn
    /// back to front.
    ///
    /// # Why back to front
    ///
    /// Because that way nothing has to be asked twice. Each background is drawn
    /// over what is already there and skips its own transparent pixels, so the
    /// nearest one that has something to say at a given pixel is the last to
    /// write it — which is precisely the rule the hardware follows, arrived at
    /// without comparing anything.
    ///
    /// Priority 3 is furthest back. Backgrounds sharing a priority are ordered
    /// by number, with 0 nearest, so counting both loops downwards puts every
    /// one of them in the right place.
    fn draw_tiled_line(&mut self, line: usize) {
        for priority in (0..4).rev() {
            for index in (0..4).rev() {
                if self.background_is_on(index) && self.priority_of(index) == priority {
                    self.draw_text_background(index, line);
                }
            }
        }
    }

    /// Whether a background exists in this mode and has been switched on.
    ///
    /// Which of the four exist is decided by the mode, and it is not a matter
    /// of a game simply not using the others: mode 2 has no background 0 or 1
    /// at all, and the enable bits for them do nothing.
    fn background_is_on(&self, index: usize) -> bool {
        if self.dispcnt & (BG0_ENABLED << index) == 0 {
            return false;
        }
        // Only the ones drawn from a map of tiles and a scroll position are
        // here. Modes 1 and 2 have backgrounds that are rotated and scaled
        // instead, and those are not drawn yet.
        match self.mode() {
            0 => true,
            1 => index < 2,
            _ => false,
        }
    }

    fn priority_of(&self, index: usize) -> u16 {
        self.backgrounds[index].control & PRIORITY
    }

    /// One line of one scrolling background.
    fn draw_text_background(&mut self, index: usize, line: usize) {
        let background = self.backgrounds[index];
        let (width, height) = text_size(background.control);
        let map_base = usize::from((background.control & MAP_BASE) >> 8) * MAP_BLOCK;
        let tile_base = usize::from((background.control & TILE_BASE) >> 2) * TILE_BLOCK;
        let full_colour = background.control & FULL_COLOUR != 0;

        // The map is a torus: scrolling off one edge brings the other edge
        // round. Both sizes are powers of two, so the wrap is a mask.
        let y = (line + usize::from(background.vofs)) & (height - 1);
        let at = line * SCREEN_WIDTH;

        for x in 0..SCREEN_WIDTH {
            let sx = (x + usize::from(background.hofs)) & (width - 1);
            let entry = self.map_entry(map_base, sx / 8, y / 8, width);

            // Which pixel of the tile, once the two mirror bits have had their
            // say. A tile is used many times over and flipped differently each
            // time, which is most of why a map costs so little.
            let mut px = sx % 8;
            let mut py = y % 8;
            if entry & FLIP_ACROSS != 0 {
                px = 7 - px;
            }
            if entry & FLIP_DOWN != 0 {
                py = 7 - py;
            }

            let tile = usize::from(entry & TILE_NUMBER);
            let colour = if full_colour {
                // A byte a pixel, and the one palette of 256.
                self.background_byte(tile_base + tile * 64 + py * 8 + px)
            } else {
                // A nibble a pixel, low half first, and one of sixteen palettes
                // of sixteen chosen per map entry.
                let pair = self.background_byte(tile_base + tile * 32 + py * 4 + px / 2);
                let index = if px & 1 == 0 { pair & 0xF } else { pair >> 4 };
                if index == 0 { 0 } else { (entry >> 12) as u8 * 16 + index }
            };

            // Index zero is nothing rather than a colour, and what is behind
            // shows through. It is what lets four backgrounds share a screen.
            if colour != 0 {
                self.frame[at + x] = self.colour(colour);
            }
        }
    }

    /// One entry of a background's map: which tile goes at that square, which
    /// way round, and out of which palette.
    ///
    /// # The map is not one rectangle
    ///
    /// It is up to four squares of 32 by 32 laid out side by side, each its own
    /// 2 KiB block, and a wide map's right-hand half is a *different block*
    /// rather than the far end of a longer row. Treating it as one rectangle
    /// draws the correct picture for the small size and a scrambled one for
    /// every other, which is the kind of mistake that only shows up on the
    /// second background a game happens to make wide.
    fn map_entry(&self, base: usize, column: usize, row: usize, width: usize) -> u16 {
        let block = column / 32 + (row / 32) * (width / 256);
        let at = base + block * MAP_BLOCK + ((row % 32) * 32 + column % 32) * 2;
        u16::from(self.background_byte(at)) | (u16::from(self.background_byte(at + 1)) << 8)
    }

    /// A byte of the half of video memory the backgrounds live in.
    ///
    /// The top 32 KiB belong to sprites in these modes and a background cannot
    /// reach them: an address that would is folded back rather than reaching
    /// something that is not its own.
    fn background_byte(&self, at: usize) -> u8 {
        self.vram[at & (BACKGROUND_VRAM - 1)]
    }

    /// A line of a picture stored as colours, two bytes each.
    ///
    /// Mode 3 covers the screen and has one picture; mode 5 is smaller and has
    /// two, so the width, the height and where it starts are all arguments
    /// rather than three near-copies of the same loop.
    fn draw_direct_line(&mut self, line: usize, base: usize, width: usize, height: usize) {
        if line >= height {
            return;
        }
        let at = line * SCREEN_WIDTH;
        let row = base + line * width * 2;
        for x in 0..width {
            let colour = self.halfword(row + x * 2);
            self.frame[at + x] = colour & WHITE;
        }
    }

    /// A line of a picture stored as palette indices, one byte each.
    ///
    /// Index zero is not a colour here: it means "nothing", and what shows
    /// through is the backdrop. That is what makes this mode useful for
    /// anything drawn on top of something else — and it is why a test ROM's
    /// digits can be written without clearing the screen first.
    fn draw_indexed_line(&mut self, line: usize) {
        let at = line * SCREEN_WIDTH;
        let row = self.picture_base() + line * SCREEN_WIDTH;
        for x in 0..SCREEN_WIDTH {
            let index = self.vram[row + x];
            if index != 0 {
                self.frame[at + x] = self.colour(index);
            }
        }
    }

    /// Which of the two pictures is being shown, as an offset into video
    /// memory. Modes 4 and 5 have a second one so that a game can draw the next
    /// picture while this one is on screen and swap between them in a single
    /// write — the only way to change a bitmap without the change being seen
    /// half-done.
    fn picture_base(&self) -> usize {
        if self.dispcnt & FRAME_SELECT != 0 { SECOND_FRAME } else { 0 }
    }

    /// One of the 256 background colours.
    fn colour(&self, index: u8) -> u16 {
        let at = index as usize * 2;
        (u16::from(self.pram[at]) | (u16::from(self.pram[at + 1]) << 8)) & WHITE
    }

    /// A halfword of video memory, little end first like everything else.
    fn halfword(&self, at: usize) -> u16 {
        u16::from(self.vram[at]) | (u16::from(self.vram[at + 1]) << 8)
    }

    /// Blanks the picture. For a machine being reset, so that what is on screen
    /// is not the last game's last frame.
    pub fn clear(&mut self) {
        self.frame[..PIXELS].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupts::Interrupts;
    use crate::ppu::{DISPCNT, FRAME_CYCLES};

    /// Reds, greens and blues in the fifteen bits: five each, blue highest.
    const RED: u16 = 0x001F;
    const GREEN: u16 = 0x03E0;
    const BLUE: u16 = 0x7C00;

    fn sweep_a_frame(ppu: &mut Ppu) {
        let mut irq = Interrupts::new();
        ppu.tick(FRAME_CYCLES, &mut irq);
    }

    fn set_colour(ppu: &mut Ppu, index: usize, colour: u16) {
        ppu.pram_mut()[index * 2] = colour as u8;
        ppu.pram_mut()[index * 2 + 1] = (colour >> 8) as u8;
    }

    fn set_halfword(ppu: &mut Ppu, at: usize, value: u16) {
        ppu.vram_mut()[at] = value as u8;
        ppu.vram_mut()[at + 1] = (value >> 8) as u8;
    }

    fn pixel(ppu: &Ppu, x: usize, y: usize) -> u16 {
        ppu.frame()[y * SCREEN_WIDTH + x]
    }

    /// Mode 3 is the picture itself, a colour per pixel, and the whole screen
    /// is covered by it.
    #[test]
    fn a_direct_bitmap_is_drawn_pixel_for_pixel() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 3);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);

        set_halfword(&mut ppu, 0, RED);
        set_halfword(&mut ppu, (SCREEN_WIDTH - 1) * 2, GREEN);
        let last = (SCREEN_HEIGHT - 1) * SCREEN_WIDTH * 2;
        set_halfword(&mut ppu, last, BLUE);

        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, 0, 0), RED, "the first pixel");
        assert_eq!(pixel(&ppu, SCREEN_WIDTH - 1, 0), GREEN, "the end of the first line");
        assert_eq!(pixel(&ppu, 0, SCREEN_HEIGHT - 1), BLUE, "and the last line");
    }

    /// Mode 4 stores an index and looks the colour up, which is what halves the
    /// cost and buys a second picture.
    #[test]
    fn an_indexed_bitmap_looks_its_colours_up() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 4);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);

        set_colour(&mut ppu, 0, BLUE);
        set_colour(&mut ppu, 7, RED);
        ppu.vram_mut()[3] = 7;

        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, 3, 0), RED, "the index was looked up");
        assert_eq!(pixel(&ppu, 4, 0), BLUE, "and the rest is the backdrop");
    }

    /// Index zero means nothing rather than a colour, so a game can draw on top
    /// of what is already there without clearing it.
    #[test]
    fn the_zeroth_index_shows_the_backdrop_through() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 4);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);
        set_colour(&mut ppu, 0, GREEN);
        set_colour(&mut ppu, 1, RED);

        ppu.vram_mut()[0] = 0;
        ppu.vram_mut()[1] = 1;
        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, 0, 0), GREEN, "nothing there, so the backdrop");
        assert_eq!(pixel(&ppu, 1, 0), RED);
    }

    /// The two pictures are what let a game draw the next one out of sight and
    /// change over in a single write.
    #[test]
    fn the_second_picture_is_shown_when_it_is_selected() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 4);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);
        set_colour(&mut ppu, 1, RED);
        set_colour(&mut ppu, 2, GREEN);

        ppu.vram_mut()[0] = 1;
        ppu.vram_mut()[SECOND_FRAME] = 2;

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "the first picture");

        ppu.write8(DISPCNT, 4 | FRAME_SELECT as u8);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), GREEN, "and the second");
    }

    /// Mode 5's picture is smaller than the screen. What it does not cover is
    /// the backdrop, not the edge pixel smeared outwards.
    #[test]
    fn the_small_bitmap_leaves_the_rest_of_the_screen_as_backdrop() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 5);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);
        set_colour(&mut ppu, 0, BLUE);

        set_halfword(&mut ppu, (SMALL_WIDTH - 1) * 2, RED);
        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, SMALL_WIDTH - 1, 0), RED, "the last pixel it covers");
        assert_eq!(pixel(&ppu, SMALL_WIDTH, 0), BLUE, "and the backdrop beside it");
        assert_eq!(pixel(&ppu, 0, SMALL_HEIGHT), BLUE, "and below it");
    }

    /// Held blank is white, whatever is in memory. A game does this while it
    /// rearranges things, and drawing the half-rearranged contents would show
    /// exactly what it is hiding.
    #[test]
    fn a_screen_held_blank_is_white_whatever_is_in_memory() {
        let mut ppu = Ppu::new();
        // The blank is the top bit of the *low* byte, alongside the mode.
        ppu.write8(DISPCNT, 3 | 0x80);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);
        set_halfword(&mut ppu, 0, RED);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), WHITE);
        assert_eq!(pixel(&ppu, 100, 100), WHITE);
    }

    /// A background that is switched off is not drawn, and what shows is the
    /// backdrop.
    #[test]
    fn a_background_that_is_switched_off_is_not_drawn() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 3);
        set_colour(&mut ppu, 0, GREEN);
        set_halfword(&mut ppu, 0, RED);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), GREEN, "the background was never enabled");
    }

    /// The top bit of a colour halfword is not part of any of the three
    /// channels and must not reach the picture.
    #[test]
    fn the_sixteenth_bit_of_a_colour_is_not_a_colour() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 3);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);
        set_halfword(&mut ppu, 0, 0x8000 | RED);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED);
    }

    /// A line is drawn out of the registers as they stood when the beam
    /// finished it, and is not revisited. This is what every mid-frame effect
    /// depends on, and drawing a whole frame at once would lose it.
    #[test]
    fn a_line_keeps_what_was_set_when_it_was_drawn() {
        let mut ppu = Ppu::new();
        let mut irq = Interrupts::new();
        ppu.write8(DISPCNT, 3);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);

        // The whole picture is red, and the backdrop is green.
        set_colour(&mut ppu, 0, GREEN);
        for line in 0..SCREEN_HEIGHT {
            set_halfword(&mut ppu, line * SCREEN_WIDTH * 2, RED);
        }

        // Eighty lines, then the background is turned off — and then the rest
        // of *this* frame and no more. Sweeping a whole frame from here would
        // come round and draw line zero again with the background off, which
        // would be this test measuring the wrong frame.
        let line = crate::ppu::LINE_CYCLES;
        ppu.tick(line * 80, &mut irq);
        ppu.write8(DISPCNT + 1, 0);
        ppu.tick(line * (u32::from(crate::ppu::LINES_PER_FRAME) - 80), &mut irq);
        assert_eq!(ppu.frames(), 1, "exactly one frame was swept");

        assert_eq!(pixel(&ppu, 0, 0), RED, "drawn while the background was on");
        assert_eq!(pixel(&ppu, 0, 120), GREEN, "and this one after it went off");
    }

    // --- The tiled modes -------------------------------------------------

    /// Points a background at a set of tiles and a map, and switches it on.
    fn background(ppu: &mut Ppu, index: usize, control: u16) {
        let at = crate::ppu::BG_CONTROL + index as u32 * 2;
        ppu.write8(at, control as u8);
        ppu.write8(at + 1, (control >> 8) as u8);
        let on = ppu.dispcnt() | (BG0_ENABLED << index);
        ppu.write8(DISPCNT + 1, (on >> 8) as u8);
    }

    fn scroll(ppu: &mut Ppu, index: usize, across: u16, down: u16) {
        let at = crate::ppu::BG_SCROLL + index as u32 * 4;
        ppu.write8(at, across as u8);
        ppu.write8(at + 1, (across >> 8) as u8);
        ppu.write8(at + 2, down as u8);
        ppu.write8(at + 3, (down >> 8) as u8);
    }

    /// Fills a tile with one colour index, four bits a pixel.
    ///
    /// Never tile 0 in these tests. A map entry that has not been written is
    /// zero, which names tile 0, so filling that one covers the whole screen
    /// with it and every assertion about an empty square measures nothing. It
    /// is the arrangement games use as well: tile 0 is left blank so that an
    /// unwritten map is an empty screen.
    fn fill_tile(ppu: &mut Ppu, base: usize, tile: usize, index: u8) {
        assert_ne!(tile, 0, "tile 0 is what an unwritten map entry names");
        let both = index | (index << 4);
        for byte in 0..32 {
            ppu.vram_mut()[base + tile * 32 + byte] = both;
        }
    }

    /// One square of a map, in whichever of its blocks that square falls.
    fn set_map(ppu: &mut Ppu, base: usize, column: usize, row: usize, width: usize, entry: u16) {
        let block = column / 32 + (row / 32) * (width / 256);
        let at = base + block * MAP_BLOCK + ((row % 32) * 32 + column % 32) * 2;
        ppu.vram_mut()[at] = entry as u8;
        ppu.vram_mut()[at + 1] = (entry >> 8) as u8;
    }

    /// A background out of a map and a set of tiles, which is the arrangement
    /// nearly every game draws with.
    #[test]
    fn a_tiled_background_puts_its_tiles_where_the_map_says() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 0, BLUE);
        set_colour(&mut ppu, 1, RED);

        // Tiles in the second 16 KiB block, map in the second 2 KiB one.
        background(&mut ppu, 0, (1 << 2) | (1 << 8));
        fill_tile(&mut ppu, TILE_BLOCK, 1, 1);
        set_map(&mut ppu, MAP_BLOCK, 2, 1, 256, 1);

        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, 16, 8), RED, "the square the map filled");
        assert_eq!(pixel(&ppu, 23, 15), RED, "and all eight pixels of it");
        assert_eq!(pixel(&ppu, 24, 8), BLUE, "the square beside it is empty");
        assert_eq!(pixel(&ppu, 16, 0), BLUE, "and the one above it");
    }

    /// The mode with one palette of 256 rather than sixteen of sixteen: a byte
    /// a pixel, and twice the memory per tile.
    #[test]
    fn a_background_can_use_one_palette_of_two_hundred_and_fifty_six() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 0, BLUE);
        set_colour(&mut ppu, 200, GREEN);

        background(&mut ppu, 0, FULL_COLOUR | (1 << 8));
        // A byte a pixel, so a tile is 64 bytes and tile 1 begins at 64.
        for byte in 64..128 {
            ppu.vram_mut()[byte] = 200;
        }
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), GREEN, "an index past sixteen was reached");
        assert_eq!(pixel(&ppu, 7, 7), GREEN);
    }

    /// Sixteen palettes of sixteen, chosen per square. It is what lets one tile
    /// be drawn in several colour schemes without a copy of it.
    #[test]
    fn a_map_entry_chooses_which_of_the_sixteen_palettes_its_tile_uses() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 1, RED);
        // The third palette's first colour is entry 2 * 16 + 1.
        set_colour(&mut ppu, 2 * 16 + 1, GREEN);

        background(&mut ppu, 0, 1 << 8);
        fill_tile(&mut ppu, 0, 1, 1);
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);
        set_map(&mut ppu, MAP_BLOCK, 1, 0, 256, (2 << 12) | 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "the first palette");
        assert_eq!(pixel(&ppu, 8, 0), GREEN, "and the same tile in the third");
    }

    /// A tile can be used either way round, which is most of why a map costs so
    /// little: a symmetrical picture is stored once.
    #[test]
    fn a_tile_can_be_mirrored_either_way() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 0, BLUE);
        set_colour(&mut ppu, 1, RED);
        background(&mut ppu, 0, 1 << 8);

        // Tile 1, with its top-left pixel set and nothing else.
        ppu.vram_mut()[32] = 0x01;
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);
        set_map(&mut ppu, MAP_BLOCK, 1, 0, 256, FLIP_ACROSS | 1);
        set_map(&mut ppu, MAP_BLOCK, 2, 0, 256, FLIP_DOWN | 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "the corner it was drawn in");
        assert_eq!(pixel(&ppu, 15, 0), RED, "mirrored across, so the far side");
        assert_eq!(pixel(&ppu, 8, 0), BLUE, "and not the near one");
        assert_eq!(pixel(&ppu, 16, 7), RED, "mirrored down, so the bottom");
        assert_eq!(pixel(&ppu, 16, 0), BLUE);
    }

    /// The nearer background wins where both have something, and the further
    /// one shows through where the nearer has nothing.
    #[test]
    fn the_nearer_background_covers_the_further_one() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 1, RED);
        set_colour(&mut ppu, 2, GREEN);

        // Background 1 is nearer despite its higher number, because priority
        // decides first: 2 is further back than 0.
        background(&mut ppu, 0, 2 | (1 << 8));
        background(&mut ppu, 1, 2 << 8);
        fill_tile(&mut ppu, 0, 1, 1);
        fill_tile(&mut ppu, 0, 2, 2);

        // The far one covers two squares, the near one only the first.
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);
        set_map(&mut ppu, MAP_BLOCK, 1, 0, 256, 1);
        set_map(&mut ppu, MAP_BLOCK * 2, 0, 0, 256, 2);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), GREEN, "the nearer background");
        assert_eq!(pixel(&ppu, 8, 0), RED, "and the further one beside it");
    }

    /// Sharing a priority, the lower-numbered background is nearer. Without
    /// that rule two backgrounds at the same priority would be undefined
    /// against each other.
    #[test]
    fn backgrounds_at_the_same_priority_are_ordered_by_number() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 1, RED);
        set_colour(&mut ppu, 2, GREEN);

        background(&mut ppu, 0, 1 << 8);
        background(&mut ppu, 1, 2 << 8);
        fill_tile(&mut ppu, 0, 1, 1);
        fill_tile(&mut ppu, 0, 2, 2);
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);
        set_map(&mut ppu, MAP_BLOCK * 2, 0, 0, 256, 2);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "background 0 is in front of 1");
    }

    /// Scrolling moves the window into the map, and running off an edge brings
    /// the other edge round rather than running out.
    #[test]
    fn scrolling_moves_the_window_and_wraps_at_the_edge() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 0, BLUE);
        set_colour(&mut ppu, 1, RED);

        background(&mut ppu, 0, 1 << 8);
        fill_tile(&mut ppu, 0, 1, 1);
        // One square, at the very top left of a 256 by 256 map.
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);

        scroll(&mut ppu, 0, 8, 0);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), BLUE, "the square scrolled off the left");

        // Scrolled by a whole map, which brings it back to where it started.
        scroll(&mut ppu, 0, 256, 0);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "and a whole map round is home again");

        // And one short of that puts its last column in the first pixel.
        scroll(&mut ppu, 0, 255, 0);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 1, 0), RED, "wrapped round the edge");
    }

    /// A map wider than 256 pixels is not one long rectangle: its right-hand
    /// half is a separate 2 KiB block. Treating it as one draws the small size
    /// correctly and scrambles every other.
    #[test]
    fn a_wide_map_keeps_its_second_half_in_the_next_block() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 0, BLUE);
        set_colour(&mut ppu, 1, RED);

        // 512 by 256, so two blocks side by side.
        background(&mut ppu, 0, (1 << 8) | 0x4000);
        fill_tile(&mut ppu, 0, 1, 1);
        // Column 32 is the first column of the second block.
        set_map(&mut ppu, MAP_BLOCK, 32, 0, 512, 1);

        scroll(&mut ppu, 0, 256, 0);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "the second block was reached");

        scroll(&mut ppu, 0, 0, 0);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), BLUE, "and is not visible from the first");
    }

    /// The regression that a picture caught and no test would have: the enable
    /// bit the bitmap modes are drawn through is background 2's, and testing it
    /// in a tiled mode blanks a screen drawn on any of the other three. Two
    /// commercial cartridges and two test ROMs came out empty.
    #[test]
    fn a_tiled_background_does_not_need_the_bitmap_modes_enable_bit() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0);
        set_colour(&mut ppu, 1, RED);

        background(&mut ppu, 0, 1 << 8);
        assert_eq!(ppu.dispcnt() & BG2_ENABLED, 0, "background 2 is switched off");
        fill_tile(&mut ppu, 0, 1, 1);
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "and background 0 is drawn regardless");
    }

    /// Which backgrounds exist is the mode's business. Mode 2 has no
    /// background 0, and its enable bit does nothing.
    #[test]
    fn a_background_the_mode_does_not_have_is_not_drawn() {
        let mut ppu = Ppu::new();
        set_colour(&mut ppu, 0, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 0, 1, 1);
        set_map(&mut ppu, MAP_BLOCK, 0, 0, 256, 1);

        ppu.write8(DISPCNT, 0);
        background(&mut ppu, 0, 1 << 8);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "mode 0 has background 0");

        ppu.write8(DISPCNT, 2);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), BLUE, "and mode 2 does not");
    }

    /// The scroll registers are write-only, and answering with what was stored
    /// would let code run here that would not run on the hardware.
    #[test]
    fn the_scroll_position_cannot_be_read_back() {
        let mut ppu = Ppu::new();
        scroll(&mut ppu, 0, 0x1FF, 0x1FF);
        for offset in 0..4 {
            assert_eq!(ppu.read8(crate::ppu::BG_SCROLL + offset), 0, "offset {offset}");
        }

        // The control register beside them does read back, so this is a rule
        // about those four and not about the block they are in.
        background(&mut ppu, 0, 0x1234);
        assert_eq!(ppu.read8(crate::ppu::BG_CONTROL), 0x34);
    }

    /// Only the drawn lines are drawn. The beam sweeps 68 more below the screen
    /// and there is no picture there to write into.
    #[test]
    fn nothing_is_drawn_below_the_screen() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 3);
        ppu.write8(DISPCNT + 1, (BG2_ENABLED >> 8) as u8);
        set_colour(&mut ppu, 0, RED);

        // Three frames' worth, which sweeps every line below the screen many
        // times over. The test is that this does not panic.
        for _ in 0..3 {
            sweep_a_frame(&mut ppu);
        }
        assert_eq!(ppu.frame().len(), PIXELS);
    }
}
