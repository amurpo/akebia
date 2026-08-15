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
//! The bitmap modes only. The tiled ones are the ones games draw with and they
//! are a larger piece of work — four backgrounds, two of which can rotate,
//! sprites on top of all of them, and a priority order between the lot.
//!
//! An unwritten mode draws the backdrop rather than nothing, which is what the
//! hardware does and is also honest: a screen in the backdrop colour says the
//! picture is not being drawn, where leaving the previous frame's pixels would
//! say nothing at all.

use super::{Ppu, PIXELS};
use crate::{SCREEN_HEIGHT, SCREEN_WIDTH};

/// White, which is every one of the fifteen bits set.
const WHITE: u16 = 0x7FFF;

/// Where the second of the two pictures starts, in the modes that have one.
const SECOND_FRAME: usize = 0xA000;

/// `DISPCNT`'s bit for the second picture, and the one that enables the
/// background all three bitmap modes are drawn as.
const FRAME_SELECT: u16 = 1 << 4;
const BG2_ENABLED: u16 = 1 << 10;

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
        // that off — in which case the backdrop is the whole picture.
        if self.dispcnt & BG2_ENABLED == 0 {
            return;
        }

        match self.mode() {
            3 => self.draw_direct_line(line, 0, SCREEN_WIDTH, SCREEN_HEIGHT),
            4 => self.draw_indexed_line(line),
            5 => self.draw_direct_line(line, self.picture_base(), SMALL_WIDTH, SMALL_HEIGHT),
            // Modes 0 to 2 are not drawn yet, and 6 and 7 are not modes: the
            // hardware draws nothing for them either.
            _ => {}
        }
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
