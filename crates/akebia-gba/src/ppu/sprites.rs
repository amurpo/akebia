//! The things that move.
//!
//! # What a sprite is, and why it is not a background
//!
//! A background covers the screen and is moved by changing where the screen
//! looks into it. A sprite is the opposite: a small picture with a position of
//! its own, drawn wherever that position says, over everything sharing its
//! priority. A game's characters, its bullets, its cursor and most of its
//! interface are sprites, which is why a machine that draws only backgrounds
//! shows a title screen and nothing that moves on it.
//!
//! There are 128 of them and they are described in a memory of their own —
//! object memory, a kilobyte holding eight bytes per sprite. Six of those bytes
//! are the sprite; the other two belong to something else entirely, which is
//! the arrangement described under [`transform`](Ppu::transform) below.
//!
//! # Back to front, again
//!
//! The rule the backgrounds are drawn by holds here too, and it now has to hold
//! across both kinds at once. Everything is drawn one priority at a time from
//! the back, and within a priority the sprites go over the backgrounds — a
//! sprite and a background that claim the same priority are not a tie, the
//! sprite simply wins. Among sprites the *lower entry number* is nearer, so
//! this counts entries downwards and lets entry 0 be the last to write.
//!
//! # A sprite's tiles are not a background's
//!
//! They live in their own end of video memory, they are numbered from there
//! rather than from the start of memory, and where that end begins depends on
//! the mode: the bitmap modes spend the first 80 KiB on the picture and leave
//! sprites 16 KiB instead of 32. A sprite's colours come from the second half
//! of the palette, so the same index is a different colour on a sprite than on
//! a background.
//!
//! The tiles of one sprite can be laid out two ways, and `DISPCNT` says which
//! for all of them at once. One-dimensional puts them one after another, so a
//! sprite is a run of tiles. Two-dimensional treats video memory as a grid 32
//! tiles wide and takes a rectangle out of it, so the next row of a sprite is
//! 32 tiles on regardless of how wide the sprite is. Neither is better; a game
//! picks the one its tools produce.

use super::blend::{self, Pixel};
use super::window::Target;
use super::{Ppu, OBJ_BASE_BITMAP, OBJ_BASE_TILED, VRAM_LEN};
use crate::SCREEN_WIDTH;

/// How many sprites there are, and how many bytes each is described by.
const SPRITES: usize = 128;
const ENTRY: usize = 8;

/// How much video memory sprites have, at the most: the last 32 KiB. It is a
/// power of two, so a tile number that runs off the end comes round.
const OBJ_VRAM: usize = 0x8000;

/// The palette is halved: backgrounds take the first 256 colours and sprites
/// the second.
const OBJ_PALETTE: usize = 0x200;

/// `DISPCNT`'s bit for sprites, and the one that says how their tiles are laid
/// out.
const OBJ_ENABLED: u16 = 1 << 12;
const ONE_DIMENSIONAL: u16 = 1 << 6;

/// The first attribute: where the sprite is down the screen, what kind of
/// sprite it is, and how it is shaped.
const Y: u16 = 0x00FF;
const KIND: u16 = 0x0300;
const GRAPHICS: u16 = 0x0C00;
const FULL_COLOUR: u16 = 1 << 13;
const SHAPE: u16 = 0xC000;

/// Three of the four kinds an entry can name. The fourth is hidden, and it has
/// no constant here because it never becomes a sprite: see [`Kind`].
const PLAIN: u16 = 0x0000;
const TRANSFORMED: u16 = 0x0100;
const TRANSFORMED_DOUBLE: u16 = 0x0300;

/// A sprite whose graphics mode is 2 is not a picture: it is the shape of a
/// window cut in the other layers. Without windows it draws nothing, which is
/// what the hardware does with it as well — it never appears on screen in its
/// own right.
const WINDOW: u16 = 0x0800;

/// Graphics mode 1: the sprite is mixed with whatever it is over, and it says
/// so itself rather than being named in a register. See [`crate::ppu::blend`].
const SEMI_TRANSPARENT: u16 = 0x0400;

/// The second attribute: where the sprite is across the screen, which way round
/// it goes, and which of the four sizes of its shape it is.
const X: u16 = 0x01FF;
const TRANSFORM_GROUP: u16 = 0x3E00;
const FLIP_ACROSS: u16 = 1 << 12;
const FLIP_DOWN: u16 = 1 << 13;
const SIZE: u16 = 0xC000;

/// The third: which tile it starts at, how near it is, and which of the sixteen
/// small palettes it uses.
const TILE: u16 = 0x03FF;
const PRIORITY: u16 = 0x0C00;
const PALETTE: u16 = 0xF000;

/// Across the screen is nine bits, and it wraps: a sprite at 500 is off the
/// left-hand edge, not the right.
const ACROSS: usize = 0x200;
/// Down the screen is eight, and wraps likewise.
const DOWN: usize = 0x100;

/// The kinds of sprite that get drawn.
///
/// There are four values of the field and only three of them are here. The
/// missing one is hidden, and it is missing on purpose: [`Sprite::read`]
/// answers with nothing for it, so a parked sprite never becomes one of these
/// and there is no later place where it might be drawn by accident.
///
/// Parking is a kind rather than a position because there is nowhere
/// off-screen to put a sprite — both coordinates wrap — and every one of the
/// 128 entries is read on every line whether a game is using it or not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Turned,
    /// Turned, and given twice the room to turn in.
    TurnedInDoubleArea,
}

/// One entry of object memory, read out.
///
/// This is the whole of a sprite as far as drawing is concerned; nothing here
/// touches memory. Reading the six bytes into named fields once, rather than
/// picking bits out where they are used, is what keeps the drawing loops about
/// pixels instead of about masks.
#[derive(Clone, Copy)]
struct Sprite {
    /// Where the top-left corner goes. Across is signed in nine bits, so this
    /// is kept as the raw value and wrapped as it is used.
    x: usize,
    y: usize,
    /// How big the picture is, in pixels. Never how big the area drawn is —
    /// see [`Sprite::area`].
    width: usize,
    height: usize,
    kind: Kind,
    /// Which of the 32 transformations, for the two kinds that use one.
    group: usize,
    flip_across: bool,
    flip_down: bool,
    full_colour: bool,
    tile: usize,
    palette: u8,
    priority: u16,
    /// Declared in the sprite's own entry, and the one thing about blending
    /// that does not come out of a register: such a sprite is always mixed with
    /// what is under it, whatever `BLDCNT` was set to.
    translucent: bool,
    /// Graphics mode 2: not a picture at all, but the shape of the third
    /// window region. It is never drawn — its pixels mark where that region
    /// is, and its colours are never looked at. See [`crate::ppu::window`].
    cuts_window: bool,
}

impl Sprite {
    /// Reads one entry, or nothing if it is not a sprite that gets drawn.
    fn read(oam: &[u8], entry: usize) -> Option<Self> {
        let at = entry * ENTRY;
        let half = |offset: usize| u16::from(oam[at + offset]) | (u16::from(oam[at + offset + 1]) << 8);
        let (attr0, attr1, attr2) = (half(0), half(2), half(4));

        let kind = match attr0 & KIND {
            PLAIN => Kind::Plain,
            TRANSFORMED => Kind::Turned,
            TRANSFORMED_DOUBLE => Kind::TurnedInDoubleArea,
            // The fourth value is hidden, which is how a game parks an entry
            // it is not using. It stops being a sprite here and nowhere later.
            _ => return None,
        };

        // Shape 3 names no sprite. The hardware draws something for it; what it
        // draws is not documented and no game asks, so nothing is safer than a
        // guess that would be wrong in a way nobody could check.
        let (width, height) = size(attr0 & SHAPE, attr1 & SIZE)?;

        Some(Self {
            x: usize::from(attr1 & X),
            y: usize::from(attr0 & Y),
            width,
            height,
            kind,
            group: usize::from((attr1 & TRANSFORM_GROUP) >> 9),
            // A transformed sprite has no mirror bits: those two bits are part
            // of the number of the transformation it uses instead. Reading them
            // as flips would turn a rotating sprite inside out.
            flip_across: kind == Kind::Plain && attr1 & FLIP_ACROSS != 0,
            flip_down: kind == Kind::Plain && attr1 & FLIP_DOWN != 0,
            full_colour: attr0 & FULL_COLOUR != 0,
            tile: usize::from(attr2 & TILE),
            palette: ((attr2 & PALETTE) >> 12) as u8,
            priority: (attr2 & PRIORITY) >> 10,
            translucent: attr0 & GRAPHICS == SEMI_TRANSPARENT,
            cuts_window: attr0 & GRAPHICS == WINDOW,
        })
    }

    /// How much of the screen is walked looking for this sprite's pixels.
    ///
    /// The same as its size, except for the one kind that asks for twice it. A
    /// square turned 45 degrees needs about 1.41 times its own width to fit its
    /// corners, and the hardware only walks the area it is told to — so a
    /// sprite rotated inside its own bounds has its corners cut off. Doubling
    /// is the fix and the cost is drawing four times the area.
    fn area(&self) -> (usize, usize) {
        if self.kind == Kind::TurnedInDoubleArea {
            (self.width * 2, self.height * 2)
        } else {
            (self.width, self.height)
        }
    }

    /// Which row of the drawn area this screen line falls on, if any.
    ///
    /// Down the screen wraps at 256 rather than at the bottom of the screen, so
    /// a sprite at 250 hangs off the top and not off the bottom. That is how a
    /// game slides one in from above, and treating the coordinate as a plain
    /// number would put it 90 lines below the screen instead.
    fn row_on(&self, line: usize) -> Option<usize> {
        let row = (line + DOWN - self.y % DOWN) % DOWN;
        (row < self.area().1).then_some(row)
    }

    /// Which pixel of the picture a place in the drawn area is, for a sprite
    /// that is not transformed: the same place, mirrored if asked.
    fn pixel_at(&self, column: usize, row: usize) -> (usize, usize) {
        let x = if self.flip_across { self.width - 1 - column } else { column };
        let y = if self.flip_down { self.height - 1 - row } else { row };
        (x, y)
    }
}

/// How big a sprite is, out of its shape and its size. Twelve of the sixteen
/// combinations are sprites; the four of shape 3 are not.
fn size(shape: u16, size: u16) -> Option<(usize, usize)> {
    let sizes: [(usize, usize); 4] = match shape {
        0x0000 => [(8, 8), (16, 16), (32, 32), (64, 64)],
        0x4000 => [(16, 8), (32, 8), (32, 16), (64, 32)],
        0x8000 => [(8, 16), (8, 32), (16, 32), (32, 64)],
        _ => return None,
    };
    Some(sizes[usize::from(size >> 14)])
}

impl Ppu {
    /// Every sprite of one priority, drawn over whatever is already there.
    ///
    /// Counting down is the whole of the ordering rule: the lower the entry
    /// number the nearer the sprite, so entry 0 is written last and covers the
    /// rest.
    pub(super) fn draw_sprites_at(&mut self, line: usize, priority: u16) {
        if self.dispcnt & OBJ_ENABLED == 0 {
            return;
        }
        for entry in (0..SPRITES).rev() {
            let Some(sprite) = Sprite::read(&self.oam[..], entry) else {
                continue;
            };
            // The ones that cut a window are not pictures and have no
            // priority worth honouring: they were walked before the line
            // started, into the mask that decided where everything else is
            // allowed.
            if sprite.cuts_window || sprite.priority != priority {
                continue;
            }
            let Some(row) = sprite.row_on(line) else {
                continue;
            };
            self.draw_sprite(&sprite, row, Target::Picture);
        }
    }

    /// Walks the sprites that cut the third window region, marking where they
    /// cover this line.
    ///
    /// It has to happen before anything is drawn, because what it marks is what
    /// decides whether the other layers are allowed at all. And it ignores
    /// priority entirely: this is not a sprite going in front of or behind
    /// anything, it is a shape.
    pub(super) fn mark_window_sprites(&mut self, line: usize) {
        if self.dispcnt & OBJ_ENABLED == 0 {
            return;
        }
        for entry in (0..SPRITES).rev() {
            let Some(sprite) = Sprite::read(&self.oam[..], entry) else {
                continue;
            };
            if !sprite.cuts_window {
                continue;
            }
            let Some(row) = sprite.row_on(line) else {
                continue;
            };
            self.draw_sprite(&sprite, row, Target::WindowMask);
        }
    }

    /// One sprite, however it is shaped, into whichever of the two things a
    /// covered pixel becomes.
    fn draw_sprite(&mut self, sprite: &Sprite, row: usize, target: Target) {
        match sprite.kind {
            Kind::Plain => self.draw_plain_sprite(sprite, row, target),
            Kind::Turned | Kind::TurnedInDoubleArea => {
                self.draw_transformed_sprite(sprite, row, target)
            }
        }
    }

    /// What a covered pixel does: become a colour, or mark the window.
    ///
    /// A sprite that cuts a window is walked for its *shape* and never for its
    /// colours — the palette entry says only whether the pixel is covered.
    fn cover(&mut self, sprite: &Sprite, x: usize, colour: u8, target: Target) {
        match target {
            Target::Picture => {
                let pixel = self.sprite_layer(sprite, colour);
                self.put(x, pixel);
            }
            Target::WindowMask => self.obj_window[x] = true,
        }
    }

    /// A sprite that is only somewhere, not turned: its pixels go straight
    /// across, mirrored if the entry asked for it.
    fn draw_plain_sprite(&mut self, sprite: &Sprite, row: usize, target: Target) {
        for column in 0..sprite.width {
            // Across the screen wraps at 512, which is what puts a sprite with
            // a large coordinate off the left-hand edge instead of the right.
            let screen_x = (sprite.x + column) % ACROSS;
            if screen_x >= SCREEN_WIDTH {
                continue;
            }
            let (px, py) = sprite.pixel_at(column, row);
            let colour = self.sprite_pixel(sprite, px, py);
            if colour != 0 {
                self.cover(sprite, screen_x, colour, target);
            }
        }
    }

    /// A sprite that has been rotated or scaled.
    ///
    /// The same trick the transformed backgrounds use, about a different point:
    /// the screen is walked and the *picture* is read at a slant, because going
    /// the other way leaves holes wherever the picture is stretched. The turn
    /// happens about the middle of the drawn area, which is why every offset
    /// here is measured from a centre rather than from a corner — a sprite
    /// rotating in place has to stay in place, and turning about its top-left
    /// corner would swing it around the screen instead.
    fn draw_transformed_sprite(&mut self, sprite: &Sprite, row: usize, target: Target) {
        let (pa, pb, pc, pd) = self.transform(sprite.group);
        let (area_width, area_height) = sprite.area();

        // How far down the area's middle this line is, and where the middle of
        // the picture is. The two differ whenever the area is doubled.
        let dy = row as i32 - (area_height / 2) as i32;
        let (half_width, half_height) = ((sprite.width / 2) as i32, (sprite.height / 2) as i32);

        for column in 0..area_width {
            let screen_x = (sprite.x + column) % ACROSS;
            if screen_x >= SCREEN_WIDTH {
                continue;
            }
            let dx = column as i32 - (area_width / 2) as i32;

            // Eight-bit fractions, so the sum is shifted down to become a
            // coordinate. What the shift throws away is what makes the picture
            // stretch.
            let px = ((i32::from(pa) * dx + i32::from(pb) * dy) >> 8) + half_width;
            let py = ((i32::from(pc) * dx + i32::from(pd) * dy) >> 8) + half_height;
            // Landing outside the picture is nothing at all. This is the edge
            // the doubled area buys room for.
            if px < 0 || py < 0 || px >= sprite.width as i32 || py >= sprite.height as i32 {
                continue;
            }

            let colour = self.sprite_pixel(sprite, px as usize, py as usize);
            if colour != 0 {
                self.cover(sprite, screen_x, colour, target);
            }
        }
    }

    /// One of the 32 transformations, which are kept in the two bytes of each
    /// entry that are not part of a sprite.
    ///
    /// # Why they are hidden there
    ///
    /// Object memory is a kilobyte and 128 sprites need six bytes each, which
    /// leaves 256 bytes spare — but as two-byte gaps every eight, not as a
    /// block. So the transformations are stored *in the gaps*: four halfwords
    /// four entries apart make one, and there are 32 of them. Nothing about a
    /// sprite's own entry says which transformation lives in its gap, and a
    /// game rewriting a sprite must leave that halfword alone.
    fn transform(&self, group: usize) -> (i16, i16, i16, i16) {
        let at = group * ENTRY * 4 + 6;
        let half = |offset: usize| {
            let at = (at + offset) % super::OAM_LEN;
            (u16::from(self.oam[at]) | (u16::from(self.oam[at + 1]) << 8)) as i16
        };
        (half(0), half(ENTRY), half(ENTRY * 2), half(ENTRY * 3))
    }

    /// The colour index of one pixel of a sprite's picture.
    ///
    /// A sprite is a rectangle of eight-by-eight tiles, and which tile a pixel
    /// falls in is the whole of the work. The two layouts differ only in how
    /// far apart consecutive rows of that rectangle are: side by side, or 32
    /// tiles apart because memory is being treated as a grid that wide.
    fn sprite_pixel(&self, sprite: &Sprite, px: usize, py: usize) -> u8 {
        // A 256-colour tile is 64 bytes where a 16-colour one is 32, but tiles
        // are numbered in 32-byte slots either way — so a full-colour sprite
        // steps two slots per tile, and its odd tile numbers name nothing.
        let step = if sprite.full_colour { 2 } else { 1 };
        let (block_x, block_y) = (px / 8, py / 8);

        let tile = if self.dispcnt & ONE_DIMENSIONAL != 0 {
            sprite.tile + (block_y * (sprite.width / 8) + block_x) * step
        } else {
            // The grid is 32 slots wide whatever the sprite is, so a row down
            // is 32 on. That the sprite's own width does not appear here is the
            // whole difference between the two layouts.
            sprite.tile + block_y * 32 + block_x * step
        };

        let base = tile * 32;
        let (x, y) = (px % 8, py % 8);
        if sprite.full_colour {
            self.sprite_byte(base + y * 8 + x)
        } else {
            let pair = self.sprite_byte(base + y * 4 + x / 2);
            if x & 1 == 0 { pair & 0xF } else { pair >> 4 }
        }
    }

    /// A byte of the sprites' end of video memory.
    ///
    /// Tile numbers count from there and not from the start of memory, and in
    /// the bitmap modes there is half as much of it because the picture has
    /// taken the rest. A tile number naming the part that is not there reads as
    /// nothing, which is what the hardware shows for it too.
    fn sprite_byte(&self, at: usize) -> u8 {
        let base = if self.mode() <= 2 { OBJ_BASE_TILED } else { OBJ_BASE_BITMAP } as usize;
        let at = base + at % OBJ_VRAM;
        if at < VRAM_LEN {
            self.vram[at]
        } else {
            0
        }
    }

    /// A sprite's colour, out of the half of the palette that is theirs.
    ///
    /// The small palettes are only for the 16-colour sprites: a full-colour one
    /// names all 256 directly and the palette field of its entry means nothing.
    fn sprite_colour(&self, sprite: &Sprite, index: u8) -> u16 {
        let index = if sprite.full_colour {
            usize::from(index)
        } else {
            usize::from(sprite.palette) * 16 + usize::from(index)
        };
        let at = OBJ_PALETTE + index * 2;
        (u16::from(self.pram[at]) | (u16::from(self.pram[at + 1]) << 8)) & 0x7FFF
    }

    /// The same colour, carrying where it came from.
    ///
    /// Every sprite is the one sprite layer whatever its priority, which is why
    /// nothing here varies but the flag: two sprites over each other are not two
    /// layers, and blending never happens between them.
    fn sprite_layer(&self, sprite: &Sprite, index: u8) -> Pixel {
        Pixel {
            colour: self.sprite_colour(sprite, index),
            layer: blend::OBJ,
            translucent: sprite.translucent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupts::Interrupts;
    use crate::ppu::{DISPCNT, FRAME_CYCLES};

    /// The kind that parks an entry, which has no constant in the code above
    /// because it never becomes a [`Sprite`].
    const HIDDEN: u16 = 0x0200;

    const RED: u16 = 0x001F;
    const GREEN: u16 = 0x03E0;
    const BLUE: u16 = 0x7C00;

    /// A machine with every sprite parked and the sprites switched on.
    ///
    /// Parking them all first is not tidiness. An unwritten entry is eight
    /// zero bytes, and eight zero bytes are a perfectly good sprite: eight
    /// pixels square, at the corner of the screen, made of tile 0. Leave them
    /// and 128 of them pile up in that corner, so every assertion about an
    /// empty screen is measuring 128 sprites instead of none. A game parks them
    /// for the same reason before it uses any.
    fn machine(mode: u8) -> Ppu {
        let mut ppu = Ppu::new();
        ppu.write8(DISPCNT, mode);
        ppu.write8(DISPCNT + 1, (OBJ_ENABLED >> 8) as u8);
        for entry in 0..SPRITES {
            put(&mut ppu, entry, HIDDEN, 0, 0);
        }
        ppu
    }

    /// The three halfwords that describe one sprite. The fourth is not a
    /// sprite's and is left alone.
    fn put(ppu: &mut Ppu, entry: usize, attr0: u16, attr1: u16, attr2: u16) {
        for (offset, value) in [attr0, attr1, attr2].into_iter().enumerate() {
            let at = entry * ENTRY + offset * 2;
            ppu.oam_mut()[at] = value as u8;
            ppu.oam_mut()[at + 1] = (value >> 8) as u8;
        }
    }

    /// One of the 32 transformations, written into the gaps between entries.
    fn put_transform(ppu: &mut Ppu, group: usize, pa: i16, pb: i16, pc: i16, pd: i16) {
        for (which, value) in [pa, pb, pc, pd].into_iter().enumerate() {
            let at = group * ENTRY * 4 + 6 + which * ENTRY;
            ppu.oam_mut()[at] = value as u8;
            ppu.oam_mut()[at + 1] = (value >> 8) as u8;
        }
    }

    /// A tile of a 16-colour sprite: 32 bytes, a nibble a pixel.
    fn fill_tile(ppu: &mut Ppu, tile: usize, index: u8) {
        let base = OBJ_BASE_TILED as usize + tile * 32;
        for byte in 0..32 {
            ppu.vram_mut()[base + byte] = index | (index << 4);
        }
    }

    /// A tile of a 256-colour sprite: 64 bytes, a byte a pixel, and two slots.
    fn fill_full_tile(ppu: &mut Ppu, tile: usize, index: u8) {
        let base = OBJ_BASE_TILED as usize + tile * 32;
        for byte in 0..64 {
            ppu.vram_mut()[base + byte] = index;
        }
    }

    fn set_colour(ppu: &mut Ppu, index: usize, colour: u16) {
        let at = OBJ_PALETTE + index * 2;
        ppu.pram_mut()[at] = colour as u8;
        ppu.pram_mut()[at + 1] = (colour >> 8) as u8;
    }

    /// The backdrop, which is the first colour of the *backgrounds'* half.
    fn set_backdrop(ppu: &mut Ppu, colour: u16) {
        ppu.pram_mut()[0] = colour as u8;
        ppu.pram_mut()[1] = (colour >> 8) as u8;
    }

    /// Lays the tiles out one after another rather than as a grid.
    ///
    /// This bit is in `DISPCNT`'s *low* byte, beside the mode — not beside the
    /// enable bits in the high one, which is where writing it would quietly do
    /// nothing and leave every test measuring the other layout.
    fn one_dimensional(ppu: &mut Ppu) {
        let low = ppu.dispcnt() | ONE_DIMENSIONAL;
        ppu.write8(DISPCNT, low as u8);
    }

    /// A background of one colour covering the screen, at the given priority.
    /// For the tests about what covers what.
    fn covering_background(ppu: &mut Ppu, priority: u16, colour: u16) {
        ppu.pram_mut()[2] = colour as u8;
        ppu.pram_mut()[3] = (colour >> 8) as u8;
        // Tile 1 of block 0, and a map of nothing but tile 1 in block 1.
        for byte in 32..64 {
            ppu.vram_mut()[byte] = 0x11;
        }
        for entry in 0..32 * 32 {
            ppu.vram_mut()[MAP + entry * 2] = 1;
        }
        ppu.write8(crate::ppu::BG_CONTROL, priority as u8);
        ppu.write8(crate::ppu::BG_CONTROL + 1, 1);
        let on = ppu.dispcnt() | (1 << 8);
        ppu.write8(DISPCNT + 1, (on >> 8) as u8);
    }

    fn sweep_a_frame(ppu: &mut Ppu) {
        let mut irq = Interrupts::new();
        ppu.tick(FRAME_CYCLES, &mut irq);
    }

    fn pixel(ppu: &Ppu, x: usize, y: usize) -> u16 {
        ppu.frame()[y * SCREEN_WIDTH + x]
    }

    /// A sprite of one tile, put somewhere. Everything below is a departure
    /// from this.
    #[test]
    fn a_sprite_is_drawn_where_its_entry_says() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        put(&mut ppu, 0, 40, 100, 1);

        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, 100, 40), RED, "the corner it names");
        assert_eq!(pixel(&ppu, 107, 47), RED, "and all eight pixels of it");
        assert_eq!(pixel(&ppu, 108, 40), BLUE, "nothing beside it");
        assert_eq!(pixel(&ppu, 100, 39), BLUE, "and nothing above it");
    }

    /// Parking a sprite is a mode, not a position. There is nowhere off-screen
    /// to put one — the coordinates wrap — so this is the only way.
    #[test]
    fn a_hidden_sprite_is_not_drawn() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        put(&mut ppu, 0, HIDDEN | 40, 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), BLUE);
    }

    /// And with the sprites switched off at `DISPCNT`, none of them are.
    #[test]
    fn sprites_switched_off_are_not_drawn() {
        let mut ppu = machine(0);
        ppu.write8(DISPCNT + 1, 0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        put(&mut ppu, 0, 40, 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), BLUE);
    }

    /// Index zero is nothing here as well, which is what gives a sprite a shape
    /// other than a rectangle.
    #[test]
    fn the_zeroth_index_of_a_sprite_shows_what_is_behind() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 0, GREEN);
        fill_tile(&mut ppu, 1, 0);
        put(&mut ppu, 0, 40, 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), BLUE, "the backdrop, not the sprite's colour 0");
    }

    /// Mirroring, which is how a character faces both ways out of one set of
    /// tiles.
    #[test]
    fn a_sprite_can_be_mirrored_either_way() {
        // A 16-by-16 sprite of four tiles, only the first of which is filled,
        // so which corner it lands in says which way round the sprite went.
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        for tile in 2..5 {
            fill_tile(&mut ppu, tile, 0);
        }
        // Shape 0, size 1: sixteen square. One-dimensional mapping.
        one_dimensional(&mut ppu);

        put(&mut ppu, 0, 40, 0x4000 | 100, 1);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), RED, "unmirrored, the filled tile is top left");
        assert_eq!(pixel(&ppu, 108, 40), BLUE);

        put(&mut ppu, 0, 40, FLIP_ACROSS | 0x4000 | 100, 1);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 108, 40), RED, "mirrored across, it is top right");
        assert_eq!(pixel(&ppu, 100, 40), BLUE);

        put(&mut ppu, 0, 40, FLIP_DOWN | 0x4000 | 100, 1);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 48), RED, "mirrored down, it is bottom left");
        assert_eq!(pixel(&ppu, 100, 40), BLUE);
    }

    /// Sixteen palettes of sixteen, chosen per sprite rather than per tile.
    #[test]
    fn a_sprite_chooses_which_of_the_sixteen_palettes_it_uses() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        set_colour(&mut ppu, 3 * 16 + 1, GREEN);
        fill_tile(&mut ppu, 1, 1);

        put(&mut ppu, 0, 40, 100, 1);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), RED, "palette 0");

        put(&mut ppu, 0, 40, 100, (3 << 12) | 1);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), GREEN, "and palette 3, the same tile");
    }

    /// A full-colour sprite names all 256 directly, and its palette field means
    /// nothing — reading it would send every such sprite to the wrong colours.
    #[test]
    fn a_full_colour_sprite_ignores_the_palette_field() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 200, GREEN);
        fill_full_tile(&mut ppu, 2, 200);

        put(&mut ppu, 0, FULL_COLOUR | 40, 100, (5 << 12) | 2);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), GREEN, "index 200, whatever the palette field says");
    }

    /// Sprites take their colours from the second half of the palette, so the
    /// same index is a different colour on a sprite and on a background.
    #[test]
    fn a_sprite_reads_the_half_of_the_palette_that_is_theirs() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        // Index 1 of the backgrounds' half, which a sprite must not reach.
        ppu.pram_mut()[2] = GREEN as u8;
        ppu.pram_mut()[3] = (GREEN >> 8) as u8;
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);

        put(&mut ppu, 0, 40, 100, 1);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), RED, "the sprites' index 1, not the backgrounds'");
    }

    /// The twelve sizes, each measured at its own far corner and one pixel past
    /// it. A table of sizes is the kind of thing that is wrong in one entry.
    #[test]
    fn the_twelve_shapes_and_sizes_are_the_sizes_they_name() {
        for (shape, size, width, height) in [
            (0u16, 0u16, 8usize, 8usize),
            (0, 1, 16, 16),
            (0, 2, 32, 32),
            (0, 3, 64, 64),
            (1, 0, 16, 8),
            (1, 1, 32, 8),
            (1, 2, 32, 16),
            (1, 3, 64, 32),
            (2, 0, 8, 16),
            (2, 1, 8, 32),
            (2, 2, 16, 32),
            (2, 3, 32, 64),
        ] {
            let mut ppu = machine(0);
            one_dimensional(&mut ppu);
            set_backdrop(&mut ppu, BLUE);
            set_colour(&mut ppu, 1, RED);
            // A sprite is at most 64 by 64, which is 64 tiles.
            for tile in 1..=64 {
                fill_tile(&mut ppu, tile, 1);
            }
            put(&mut ppu, 0, shape << 14, size << 14, 1);
            sweep_a_frame(&mut ppu);

            let what = format!("shape {shape} size {size} is {width} by {height}");
            assert_eq!(pixel(&ppu, width - 1, height - 1), RED, "{what}: its far corner");
            assert_eq!(pixel(&ppu, width, 0), BLUE, "{what}: one pixel past its width");
            assert_eq!(pixel(&ppu, 0, height), BLUE, "{what}: one line past its height");
        }
    }

    /// Shape 3 names no sprite, and nothing is drawn for it.
    ///
    /// This is a decision rather than an omission. The hardware draws
    /// *something* — the shape and size fields still pick a size out of
    /// whatever the silicon does with a value that means nothing — and what it
    /// picks is undocumented, no game asks, and a guess would be wrong in a way
    /// nobody could check. Drawing nothing is at least wrong in a way that is
    /// visible.
    #[test]
    fn the_fourth_shape_names_no_sprite() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        for tile in 1..=64 {
            fill_tile(&mut ppu, tile, 1);
        }
        for size in 0..4 {
            put(&mut ppu, 0, 3 << 14, size << 14, 1);
            sweep_a_frame(&mut ppu);
            assert_eq!(pixel(&ppu, 0, 0), BLUE, "shape 3 size {size}");
        }
    }

    /// The two layouts differ in where the second row of a sprite's tiles is:
    /// straight after the first, or 32 tiles on because memory is a grid that
    /// wide. A sprite drawn under the wrong one is made of the right tiles in
    /// the wrong places, which is why this is set for the whole machine and not
    /// per sprite.
    #[test]
    fn the_two_tile_layouts_put_the_second_row_in_different_places() {
        // A 16-by-16 sprite starting at tile 1. Its second row is tile 3 under
        // one layout and tile 33 under the other; only one of them is filled.
        let draw = |in_a_row: bool, filled: usize| {
            let mut ppu = machine(0);
            if in_a_row {
                one_dimensional(&mut ppu);
            }
            set_backdrop(&mut ppu, BLUE);
            set_colour(&mut ppu, 1, RED);
            fill_tile(&mut ppu, filled, 1);
            put(&mut ppu, 0, 0, 0x4000, 1);
            sweep_a_frame(&mut ppu);
            pixel(&ppu, 0, 8)
        };

        assert_eq!(draw(true, 3), RED, "one-dimensional: the row after the first two");
        assert_eq!(draw(true, 33), BLUE, "and not 32 on");
        assert_eq!(draw(false, 33), RED, "two-dimensional: a row of the grid down");
        assert_eq!(draw(false, 3), BLUE, "and not the next tile along");
    }

    /// A full-colour tile is 64 bytes but tiles are numbered in 32-byte slots,
    /// so such a sprite steps two slots per tile. Stepping one would draw its
    /// second tile out of the middle of its first.
    #[test]
    fn a_full_colour_sprite_steps_two_slots_for_every_tile() {
        let mut ppu = machine(0);
        one_dimensional(&mut ppu);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 100, RED);
        set_colour(&mut ppu, 200, GREEN);
        // Tile 2 is the first block, tile 4 the second: two slots apart.
        fill_full_tile(&mut ppu, 2, 100);
        fill_full_tile(&mut ppu, 4, 200);

        // Shape 1 size 0: sixteen across by eight down, so two tiles.
        put(&mut ppu, 0, FULL_COLOUR, 0x4000, 2);
        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, 0, 0), RED, "the first tile");
        assert_eq!(pixel(&ppu, 8, 0), GREEN, "and the second, two slots on");
    }

    /// Across the screen is nine bits and wraps at 512, so a large coordinate
    /// is a sprite hanging off the left-hand edge. Read as a plain number it
    /// would be off the right instead, and a sprite entering from the left
    /// would never appear.
    #[test]
    fn a_sprite_past_the_end_of_the_line_hangs_off_the_left_instead() {
        let mut ppu = machine(0);
        one_dimensional(&mut ppu);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        for tile in 1..=4 {
            fill_tile(&mut ppu, tile, 1);
        }
        // Sixteen square at 504, which is eight short of the wrap: half of it
        // is off the edge and the other half is at the corner of the screen.
        put(&mut ppu, 0, 0, 0x4000 | 504, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "the half that is on screen");
        assert_eq!(pixel(&ppu, 8, 0), BLUE, "and it ends where the sprite does");

        // And a coordinate that is merely past the right-hand edge is off the
        // screen and stays off it. This is the half of the rule that says where
        // the wrap is: at 512 and not at 256. Both coordinates put the sprite
        // above in the same place, and only this one tells them apart.
        put(&mut ppu, 0, 0, 0x4000 | 300, 1);
        sweep_a_frame(&mut ppu);
        for x in 0..SCREEN_WIDTH {
            assert_eq!(pixel(&ppu, x, 0), BLUE, "nothing at {x}: 300 is off the right");
        }
    }

    /// Down the screen wraps at 256, which is below the screen rather than at
    /// the end of it — so a sprite at 250 hangs off the *top*.
    #[test]
    fn a_sprite_below_the_wrap_hangs_off_the_top() {
        let mut ppu = machine(0);
        one_dimensional(&mut ppu);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        for tile in 1..=4 {
            fill_tile(&mut ppu, tile, 1);
        }
        // Sixteen square at 248: eight lines above the wrap, eight below it.
        put(&mut ppu, 0, 248, 0x4000 | 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 0), RED, "the part that came round to the top");
        assert_eq!(pixel(&ppu, 100, 7), RED);
        assert_eq!(pixel(&ppu, 100, 8), BLUE, "and it ends eight lines down");
    }

    /// A sprite and a background claiming the same priority is not a tie: the
    /// sprite is in front.
    #[test]
    fn a_sprite_covers_a_background_of_the_same_priority() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        // Both at priority 0.
        covering_background(&mut ppu, 0, GREEN);

        put(&mut ppu, 0, 40, 100, 1);
        sweep_a_frame(&mut ppu);

        assert_eq!(pixel(&ppu, 100, 40), RED, "the sprite is in front");
        assert_eq!(pixel(&ppu, 120, 40), GREEN, "and the background beside it");
    }

    /// But a background of a nearer priority does cover a sprite. Priority is
    /// compared first and the sprite only wins the tie.
    #[test]
    fn a_nearer_background_covers_a_sprite() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        // Background priority 0, sprite priority 1.
        covering_background(&mut ppu, 0, GREEN);

        put(&mut ppu, 0, 40, 100, (1 << 10) | 1);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), GREEN, "the background is nearer");
    }

    /// Among sprites the lower entry number is nearer, whatever order they are
    /// written in and wherever they are in memory.
    #[test]
    fn the_lower_numbered_sprite_is_the_nearer_one() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        set_colour(&mut ppu, 2, GREEN);
        fill_tile(&mut ppu, 1, 1);
        fill_tile(&mut ppu, 2, 2);

        // Written in the order that would give the wrong answer if the later
        // write simply won.
        put(&mut ppu, 5, 40, 100, 1);
        put(&mut ppu, 2, 40, 100, 2);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), GREEN, "entry 2 is in front of entry 5");
    }

    /// A sprite whose graphics mode is 2 is the shape of a window, not a
    /// picture. It never appears on screen in its own right.
    #[test]
    fn a_window_sprite_is_not_a_picture() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        put(&mut ppu, 0, WINDOW | 40, 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), BLUE);
    }

    // --- The sprites that turn --------------------------------------------

    const ONE: i16 = 0x100;

    /// The identity again: a transformed sprite that is not transformed sits
    /// exactly where a plain one would.
    #[test]
    fn a_transformed_sprite_left_untransformed_sits_where_a_plain_one_would() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        put_transform(&mut ppu, 0, ONE, 0, 0, ONE);
        put(&mut ppu, 0, TRANSFORMED | 40, 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), RED, "the corner it names");
        assert_eq!(pixel(&ppu, 107, 47), RED, "and all eight pixels");
        assert_eq!(pixel(&ppu, 108, 40), BLUE, "and no more");
    }

    /// Halving the step doubles the size — but about the middle, not the
    /// corner. A sprite scaled up in place has to stay in place, so it grows
    /// equally in both directions and its corner moves back.
    #[test]
    fn a_scaled_sprite_grows_about_its_middle() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        // Half a step per pixel: twice the size. The area is not doubled, so
        // only the middle eight pixels of the sixteen are on the screen.
        put_transform(&mut ppu, 0, ONE / 2, 0, 0, ONE / 2);
        put(&mut ppu, 0, TRANSFORMED | 40, 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), RED, "the area it is allowed is full");
        assert_eq!(pixel(&ppu, 107, 47), RED);
        assert_eq!(pixel(&ppu, 108, 40), BLUE, "and it is not allowed any more");
    }

    /// Which is what the doubled area is for: room for the parts that a turn or
    /// a scale pushes outside the sprite's own bounds. Without it they are cut
    /// off, and with it the sprite sits eight pixels further back.
    #[test]
    fn the_doubled_area_gives_a_scaled_sprite_room_to_grow_into() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        put_transform(&mut ppu, 0, ONE / 2, 0, 0, ONE / 2);
        put(&mut ppu, 0, TRANSFORMED_DOUBLE | 40, 100, 1);

        sweep_a_frame(&mut ppu);

        // The area is sixteen square from 100, and the doubled sprite fills it.
        assert_eq!(pixel(&ppu, 100, 40), RED, "the whole of the doubled area");
        assert_eq!(pixel(&ppu, 115, 55), RED);
        assert_eq!(pixel(&ppu, 116, 40), BLUE, "and it ends there");
    }

    /// A quarter turn, which is the one rotation whose answer can be read
    /// off without trusting any arithmetic: the sprite's rows become its
    /// columns.
    #[test]
    fn a_quarter_turn_puts_the_rows_where_the_columns_were() {
        // A 16-by-16 sprite with only its top-left tile filled. Turned a
        // quarter, that corner has to move to another corner.
        let mut ppu = machine(0);
        one_dimensional(&mut ppu);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        for tile in 2..5 {
            fill_tile(&mut ppu, tile, 0);
        }
        // Reading the picture at a quarter turn: across becomes down.
        put_transform(&mut ppu, 0, 0, ONE, -ONE, 0);
        put(&mut ppu, 0, TRANSFORMED, 0x4000, 1);

        sweep_a_frame(&mut ppu);

        // Measured well inside each quarter rather than at its edge. The turn
        // is about the middle of an area of even width, which is half a pixel
        // away from any pixel's own middle, so the boundary lands a pixel to
        // one side — on the hardware as much as here. An assertion on the seam
        // would be measuring that rounding and not the rotation.
        assert_eq!(pixel(&ppu, 4, 4), BLUE, "the filled quarter is no longer top left");
        assert_eq!(pixel(&ppu, 12, 4), RED, "it is top right");
        assert_eq!(pixel(&ppu, 12, 12), BLUE, "and the other quarters are still empty");
        assert_eq!(pixel(&ppu, 4, 12), BLUE);
    }

    /// The transformations live in the gaps between entries, four halfwords
    /// four entries apart. Nothing about a sprite's entry says which one is in
    /// its own gap, and a group is reached by number alone.
    #[test]
    fn the_transformations_are_kept_in_the_gaps_between_entries() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);

        // Group 3 is the gaps of entries 12 to 15 — nowhere near sprite 0,
        // which is the one that uses it.
        put_transform(&mut ppu, 3, ONE, 0, 0, ONE);
        put(&mut ppu, 0, TRANSFORMED | 40, (3 << 9) | 100, 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), RED, "group 3 was found where it lives");
        assert_eq!(pixel(&ppu, 108, 40), BLUE);
    }

    /// A transformed sprite has no mirror bits: those two bits are part of the
    /// number of the transformation instead. Reading them as flips would send
    /// half the sprites on screen to the wrong transformation as well.
    #[test]
    fn a_transformed_sprite_has_no_mirror_bits() {
        let mut ppu = machine(0);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        fill_tile(&mut ppu, 1, 1);
        for tile in 2..5 {
            fill_tile(&mut ppu, tile, 0);
        }
        one_dimensional(&mut ppu);

        // Group 6 is bits 12 and 13 set — the two that mean mirroring on a
        // plain sprite.
        put_transform(&mut ppu, 6, ONE, 0, 0, ONE);
        put(&mut ppu, 0, TRANSFORMED, 0x4000 | (6 << 9), 1);

        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 0, 0), RED, "the filled tile stayed top left");
        assert_eq!(pixel(&ppu, 8, 8), BLUE, "the sprite was not mirrored either way");
    }

    /// The bitmap modes leave sprites half as much memory, because the picture
    /// has taken the rest. The same tile number is a different place.
    #[test]
    fn the_bitmap_modes_move_where_sprite_tiles_begin() {
        // Mode 3 with the bitmap switched off, so only the sprite is drawn.
        let mut ppu = machine(3);
        set_backdrop(&mut ppu, BLUE);
        set_colour(&mut ppu, 1, RED);
        // Tile 512 of the tiled modes is where the bitmap modes' tile 0 is.
        fill_tile(&mut ppu, 512, 1);

        put(&mut ppu, 0, 40, 100, 0);
        sweep_a_frame(&mut ppu);
        assert_eq!(pixel(&ppu, 100, 40), RED, "tile 0 counts from the sprites' end");
    }

    /// Where a map goes in the test above. The second 2 KiB block, which is
    /// what `BG_CONTROL` is set to name.
    const MAP: usize = 2 * 1024;
}
