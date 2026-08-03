//! The PPU: video generation.
//!
//! # State machine
//!
//! The PPU does not draw "a frame": it walks 154 lines of 456 T-cycles each,
//! going through four modes. The first 144 are visible; the last 10 are the
//! vertical blanking.
//!
//! ```text
//!  line 0..143    ├─ mode 2 ─┼─ mode 3 ─┼──── mode 0 ────┤  456 T-cycles
//!                 │ OAM scan │ drawing  │     HBlank     │
//!                 │    80    │ 172..289 │    204..87     │
//!
//!  line 144..153  ├──────────── mode 1 (VBlank) ─────────┤  456 T-cycles
//! ```
//!
//! **Mode 3 does not last the same on every line.** Fine scrolling (`SCX`), the
//! window and each sprite force the PPU to stall, and that time is taken away
//! from mode 0 —the only window in which the CPU can touch VRAM—. Both extremes
//! are fixed by the hardware: 172 dots for a bare line, and 289 for the worst
//! possible one, because HBlank never drops below 87 and the three modes have to
//! fit in 456. The computation lives in [`render`], in `mode3_cycles`.
//!
//! # Rendering strategy
//!
//! There are two possible approaches and it is worth deciding deliberately:
//!
//! - **Per scanline**: when mode 3 ends the whole line is drawn at once, reading
//!   the registers as they stand at that instant. That is the approach `render`
//!   uses, and it is enough for the vast majority of the catalogue: scroll
//!   changes *between* lines —the wave effect, fixed status bars— work just the
//!   same.
//! - **Pixel FIFO**: the hardware is emulated pixel by pixel, cycle by cycle. It
//!   is the only thing that reproduces mid-line effects. Switching from one to
//!   the other is practically a rewrite of the module, so it is best not to get
//!   tied down: that is why all the rendering lives in `render` and nothing
//!   outside it assumes anything about how the pixels are produced.

mod color;
mod registers;
mod render;

pub use color::{PaletteRam, Rgb555, PALETTE_RAM_SIZE};
pub use registers::{Lcdc, Stat};

use crate::cpu::{Interrupt, InterruptController};
use crate::model::Model;

pub const SCREEN_WIDTH: usize = 160;
pub const SCREEN_HEIGHT: usize = 144;

/// T-cycles a complete line lasts, visible or not.
const T_CYCLES_PER_LINE: u32 = 456;
/// Duration of the OAM scan.
const OAM_SCAN_CYCLES: u32 = 80;
/// **Minimum** drawing duration: a line with no fine scroll, no window and no
/// sprites.
const DRAW_CYCLES: u32 = 172;

/// Maximum drawing duration.
///
/// It is not a made-up cap: mode 0 lasts at least 87 dots, and the three modes
/// have to fit in the 456 of the line. `456 - 80 - 87 = 289`.
const MAX_DRAW_CYCLES: u32 = T_CYCLES_PER_LINE - OAM_SCAN_CYCLES - 87;
/// Last visible line.
const LAST_VISIBLE_LINE: u8 = 143;
/// Total lines, including the vertical blanking.
const TOTAL_LINES: u8 = 154;

/// Size of **one** VRAM bank. The DMG has one; the CGB, two.
pub const VRAM_BANK_SIZE: usize = 8 * 1024;
/// Total VRAM reserved. Both banks are always allocated even when emulating a
/// DMG: it is 8 KiB extra and it avoids two different indexing paths.
pub const VRAM_SIZE: usize = 2 * VRAM_BANK_SIZE;
pub const OAM_SIZE: usize = 160;

/// Colour index within a palette: 0 to 3.
///
/// It is what comes out of a tile. **It is not a colour**: on DMG it goes
/// through `BGP`/`OBP` and then through the four shades the frontend sets; on
/// CGB it indexes one of the game's eight colour palettes.
pub type ColorIndex = u8;

/// A complete frame, already in colour.
#[derive(Clone)]
pub struct FrameBuffer {
    pixels: Box<[Rgb555; SCREEN_WIDTH * SCREEN_HEIGHT]>,
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self { pixels: Box::new([Rgb555::WHITE; SCREEN_WIDTH * SCREEN_HEIGHT]) }
    }

    pub fn get(&self, x: usize, y: usize) -> Rgb555 {
        self.pixels[y * SCREEN_WIDTH + x]
    }

    pub fn set(&mut self, x: usize, y: usize, color: Rgb555) {
        self.pixels[y * SCREEN_WIDTH + x] = color;
    }

    pub fn as_slice(&self) -> &[Rgb555] {
        self.pixels.as_slice()
    }

    pub fn clear(&mut self, color: Rgb555) {
        self.pixels.fill(color);
    }
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// PPU mode. The discriminant is what is read in bits 1-0 of `STAT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// Mode 0: the CPU may access VRAM and OAM.
    HBlank = 0,
    /// Mode 1: free access; it is the window in which games update video.
    VBlank = 1,
    /// Mode 2: the PPU looks for the line's sprites. OAM locked.
    OamScan = 2,
    /// Mode 3: drawing. VRAM and OAM locked.
    Drawing = 3,
}

pub struct Ppu {
    /// The two VRAM banks, contiguous. Bank 1 only exists on CGB and holds the
    /// attributes of the background tiles.
    vram: Box<[u8; VRAM_SIZE]>,
    oam: Box<[u8; OAM_SIZE]>,

    model: Model,
    /// VRAM bank visible at `0x8000..0xA000` (`VBK`, 0xFF4F).
    vram_bank: usize,

    /// CGB colour palettes.
    bg_palettes: PaletteRam,
    obj_palettes: PaletteRam,
    /// `OPRI` (0xFF6C): with bit 0 set, sprites are ordered as on a DMG.
    dmg_object_priority: bool,

    /// The four shades used by DMG mode, from lightest to darkest. The frontend
    /// sets them; the core does not decide "which green".
    dmg_shades: [Rgb555; 4],

    lcdc: Lcdc,
    stat: Stat,
    /// Background scroll.
    scy: u8,
    scx: u8,
    /// Current line (`LY`, 0xFF44). Read-only from the CPU.
    ly: u8,
    /// Comparison line (`LYC`, 0xFF45).
    lyc: u8,
    /// Window position.
    wy: u8,
    wx: u8,
    /// Monochrome palettes.
    bgp: u8,
    obp0: u8,
    obp1: u8,

    mode: Mode,
    /// T-cycles elapsed within the current line.
    dot: u32,
    /// Duration of mode 3 on the line in progress. Recomputed when entering it.
    draw_cycles: u32,
    /// Previous level of the STAT interrupt line, to detect the edge.
    stat_line: bool,
    /// The window's own line counter. It is **not** `LY - WY`: it only advances
    /// on the lines where the window actually got drawn, so hiding it and
    /// showing it again does not shift its contents.
    window_line: u8,

    framebuffer: FrameBuffer,
    /// Raised when line 143 completes and consumed by the frontend.
    frame_ready: bool,
    /// Instrumentation of the frame in progress. `None` while nobody asks for
    /// it, which is the normal case: switched off it does not cost even one
    /// allocation.
    trace: Option<Vec<crate::debug::ScanlineTrace>>,
    /// Accepted VRAM writes: `[b0 tiles, b0 map, b1 tiles, b1 attr]`.
    vram_writes: [u32; 4],
    /// VRAM writes rejected by the mode 3 lock.
    vram_blocked: u32,
    /// HDMA blocks in the frame, counted no matter what.
    ///
    /// It is separate from the trace's per-line counter: that one hangs each
    /// block off the last captured line, and the frontend empties that list when
    /// it delivers the frame —right before VBlank, which is when games fire the
    /// GDMA—. VBlank blocks landed in an empty list and were never counted.
    hdma_blocks: u32,
}

/// The original Game Boy greens, as a fallback until the frontend says
/// otherwise.
const DEFAULT_DMG_SHADES: [Rgb555; 4] = [
    Rgb555::from_rgb888(0x9B, 0xBC, 0x0F),
    Rgb555::from_rgb888(0x8B, 0xAC, 0x0F),
    Rgb555::from_rgb888(0x30, 0x62, 0x30),
    Rgb555::from_rgb888(0x0F, 0x38, 0x0F),
];

impl Ppu {
    pub fn new(model: Model) -> Self {
        Self {
            vram: Box::new([0; VRAM_SIZE]),
            oam: Box::new([0; OAM_SIZE]),
            model,
            vram_bank: 0,
            bg_palettes: PaletteRam::new(),
            obj_palettes: PaletteRam::new(),
            dmg_object_priority: false,
            dmg_shades: DEFAULT_DMG_SHADES,
            lcdc: Lcdc::from_bits(0x91),
            stat: Stat::from_bits(0x85),
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            wy: 0,
            wx: 0,
            bgp: 0xFC,
            obp0: 0xFF,
            obp1: 0xFF,
            mode: Mode::OamScan,
            dot: 0,
            draw_cycles: DRAW_CYCLES,
            stat_line: false,
            window_line: 0,
            framebuffer: FrameBuffer::new(),
            frame_ready: false,
            trace: None,
            vram_writes: [0; 4],
            vram_blocked: 0,
            hdma_blocks: 0,
        }
    }

    /// The frame's VRAM write counters, resetting them to zero.
    pub fn take_vram_counters(&mut self) -> ([u32; 4], u32) {
        (core::mem::take(&mut self.vram_writes), core::mem::take(&mut self.vram_blocked))
    }

    /// The frame's HDMA blocks, resetting them to zero.
    pub fn take_hdma_blocks(&mut self) -> u32 {
        core::mem::take(&mut self.hdma_blocks)
    }

    /// Turns the per-line capture on or off. See [`crate::debug`].
    pub fn set_trace_enabled(&mut self, enabled: bool) {
        self.trace = enabled.then(|| Vec::with_capacity(SCREEN_HEIGHT));
    }

    /// Takes what was captured and leaves the buffer ready for the next frame.
    pub fn take_trace(&mut self) -> Vec<crate::debug::ScanlineTrace> {
        match &mut self.trace {
            Some(lines) => core::mem::take(lines),
            None => Vec::new(),
        }
    }

    /// Notes an HDMA block on the line that has just been drawn.
    ///
    /// The HDMA copies during HBlank, which comes right **after** that line is
    /// drawn, so the block is added to the last captured entry.
    pub(crate) fn note_hdma_block(&mut self) {
        self.hdma_blocks = self.hdma_blocks.saturating_add(1);
        if let Some(last) = self.trace.as_mut().and_then(|t| t.last_mut()) {
            last.hdma_blocks = last.hdma_blocks.saturating_add(1);
        }
    }

    /// Records the registers the current line is being drawn with.
    pub(super) fn capture_scanline(&mut self) {
        let entry = crate::debug::ScanlineTrace {
            ly: self.ly,
            lcdc: self.lcdc.bits(),
            scx: self.scx,
            scy: self.scy,
            wx: self.wx,
            wy: self.wy,
            window_line: self.window_line,
            vram_bank: self.vram_bank as u8,
            hdma_blocks: 0,
        };
        if let Some(lines) = &mut self.trace {
            lines.push(entry);
        }
    }

    /// Copy of a VRAM bank, for dumping from the frontend.
    pub fn vram_bank_snapshot(&self, bank: usize) -> &[u8] {
        let base = bank * VRAM_BANK_SIZE;
        &self.vram[base..base + VRAM_BANK_SIZE]
    }

    /// Contents of OAM, for dumping from the frontend.
    pub fn oam_snapshot(&self) -> &[u8] {
        &self.oam[..]
    }

    /// The CGB's eight background palettes, for dumping from the frontend.
    pub fn bg_palette_ram(&self) -> &PaletteRam {
        &self.bg_palettes
    }

    /// The CGB's eight sprite palettes.
    pub fn obj_palette_ram(&self) -> &PaletteRam {
        &self.obj_palettes
    }

    /// Bank selected by `VBK`, for the trace.
    pub fn vram_bank(&self) -> usize {
        self.vram_bank
    }

    /// Current line, for the trace.
    pub fn ly(&self) -> u8 {
        self.ly
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn model(&self) -> Model {
        self.model
    }

    pub fn framebuffer(&self) -> &FrameBuffer {
        &self.framebuffer
    }

    /// Sets the four shades of DMG mode, from lightest to darkest.
    ///
    /// In CGB mode it has no effect: the game supplies the colours.
    pub fn set_dmg_shades(&mut self, shades: [Rgb555; 4]) {
        self.dmg_shades = shades;
    }

    /// `true` if the PPU is in HBlank and a line has just finished. The HDMA
    /// checks it to know when to copy its next block.
    pub fn in_hblank(&self) -> bool {
        self.mode == Mode::HBlank && self.lcdc.lcd_enabled()
    }

    /// Consumes the "frame finished" signal. Returns `true` exactly once per
    /// frame.
    pub fn take_frame_ready(&mut self) -> bool {
        core::mem::take(&mut self.frame_ready)
    }

    /// Advances the PPU by `t_cycles` T-cycles.
    ///
    /// It is called from the bus on every CPU M-cycle, so that the state the CPU
    /// observes when reading `STAT` or `LY` is the right one at that instant.
    pub fn tick(&mut self, t_cycles: u32, ic: &mut InterruptController) {
        if !self.lcdc.lcd_enabled() {
            return;
        }

        for _ in 0..t_cycles {
            self.dot += 1;
            self.advance(ic);
        }
        self.update_stat_line(ic);
    }

    /// One T-cycle of the state machine.
    fn advance(&mut self, ic: &mut InterruptController) {
        match self.mode {
            Mode::OamScan if self.dot >= OAM_SCAN_CYCLES => {
                // The duration is fixed on entering mode 3, not before: it
                // depends on the scroll, the window and the sprites of **this**
                // line, and the hardware has just finished looking for them.
                self.draw_cycles = self.mode3_cycles();
                self.mode = Mode::Drawing;
            }
            Mode::Drawing if self.dot >= OAM_SCAN_CYCLES + self.draw_cycles => {
                self.render_scanline();
                self.mode = Mode::HBlank;
            }
            Mode::HBlank | Mode::VBlank if self.dot >= T_CYCLES_PER_LINE => {
                self.dot = 0;
                self.ly = (self.ly + 1) % TOTAL_LINES;

                self.mode = if self.ly == LAST_VISIBLE_LINE + 1 {
                    // Entering VBlank is what marks the end of the frame.
                    self.frame_ready = true;
                    ic.request(Interrupt::VBlank);
                    Mode::VBlank
                } else if self.ly > LAST_VISIBLE_LINE {
                    Mode::VBlank
                } else {
                    if self.ly == 0 {
                        self.reset_window_line();
                    }
                    Mode::OamScan
                };
            }
            _ => {}
        }
    }

    /// The STAT interrupt fires on the **rising edge** of an OR of four
    /// conditions, not on each condition separately.
    ///
    /// Hence the famous *STAT blocking*: if two conditions overlap, there is
    /// only one interrupt. Emulating it with edges —and not with levels— is what
    /// keeps games like Road Rash from being flooded with interrupts.
    fn update_stat_line(&mut self, ic: &mut InterruptController) {
        let coincidence = self.ly == self.lyc;
        let line = (self.stat.lyc_interrupt() && coincidence)
            || (self.stat.mode2_interrupt() && self.mode == Mode::OamScan)
            || (self.stat.mode1_interrupt() && self.mode == Mode::VBlank)
            || (self.stat.mode0_interrupt() && self.mode == Mode::HBlank);

        if line && !self.stat_line {
            ic.request(Interrupt::LcdStat);
        }
        self.stat_line = line;
    }

    // ---- Memory-mapped registers -------------------------------------------

    pub fn read_register(&self, addr: u16) -> u8 {
        match addr {
            0xFF40 => self.lcdc.bits(),
            0xFF41 => self.stat.bits_with_mode(self.mode, self.ly == self.lyc),
            0xFF42 => self.scy,
            0xFF43 => self.scx,
            0xFF44 => self.ly,
            0xFF45 => self.lyc,
            0xFF47 => self.bgp,
            0xFF48 => self.obp0,
            0xFF49 => self.obp1,
            0xFF4A => self.wy,
            0xFF4B => self.wx,
            // From here on, everything is CGB-only. On a DMG these registers do
            // not exist and must read as open bus, or the software will believe
            // it is running on a colour console.
            0xFF4F if self.model.is_cgb() => self.vram_bank as u8 | 0xFE,
            0xFF68 if self.model.is_cgb() => self.bg_palettes.read_index(),
            0xFF69 if self.model.is_cgb() => self.bg_palettes.read_data(),
            0xFF6A if self.model.is_cgb() => self.obj_palettes.read_index(),
            0xFF6B if self.model.is_cgb() => self.obj_palettes.read_data(),
            0xFF6C if self.model.is_cgb() => u8::from(self.dmg_object_priority) | 0xFE,
            _ => 0xFF,
        }
    }

    pub fn write_register(&mut self, addr: u16, value: u8) {
        match addr {
            0xFF40 => {
                let was_on = self.lcdc.lcd_enabled();
                self.lcdc = Lcdc::from_bits(value);
                // Turning the LCD off resets the PPU: LY goes back to 0 and the
                // mode to 0. Doing it outside VBlank damages the screen on real
                // hardware.
                if was_on && !self.lcdc.lcd_enabled() {
                    self.ly = 0;
                    self.dot = 0;
                    self.mode = Mode::HBlank;
                    self.window_line = 0;
                    self.framebuffer.clear(Rgb555::WHITE);
                } else if !was_on && self.lcdc.lcd_enabled() {
                    self.mode = Mode::OamScan;
                    self.dot = 0;
                }
            }
            0xFF41 => self.stat.write(value),
            0xFF42 => self.scy = value,
            0xFF43 => self.scx = value,
            0xFF44 => {} // LY is read-only.
            0xFF45 => self.lyc = value,
            0xFF47 => self.bgp = value,
            0xFF48 => self.obp0 = value,
            0xFF49 => self.obp1 = value,
            0xFF4A => self.wy = value,
            0xFF4B => self.wx = value,
            0xFF4F if self.model.is_cgb() => self.vram_bank = usize::from(value & 0x01),
            0xFF68 if self.model.is_cgb() => self.bg_palettes.write_index(value),
            0xFF69 if self.model.is_cgb() => self.bg_palettes.write_data(value),
            0xFF6A if self.model.is_cgb() => self.obj_palettes.write_index(value),
            0xFF6B if self.model.is_cgb() => self.obj_palettes.write_data(value),
            0xFF6C if self.model.is_cgb() => {
                self.dmg_object_priority = value & 0x01 != 0;
            }
            _ => {}
        }
    }

    // ---- VRAM and OAM ------------------------------------------------------
    //
    // During modes 3 (VRAM) and 2-3 (OAM) the bus belongs to the PPU and the CPU
    // reads 0xFF. Very few games depend on it, but the accuracy tests do.

    /// VRAM byte in a specific bank. It is how rendering reads it, needing bank
    /// 0 for the tiles and bank 1 for their attributes, regardless of which one
    /// the CPU has selected.
    pub(super) fn vram_at(&self, bank: usize, addr: u16) -> u8 {
        self.vram[bank * VRAM_BANK_SIZE + (addr as usize - 0x8000) % VRAM_BANK_SIZE]
    }

    fn vram_index(&self, addr: u16) -> usize {
        self.vram_bank * VRAM_BANK_SIZE + (addr as usize - 0x8000) % VRAM_BANK_SIZE
    }

    pub fn read_vram(&self, addr: u16) -> u8 {
        if self.mode == Mode::Drawing && self.lcdc.lcd_enabled() {
            return 0xFF;
        }
        self.vram[self.vram_index(addr)]
    }

    pub fn write_vram(&mut self, addr: u16, value: u8) {
        if self.mode == Mode::Drawing && self.lcdc.lcd_enabled() {
            // What gets discarded is counted: if a game loses tiles, the first
            // thing to find out is whether it is writing them and we are eating
            // them.
            self.vram_blocked = self.vram_blocked.saturating_add(1);
            return;
        }
        // Split by region: "writes to bank 0" does not say the same thing as
        // "writes tiles to bank 0". Tiles live below 0x9800 and the maps —and,
        // in bank 1, the attributes— above it.
        let region = usize::from(addr >= 0x9800);
        self.vram_writes[self.vram_bank * 2 + region] =
            self.vram_writes[self.vram_bank * 2 + region].saturating_add(1);
        let index = self.vram_index(addr);
        self.vram[index] = value;
    }

    pub fn read_oam(&self, addr: u16) -> u8 {
        if matches!(self.mode, Mode::OamScan | Mode::Drawing) && self.lcdc.lcd_enabled() {
            return 0xFF;
        }
        self.oam[(addr as usize - 0xFE00) % OAM_SIZE]
    }

    pub fn write_oam(&mut self, addr: u16, value: u8) {
        if matches!(self.mode, Mode::OamScan | Mode::Drawing) && self.lcdc.lcd_enabled() {
            return;
        }
        self.oam[(addr as usize - 0xFE00) % OAM_SIZE] = value;
    }

    /// Reads without checking the mode, for the debugger and the DMA.
    pub(crate) fn peek_vram(&self, addr: u16) -> u8 {
        self.vram[self.vram_index(addr)]
    }

    pub(crate) fn peek_oam(&self, addr: u16) -> u8 {
        self.oam[(addr as usize - 0xFE00) % OAM_SIZE]
    }

    /// OAM write without checking the mode, for the DMA transfer.
    pub(crate) fn write_oam_dma(&mut self, index: usize, value: u8) {
        self.oam[index % OAM_SIZE] = value;
    }

    /// VRAM write without checking the mode, for the CGB's HDMA.
    ///
    /// The HDMA copies during HBlank, when VRAM is free, so skipping the check
    /// is correct and not a shortcut.
    pub(crate) fn write_vram_dma(&mut self, addr: u16, value: u8) {
        let index = self.vram_index(addr);
        self.vram[index] = value;
    }
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new(Model::Dmg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn powered_ppu() -> (Ppu, InterruptController) {
        (Ppu::new(Model::Dmg), InterruptController::new())
    }

    #[test]
    fn it_walks_the_modes_in_order() {
        let (mut ppu, mut ic) = powered_ppu();
        assert_eq!(ppu.mode(), Mode::OamScan);

        ppu.tick(OAM_SCAN_CYCLES, &mut ic);
        assert_eq!(ppu.mode(), Mode::Drawing);

        ppu.tick(DRAW_CYCLES, &mut ic);
        assert_eq!(ppu.mode(), Mode::HBlank);

        ppu.tick(T_CYCLES_PER_LINE - OAM_SCAN_CYCLES - DRAW_CYCLES, &mut ic);
        assert_eq!(ppu.mode(), Mode::OamScan, "it starts over on the next line");
        assert_eq!(ppu.read_register(0xFF44), 1);
    }

    #[test]
    fn it_enters_vblank_and_requests_an_interrupt() {
        let (mut ppu, mut ic) = powered_ppu();
        ic.write_enable(0xFF);

        ppu.tick(T_CYCLES_PER_LINE * 144, &mut ic);
        assert_eq!(ppu.read_register(0xFF44), 144);
        assert_eq!(ppu.mode(), Mode::VBlank);
        assert_eq!(ic.pending(), Some(Interrupt::VBlank));
        assert!(ppu.take_frame_ready());
        assert!(!ppu.take_frame_ready(), "the signal is consumed only once");
    }

    #[test]
    fn a_complete_frame_lasts_70224_t_cycles() {
        let (mut ppu, mut ic) = powered_ppu();
        ppu.tick(T_CYCLES_PER_LINE * u32::from(TOTAL_LINES), &mut ic);
        assert_eq!(ppu.read_register(0xFF44), 0, "LY returns to 0 after 154 lines");
        assert_eq!(T_CYCLES_PER_LINE * u32::from(TOTAL_LINES), 70_224);
    }

    #[test]
    fn the_stat_interrupt_fires_on_an_edge() {
        let (mut ppu, mut ic) = powered_ppu();
        ic.write_enable(0xFF);
        ppu.write_register(0xFF45, 2); // LYC = 2
        ppu.write_register(0xFF41, 0b0100_0000); // enable the LYC source

        ppu.tick(T_CYCLES_PER_LINE * 2, &mut ic);
        assert_eq!(ic.pending(), Some(Interrupt::LcdStat));

        // It keeps matching for the whole line, but it is not requested again.
        ic.acknowledge(Interrupt::LcdStat);
        ppu.tick(100, &mut ic);
        assert_eq!(ic.pending(), None, "STAT blocking: no edge, no interrupt");
    }

    #[test]
    fn vram_is_locked_during_drawing() {
        let (mut ppu, mut ic) = powered_ppu();
        ppu.write_vram(0x8000, 0x42);
        assert_eq!(ppu.read_vram(0x8000), 0x42);

        ppu.tick(OAM_SCAN_CYCLES, &mut ic);
        assert_eq!(ppu.mode(), Mode::Drawing);
        assert_eq!(ppu.read_vram(0x8000), 0xFF, "the CPU cannot see VRAM in mode 3");
        ppu.write_vram(0x8000, 0x00);

        ppu.tick(DRAW_CYCLES, &mut ic);
        assert_eq!(ppu.read_vram(0x8000), 0x42, "the blocked write was discarded");
    }

    #[test]
    fn turning_the_lcd_off_freezes_the_ppu() {
        let (mut ppu, mut ic) = powered_ppu();
        ppu.tick(T_CYCLES_PER_LINE * 5, &mut ic);
        assert_eq!(ppu.read_register(0xFF44), 5);

        ppu.write_register(0xFF40, 0x00);
        assert_eq!(ppu.read_register(0xFF44), 0, "turning the LCD off resets LY");
        ppu.tick(T_CYCLES_PER_LINE * 5, &mut ic);
        assert_eq!(ppu.read_register(0xFF44), 0, "with the LCD off it does not advance");
    }
}
