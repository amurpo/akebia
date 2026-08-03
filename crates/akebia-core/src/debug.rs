//! PPU instrumentation for diagnosing rendering problems.
//!
//! A graphics glitch is almost never visible in the frame: it is visible in
//! **what changed between one line and the next**. The screen is drawn line by
//! line reading the registers as they stand at that instant, so a corrupt band
//! halfway down means something changed at that height —the scroll, `LCDC`, the
//! tile map— or that the tile that line needed arrived late.
//!
//! That is why what gets captured is a snapshot of the registers **at the exact
//! moment each line was drawn**, plus the HDMA blocks copied during its HBlank.
//! Comparing consecutive lines points at the height of the problem.
//!
//! It is off unless asked for (`GameBoy::set_trace_enabled`): until it is turned
//! on, not one byte is allocated.

/// A CPU write to VRAM or to an I/O register, exactly as it was executed.
///
/// It is the log needed when the VRAM contents come out wrong: it does not infer
/// the path from the final state —that admits many different stories— but
/// records every write with **the address it went to and the `PC` that ordered
/// it**. The `PC` tells which routine of the game writes, and the sequence of
/// addresses gives the real stride from one to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemWrite {
    /// Address of the instruction that executed it.
    pub pc: u16,
    pub addr: u16,
    pub value: u8,
    /// Bank selected by `VBK` at that instant. It only means something on VRAM
    /// writes.
    pub bank: u8,
    pub ly: u8,
    /// PPU mode: 3 means the write was lost.
    pub mode: u8,
}

impl MemWrite {
    /// `false` if the PPU had VRAM locked and the write was discarded. It only
    /// applies to VRAM: an I/O register is written in any mode.
    pub fn accepted(&self) -> bool {
        self.mode != 3
    }
}

/// The registers a specific line was drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanlineTrace {
    pub ly: u8,
    pub lcdc: u8,
    pub scx: u8,
    pub scy: u8,
    pub wx: u8,
    pub wy: u8,
    /// The window's internal counter, which is not `LY - WY`.
    pub window_line: u8,
    /// VRAM bank selected by `VBK`.
    pub vram_bank: u8,
    /// 16-byte blocks the HDMA copied during this line's HBlank, that is,
    /// **after** drawing it and before the next one.
    pub hdma_blocks: u16,
}

impl ScanlineTrace {
    /// Everything that describes *how* it is drawn, without `LY` or the HDMA.
    ///
    /// It is the part that should stay constant between lines: if it changes
    /// halfway down the screen, that is where the effect —or the bug— is.
    ///
    /// `VBK` is left out on purpose: it only chooses which bank the CPU writes
    /// to, and rendering does not look at it —each tile takes its bank from the
    /// attribute—. Including it would flag a "change" every time the game is
    /// about to write attributes, which is constantly. It is still shown in the
    /// dump just in case.
    fn shape(&self) -> (u8, u8, u8, u8, u8) {
        (self.lcdc, self.scx, self.scy, self.wx, self.wy)
    }

    /// `true` if this line is drawn differently from the previous one.
    pub fn differs_from(&self, previous: &Self) -> bool {
        self.shape() != previous.shape()
    }
}

/// Summary of a frame: only the lines where something changed.
///
/// A normal frame boils down to a single entry; one with a per-line scroll
/// effect, to a handful. That is what makes the live dump readable while
/// playing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameTrace {
    /// Lines where something changed, the first one always included.
    pub changes: Vec<ScanlineTrace>,
    /// HDMA blocks for the whole frame. A game streaming tiles into VRAM while
    /// drawing moves tens or hundreds of blocks per frame here.
    pub hdma_blocks: u16,
    /// Visible lines that actually got drawn.
    pub lines: u16,
    /// Accepted VRAM writes, split by bank and region:
    /// `[bank0 tiles, bank0 map, bank1 tiles, bank1 attributes]`.
    pub vram_writes: [u32; 4],
    /// VRAM writes the mode 3 lock threw away.
    ///
    /// It is the figure that separates "the game did not write the tile" from
    /// "it wrote it and we ate it": if it climbs into the thousands, the lock is
    /// lying.
    pub vram_blocked: u32,
}

impl FrameTrace {
    /// Condenses the 144 lines down to the points where the drawing changes.
    pub fn from_lines(lines: &[ScanlineTrace]) -> Self {
        let mut changes = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i == 0 || line.differs_from(&lines[i - 1]) {
                changes.push(*line);
            }
        }
        Self {
            changes,
            hdma_blocks: lines.iter().map(|l| l.hdma_blocks).sum(),
            lines: lines.len() as u16,
            vram_writes: [0; 4],
            vram_blocked: 0,
        }
    }

    /// `true` if the frame was drawn the same way as another.
    ///
    /// The HDMA is compared by total and not line by line: a game streaming
    /// tiles continuously spreads the blocks slightly differently each frame,
    /// and that is not a change worth reprinting.
    pub fn looks_like(&self, other: &Self) -> bool {
        self.lines == other.lines
            && self.hdma_blocks == other.hdma_blocks
            // The exact number of writes wobbles every frame and is not worth
            // reprinting, but writes **starting** to be discarded is: that is an
            // alarm and has to break the silence.
            && (self.vram_blocked == 0) == (other.vram_blocked == 0)
            && self.changes.len() == other.changes.len()
            && self
                .changes
                .iter()
                .zip(&other.changes)
                .all(|(a, b)| a.ly == b.ly && !a.differs_from(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ly: u8, scy: u8) -> ScanlineTrace {
        ScanlineTrace { ly, scy, ..Default::default() }
    }

    #[test]
    fn a_frame_with_no_changes_boils_down_to_one_line() {
        let lines: Vec<_> = (0..144).map(|ly| line(ly, 7)).collect();
        let t = FrameTrace::from_lines(&lines);

        assert_eq!(t.changes.len(), 1, "only the first one");
        assert_eq!(t.changes[0].ly, 0);
        assert_eq!(t.lines, 144);
    }

    #[test]
    fn the_exact_line_where_the_scroll_changes_is_flagged() {
        let lines: Vec<_> = (0..144).map(|ly| line(ly, if ly < 80 { 7 } else { 200 })).collect();
        let t = FrameTrace::from_lines(&lines);

        assert_eq!(t.changes.len(), 2);
        assert_eq!(t.changes[1].ly, 80, "the cut is on line 80");
        assert_eq!(t.changes[1].scy, 200);
    }

    #[test]
    fn the_hdma_is_summed_over_the_whole_frame() {
        let mut lines: Vec<_> = (0..144).map(|ly| line(ly, 0)).collect();
        lines[10].hdma_blocks = 4;
        lines[11].hdma_blocks = 4;

        assert_eq!(FrameTrace::from_lines(&lines).hdma_blocks, 8);
    }

    #[test]
    fn two_identical_frames_are_recognised_as_equal() {
        let lines: Vec<_> = (0..144).map(|ly| line(ly, 7)).collect();
        let a = FrameTrace::from_lines(&lines);
        assert!(a.looks_like(&FrameTrace::from_lines(&lines)));

        let others: Vec<_> = (0..144).map(|ly| line(ly, 9)).collect();
        assert!(!a.looks_like(&FrameTrace::from_lines(&others)));
    }
}
