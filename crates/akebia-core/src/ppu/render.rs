//! Scanline rendering: background, window and sprites.
//!
//! # Tile format: planar 2bpp
//!
//! A tile is 8×8 pixels of 2 bits, packed into 16 bytes. The surprising part is
//! that the two bits of a pixel are **not contiguous**: each row takes two
//! bytes, and the pixel is formed by taking the same bit from each one.
//!
//! ```text
//!   byte 0 (low plane)    1 0 1 1 0 0 0 1
//!   byte 1 (high plane)   0 1 1 0 1 0 0 1
//!                         ─────────────────
//!   colour index          1 2 3 1 2 0 0 3
//!                         ▲
//!                         └ bit 7 is the **leftmost** pixel
//! ```
//!
//! # Colour index versus colour
//!
//! The indices 0..3 that come out of the tile are not colours: they are palette
//! entries. The distinction matters for two reasons:
//!
//! - In sprites, index **0 is transparent**, whatever colour the palette
//!   translates it into.
//! - Background-over-sprite priority is decided on the index *before* applying
//!   the palette: a background of index 0 never covers a sprite even if the
//!   palette paints it black.
//!
//! That is why rendering keeps the background indices in a separate buffer.

use super::{
    ColorIndex, Mode, Ppu, DRAW_CYCLES, LAST_VISIBLE_LINE, MAX_DRAW_CYCLES, SCREEN_HEIGHT,
    SCREEN_WIDTH,
};

/// Bytes a tile takes up: 8 rows × 2 bytes.
const TILE_SIZE: u16 = 16;
/// Width in tiles of the two background maps.
const TILEMAP_WIDTH: u16 = 32;
/// Sprites the PPU can draw on a single line. The eleventh and beyond simply do
/// not appear: it is a hardware limitation that several games trigger on purpose
/// to make enemies flicker.
const MAX_SPRITES_PER_LINE: usize = 10;
/// Bytes per OAM entry.
const OAM_ENTRY_SIZE: usize = 4;

/// The window is positioned with a 7-pixel offset: `WX = 7` leaves it flush
/// against the left edge.
const WINDOW_X_OFFSET: i16 = 7;

/// Dots it costs to start the window on a line.
const WINDOW_PENALTY: u32 = 6;
/// Dots the worst-aligned sprite costs. The best one costs five fewer.
const SPRITE_MAX_PENALTY: u32 = 11;

/// An OAM sprite, already decoded.
struct Sprite {
    /// Screen coordinate of the top edge (`OAM.y - 16`).
    y: i16,
    /// Screen coordinate of the left edge (`OAM.x - 8`).
    x: i16,
    tile: u8,
    attrs: u8,
    /// Position in OAM: breaks the priority tie when two sprites share `x`, and
    /// on CGB it **is** the only criterion.
    index: usize,
}

impl Sprite {
    /// Bit 7: the sprite goes **behind** background indices 1..3.
    fn behind_background(&self) -> bool {
        self.attrs & 0x80 != 0
    }
    /// Bit 6: vertical flip.
    fn flip_y(&self) -> bool {
        self.attrs & 0x40 != 0
    }
    /// Bit 5: horizontal flip.
    fn flip_x(&self) -> bool {
        self.attrs & 0x20 != 0
    }
    /// Bit 4: on DMG it chooses between `OBP0` and `OBP1`.
    fn uses_obp1(&self) -> bool {
        self.attrs & 0x10 != 0
    }
    /// Bit 3: on CGB, the VRAM bank the tile comes from.
    fn vram_bank(&self) -> usize {
        usize::from(self.attrs >> 3 & 1)
    }
    /// Bits 0-2: on CGB, which of the eight sprite palettes it uses.
    fn cgb_palette(&self) -> u8 {
        self.attrs & 0x07
    }
}

/// Attribute of a background tile, which on CGB lives in VRAM bank 1, at the
/// same address as its index in bank 0.
///
/// It is the piece that lets a colour background need no extra map memory: the
/// second bank reuses the same addresses to describe the tile instead of
/// identifying it.
#[derive(Clone, Copy, Default)]
struct BgAttributes(u8);

impl BgAttributes {
    /// Bits 0-2: background palette (0..7).
    fn palette(self) -> u8 {
        self.0 & 0x07
    }
    /// Bit 3: VRAM bank the tile comes from.
    fn vram_bank(self) -> usize {
        usize::from(self.0 >> 3 & 1)
    }
    /// Bit 5: horizontal flip. The DMG cannot flip the background; the CGB can.
    fn flip_x(self) -> bool {
        self.0 & 0x20 != 0
    }
    /// Bit 6: vertical flip.
    fn flip_y(self) -> bool {
        self.0 & 0x40 != 0
    }
    /// Bit 7: this tile goes **in front of** the sprites.
    fn priority(self) -> bool {
        self.0 & 0x80 != 0
    }
}

/// Translates a colour index through a DMG palette register.
///
/// The four indices take two bits each: index `n` is in bits `2n+1..2n`.
fn apply_palette(palette: u8, index: ColorIndex) -> ColorIndex {
    (palette >> (index * 2)) & 0x03
}

/// A resolved background pixel, with what the sprites need to know in order to
/// decide whether they cover it.
#[derive(Clone, Copy, Default)]
struct BgPixel {
    /// Index **before** the palette: 0 never covers a sprite.
    index: ColorIndex,
    /// Bit 7 of the CGB attribute: the background wins even if the sprite does
    /// not ask for it.
    priority: bool,
}

impl Ppu {
    /// Draws the whole `ly` line into the framebuffer.
    ///
    /// It is invoked once per visible line, when mode 3 ends. It reads the
    /// registers as they stand at that instant, so scroll changes between lines
    /// —the classic "wave" effect— work; mid-line changes do not. That would
    /// take a pixel FIFO.
    pub(super) fn render_scanline(&mut self) {
        let y = self.ly;
        if y as usize >= SCREEN_HEIGHT {
            return;
        }
        // Before drawing: this is exactly where the registers hold what is going
        // to end up on the screen.
        self.capture_scanline();

        // Background state *before* the palette: it is what decides priority
        // against the sprites.
        let mut background = [BgPixel::default(); SCREEN_WIDTH];

        // Key CGB difference: bit 0 of `LCDC` stops turning the background off
        // and comes to mean "the background loses all priority over the
        // sprites". The background is still always drawn.
        if self.model.is_cgb() || self.lcdc.bg_enabled() {
            self.render_background_line(y, &mut background);
        } else {
            let white = self.dmg_shades[0];
            for x in 0..SCREEN_WIDTH {
                self.framebuffer.set(x, y as usize, white);
            }
        }

        if self.lcdc.sprites_enabled() {
            self.render_sprite_line(y, &background);
        }
    }

    /// Background and window. The window is drawn on top, replacing the
    /// background from `WX - 7` onwards.
    fn render_background_line(&mut self, y: u8, background: &mut [BgPixel; SCREEN_WIDTH]) {
        let window_active = self.lcdc.window_enabled() && y >= self.wy;
        let window_start = i16::from(self.wx) - WINDOW_X_OFFSET;
        let cgb = self.model.is_cgb();
        let mut window_drawn = false;

        for (x, bg_pixel) in background.iter_mut().enumerate() {
            let in_window = window_active && (x as i16) >= window_start;

            let (map_base, map_x, map_y) = if in_window {
                window_drawn = true;
                (
                    self.lcdc.window_tilemap(),
                    (x as i16 - window_start) as u16,
                    u16::from(self.window_line),
                )
            } else {
                (
                    self.lcdc.bg_tilemap(),
                    // The background is a 256×256 canvas that repeats.
                    (x as u16 + u16::from(self.scx)) & 0xFF,
                    (u16::from(y) + u16::from(self.scy)) & 0xFF,
                )
            };

            let map_addr = map_base + (map_y / 8) * TILEMAP_WIDTH + (map_x / 8);
            let tile = self.vram_at(0, map_addr);
            // Bank 1 describes the same tile that bank 0 identifies.
            let attrs =
                if cgb { BgAttributes(self.vram_at(1, map_addr)) } else { BgAttributes::default() };

            let mut row = map_y % 8;
            let mut col = map_x % 8;
            if attrs.flip_y() {
                row = 7 - row;
            }
            if attrs.flip_x() {
                col = 7 - col;
            }

            let index = self.tile_pixel(attrs.vram_bank(), self.bg_tile_address(tile), row, col);

            *bg_pixel = BgPixel { index, priority: attrs.priority() };

            let color = if cgb {
                self.bg_palettes.color(attrs.palette(), index)
            } else {
                self.dmg_shades[apply_palette(self.bgp, index) as usize]
            };
            self.framebuffer.set(x, y as usize, color);
        }

        // The window's line counter is independent of `LY`: it only advances on
        // the lines where the window actually got drawn. That is why a game can
        // hide and re-show the window without its contents jumping around.
        if window_drawn {
            self.window_line = self.window_line.saturating_add(1);
        }
    }

    /// Address of the background or window tile with index `tile`.
    ///
    /// Here is the PPU's most famous quirk: with `LCDC.4 = 0` the base is 0x9000
    /// and **the index is read as signed**, so 0..127 falls in 0x9000..0x9800
    /// and 128..255 in 0x8800..0x9000. With `LCDC.4 = 1` the base is 0x8000 and
    /// the index is unsigned, plain and simple.
    fn bg_tile_address(&self, tile: u8) -> u16 {
        if self.lcdc.bg_tile_data_signed() {
            (0x9000 + i32::from(tile as i8) * i32::from(TILE_SIZE)) as u16
        } else {
            0x8000 + u16::from(tile) * TILE_SIZE
        }
    }

    /// Extracts the colour index of a pixel within a tile.
    ///
    /// `row` is row 0..7 and `col` column 0..7, with column 0 on the left (that
    /// is, bit 7 of each byte). `bank` is always 0 on DMG.
    fn tile_pixel(&self, bank: usize, tile_addr: u16, row: u16, col: u16) -> ColorIndex {
        let row_addr = tile_addr.wrapping_add(row * 2);
        let low = self.vram_at(bank, row_addr);
        let high = self.vram_at(bank, row_addr.wrapping_add(1));

        let bit = 7 - col;
        (((high >> bit) & 1) << 1) | ((low >> bit) & 1)
    }

    /// The sprites covering this line, already clipped to the hardware limit.
    ///
    /// The clip to 10 is done in **OAM order**, not by position: if there are 12
    /// sprites on the line, the two with the highest OAM index disappear, even
    /// if they were further to the left.
    fn sprites_on_line(&self, y: u8) -> Vec<Sprite> {
        let height = i16::from(self.lcdc.sprite_height());
        let line = i16::from(y);
        let mut sprites = Vec::with_capacity(MAX_SPRITES_PER_LINE);

        for index in 0..self.oam.len() / OAM_ENTRY_SIZE {
            let entry = &self.oam[index * OAM_ENTRY_SIZE..][..OAM_ENTRY_SIZE];
            // OAM stores the coordinates shifted so that a sprite can be
            // represented entering through the top or left edge of the screen.
            let sprite = Sprite {
                y: i16::from(entry[0]) - 16,
                x: i16::from(entry[1]) - 8,
                tile: entry[2],
                attrs: entry[3],
                index,
            };

            if line >= sprite.y && line < sprite.y + height {
                sprites.push(sprite);
                if sprites.len() == MAX_SPRITES_PER_LINE {
                    break;
                }
            }
        }
        sprites
    }

    fn render_sprite_line(&mut self, y: u8, background: &[BgPixel; SCREEN_WIDTH]) {
        let cgb = self.model.is_cgb();
        let mut sprites = self.sprites_on_line(y);

        // The two consoles order them differently. The DMG compares the `X`
        // coordinate —the leftmost sprite wins— because its circuit resolves
        // priority while scanning the line. The CGB uses only the OAM order,
        // which is more predictable and saves it the comparison; a game can ask
        // for the old behaviour through `OPRI`, and that is how DMG titles run
        // on a colour console look the same.
        if cgb && !self.dmg_object_priority {
            sprites.sort_by_key(|s| s.index);
        } else {
            sprites.sort_by_key(|s| (s.x, s.index));
        }

        // On CGB, bit 0 of `LCDC` cleared cancels the background's priority: the
        // sprites go in front of everything, whatever the attributes say.
        let background_may_win = !cgb || self.lcdc.bg_enabled();

        let height = i16::from(self.lcdc.sprite_height());
        // A pixel already painted by a higher-priority sprite is not repainted.
        let mut painted = [false; SCREEN_WIDTH];

        for sprite in &sprites {
            let mut row = i16::from(y) - sprite.y;
            if sprite.flip_y() {
                row = height - 1 - row;
            }

            // In 8×16 mode bit 0 of the index is ignored: the pair of
            // consecutive tiles forms a single tall sprite.
            let tile = if height == 16 { sprite.tile & 0xFE } else { sprite.tile };
            // Sprites always use the unsigned 0x8000 base.
            let tile_addr = 0x8000 + u16::from(tile) * TILE_SIZE;
            let bank = if cgb { sprite.vram_bank() } else { 0 };

            for col in 0..8i16 {
                let x = sprite.x + col;
                if !(0..SCREEN_WIDTH as i16).contains(&x) {
                    continue;
                }
                let x = x as usize;
                if painted[x] {
                    continue;
                }

                let source_col = if sprite.flip_x() { 7 - col } else { col };
                let index = self.tile_pixel(bank, tile_addr, row as u16, source_col as u16);

                // Index 0 is transparent: it is neither drawn nor blocking.
                if index == 0 {
                    continue;
                }

                // The background covers the sprite if either of the two asks for
                // it: the sprite with its bit 7, or —on CGB only— the tile with
                // its own.
                let bg = background[x];
                let bg_wins = background_may_win
                    && bg.index != 0
                    && (sprite.behind_background() || bg.priority);
                if bg_wins {
                    continue;
                }

                let color = if cgb {
                    self.obj_palettes.color(sprite.cgb_palette(), index)
                } else {
                    let palette = if sprite.uses_obp1() { self.obp1 } else { self.obp0 };
                    self.dmg_shades[apply_palette(palette, index) as usize]
                };
                self.framebuffer.set(x, y as usize, color);
                painted[x] = true;
            }
        }
    }

    /// How long mode 3 is going to last on the line that is starting.
    ///
    /// Drawing does not take the same time on every line: the PPU stalls to look
    /// things up and that time is taken away from HBlank, which is the window in
    /// which the CPU can touch VRAM. A game that writes right at the edge
    /// notices the difference, and the accuracy suites measure it directly.
    ///
    /// Three penalties, all documented in Pan Docs:
    ///
    /// - **Fine scroll**: the `SCX % 8` leftover pixels of the first tile are
    ///   generated and thrown away, and that costs one dot per pixel.
    /// - **Window**: starting it mid-line forces the background fetcher to
    ///   restart. That is 6 dots.
    /// - **Sprites**: each one aborts the background fetch in progress. It costs
    ///   between 6 and 11 dots depending on how it lines up with the tile grid;
    ///   the ones landing right on the edge are the cheapest.
    ///
    /// The extremes match the hardware: with nothing it is 172 dots, and a line
    /// with `SCX = 7` and ten sprites in the worst spot gives
    /// `172 + 7 + 110 = 289`, which is exactly the maximum that fits.
    pub(super) fn mode3_cycles(&self) -> u32 {
        if !self.lcdc.lcd_enabled() {
            return DRAW_CYCLES;
        }
        let mut dots = DRAW_CYCLES + u32::from(self.scx % 8);

        // The window only costs anything if it really gets drawn on the line.
        let window_x = i16::from(self.wx) - WINDOW_X_OFFSET;
        if self.lcdc.window_enabled() && self.ly >= self.wy && window_x < SCREEN_WIDTH as i16 {
            dots += WINDOW_PENALTY;
        }

        if self.lcdc.sprites_enabled() {
            for sprite in self.sprites_on_line(self.ly) {
                // A sprite flush against the left edge counts from its real x,
                // even if it only peeks halfway in.
                let x = sprite.x.rem_euclid(8) as u32;
                dots += SPRITE_MAX_PENALTY - x.min(5);
            }
        }

        dots.min(MAX_DRAW_CYCLES)
    }

    /// Resets the window's line counter. It is called at the start of every
    /// frame, not when reaching `WY`.
    pub(super) fn reset_window_line(&mut self) {
        self.window_line = 0;
    }
}

/// Compile-time check that the modes are still aligned with the values `STAT`
/// exposes.
const _: () = {
    assert!(Mode::HBlank as u8 == 0 && Mode::Drawing as u8 == 3);
    assert!(LAST_VISIBLE_LINE as usize + 1 == SCREEN_HEIGHT);
};

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::model::Model;
    use crate::ppu::Rgb555;

    /// Base address of the first tilemap, already in VRAM coordinates.
    pub(super) const TILEMAP: usize = 0x9800 - 0x8000;

    /// PPU powered on with identity palettes, so that the colour index coming
    /// out of the tile is the same one that reaches the framebuffer.
    pub(super) fn ready_ppu() -> Ppu {
        ready_ppu_for(Model::Dmg)
    }

    /// The four DMG-mode shades in the tests are the literals 0,1,2,3, so they
    /// can be compared against the index coming out of the tile.
    pub(super) const IDENTITY_SHADES: [Rgb555; 4] =
        [Rgb555::from_bits(0), Rgb555::from_bits(1), Rgb555::from_bits(2), Rgb555::from_bits(3)];

    pub(super) fn ready_ppu_for(model: Model) -> Ppu {
        let mut ppu = Ppu::new(model);
        ppu.set_dmg_shades(IDENTITY_SHADES);
        ppu.write_register(0xFF40, 0b1001_0011); // LCD+BG+sprites, tiles 0x8000
        for reg in [0xFF47, 0xFF48, 0xFF49] {
            ppu.write_register(reg, 0xE4); // 11 10 01 00 → identity
        }
        ppu
    }

    /// Writes an 8×8 tile in planar 2bpp format from its indices.
    pub(super) fn write_tile(ppu: &mut Ppu, tile: usize, rows: [[u8; 8]; 8]) {
        for (r, row) in rows.iter().enumerate() {
            let (mut low, mut high) = (0u8, 0u8);
            for (c, &index) in row.iter().enumerate() {
                let bit = 7 - c;
                low |= (index & 1) << bit;
                high |= ((index >> 1) & 1) << bit;
            }
            ppu.vram[tile * 16 + r * 2] = low;
            ppu.vram[tile * 16 + r * 2 + 1] = high;
        }
    }

    /// A whole tile of a single colour index.
    pub(super) fn solid_tile(ppu: &mut Ppu, tile: usize, index: u8) {
        write_tile(ppu, tile, [[index; 8]; 8]);
    }

    /// Points all 32×32 cells of a tilemap at the same tile.
    pub(super) fn fill_tilemap(ppu: &mut Ppu, base: usize, tile: u8) {
        let cells = (TILEMAP_WIDTH * TILEMAP_WIDTH) as usize;
        ppu.vram[base..base + cells].fill(tile);
    }

    /// Draws a line and returns its 160 pixels as shade indices. It only makes
    /// sense on DMG, where the shades were fixed to 0..3.
    pub(super) fn line(ppu: &mut Ppu, y: u8) -> Vec<ColorIndex> {
        colors(ppu, y).into_iter().map(|c| c.bits() as u8).collect()
    }

    /// Draws a line and returns its colours as they are.
    pub(super) fn colors(ppu: &mut Ppu, y: u8) -> Vec<Rgb555> {
        ppu.ly = y;
        ppu.render_scanline();
        (0..SCREEN_WIDTH).map(|x| ppu.framebuffer.get(x, y as usize)).collect()
    }

    /// Writes a tile into VRAM bank 1, where the CGB keeps the ones referenced
    /// by attributes with bit 3 set.
    pub(super) fn write_tile_bank1(ppu: &mut Ppu, tile: usize, index: u8) {
        for r in 0..8 {
            let base = super::super::VRAM_BANK_SIZE + tile * 16 + r * 2;
            ppu.vram[base] = if index & 1 != 0 { 0xFF } else { 0x00 };
            ppu.vram[base + 1] = if index & 2 != 0 { 0xFF } else { 0x00 };
        }
    }

    /// Writes the CGB attributes of a tilemap cell (bank 1).
    pub(super) fn attributes(ppu: &mut Ppu, cell: usize, attrs: u8) {
        ppu.vram[super::super::VRAM_BANK_SIZE + TILEMAP + cell] = attrs;
    }

    /// Sets a colour of a CGB palette by going through the registers.
    pub(super) fn set_color(
        ppu: &mut Ppu,
        background: bool,
        palette: u8,
        color: u8,
        value: Rgb555,
    ) {
        let (idx, dat) = if background { (0xFF68, 0xFF69) } else { (0xFF6A, 0xFF6B) };
        let offset = palette * 8 + color * 2;
        ppu.write_register(idx, 0x80 | offset); // with auto-increment
        ppu.write_register(dat, value.bits() as u8);
        ppu.write_register(dat, (value.bits() >> 8) as u8);
    }

    /// Places a sprite in OAM. `x` and `y` are screen coordinates.
    pub(super) fn set_sprite(ppu: &mut Ppu, slot: usize, x: i16, y: i16, tile: u8, attrs: u8) {
        let base = slot * OAM_ENTRY_SIZE;
        ppu.oam[base] = (y + 16) as u8;
        ppu.oam[base + 1] = (x + 8) as u8;
        ppu.oam[base + 2] = tile;
        ppu.oam[base + 3] = attrs;
    }

    // ---- Background --------------------------------------------------------

    #[test]
    fn it_draws_a_background_tile() {
        let mut ppu = ready_ppu();
        // A tile with a horizontal gradient 0,1,2,3,0,1,2,3.
        write_tile(&mut ppu, 1, [[0, 1, 2, 3, 0, 1, 2, 3]; 8]);
        ppu.vram[TILEMAP] = 1;

        let l = line(&mut ppu, 0);
        assert_eq!(&l[0..8], &[0, 1, 2, 3, 0, 1, 2, 3]);
        assert_eq!(&l[8..16], &[0; 8], "the rest of the map points at tile 0, empty");
    }

    #[test]
    fn bit_7_of_the_tile_is_the_left_pixel() {
        let mut ppu = ready_ppu();
        write_tile(&mut ppu, 1, [[3, 0, 0, 0, 0, 0, 0, 0]; 8]);
        ppu.vram[TILEMAP] = 1;

        let l = line(&mut ppu, 0);
        assert_eq!(l[0], 3, "the first pixel of the row comes from bit 7");
        assert_eq!(l[1], 0);
    }

    #[test]
    fn the_palette_translates_the_indices() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 1);
        ppu.vram[TILEMAP] = 1;
        // Inverted BGP: index 1 becomes colour 3.
        ppu.write_register(0xFF47, 0b00_01_11_10);

        assert_eq!(line(&mut ppu, 0)[0], 3);
    }

    #[test]
    fn the_scroll_shifts_the_background() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 3);
        ppu.vram[TILEMAP + 1] = 1; // tile 1 occupies the second column

        assert_eq!(line(&mut ppu, 0)[0], 0, "with no scroll the first column shows");

        ppu.write_register(0xFF43, 8); // SCX = 8
        assert_eq!(line(&mut ppu, 0)[0], 3, "with SCX=8 it advances one tile");
    }

    #[test]
    fn the_background_wraps_on_overflow() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 2);
        ppu.vram[TILEMAP] = 1; // tile at corner (0,0) of the canvas

        // SCX = 248 leaves the last 8 pixels of the canvas to the left of the
        // screen, so the corner reappears at column 8.
        ppu.write_register(0xFF43, 248);
        assert_eq!(line(&mut ppu, 0)[8], 2, "the 256×256 canvas wraps around");
    }

    #[test]
    fn with_lcdc4_off_the_tile_index_is_signed() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF40, 0b1000_0011); // LCDC.4 = 0 → base 0x9000

        // Index 0xFF, read as signed, is -1: it falls at 0x9000 - 16 = 0x8FF0.
        let base = 0x8FF0 - 0x8000;
        for r in 0..8 {
            ppu.vram[base + r * 2] = 0xFF;
            ppu.vram[base + r * 2 + 1] = 0xFF;
        }
        ppu.vram[TILEMAP] = 0xFF;

        assert_eq!(line(&mut ppu, 0)[0], 3, "index 0xFF must be read as -1");
    }

    #[test]
    fn clearing_bit_0_of_lcdc_erases_the_background() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 3);
        ppu.vram[TILEMAP] = 1;
        assert_eq!(line(&mut ppu, 0)[0], 3);

        ppu.write_register(0xFF40, 0b1001_0010); // LCDC.0 = 0
        assert_eq!(line(&mut ppu, 0)[0], 0);
    }

    // ---- Window ------------------------------------------------------------

    #[test]
    fn the_window_replaces_the_background_from_wx_minus_7() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 1); // background
        solid_tile(&mut ppu, 2, 3); // window
        fill_tilemap(&mut ppu, TILEMAP, 1);
        fill_tilemap(&mut ppu, 0x9C00 - 0x8000, 2);

        ppu.write_register(0xFF40, 0b1111_0011); // + window at 0x9C00
        ppu.write_register(0xFF4A, 0); // WY = 0
        ppu.write_register(0xFF4B, 87); // WX = 87 → starts at x = 80

        let l = line(&mut ppu, 0);
        assert_eq!(l[79], 1, "before WX-7 the background continues");
        assert_eq!(l[80], 3, "from WX-7 on the window rules");
    }

    #[test]
    fn the_window_does_not_appear_above_wy() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 1);
        solid_tile(&mut ppu, 2, 3);
        fill_tilemap(&mut ppu, TILEMAP, 1);
        fill_tilemap(&mut ppu, 0x9C00 - 0x8000, 2);
        ppu.write_register(0xFF40, 0b1111_0011);
        ppu.write_register(0xFF4A, 50); // WY = 50
        ppu.write_register(0xFF4B, 7);

        assert_eq!(line(&mut ppu, 49)[0], 1, "above WY there is no window");
        assert_eq!(line(&mut ppu, 50)[0], 3);
    }

    #[test]
    fn the_window_counter_only_advances_when_it_is_drawn() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF40, 0b1111_0011);
        ppu.write_register(0xFF4A, 0);
        ppu.write_register(0xFF4B, 7);

        line(&mut ppu, 0);
        line(&mut ppu, 1);
        assert_eq!(ppu.window_line, 2);

        // With the window off, drawing more lines does not move its counter.
        ppu.write_register(0xFF40, 0b1001_0011);
        line(&mut ppu, 2);
        line(&mut ppu, 3);
        assert_eq!(ppu.window_line, 2, "the window counter is not LY - WY");
    }

    // ---- Mode 3 duration ---------------------------------------------------

    #[test]
    fn a_bare_line_lasts_the_minimum() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF40, 0b1000_0001); // LCD and background, no sprites
        assert_eq!(ppu.mode3_cycles(), 172);
    }

    #[test]
    fn the_fine_scroll_lengthens_the_drawing() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF40, 0b1000_0001);

        for scx in 0..16u8 {
            ppu.write_register(0xFF43, scx);
            assert_eq!(
                ppu.mode3_cycles(),
                172 + u32::from(scx % 8),
                "SCX = {scx}: it costs one dot per discarded pixel"
            );
        }
    }

    #[test]
    fn the_window_costs_six_dots_only_if_it_is_drawn() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF40, 0b1010_0001); // + window
        ppu.write_register(0xFF4A, 0); // WY = 0
        ppu.write_register(0xFF4B, 7); // flush against the edge
        assert_eq!(ppu.mode3_cycles(), 178);

        // With WY below the line it does not show up yet.
        ppu.write_register(0xFF4A, 100);
        assert_eq!(ppu.mode3_cycles(), 172);

        // And with WX off screen it does not either, even when enabled.
        ppu.write_register(0xFF4A, 0);
        ppu.write_register(0xFF4B, 200);
        assert_eq!(ppu.mode3_cycles(), 172);
    }

    #[test]
    fn each_sprite_costs_between_six_and_eleven_dots() {
        let mut ppu = ready_ppu();
        // A sprite aligned with the grid is the most expensive; the one on the
        // edge, the cheapest.
        set_sprite(&mut ppu, 0, 0, 0, 1, 0);
        assert_eq!(ppu.mode3_cycles(), 172 + 11);

        set_sprite(&mut ppu, 0, 5, 0, 1, 0);
        assert_eq!(ppu.mode3_cycles(), 172 + 6, "x=5 is the cheapest");

        set_sprite(&mut ppu, 0, 7, 0, 1, 0);
        assert_eq!(ppu.mode3_cycles(), 172 + 6, "past 5 it does not drop any further");
    }

    #[test]
    fn disabled_sprites_cost_nothing() {
        let mut ppu = ready_ppu();
        set_sprite(&mut ppu, 0, 0, 0, 1, 0);
        ppu.write_register(0xFF40, 0b1000_0001); // LCDC.1 = 0
        assert_eq!(ppu.mode3_cycles(), 172);
    }

    #[test]
    fn the_worst_case_fits_exactly_in_the_line() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF43, 7); // the most expensive fine scroll
        for i in 0..10 {
            set_sprite(&mut ppu, i, i as i16 * 8, 0, 1, 0); // all aligned
        }
        // 172 + 7 + 10×11 = 289, which is exactly what the minimum 87-dot
        // HBlank leaves free: 80 + 289 + 87 = 456.
        assert_eq!(ppu.mode3_cycles(), 289);
        assert_eq!(80 + ppu.mode3_cycles() + 87, 456);
    }

    #[test]
    fn drawing_never_eats_the_whole_hblank() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF40, 0b1010_0011); // everything on, with window
        ppu.write_register(0xFF43, 7);
        ppu.write_register(0xFF4A, 0);
        ppu.write_register(0xFF4B, 7);
        for i in 0..10 {
            set_sprite(&mut ppu, i, i as i16 * 8, 0, 1, 0);
        }
        // Added up it would be 295, but that does not fit in the line.
        assert_eq!(ppu.mode3_cycles(), 289);
    }

    // ---- Sprites -----------------------------------------------------------

    #[test]
    fn a_sprite_is_drawn_on_top_of_the_background() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 1); // background
        solid_tile(&mut ppu, 2, 3); // sprite
        fill_tilemap(&mut ppu, TILEMAP, 1);
        set_sprite(&mut ppu, 0, 0, 0, 2, 0);

        let l = line(&mut ppu, 0);
        assert_eq!(&l[0..8], &[3; 8], "the sprite covers the background");
        assert_eq!(l[8], 1, "past the sprite the background is back");
    }

    #[test]
    fn index_0_of_the_sprite_is_transparent() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 1, 1);
        ppu.vram[TILEMAP] = 1;
        // Left half opaque (index 3), right half transparent.
        write_tile(&mut ppu, 2, [[3, 3, 3, 3, 0, 0, 0, 0]; 8]);
        set_sprite(&mut ppu, 0, 0, 0, 2, 0);

        let l = line(&mut ppu, 0);
        assert_eq!(&l[0..4], &[3; 4]);
        assert_eq!(&l[4..8], &[1; 4], "index 0 lets the background through");
    }

    #[test]
    fn the_priority_bit_hides_the_sprite_behind_the_background() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 2, 3);
        set_sprite(&mut ppu, 0, 0, 0, 2, 0x80); // bit 7: behind the background

        // With a background of index 0 the sprite still shows.
        assert_eq!(line(&mut ppu, 0)[0], 3, "background index 0 does not cover");

        // With a background of index 1..3, the sprite is hidden.
        solid_tile(&mut ppu, 1, 1);
        ppu.vram[TILEMAP] = 1;
        assert_eq!(line(&mut ppu, 0)[0], 1);
    }

    #[test]
    fn the_flips_invert_the_sprite() {
        let mut ppu = ready_ppu();
        write_tile(&mut ppu, 2, [[3, 0, 0, 0, 0, 0, 0, 0]; 8]);

        set_sprite(&mut ppu, 0, 0, 0, 2, 0x20); // horizontal flip
        assert_eq!(line(&mut ppu, 0)[7], 3, "the pixel jumps to the other end");

        // Vertical flip: one row different from the rest.
        let mut rows = [[0u8; 8]; 8];
        rows[0] = [3; 8];
        write_tile(&mut ppu, 2, rows);
        set_sprite(&mut ppu, 0, 0, 0, 2, 0x40);
        assert_eq!(line(&mut ppu, 7)[0], 3, "row 0 becomes row 7");
    }

    #[test]
    fn sprites_of_16_pixels_use_two_tiles() {
        let mut ppu = ready_ppu();
        ppu.write_register(0xFF40, 0b1001_0111); // LCDC.2 = 1 → 8×16
        solid_tile(&mut ppu, 2, 1); // top half
        solid_tile(&mut ppu, 3, 3); // bottom half
                                    // Bit 0 of the index is ignored: 0x03 is treated as 0x02.
        set_sprite(&mut ppu, 0, 0, 0, 0x03, 0);

        assert_eq!(line(&mut ppu, 0)[0], 1, "the first 8 rows are the even tile");
        assert_eq!(line(&mut ppu, 8)[0], 3, "the next 8, the odd one");
    }

    #[test]
    fn on_dmg_the_sprite_with_the_lower_x_wins() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 2, 1);
        solid_tile(&mut ppu, 3, 3);

        // The one with the higher OAM index is further left: it must win.
        set_sprite(&mut ppu, 0, 4, 0, 2, 0);
        set_sprite(&mut ppu, 1, 0, 0, 3, 0);

        let l = line(&mut ppu, 0);
        assert_eq!(l[4], 3, "when overlapping, the lower x rules despite the OAM order");
    }

    #[test]
    fn only_ten_sprites_are_drawn_per_line() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 2, 3);
        // 12 sprites in a row, one every 8 pixels.
        for i in 0..12 {
            set_sprite(&mut ppu, i, i as i16 * 8, 0, 2, 0);
        }

        let l = line(&mut ppu, 0);
        assert_eq!(l[9 * 8], 3, "the tenth sprite is drawn");
        assert_eq!(l[10 * 8], 0, "the eleventh no longer fits");
        assert_eq!(l[11 * 8], 0);
    }

    #[test]
    fn sprites_can_enter_through_the_edge() {
        let mut ppu = ready_ppu();
        solid_tile(&mut ppu, 2, 3);
        set_sprite(&mut ppu, 0, -4, 0, 2, 0); // half off the left edge

        let l = line(&mut ppu, 0);
        assert_eq!(&l[0..4], &[3; 4], "the right half of the sprite shows");
        assert_eq!(l[4], 0);
    }
}

#[cfg(test)]
mod tests_cgb {
    use super::tests::*;
    use crate::model::Model;
    use crate::ppu::Rgb555;

    const RED: Rgb555 = Rgb555::from_bits(0x001F);
    const BLUE: Rgb555 = Rgb555::from_bits(0x7C00);
    const GREEN: Rgb555 = Rgb555::from_bits(0x03E0);

    #[test]
    fn the_background_comes_from_the_colour_palettes() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        solid_tile(&mut ppu, 1, 1);
        fill_tilemap(&mut ppu, TILEMAP, 1);
        // The tilemap points at palette 3; its colour 1 will be red.
        attributes(&mut ppu, 0, 3);
        set_color(&mut ppu, true, 3, 1, RED);

        assert_eq!(colors(&mut ppu, 0)[0], RED);
    }

    #[test]
    fn each_cell_can_use_a_different_palette() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        solid_tile(&mut ppu, 1, 1);
        fill_tilemap(&mut ppu, TILEMAP, 1);
        attributes(&mut ppu, 0, 0);
        attributes(&mut ppu, 1, 1);
        set_color(&mut ppu, true, 0, 1, RED);
        set_color(&mut ppu, true, 1, 1, BLUE);

        let l = colors(&mut ppu, 0);
        assert_eq!(l[0], RED, "first tile with palette 0");
        assert_eq!(l[8], BLUE, "second tile with palette 1");
    }

    #[test]
    fn the_attributes_flip_the_background() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        write_tile(&mut ppu, 1, [[1, 0, 0, 0, 0, 0, 0, 0]; 8]);
        fill_tilemap(&mut ppu, TILEMAP, 1);
        set_color(&mut ppu, true, 0, 1, RED);

        assert_eq!(colors(&mut ppu, 0)[0], RED, "unflipped, the pixel goes to the left");

        attributes(&mut ppu, 0, 0x20); // horizontal flip
        let l = colors(&mut ppu, 0);
        assert_eq!(l[7], RED, "flipped, it jumps to the other end of the tile");
        assert_ne!(l[0], RED);
    }

    #[test]
    fn the_bank_attribute_takes_the_tile_from_bank_1() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        solid_tile(&mut ppu, 1, 0); // bank 0: empty tile
        write_tile_bank1(&mut ppu, 1, 2); // bank 1: same index, different drawing
        fill_tilemap(&mut ppu, TILEMAP, 1);
        set_color(&mut ppu, true, 0, 2, GREEN);

        attributes(&mut ppu, 0, 0x08); // bit 3: use bank 1
        assert_eq!(colors(&mut ppu, 0)[0], GREEN);
    }

    #[test]
    fn the_background_priority_bit_covers_the_sprite() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        solid_tile(&mut ppu, 1, 1);
        fill_tilemap(&mut ppu, TILEMAP, 1);
        set_color(&mut ppu, true, 0, 1, BLUE);

        solid_tile(&mut ppu, 2, 1);
        set_color(&mut ppu, false, 0, 1, RED);
        // The sprite does not ask to go behind: on a DMG it would always win.
        set_sprite(&mut ppu, 0, 0, 0, 2, 0);

        assert_eq!(colors(&mut ppu, 0)[0], RED, "with no priority, the sprite rules");

        attributes(&mut ppu, 0, 0x80); // bit 7 of the tile attribute
        assert_eq!(colors(&mut ppu, 0)[0], BLUE, "the background can impose itself alone");
    }

    #[test]
    fn with_lcdc0_off_the_sprites_go_in_front_of_everything() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        solid_tile(&mut ppu, 1, 1);
        fill_tilemap(&mut ppu, TILEMAP, 1);
        attributes(&mut ppu, 0, 0x80); // the background claims priority
        set_color(&mut ppu, true, 0, 1, BLUE);

        solid_tile(&mut ppu, 2, 1);
        set_color(&mut ppu, false, 0, 1, RED);
        set_sprite(&mut ppu, 0, 0, 0, 2, 0x80); // and the sprite asks to go behind

        assert_eq!(colors(&mut ppu, 0)[0], BLUE);

        // On CGB bit 0 of LCDC does not turn the background off: it takes away
        // its priority.
        ppu.write_register(0xFF40, 0b1001_0010);
        assert_eq!(colors(&mut ppu, 0)[0], RED, "the sprites always win");
    }

    #[test]
    fn the_sprites_are_ordered_by_oam_index() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        solid_tile(&mut ppu, 2, 1);
        solid_tile(&mut ppu, 3, 1);
        set_color(&mut ppu, false, 0, 1, RED);
        set_color(&mut ppu, false, 1, 1, BLUE);

        // The one at OAM 0 is further right: on DMG it would lose, on CGB it wins.
        set_sprite(&mut ppu, 0, 4, 0, 2, 0x00); // palette 0 → red
        set_sprite(&mut ppu, 1, 0, 0, 3, 0x01); // palette 1 → blue

        assert_eq!(colors(&mut ppu, 0)[4], RED, "on CGB the OAM index rules");

        // OPRI brings back the DMG rule: the lower x wins.
        ppu.write_register(0xFF6C, 0x01);
        assert_eq!(colors(&mut ppu, 0)[4], BLUE);
    }

    #[test]
    fn the_sprites_use_their_own_palette_and_bank() {
        let mut ppu = ready_ppu_for(Model::Cgb);
        write_tile_bank1(&mut ppu, 2, 1);
        set_color(&mut ppu, false, 5, 1, GREEN);
        // Palette 5 (bits 0-2) and bank 1 (bit 3).
        set_sprite(&mut ppu, 0, 0, 0, 2, 0x08 | 0x05);

        assert_eq!(colors(&mut ppu, 0)[0], GREEN);
    }
}
