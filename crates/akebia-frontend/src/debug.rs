//! Live PPU dump, to diagnose rendering bugs while playing.
//!
//! The core captures the registers each line was drawn with (see
//! [`akebia_core::debug`]); here they are only formatted. Two rules keep the
//! output readable while the game runs at 60 frames per second:
//!
//! - Of the 144 lines, **only those where something changes** are printed, plus
//!   the first one. A normal screen takes one line of text.
//! - A frame drawn the same as the previous one is not reprinted; it is counted.
//!
//! The result is that the output stays quiet while nothing happens and speaks up
//! exactly when the image changes, which is when it is useful.

use std::io::Write;
use std::path::{Path, PathBuf};

use akebia_core::debug::{FrameTrace, ScanlineTrace};
use akebia_core::GameBoy;

/// Legend for the eight bits of `LCDC`, from 7 down to 0.
///
/// Each bit is printed as a letter: uppercase if it is 1, lowercase if it is 0.
/// That way one line of text states the register's full state and two lines can
/// be compared at a glance.
const LCDC_FLAGS: [(u8, char); 8] = [
    (7, 'L'), // LCD on
    (6, 'M'), // window map at 0x9C00
    (5, 'W'), // window visible
    (4, 'T'), // tile data at 0x8000 (unsigned)
    (3, 'B'), // background map at 0x9C00
    (2, 'H'), // 8×16 sprites
    (1, 'O'), // sprites visible
    (0, 'P'), // background on (DMG) / with priority (CGB)
];

pub const LEGEND: &str = "\
akebia --debug: the PPU registers each line was drawn with.

  LCDC  L=LCD  M=window map 9C00  W=window  T=tiles 8000
        B=background map 9C00  H=8x16 sprites  O=sprites  P=background/priority
        (lowercase = that bit is off)
  WLN   the window's internal counter, which is not LY - WY
  HDMA  16-byte blocks copied to VRAM during that line's HBlank

Only the lines where something changes are printed. A frame identical to the
previous one is not repeated: it is counted.

Press D to dump the frame and VRAM as images (frame, bg, window, tiles).
";

fn lcdc_flags(lcdc: u8) -> String {
    LCDC_FLAGS
        .iter()
        .map(|&(bit, c)| if lcdc >> bit & 1 != 0 { c } else { c.to_ascii_lowercase() })
        .collect()
}

/// Prints the frames that change and stays quiet about the repeated ones.
pub struct LiveTrace {
    previous: Option<FrameTrace>,
    /// Consecutive frames drawn the same as the last printed one.
    repeated: u32,
    /// VRAM writes accumulated over those silenced frames.
    ///
    /// Without this the counters would only show up on the frames that get
    /// printed, which are exactly the rare ones: a game loads its tiles while
    /// the screen is still, and those are the frames the summary keeps quiet
    /// about.
    silenced: ([u64; 4], u64),
    frame: u64,
}

impl LiveTrace {
    // Clippy asks for a `Default` now that this is a library's public API and no
    // longer a binary's own business. It is not going to get one: printing the
    // legend is a side effect, and a `default()` that writes to stderr lies
    // about what it does.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        eprint!("{LEGEND}");
        Self { previous: None, repeated: 0, silenced: ([0; 4], 0), frame: 0 }
    }

    pub fn frame(&mut self, trace: FrameTrace) {
        self.frame += 1;
        // With the LCD off no line is drawn and there is nothing to say.
        if trace.lines == 0 && self.previous.is_none() {
            return;
        }

        if self.previous.as_ref().is_some_and(|p| trace.looks_like(p)) {
            self.repeated += 1;
            for i in 0..4 {
                self.silenced.0[i] += u64::from(trace.vram_writes[i]);
            }
            self.silenced.1 += u64::from(trace.vram_blocked);
            return;
        }

        let mut err = std::io::stderr().lock();
        if self.repeated > 0 {
            let (w, b) = self.silenced;
            let plural = if self.repeated == 1 { "identical frame" } else { "identical frames" };
            let discarded = if b > 0 { format!(", {b} DISCARDED!") } else { String::new() };
            let _ = writeln!(
                err,
                "   … {} {plural} · VRAM {}{discarded}",
                self.repeated,
                vram_summary(&w)
            );
        }
        self.repeated = 0;
        self.silenced = ([0; 4], 0);

        let _ = writeln!(
            err,
            "\n── frame {} · {} lines · HDMA {} · VRAM {}{}",
            self.frame,
            trace.lines,
            trace.hdma_blocks,
            vram_summary(&trace.vram_writes.map(u64::from)),
            match trace.vram_blocked {
                0 => String::new(),
                n => format!(" · {n} DISCARDED by mode 3!"),
            }
        );
        let _ = writeln!(err, "    LY  LCDC          SCX  SCY   WX   WY  WLN  VBK  HDMA");
        for l in &trace.changes {
            let _ = writeln!(err, "{}", row(l));
        }
        let _ = err.flush();

        self.previous = Some(trace);
    }
}

/// The four VRAM regions on one line.
///
/// Separating them is what tells "the game writes into bank 0" from "the game
/// writes **tiles** into bank 0", which is the question that matters when the
/// background comes out with stale graphics.
fn vram_summary(w: &[u64; 4]) -> String {
    format!("b0[tiles {} map {}] b1[tiles {} attr {}]", w[0], w[1], w[2], w[3])
}

fn row(l: &ScanlineTrace) -> String {
    format!(
        "   {:3}  {:02X} {}  {:3}  {:3}  {:3}  {:3}  {:3}  {:3}  {:4}",
        l.ly,
        l.lcdc,
        lcdc_flags(l.lcdc),
        l.scx,
        l.scy,
        l.wx,
        l.wy,
        l.window_line,
        l.vram_bank,
        l.hdma_blocks,
    )
}

/// Dumps the frame and the VRAM that produced it to disk, **as images**.
///
/// A band of wrong tiles is not diagnosed by reading hexadecimal: it is
/// diagnosed by looking at four images and seeing which of them is already
/// wrong.
///
/// | File | What it answers |
/// |---|---|
/// | `frame.ppm` | what was seen on screen |
/// | `bg.ppm` | the whole 256×256 background map, with the visible area boxed |
/// | `window.ppm` | the window map, which on CGB is usually the menu |
/// | `tiles.ppm` | the 384 tiles of each bank, exactly as they sit in VRAM |
///
/// The reading is by elimination. If the tiles in `tiles.ppm` already come out
/// broken, the fault is in how they reach VRAM —the mapper or the HDMA— and not
/// in the rendering. If the tiles are fine but `bg.ppm` places them wrong, the
/// problem is the map. And if `bg.ppm` is whole but `frame.ppm` is not, then it
/// really is the renderer or the scroll.
pub fn snapshot(gb: &GameBoy, dir: &Path, n: u32) -> Result<PathBuf, String> {
    // All together in one folder: a capture is eight files, and loose in the
    // project root they get in the way more than they help.
    let dir = dir.join(CAPTURES);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let base = dir.join(format!("{n:03}"));
    let ppu = &gb.bus().ppu;
    let lcdc = gb.peek(0xFF40);

    let txt = base.with_extension("txt");
    let mut s = String::new();
    registers(gb, &mut s);
    maps(ppu, &mut s);
    sprites(ppu, &mut s);
    palettes(ppu, &mut s);
    write_file(&txt, s.as_bytes())?;

    // What was seen.
    let frame = ppu.framebuffer();
    write_ppm(
        &base.with_extension("frame.ppm"),
        akebia_core::SCREEN_WIDTH,
        akebia_core::SCREEN_HEIGHT,
        frame.as_slice(),
    )?;

    // The whole background map, with the area the screen is showing boxed.
    let bg_base = if lcdc & 0x08 != 0 { 0x1C00 } else { 0x1800 };
    let mut bg = render_tilemap(ppu, bg_base, lcdc);
    mark_viewport(&mut bg, gb.peek(0xFF43), gb.peek(0xFF42));
    write_ppm(&base.with_extension("bg.ppm"), MAP, MAP, &bg)?;

    let win_base = if lcdc & 0x40 != 0 { 0x1C00 } else { 0x1800 };
    let win = render_tilemap(ppu, win_base, lcdc);
    write_ppm(&base.with_extension("window.ppm"), MAP, MAP, &win)?;

    let (tiles, width, height) = render_tilesheet(ppu);
    write_ppm(&base.with_extension("tiles.ppm"), width, height, &tiles)?;

    write_file(&base.with_extension("writes.txt"), writes(gb).as_bytes())?;
    write_file(&base.with_extension("io.txt"), io_writes(gb).as_bytes())?;
    write_file(&base.with_extension("vram0.bin"), ppu.vram_bank_snapshot(0))?;
    write_file(&base.with_extension("vram1.bin"), ppu.vram_bank_snapshot(1))?;
    write_file(&base.with_extension("oam.bin"), ppu.oam_snapshot())?;

    Ok(txt)
}

/// The latest VRAM writes, with the `PC` that ordered them.
///
/// It is the only thing that answers "which address did each write go to?"
/// without inferring it from the final state, which admits many different
/// stories. Next to each one goes the **stride** with respect to the previous
/// write from the same routine: if the map fills in wrong, that is where one
/// sees whether the game advances 32 bytes per row, as it should, or something
/// else.
fn writes(gb: &GameBoy) -> String {
    use core::fmt::Write;

    let log = gb.vram_log();
    let mut s = String::new();
    let _ = writeln!(s, "== Latest {} writes to VRAM ==", log.len());
    let _ = writeln!(s, "mode 3 = the PPU had VRAM locked and it was lost\n");
    let _ = writeln!(s, "   PC    address bank value  LY mode  stride");

    // The stride is measured against the previous write from the same `PC`,
    // which is what isolates one routine's progress from the other writes.
    let mut last: std::collections::HashMap<u16, u16> = std::collections::HashMap::new();
    for w in &log {
        let stride = match last.insert(w.pc, w.addr) {
            Some(previous) => format!("{:+}", w.addr as i32 - previous as i32),
            None => "-".to_owned(),
        };
        let _ = writeln!(
            s,
            " {:04X}   {:04X}      {}    {:02X}   {:3}    {}  {:>6}{}",
            w.pc,
            w.addr,
            w.bank,
            w.value,
            w.ly,
            w.mode,
            stride,
            if w.accepted() { "" } else { "  ← LOST" },
        );
    }
    s
}

/// Name of the I/O registers that matter for video.
fn io_name(addr: u16) -> &'static str {
    match addr {
        0xFF40 => "LCDC",
        0xFF41 => "STAT",
        0xFF42 => "SCY",
        0xFF43 => "SCX",
        0xFF45 => "LYC",
        0xFF46 => "DMA",
        0xFF47 => "BGP",
        0xFF48 => "OBP0",
        0xFF49 => "OBP1",
        0xFF4A => "WY",
        0xFF4B => "WX",
        0xFF4F => "VBK",
        0xFF51..=0xFF55 => "HDMA",
        0xFF68..=0xFF69 => "BGP-CGB",
        0xFF6A..=0xFF6B => "OBP-CGB",
        0xFF6C => "OPRI",
        0xFF70 => "SVBK",
        _ => "",
    }
}

/// The latest writes to I/O registers, with the `PC` that ordered them.
///
/// The video registers come first and with their names, because they are the
/// ones that explain *why* the screen shows what it shows: who moved `WY`, who
/// turned the window on, who changed the scroll and from which instruction. The
/// rest —APU, timer, joypad— comes after and unnamed, so as not to get in the
/// way.
fn io_writes(gb: &GameBoy) -> String {
    use core::fmt::Write;

    let log = gb.io_log();
    let (video, rest): (Vec<_>, Vec<_>) = log.iter().partition(|w| !io_name(w.addr).is_empty());

    let mut s = String::new();
    let _ = writeln!(s, "== Latest {} writes to I/O ==\n", log.len());

    let mut table = |title: &str, ws: &[&akebia_core::debug::MemWrite]| {
        let _ = writeln!(s, "-- {title} ({}) --", ws.len());
        let _ = writeln!(s, "   PC   register       value   LY mode");
        for w in ws {
            let _ = writeln!(
                s,
                " {:04X}   {:04X} {:<8}  {:02X}    {:3}    {}",
                w.pc,
                w.addr,
                io_name(w.addr),
                w.value,
                w.ly,
                w.mode
            );
        }
        let _ = writeln!(s);
    };
    table("video", &video);
    table("rest", &rest);
    s
}

/// Side of the background canvas, in pixels: 32×32 tiles of 8×8.
const MAP: usize = 256;

/// Folder where the captures pile up, relative to the current directory.
pub const CAPTURES: &str = "akebia-debug";

/// Writes pixels as binary PPM (P6), which any viewer opens and which needs no
/// dependency to generate.
pub fn write_ppm(
    path: &Path,
    width: usize,
    height: usize,
    pixels: &[akebia_core::Rgb555],
) -> Result<(), String> {
    let mut data = format!("P6\n{width} {height}\n255\n").into_bytes();
    for color in pixels {
        data.extend_from_slice(&color.to_rgb888());
    }
    write_file(path, &data)
}

/// Colour index 0..3 of a pixel within a tile, in the given bank.
fn tile_pixel(
    ppu: &akebia_core::ppu::Ppu,
    bank: usize,
    tile_addr: usize,
    row: usize,
    col: usize,
) -> u8 {
    let vram = ppu.vram_bank_snapshot(bank);
    let i = (tile_addr + row * 2) & 0x1FFF;
    let (low, high) = (vram[i], vram[(i + 1) & 0x1FFF]);
    let bit = 7 - col;
    (((high >> bit) & 1) << 1) | ((low >> bit) & 1)
}

/// Address of the background tile, with the `LCDC` bit 4 rule.
fn bg_tile_addr(lcdc: u8, tile: u8) -> usize {
    if lcdc & 0x10 != 0 {
        usize::from(tile) * 16
    } else {
        // Base 0x9000 with a **signed** index.
        (0x1000 + i32::from(tile as i8) * 16) as usize & 0x1FFF
    }
}

/// Draws a complete 32×32-cell tile map, applying the CGB attributes. It is the
/// background as the game has it assembled, unclipped by the screen or the
/// scroll.
fn render_tilemap(ppu: &akebia_core::ppu::Ppu, base: usize, lcdc: u8) -> Vec<akebia_core::Rgb555> {
    let mut out = vec![akebia_core::Rgb555::BLACK; MAP * MAP];
    let map0 = ppu.vram_bank_snapshot(0);
    let map1 = ppu.vram_bank_snapshot(1);

    for cell in 0..32 * 32 {
        let tile = map0[base + cell];
        let attrs = map1[base + cell];
        let addr = bg_tile_addr(lcdc, tile);
        let (cx, cy) = ((cell % 32) * 8, (cell / 32) * 8);

        for row in 0..8 {
            for col in 0..8 {
                let r = if attrs & 0x40 != 0 { 7 - row } else { row };
                let c = if attrs & 0x20 != 0 { 7 - col } else { col };
                let index = tile_pixel(ppu, usize::from(attrs >> 3 & 1), addr, r, c);
                out[(cy + row) * MAP + cx + col] = ppu.bg_palette_ram().color(attrs & 0x07, index);
            }
        }
    }
    out
}

/// Marks on the canvas the 160×144 rectangle the screen is showing.
///
/// Without that reference one has to count tiles by eye to know which part of
/// the map matches what is visible. The border wraps around just like the real
/// background.
fn mark_viewport(canvas: &mut [akebia_core::Rgb555], scx: u8, scy: u8) {
    const MARK: akebia_core::Rgb555 = akebia_core::Rgb555::from_bits(0x7C1F); // magenta
    let (sx, sy) = (usize::from(scx), usize::from(scy));

    for i in 0..akebia_core::SCREEN_WIDTH {
        let x = (sx + i) % MAP;
        for y in [sy % MAP, (sy + akebia_core::SCREEN_HEIGHT - 1) % MAP] {
            canvas[y * MAP + x] = MARK;
        }
    }
    for i in 0..akebia_core::SCREEN_HEIGHT {
        let y = (sy + i) % MAP;
        for x in [sx % MAP, (sx + akebia_core::SCREEN_WIDTH - 1) % MAP] {
            canvas[y * MAP + x] = MARK;
        }
    }
}

/// The 384 tiles of each bank, in two columns 16 tiles wide.
///
/// It is the dump that separates "the game has not loaded the graphic" from "the
/// game loaded it and we placed it wrong". It goes in greyscale on purpose: with
/// no attribute there is no palette to apply, and the fixed contrast lets the
/// shape show through.
fn render_tilesheet(ppu: &akebia_core::ppu::Ppu) -> (Vec<akebia_core::Rgb555>, usize, usize) {
    const TILES: usize = 384;
    const PER_ROW: usize = 16;
    const GREYS: [akebia_core::Rgb555; 4] = [
        akebia_core::Rgb555::WHITE,
        akebia_core::Rgb555::from_bits(0x56B5),
        akebia_core::Rgb555::from_bits(0x2529),
        akebia_core::Rgb555::BLACK,
    ];
    /// Gap between the two banks, so they do not get confused.
    const GAP: usize = 8;

    let width = PER_ROW * 8 * 2 + GAP;
    let height = TILES / PER_ROW * 8;
    let mut out = vec![akebia_core::Rgb555::from_bits(0x7C1F); width * height];

    for bank in 0..2 {
        let dx = bank * (PER_ROW * 8 + GAP);
        for tile in 0..TILES {
            let (cx, cy) = (dx + (tile % PER_ROW) * 8, (tile / PER_ROW) * 8);
            for row in 0..8 {
                for col in 0..8 {
                    let index = tile_pixel(ppu, bank, tile * 16, row, col);
                    out[(cy + row) * width + cx + col] = GREYS[index as usize];
                }
            }
        }
    }
    (out, width, height)
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn registers(gb: &GameBoy, s: &mut String) {
    use core::fmt::Write;
    let p = |addr| gb.peek(addr);

    let _ = writeln!(s, "== Registers ==");
    let lcdc = p(0xFF40);
    let _ = writeln!(s, "LCDC 0xFF40 = {lcdc:02X}  {}", lcdc_flags(lcdc));
    for (addr, name) in [
        (0xFF41u16, "STAT"),
        (0xFF42, "SCY "),
        (0xFF43, "SCX "),
        (0xFF44, "LY  "),
        (0xFF45, "LYC "),
        (0xFF4A, "WY  "),
        (0xFF4B, "WX  "),
        (0xFF47, "BGP "),
        (0xFF48, "OBP0"),
        (0xFF49, "OBP1"),
        (0xFF4F, "VBK "),
        (0xFF55, "HDMA"),
        (0xFF68, "BGPI"),
        (0xFF6A, "OBPI"),
        (0xFF6C, "OPRI"),
    ] {
        let _ = writeln!(s, "{name} 0x{addr:04X} = {:02X}", p(addr));
    }
}

/// The two 32×32 maps, with the tile index and —on CGB— its attribute.
///
/// It is the dump that says whether the corruption is in the map (the game wrote
/// odd indices) or in the data (the map is fine and the tile never arrived).
fn maps(ppu: &akebia_core::ppu::Ppu, s: &mut String) {
    use core::fmt::Write;

    for (base, name) in [(0x1800usize, "0x9800"), (0x1C00, "0x9C00")] {
        let _ = writeln!(s, "\n== Tile map {name} (index/attribute) ==");
        let bank0 = ppu.vram_bank_snapshot(0);
        let bank1 = ppu.vram_bank_snapshot(1);
        for row in 0..32 {
            let _ = write!(s, "{row:2} ");
            for col in 0..32 {
                let i = base + row * 32 + col;
                let _ = write!(s, "{:02X}/{:02X} ", bank0[i], bank1[i]);
            }
            let _ = writeln!(s);
        }
    }
}

fn sprites(ppu: &akebia_core::ppu::Ppu, s: &mut String) {
    use core::fmt::Write;
    let oam = ppu.oam_snapshot();

    let _ = writeln!(s, "\n== OAM (only the sprites on screen) ==");
    let _ = writeln!(s, "  #   X    Y  tile  attr");
    for i in 0..oam.len() / 4 {
        let e = &oam[i * 4..][..4];
        // Y = 0 or Y >= 160 leaves the sprite entirely off screen.
        if e[0] == 0 || e[0] >= 160 {
            continue;
        }
        let _ = writeln!(
            s,
            " {i:2}  {:3}  {:3}    {:02X}    {:02X}",
            i16::from(e[1]) - 8,
            i16::from(e[0]) - 16,
            e[2],
            e[3]
        );
    }
}

/// The sixteen CGB palettes in 24-bit RGB.
///
/// A wrong colour here explains a band with swapped shades without there being
/// anything wrong with the tiles, so it is worth ruling out before looking at
/// VRAM.
fn palettes(ppu: &akebia_core::ppu::Ppu, s: &mut String) {
    use core::fmt::Write;
    let _ = writeln!(s, "\n== CGB palettes ==");

    for (name, ram) in [("background", ppu.bg_palette_ram()), ("sprites", ppu.obj_palette_ram())] {
        let _ = writeln!(s, "-- {name} --");
        for palette in 0..8u8 {
            let _ = write!(s, " {palette}:");
            for color in 0..4u8 {
                let [r, g, b] = ram.color(palette, color).to_rgb888();
                let _ = write!(s, "  #{r:02X}{g:02X}{b:02X}");
            }
            let _ = writeln!(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lcdc_flags_tell_on_from_off() {
        assert_eq!(lcdc_flags(0xFF), "LMWTBHOP", "all on goes in uppercase");
        assert_eq!(lcdc_flags(0x00), "lmwtbhop");
        // 0x91 is the boot value: LCD, tiles at 0x8000 and background.
        assert_eq!(lcdc_flags(0x91), "LmwTbhoP");
    }

    #[test]
    fn the_row_aligns_the_fields() {
        let l = ScanlineTrace { ly: 72, lcdc: 0x91, scx: 32, scy: 200, ..Default::default() };
        let text = row(&l);
        assert!(text.contains(" 72  91 LmwTbhoP"));
        assert!(text.contains("200"), "the vertical scroll shows up");
    }

    /// The capture with F2 is used once, mid-game and with the bug right there.
    /// If it fails then, the moment is lost: a test is worth it.
    #[test]
    fn the_capture_writes_all_four_pieces() {
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0143] = 0xC0; // CGB exclusive, so there are colour palettes
        let gb = GameBoy::new(rom).unwrap();

        let dir = std::env::temp_dir().join(format!("gb-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let txt = snapshot(&gb, &dir, 7).unwrap();
        assert_eq!(txt.file_name().unwrap(), "007.txt");
        assert_eq!(txt.parent().unwrap().file_name().unwrap(), CAPTURES);
        let dir = dir.join(CAPTURES);

        let contents = std::fs::read_to_string(&txt).unwrap();
        for section in ["== Registers ==", "0x9800", "0x9C00", "== OAM", "== CGB palettes =="] {
            assert!(contents.contains(section), "the {section} section is missing");
        }

        // Both whole VRAM banks and OAM, raw.
        let size = |ext: &str| std::fs::metadata(dir.join(format!("007.{ext}"))).unwrap().len();
        assert_eq!(size("vram0.bin"), 8 * 1024);
        assert_eq!(size("vram1.bin"), 8 * 1024);
        assert_eq!(size("oam.bin"), 160);

        // And the four images, with their PPM header and the right size.
        for (ext, width, height) in [
            ("frame.ppm", 160, 144),
            ("bg.ppm", 256, 256),
            ("window.ppm", 256, 256),
            ("tiles.ppm", 264, 192),
        ] {
            let data = std::fs::read(dir.join(format!("007.{ext}"))).unwrap();
            let header = format!("P6\n{width} {height}\n255\n");
            assert!(data.starts_with(header.as_bytes()), "{ext}: bad header");
            assert_eq!(
                data.len(),
                header.len() + width * height * 3,
                "{ext}: pixels missing or left over"
            );
        }
    }

    #[test]
    fn the_tile_index_is_signed_with_bit_4_off() {
        // With LCDC.4 = 1 the base is 0x8000 and the index is direct.
        assert_eq!(bg_tile_addr(0x10, 2), 32);
        // With LCDC.4 = 0 the base is 0x9000 and 0xFF reads as -1.
        assert_eq!(bg_tile_addr(0x00, 0), 0x1000);
        assert_eq!(bg_tile_addr(0x00, 0xFF), 0x0FF0);
    }

    #[test]
    fn the_viewport_box_wraps_around_the_canvas() {
        let mut canvas = vec![akebia_core::Rgb555::BLACK; 256 * 256];
        // With SCX = 200 the right edge falls at 200 + 159 - 256 = 103.
        mark_viewport(&mut canvas, 200, 0);

        let magenta = akebia_core::Rgb555::from_bits(0x7C1F);
        assert_eq!(canvas[200], magenta, "top-left corner");
        assert_eq!(canvas[103], magenta, "the right edge reappeared on the left");
    }
}
