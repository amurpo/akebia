//! The picture unit: the sweep across the screen, and the four registers that
//! describe it.
//!
//! # Why the sweep comes before the drawing
//!
//! Because on this machine the sweep is not only how a picture is produced, it
//! is how a game knows what time it is. There is no other clock a program can
//! read directly. A game finds out that a sixtieth of a second has passed by
//! watching `VCOUNT` change, or by being interrupted when the beam reaches the
//! bottom, and almost every game does one or the other before it does anything
//! else at all.
//!
//! So a machine with no picture unit does not merely show nothing — it stops.
//! A cartridge here ran its own code correctly and then sat in a three
//! instruction loop reading `VCOUNT` for as long as it was left to, because the
//! answer was zero and would always be zero. The drawing can wait; the sweep
//! cannot, and this module is the sweep.
//!
//! # What the sweep is
//!
//! A line is 308 dots, of which 240 are drawn and 68 are the gap afterwards. A
//! frame is 228 lines, of which 160 are drawn and 68 are the gap afterwards.
//! Every dot is four cycles of the master clock, so a line is 1232 cycles and a
//! frame is 280 896 — a little under sixty a second.
//!
//! The gaps are not waste. They are when video memory is free for a game to
//! write to, which is why the two interrupts that mark them are the ones
//! everything is built around: the long one at the bottom of the frame for
//! wholesale changes, the short one at the end of each line for changing
//! something *between* lines, which is how a machine with four backgrounds
//! draws effects that need more.
//!
//! # Where the status bits live
//!
//! Not in the register. `DISPSTAT`'s low three bits are read-only reports about
//! where the beam is, and they are worked out from the sweep every time they
//! are read rather than stored and updated. Stored, they would be a second copy
//! of the position that could disagree with the first, and the disagreement
//! would show up as a game waiting on a blanking period that had already been
//! and gone.
//!
//! # What is not modelled
//!
//! The horizontal flag is raised where the drawn part of the line ends, at dot
//! 240. On hardware it lags that by roughly forty cycles, and the interrupt
//! lags with it. Nothing that has been tried here can see the difference, and
//! writing in a number that was measured on someone else's hardware would make
//! this look more exact than it is.

pub mod blend;
pub mod render;
mod sprites;
pub mod window;

use blend::Pixel;
use sprites::Sprite;

use crate::interrupts::{Interrupts, Source};
use crate::{SCREEN_HEIGHT, SCREEN_WIDTH};

/// Palette memory: 512 colours in 15 bits each, the first 256 for backgrounds
/// and the rest for sprites.
pub const PRAM_LEN: usize = 1024;
pub const VRAM_LEN: usize = 96 * 1024;
/// 128 sprites' worth of attributes.
pub const OAM_LEN: usize = 1024;

/// Pixels in a picture.
pub const PIXELS: usize = SCREEN_WIDTH * SCREEN_HEIGHT;

/// A dot is four cycles of the master clock.
const DOT_CYCLES: u32 = 4;

/// Dots in a line: the 240 that are drawn, and 68 more that are not.
const DOTS_PER_LINE: u32 = 308;

/// Where the drawn part of a line ends, in cycles from its start.
const HBLANK_AT: u32 = SCREEN_WIDTH as u32 * DOT_CYCLES;

/// A whole line, in cycles.
pub const LINE_CYCLES: u32 = DOTS_PER_LINE * DOT_CYCLES;

/// Lines in a frame: the 160 that are drawn, and 68 more that are not.
pub const LINES_PER_FRAME: u16 = 228;

/// The line the drawn part of the frame ends on.
const VBLANK_AT: u16 = SCREEN_HEIGHT as u16;

/// A whole frame, in cycles. A little under a sixtieth of a second.
pub const FRAME_CYCLES: u32 = LINE_CYCLES * LINES_PER_FRAME as u32;

/// The first register the picture unit answers, and the last. The bus needs
/// the pair to know what to hand over.
pub const DISPCNT: u32 = 0x0400_0000;
/// It reaches past the four registers this module was named for: the windows,
/// the mosaic and the blending live in the same block, and of those only the
/// blending is answered. The rest read zero as they did when the bus was
/// dropping them — the difference is only which `_` arm swallows them.
pub const LAST: u32 = 0x0400_0057;

const GREEN_SWAP: u32 = 0x0400_0002;
const DISPSTAT: u32 = 0x0400_0004;
pub const VCOUNT: u32 = 0x0400_0006;

/// The four backgrounds' control registers, one halfword each, and their
/// scroll positions, two halfwords each.
const BG_CONTROL: u32 = 0x0400_0008;
const BG_SCROLL: u32 = 0x0400_0010;
/// Sixteen bytes each for the two backgrounds that can be rotated: four
/// multipliers and two starting points.
const BG_AFFINE: u32 = 0x0400_0020;
const AFFINE_STRIDE: u32 = 16;
/// Where the transformations stop and the windows, the mosaic and the blending
/// begin. Only the last of those three is answered.
const BLEND: u32 = 0x0400_0040;

/// `DISPCNT`'s video mode, and the bit that says the screen is being held
/// blank whatever the mode.
const MODE: u16 = 0x0007;
const FORCED_BLANK: u16 = 1 << 7;

/// The one bit of `DISPCNT` a game may not write: it reports whether the
/// machine is running an older console's cartridge, which is the hardware's to
/// say and not a program's.
const DISPCNT_READ_ONLY: u16 = 1 << 3;

/// `DISPSTAT`'s three reports, which a write does not touch.
const VBLANK_FLAG: u16 = 1 << 0;
const HBLANK_FLAG: u16 = 1 << 1;
const VCOUNT_FLAG: u16 = 1 << 2;

/// And its three requests, which are all a game may set besides the line to
/// match. Bits 6 and 7 name nothing on this machine.
const VBLANK_IRQ: u16 = 1 << 3;
const HBLANK_IRQ: u16 = 1 << 4;
const VCOUNT_IRQ: u16 = 1 << 5;
const DISPSTAT_WRITABLE: u16 = VBLANK_IRQ | HBLANK_IRQ | VCOUNT_IRQ | 0xFF00;

/// Where sprites begin in video memory, which is not the same in every mode:
/// the bitmap modes spend the first 80 KiB on the picture itself and leave
/// sprites the rest.
const OBJ_BASE_TILED: u32 = 0x1_0000;
const OBJ_BASE_BITMAP: u32 = 0x1_4000;

/// One background's registers.
///
/// # Why the scroll cannot be read back
///
/// It is write-only on this hardware, and that is not an oversight to be
/// papered over: a game keeps its own copy of where it has scrolled to, and one
/// that read the register instead would be reading a number the machine does
/// not report. Answering with the stored value would make an emulator that runs
/// code the hardware would not.
#[derive(Clone, Copy, Default)]
pub(super) struct Background {
    pub control: u16,
    pub hofs: u16,
    pub vofs: u16,
}

/// The two backgrounds that can be rotated and scaled, and where they have got
/// to.
///
/// # Why the starting point is kept twice
///
/// The four multipliers describe a transformation and the two starting points
/// say which part of the map the top-left pixel of the screen comes from. But
/// the beam moves down the screen, so the starting point of *each line* is a
/// different number, and the hardware keeps a running pair that it advances by
/// two of the multipliers after every line.
///
/// A game exploits this. Writing the starting point mid-frame moves the
/// remaining lines and leaves the ones already drawn alone, which is how the
/// floor of a racing game is drawn: one background, one line at a time, with
/// the transformation changed between each. So the pair a game writes and the
/// pair being walked have to be separate, and the walked pair is reloaded from
/// the written one once a frame — not once a line, or the effect would be
/// impossible.
#[derive(Clone, Copy, Default)]
pub(super) struct Affine {
    /// The transformation, as eight-bit fractions.
    pub pa: i16,
    pub pb: i16,
    pub pc: i16,
    pub pd: i16,
    /// What the game wrote: 28 bits signed, with eight of them fractional.
    pub x: i32,
    pub y: i32,
    /// Where this line starts, which the sweep advances.
    pub at_x: i32,
    pub at_y: i32,
}

impl Affine {
    /// Takes the written starting point as the current one. Once a frame, as
    /// the beam leaves the screen, and whenever the game writes it.
    fn reload(&mut self) {
        self.at_x = self.x;
        self.at_y = self.y;
    }

    /// Moves to the next line, which is a step down the transformed axes and
    /// not simply one pixel down.
    fn next_line(&mut self) {
        self.at_x = self.at_x.wrapping_add(i32::from(self.pb));
        self.at_y = self.at_y.wrapping_add(i32::from(self.pd));
    }
}

/// A starting point is 28 bits and signed, so the top four have to be brought
/// down before the sign means anything.
fn signed_28(value: u32) -> i32 {
    ((value << 4) as i32) >> 4
}

/// Which background a register address belongs to.
///
/// The control registers are a halfword apart and the scroll registers are two,
/// so the arithmetic differs either side of the boundary between them.
fn background_of(addr: u32) -> usize {
    let index = if addr < BG_SCROLL {
        (addr - BG_CONTROL) / 2
    } else {
        (addr - BG_SCROLL) / 4
    };
    index as usize & 3
}

/// Which blanking periods a sweep began.
///
/// It exists for the memory mover, which does its work in the gaps because
/// that is when video memory is free — so it has to be told when a gap starts
/// and cannot be left to watch the line counter and guess.
///
/// The report at the end of a line covers the drawn lines only, which is
/// narrower than the interrupt of the same name. Below the screen there is
/// nothing being drawn to interleave with, and hardware does not start a
/// transfer there; a mover that ran anyway would move 68 times more than it
/// was asked to, every frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crossed {
    /// The beam reached the bottom of the screen.
    pub vblank: bool,
    /// The beam reached the end of a line that was being drawn.
    pub hblank: bool,
}

/// The picture unit.
pub struct Ppu {
    dispcnt: u16,
    /// A curiosity kept because it exists: it swaps the green of every pair of
    /// neighbouring pixels, for an effect on a screen this emulator does not
    /// have. Stored so it reads back, and used by nothing.
    green_swap: u16,
    /// Only the bits a game may write. The three reports are worked out from
    /// the sweep — see the note at the top of this module.
    dispstat: u16,
    /// Which line the beam is on, counting the ones below the screen.
    vcount: u16,
    /// Cycles into the current line.
    dot: u32,
    /// The four backgrounds. Which of them exist depends on the mode.
    pub(super) backgrounds: [Background; 4],
    /// The rotation and scaling of backgrounds 2 and 3, in that order. Only
    /// those two can be transformed, which is why there are two and not four.
    pub(super) affine: [Affine; 2],
    /// How many frames have been swept. Nothing in the machine can read this;
    /// it is for whatever is driving the emulator to know when a picture is
    /// finished.
    frames: u64,
    /// The three memories the picture is made of. They live here rather than in
    /// the memory map because this is the only thing that reads them, and the
    /// map reaches them the same way it reaches the registers: by asking.
    pram: Box<[u8; PRAM_LEN]>,
    vram: Box<[u8; VRAM_LEN]>,
    oam: Box<[u8; OAM_LEN]>,
    /// The picture, a 15-bit colour per pixel, filled a line at a time as the
    /// beam passes.
    frame: Box<[u16; PIXELS]>,
    /// Which layers are mixed with which, how, and how far. See [`blend`].
    bldcnt: u16,
    bldalpha: u16,
    bldy: u16,
    /// The line being built, two pixels deep: what is on top of each column and
    /// what is directly behind it.
    ///
    /// Blending is the reason these exist. Every other effect can be drawn
    /// straight into the picture and forgotten, because the nearest layer with
    /// something to say is simply the last to write; mixing needs the one it
    /// covered, and by then it has been painted over. See [`blend`].
    top: Box<[Pixel; SCREEN_WIDTH]>,
    below: Box<[Pixel; SCREEN_WIDTH]>,
    /// The two windows' edges, and what each region contains. See [`window`].
    win_h: [u16; 2],
    win_v: [u16; 2],
    winin: u16,
    winout: u16,
    /// Which layers each column of this line may show, worked out once before
    /// the line is drawn.
    allowed: Box<[u16; SCREEN_WIDTH]>,
    /// Where the sprites that cut a region of their own cover this line. Only
    /// filled when a game asks for that region.
    obj_window: Box<[bool; SCREEN_WIDTH]>,
    /// The sprites reaching the line being drawn, with the row of each, read
    /// out of object memory once. Kept between lines only so that its capacity
    /// is: it is cleared and refilled for every one.
    on_this_line: Vec<(Sprite, usize)>,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            dispcnt: 0,
            green_swap: 0,
            dispstat: 0,
            vcount: 0,
            dot: 0,
            backgrounds: [Background::default(); 4],
            affine: [Affine::default(); 2],
            frames: 0,
            pram: Box::new([0; PRAM_LEN]),
            vram: Box::new([0; VRAM_LEN]),
            oam: Box::new([0; OAM_LEN]),
            frame: Box::new([0; PIXELS]),
            bldcnt: 0,
            bldalpha: 0,
            bldy: 0,
            top: Box::new([Pixel::NONE; SCREEN_WIDTH]),
            below: Box::new([Pixel::NONE; SCREEN_WIDTH]),
            win_h: [0; 2],
            win_v: [0; 2],
            winin: 0,
            winout: 0,
            allowed: Box::new([window::EVERYTHING; SCREEN_WIDTH]),
            obj_window: Box::new([false; SCREEN_WIDTH]),
            on_this_line: Vec::new(),
        }
    }

    pub fn pram(&self) -> &[u8] {
        &self.pram[..]
    }

    pub fn pram_mut(&mut self) -> &mut [u8] {
        &mut self.pram[..]
    }

    pub fn vram(&self) -> &[u8] {
        &self.vram[..]
    }

    pub fn vram_mut(&mut self) -> &mut [u8] {
        &mut self.vram[..]
    }

    pub fn oam(&self) -> &[u8] {
        &self.oam[..]
    }

    pub fn oam_mut(&mut self) -> &mut [u8] {
        &mut self.oam[..]
    }

    /// The picture as it stands, a 15-bit colour per pixel in rows of
    /// [`SCREEN_WIDTH`].
    ///
    /// Whatever is driving the emulator reads it when a frame is finished. It
    /// is not double-buffered: reading it mid-frame gives the lines drawn so
    /// far and the previous frame's below them, which is what the hardware
    /// would be sending as well.
    pub fn frame(&self) -> &[u16] {
        &self.frame[..]
    }

    /// Moves the beam on by that many cycles, raising whatever it passes and
    /// reporting which blanking periods began.
    ///
    /// It steps from one boundary to the next rather than a cycle at a time, so
    /// handing it a whole frame's worth costs the same as handing it a line's.
    /// How many cycles may pass before the beam reaches somewhere that matters.
    ///
    /// There are only two such places on a line — where the drawn part ends and
    /// where the line does — which is what makes this worth asking at all: a
    /// line is 1232 cycles and nothing happens in 1231 of them.
    pub fn until_next(&self) -> u32 {
        let boundary = if self.dot < HBLANK_AT { HBLANK_AT } else { LINE_CYCLES };
        boundary - self.dot
    }

    pub fn tick(&mut self, cycles: u32, irq: &mut Interrupts) -> Crossed {
        let mut crossed = Crossed::default();
        let mut left = cycles;
        while left > 0 {
            // The only two places anything happens: where the drawn part of the
            // line ends, and where the line does.
            let boundary = if self.dot < HBLANK_AT { HBLANK_AT } else { LINE_CYCLES };
            let step = (boundary - self.dot).min(left);
            self.dot += step;
            left -= step;

            if self.dot == HBLANK_AT {
                // The line is drawn here, where the drawing of it ends, out of
                // the registers as they stand at this moment. See
                // [`render`](crate::ppu::render).
                if self.vcount < VBLANK_AT {
                    self.draw_line(self.vcount);
                    // The transformed backgrounds step down their own axes,
                    // which is not the same as one pixel down the screen.
                    for affine in &mut self.affine {
                        affine.next_line();
                    }
                }

                // The interrupt goes out in every line, not only the drawn
                // ones: the beam keeps sweeping below the screen and a game can
                // time on it there. What is *reported* is narrower — see
                // [`Crossed`].
                if self.dispstat & HBLANK_IRQ != 0 {
                    irq.raise(Source::HBlank);
                }
                crossed.hblank |= self.vcount < VBLANK_AT;
            } else if self.dot == LINE_CYCLES {
                self.dot = 0;
                crossed.vblank |= self.finish_line(irq);
            }
        }
        crossed
    }

    /// Ends the line, and says whether that was the one the screen ends on.
    fn finish_line(&mut self, irq: &mut Interrupts) -> bool {
        self.vcount += 1;
        if self.vcount == LINES_PER_FRAME {
            self.vcount = 0;
            self.frames += 1;
        }

        let bottom = self.vcount == VBLANK_AT;
        if bottom {
            // Off the bottom of the screen, the transformed backgrounds go back
            // to the starting point the game wrote — once a frame, so that
            // anything written part way down survives to the bottom.
            for affine in &mut self.affine {
                affine.reload();
            }
            if self.dispstat & VBLANK_IRQ != 0 {
                irq.raise(Source::VBlank);
            }
        }
        // The comparison happens as the line changes, which is why a game can
        // set the line to match to one it is already on and hear nothing until
        // the beam comes round again.
        if self.vcount == self.match_line() && self.dispstat & VCOUNT_IRQ != 0 {
            irq.raise(Source::VCount);
        }
        bottom
    }

    /// One byte of the four registers.
    pub fn read8(&self, addr: u32) -> u8 {
        let half = |value: u16| (value >> ((addr & 1) * 8)) as u8;
        match addr & !1 {
            DISPCNT => half(self.dispcnt),
            GREEN_SWAP => half(self.green_swap),
            DISPSTAT => half(self.status()),
            VCOUNT => half(self.vcount),
            BG_CONTROL..BG_SCROLL => half(self.backgrounds[background_of(addr)].control),
            window::WIN0H..=window::WINOUT => self.read_window8(addr),
            blend::BLDCNT..=blend::BLDY => self.read_blend8(addr),
            // The scroll positions are write-only. See [`Background`].
            _ => 0,
        }
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        let widened = |existing: u16| -> u16 {
            let shift = (addr & 1) * 8;
            (existing & !(0xFFu16 << shift)) | (u16::from(value) << shift)
        };
        match addr & !1 {
            DISPCNT => {
                let updated = widened(self.dispcnt);
                self.dispcnt = (updated & !DISPCNT_READ_ONLY) | (self.dispcnt & DISPCNT_READ_ONLY);
            }
            GREEN_SWAP => self.green_swap = widened(self.green_swap),
            // The three reports are the sweep's and a write must not disturb
            // them, which composing the whole halfword would.
            DISPSTAT => self.dispstat = widened(self.dispstat) & DISPSTAT_WRITABLE,
            BG_CONTROL..BG_SCROLL => {
                let index = background_of(addr);
                self.backgrounds[index].control = widened(self.backgrounds[index].control);
            }
            window::WIN0H..=window::WINOUT => self.write_window8(addr, value),
            blend::BLDCNT..=blend::BLDY => self.write_blend8(addr, value),
            // The transformation, all of it write-only like the scroll.
            BG_AFFINE..BLEND => {
                let index = ((addr - BG_AFFINE) / AFFINE_STRIDE) as usize & 1;
                let offset = (addr - BG_AFFINE) % AFFINE_STRIDE;
                let affine = &mut self.affine[index];
                let byte = |existing: i32, at: u32| -> i32 {
                    let at = at * 8;
                    ((existing as u32 & !(0xFFu32 << at)) | (u32::from(value) << at)) as i32
                };
                match offset {
                    0 | 1 => affine.pa = widened(affine.pa as u16) as i16,
                    2 | 3 => affine.pb = widened(affine.pb as u16) as i16,
                    4 | 5 => affine.pc = widened(affine.pc as u16) as i16,
                    6 | 7 => affine.pd = widened(affine.pd as u16) as i16,
                    8..=11 => {
                        affine.x = signed_28(byte(affine.x, offset - 8) as u32);
                        // Writing the starting point takes effect from the next
                        // line drawn, not the next frame. That is what makes a
                        // per-line effect possible at all.
                        affine.reload();
                    }
                    _ => {
                        affine.y = signed_28(byte(affine.y, offset - 12) as u32);
                        affine.reload();
                    }
                }
            }
            BG_SCROLL..BG_AFFINE => {
                let index = background_of(addr);
                let background = &mut self.backgrounds[index];
                // Two halfwords each: the across one first, then the down one.
                // Nine bits are kept of each; a game may write more and the map
                // it scrolls into is not that wide.
                if addr & 2 == 0 {
                    background.hofs = widened(background.hofs) & 0x1FF;
                } else {
                    background.vofs = widened(background.vofs) & 0x1FF;
                }
            }
            // `VCOUNT` is where the beam is. There is no writing that.
            _ => {}
        }
    }

    /// `DISPSTAT` as a game reads it: what was written, plus the three reports.
    pub fn status(&self) -> u16 {
        let mut value = self.dispstat;
        if self.in_vblank() {
            value |= VBLANK_FLAG;
        }
        if self.dot >= HBLANK_AT {
            value |= HBLANK_FLAG;
        }
        if self.vcount == self.match_line() {
            value |= VCOUNT_FLAG;
        }
        value
    }

    /// Whether the beam is below the screen.
    ///
    /// It stops reporting so one line *before* the frame ends, on line 227 and
    /// not on 228. That is the hardware, and it is not an off-by-one to be
    /// tidied: a game that waits for the flag to clear before setting up the
    /// next frame gets a whole line to do it in, which it would not if the flag
    /// cleared as the frame wrapped.
    fn in_vblank(&self) -> bool {
        self.vcount >= VBLANK_AT && self.vcount < LINES_PER_FRAME - 1
    }

    /// The line `DISPSTAT`'s top byte asks to be told about.
    fn match_line(&self) -> u16 {
        self.dispstat >> 8
    }

    pub fn vcount(&self) -> u16 {
        self.vcount
    }

    pub fn dispcnt(&self) -> u16 {
        self.dispcnt
    }

    /// Which of the six ways of drawing is running. Six and seven are not
    /// modes; the hardware draws nothing for them.
    pub fn mode(&self) -> u8 {
        (self.dispcnt & MODE) as u8
    }

    /// Whether the screen is being held white regardless of the mode. A game
    /// sets this while it rearranges video memory, and on the way out of the
    /// BIOS it is the state the machine starts in.
    pub fn forced_blank(&self) -> bool {
        self.dispcnt & FORCED_BLANK != 0
    }

    /// Where sprites begin in video memory, which the memory map needs in order
    /// to know whether a write of a single byte lands or is dropped.
    pub fn obj_base(&self) -> u32 {
        if self.mode() <= 2 { OBJ_BASE_TILED } else { OBJ_BASE_BITMAP }
    }

    /// How many frames have been swept since the machine started.
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_for_everything() -> Interrupts {
        let mut irq = Interrupts::new();
        irq.set_enabled(Source::VBlank.bit() | Source::HBlank.bit() | Source::VCount.bit());
        irq
    }

    /// The shape of the thing: 308 dots a line, 228 lines a frame, four cycles
    /// a dot. Every other test here is measured against these three numbers.
    #[test]
    fn a_frame_is_two_hundred_and_eighty_thousand_cycles() {
        assert_eq!(LINE_CYCLES, 1232);
        assert_eq!(FRAME_CYCLES, 280_896);
        // Which is a little under sixty a second.
        assert_eq!(crate::CLOCK_HZ / FRAME_CYCLES, 59);
    }

    /// The counter a stuck cartridge was reading. If this does not move,
    /// nothing else in the machine does either.
    #[test]
    fn the_line_counter_climbs_and_wraps() {
        let mut ppu = Ppu::new();
        let mut irq = Interrupts::new();
        assert_eq!(ppu.vcount(), 0);

        ppu.tick(LINE_CYCLES, &mut irq);
        assert_eq!(ppu.vcount(), 1, "one line on");

        ppu.tick(LINE_CYCLES * 158, &mut irq);
        assert_eq!(ppu.vcount(), 159, "the last drawn line");

        ppu.tick(LINE_CYCLES, &mut irq);
        assert_eq!(ppu.vcount(), 160, "and past the bottom of the screen");

        ppu.tick(LINE_CYCLES * 68, &mut irq);
        assert_eq!(ppu.vcount(), 0, "round to the top again");
        assert_eq!(ppu.frames(), 1, "having finished one frame");
    }

    /// A tick of a whole frame has to land in the same place as 280 896 ticks
    /// of one, or the shortcut in [`Ppu::tick`] is a lie.
    #[test]
    fn ticking_in_one_go_lands_where_ticking_singly_would() {
        let mut coarse = Ppu::new();
        let mut fine = Ppu::new();
        let mut one = Interrupts::new();
        let mut many = Interrupts::new();

        coarse.tick(FRAME_CYCLES + 700, &mut one);
        for _ in 0..FRAME_CYCLES + 700 {
            fine.tick(1, &mut many);
        }

        assert_eq!(coarse.vcount(), fine.vcount());
        assert_eq!(coarse.status(), fine.status());
        assert_eq!(coarse.frames(), fine.frames());
    }

    /// The interrupt every game waits on, arriving exactly once a frame and at
    /// the line the screen ends on.
    #[test]
    fn the_bottom_of_the_frame_interrupts_once_a_frame() {
        let mut ppu = Ppu::new();
        let mut irq = enabled_for_everything();
        ppu.write8(DISPSTAT, VBLANK_IRQ as u8);

        ppu.tick(LINE_CYCLES * 160 - 1, &mut irq);
        assert_eq!(irq.requested() & Source::VBlank.bit(), 0, "not while the screen is drawn");

        ppu.tick(1, &mut irq);
        assert_eq!(ppu.vcount(), 160);
        assert_ne!(irq.requested() & Source::VBlank.bit(), 0, "and there it is");

        // Once. A handler that retires it does not find it back a line later.
        irq.acknowledge(Source::VBlank.bit());
        ppu.tick(LINE_CYCLES * 60, &mut irq);
        assert_eq!(irq.requested() & Source::VBlank.bit(), 0, "not again during the gap");

        ppu.tick(LINE_CYCLES * 168, &mut irq);
        assert_ne!(irq.requested() & Source::VBlank.bit(), 0, "and again the next frame");
    }

    /// The end of a line interrupts in every line, not only the drawn ones.
    /// A game timing on it below the screen depends on that.
    #[test]
    fn the_end_of_a_line_interrupts_below_the_screen_as_well() {
        let mut ppu = Ppu::new();
        let mut irq = enabled_for_everything();
        ppu.write8(DISPSTAT, HBLANK_IRQ as u8);

        // Straight to a line well below the screen.
        ppu.tick(LINE_CYCLES * 200, &mut irq);
        irq.acknowledge(0xFFFF);
        assert_eq!(ppu.vcount(), 200);

        ppu.tick(HBLANK_AT - 1, &mut irq);
        assert_eq!(irq.requested() & Source::HBlank.bit(), 0, "not while the line is drawn");

        ppu.tick(1, &mut irq);
        assert_ne!(irq.requested() & Source::HBlank.bit(), 0, "raised where the drawing ends");
    }

    /// Nothing is raised for a request that was not made, however far the beam
    /// travels. This is the test that says the enables are read at all.
    #[test]
    fn a_sweep_with_nothing_asked_for_raises_nothing() {
        let mut ppu = Ppu::new();
        let mut irq = enabled_for_everything();

        ppu.tick(FRAME_CYCLES * 3, &mut irq);
        assert_eq!(irq.requested(), 0);
        assert_eq!(ppu.frames(), 3, "and the sweep did happen");
    }

    /// The chosen line reports and interrupts, and the report stands for as
    /// long as the beam is on that line.
    #[test]
    fn the_chosen_line_reports_and_interrupts_when_it_is_reached() {
        let mut ppu = Ppu::new();
        let mut irq = enabled_for_everything();
        ppu.write8(DISPSTAT, VCOUNT_IRQ as u8);
        ppu.write8(DISPSTAT + 1, 80);

        ppu.tick(LINE_CYCLES * 79, &mut irq);
        assert_eq!(ppu.status() & VCOUNT_FLAG, 0, "a line short");
        assert_eq!(irq.requested() & Source::VCount.bit(), 0);

        ppu.tick(LINE_CYCLES, &mut irq);
        assert_eq!(ppu.vcount(), 80);
        assert_ne!(ppu.status() & VCOUNT_FLAG, 0, "on the line, and reported");
        assert_ne!(irq.requested() & Source::VCount.bit(), 0);

        ppu.tick(LINE_CYCLES, &mut irq);
        assert_eq!(ppu.status() & VCOUNT_FLAG, 0, "and past it again");
    }

    /// The report below the screen stops one line before the frame does. A game
    /// that waits for it to clear gets a line to work in, which it would not if
    /// this cleared on the wrap.
    #[test]
    fn the_report_below_the_screen_stops_a_line_before_the_frame_does() {
        let mut ppu = Ppu::new();
        let mut irq = Interrupts::new();

        for line in 0..LINES_PER_FRAME {
            let expected = (160..227).contains(&line);
            assert_eq!(ppu.status() & VBLANK_FLAG != 0, expected, "line {line}");
            ppu.tick(LINE_CYCLES, &mut irq);
        }
    }

    /// The report at the end of a line is on for the 68 dots the gap lasts and
    /// off for the 240 the drawing does.
    #[test]
    fn the_report_at_the_end_of_a_line_covers_the_gap_and_not_the_drawing() {
        let mut ppu = Ppu::new();
        let mut irq = Interrupts::new();

        assert_eq!(ppu.status() & HBLANK_FLAG, 0, "at the start of a line");
        ppu.tick(HBLANK_AT - 4, &mut irq);
        assert_eq!(ppu.status() & HBLANK_FLAG, 0, "at the last drawn dot");
        ppu.tick(4, &mut irq);
        assert_ne!(ppu.status() & HBLANK_FLAG, 0, "one dot later");
        ppu.tick(LINE_CYCLES - HBLANK_AT, &mut irq);
        assert_eq!(ppu.status() & HBLANK_FLAG, 0, "and off again with the new line");
    }

    /// The three reports belong to the sweep, and a game writing the register
    /// must not be able to set or clear one.
    #[test]
    fn writing_the_status_register_cannot_touch_the_three_reports() {
        let mut ppu = Ppu::new();
        let mut irq = Interrupts::new();
        ppu.tick(LINE_CYCLES * 170, &mut irq);
        assert_ne!(ppu.status() & VBLANK_FLAG, 0, "below the screen");

        // Every bit set, including the reports.
        ppu.write8(DISPSTAT, 0xFF);
        assert_ne!(ppu.status() & VBLANK_FLAG, 0, "still below the screen");
        assert_eq!(ppu.status() & HBLANK_FLAG, 0, "and still not at the end of a line");

        // And no bits at all, which cannot clear a report that is true.
        ppu.write8(DISPSTAT, 0x00);
        assert_ne!(ppu.status() & VBLANK_FLAG, 0);
    }

    /// The registers are halfwords and a game may write either byte of one.
    #[test]
    fn each_register_can_be_reached_a_byte_at_a_time() {
        let mut ppu = Ppu::new();

        ppu.write8(DISPCNT, 0x40);
        ppu.write8(DISPCNT + 1, 0x1F);
        assert_eq!(ppu.dispcnt(), 0x1F40);
        assert_eq!(ppu.read8(DISPCNT), 0x40);
        assert_eq!(ppu.read8(DISPCNT + 1), 0x1F);

        ppu.write8(DISPSTAT + 1, 0x9C);
        assert_eq!(ppu.read8(DISPSTAT + 1), 0x9C, "the line to match is the top byte");
        assert_eq!(ppu.status() >> 8, 0x9C);
    }

    /// The line the beam is on is the hardware's to say.
    #[test]
    fn the_line_counter_cannot_be_written() {
        let mut ppu = Ppu::new();
        let mut irq = Interrupts::new();
        ppu.tick(LINE_CYCLES * 5, &mut irq);

        ppu.write8(VCOUNT, 99);
        ppu.write8(VCOUNT + 1, 99);
        assert_eq!(ppu.vcount(), 5);
        assert_eq!(ppu.read8(VCOUNT), 5);
        assert_eq!(ppu.read8(VCOUNT + 1), 0, "and its top byte is nothing");
    }

    /// The bit that says which console's cartridge is in belongs to the
    /// hardware, and a game writing ones everywhere must not set it.
    #[test]
    fn the_read_only_bit_of_the_control_register_stays_where_it_was() {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, 0xFF);
        assert_eq!(ppu.dispcnt() & DISPCNT_READ_ONLY, 0);
        assert_eq!(ppu.dispcnt() & 0xF7, 0xF7, "and every other bit did land");
    }

    /// Where sprites begin moves with the mode, and the memory map cannot know
    /// it without asking.
    #[test]
    fn sprites_begin_further_in_once_the_picture_is_a_bitmap() {
        let mut ppu = Ppu::new();
        for mode in 0..=2 {
            ppu.write8(DISPCNT, mode);
            assert_eq!(ppu.obj_base(), OBJ_BASE_TILED, "mode {mode}");
        }
        for mode in 3..=5 {
            ppu.write8(DISPCNT, mode);
            assert_eq!(ppu.obj_base(), OBJ_BASE_BITMAP, "mode {mode}");
        }
    }

    #[test]
    fn the_mode_and_the_blank_are_read_out_of_the_control_register() {
        let mut ppu = Ppu::new();
        assert_eq!(ppu.mode(), 0);
        assert!(!ppu.forced_blank());

        ppu.write8(DISPCNT, 0x84);
        assert_eq!(ppu.mode(), 4);
        assert!(ppu.forced_blank(), "held blank whatever the mode says");
    }
}
