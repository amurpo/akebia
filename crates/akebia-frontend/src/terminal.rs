//! Video adapter for the terminal.
//!
//! **Adapter pattern**: it implements the core's [`VideoOutput`] port without
//! the core knowing anything about ANSI or terminals.
//!
//! Technique: each `▀` character ("upper half block") draws **two** vertical
//! pixels, the top one in the foreground colour and the bottom one in the
//! background colour. That way a 160×144 screen fits in 160×72 cells instead of
//! 160×144, which no terminal is tall enough for.

use std::io::{BufWriter, StdoutLock, Write};

use akebia_core::ports::VideoOutput;
use akebia_core::{FrameBuffer, Rgb555, SCREEN_HEIGHT, SCREEN_WIDTH};

const UPPER_HALF_BLOCK: char = '▀';

/// ANSI sequences used. Grouped so as not to scatter literals through the code.
mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const HIDE_CURSOR: &str = "\x1b[?25l";
    pub const SHOW_CURSOR: &str = "\x1b[?25h";
    pub const CLEAR: &str = "\x1b[2J";
    pub const HOME: &str = "\x1b[H";
    pub const ALT_SCREEN_ON: &str = "\x1b[?1049h";
    pub const ALT_SCREEN_OFF: &str = "\x1b[?1049l";
}

pub struct TerminalVideo<'a> {
    out: BufWriter<StdoutLock<'a>>,
    /// Draw every other column, to fit in 80 characters.
    narrow: bool,
    /// Buffer reused between frames: avoids allocating 40 KiB per frame.
    scratch: String,
    frames: u64,
}

impl<'a> TerminalVideo<'a> {
    pub fn new(out: StdoutLock<'a>, narrow: bool) -> std::io::Result<Self> {
        let mut video = Self {
            out: BufWriter::with_capacity(1 << 16, out),
            narrow,
            scratch: String::with_capacity(1 << 16),
            frames: 0,
        };
        write!(video.out, "{}{}{}", ansi::ALT_SCREEN_ON, ansi::HIDE_CURSOR, ansi::CLEAR)?;
        video.out.flush()?;
        Ok(video)
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    fn step(&self) -> usize {
        if self.narrow {
            2
        } else {
            1
        }
    }

    /// Converts the frame into an ANSI string, minimising colour changes: an
    /// escape sequence is only emitted when the (top, bottom) pair changes with
    /// respect to the previous character.
    fn encode(&mut self, frame: &FrameBuffer) {
        use std::fmt::Write as _;

        let step = self.step();
        self.scratch.clear();
        self.scratch.push_str(ansi::HOME);

        for y in (0..SCREEN_HEIGHT).step_by(2) {
            let mut last: Option<(Rgb555, Rgb555)> = None;

            for x in (0..SCREEN_WIDTH).step_by(step) {
                let top = frame.get(x, y);
                let bottom = frame.get(x, y + 1);

                if last != Some((top, bottom)) {
                    let [tr, tg, tb] = top.to_rgb888();
                    let [br, bg, bb] = bottom.to_rgb888();
                    let _ =
                        write!(self.scratch, "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg};{bb}m");
                    last = Some((top, bottom));
                }
                self.scratch.push(UPPER_HALF_BLOCK);
            }
            self.scratch.push_str(ansi::RESET);
            self.scratch.push('\n');
        }
    }
}

impl VideoOutput for TerminalVideo<'_> {
    fn present(&mut self, frame: &FrameBuffer) {
        self.frames += 1;
        self.encode(frame);
        // A write failure here (a closed pipe, for instance) must not take the
        // emulation down: the core keeps running without a screen.
        let _ = self.out.write_all(self.scratch.as_bytes());
        let _ = self.out.flush();
    }
}

impl Drop for TerminalVideo<'_> {
    /// **RAII**: restoring the terminal cannot depend on the program ending
    /// through the happy path. If the emulator panics or fails, the cursor and
    /// the normal screen come back all the same.
    fn drop(&mut self) {
        let _ = write!(self.out, "{}{}{}", ansi::SHOW_CURSOR, ansi::ALT_SCREEN_OFF, ansi::RESET);
        let _ = self.out.flush();
    }
}
