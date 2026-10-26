//! The machine as one thing.
//!
//! # Why this exists
//!
//! Everything below is a part: a processor, a memory map, a picture unit, four
//! counters. Running a game means holding two of them at once and stepping one
//! against the other, and until now the only place that knew how was an example
//! program. That was fine while the only caller was a diagnostic harness, and
//! stops being fine the moment a second caller appears — a frontend would have
//! had to know which registers the BIOS leaves set, how many steps a frame is
//! worth, and that the beam has to be swept twice to be sure of a whole
//! picture. None of that is a frontend's business.
//!
//! So this is the seam: below it, parts that know nothing about being driven;
//! above it, a console that runs a frame and shows a picture.
//!
//! # What a frame costs, and the ceiling that should never be reached
//!
//! A frame is swept when the beam says so, not when a step count says so, so
//! [`Gba::run_frame`] steps until the picture unit reports one more frame than
//! it had.
//!
//! There is a cap on that, and it is worth being straight about what it is for:
//! **as the machine stands it cannot be reached**. Every step charges the clock
//! before anything else it might do — deliberately, so that a halted processor
//! still pays time and the thing it is waiting for can arrive — so the beam
//! moves whatever the program does, and a frame always ends. The cap is there
//! for the day that stops being true. A frontend calling this must get a
//! picture back or an error back; what it must never get is neither, because a
//! wrong picture can be looked at and a hung window cannot.

use crate::bus::Memory;
use crate::cpu::{Cpu, Fault};
use crate::keypad::Button;
use crate::ppu::FRAME_CYCLES;
use crate::save;
use crate::sound::StereoSample;
use crate::{Mode, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Where a cartridge is mapped, and so where a machine without a BIOS starts.
pub const ROM_BASE: u32 = 0x0800_0000;

/// The most steps one frame may take before it is given up on.
///
/// One step charges one cycle, so an honest frame costs `FRAME_CYCLES` of them.
/// The margin is for the fact that this is a floor and not a measurement: when
/// the timing is made real an instruction will cost more than one cycle, and a
/// frame will cost fewer steps than this rather than more.
const STEPS_PER_FRAME_CEILING: u64 = FRAME_CYCLES as u64 * 2;

/// Why a frame ended early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// The processor could not carry on.
    Faulted(Fault),
    /// The beam never reached the bottom. Something is holding the machine
    /// somewhere it cannot get out of.
    Stuck,
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Faulted(fault) => write!(f, "{fault}"),
            Self::Stuck => write!(f, "the beam did not reach the bottom of the screen"),
        }
    }
}

/// A Game Boy Advance.
pub struct Gba {
    cpu: Cpu,
    mem: Memory,
}

impl Default for Gba {
    fn default() -> Self {
        Self::new()
    }
}

impl Gba {
    /// A machine with nothing in it. Put a cartridge in before running it.
    pub fn new() -> Self {
        let mut gba = Self { cpu: Cpu::new(), mem: Memory::new() };
        gba.reset();
        gba
    }

    /// A machine with a cartridge in, started the way a game expects.
    pub fn with_rom(rom: &[u8]) -> Self {
        let mut gba = Self::new();
        gba.load_rom(rom);
        gba.reset();
        gba
    }

    pub fn load_rom(&mut self, image: &[u8]) {
        self.mem.load_rom(image);
    }

    /// Puts a BIOS image in. Without one, a game that calls into the BIOS finds
    /// an empty vector there — see [`Gba::reset`] for what that costs.
    pub fn load_bios(&mut self, image: &[u8]) {
        self.mem.load_bios(image);
    }

    pub fn has_bios(&self) -> bool {
        self.mem.has_bios()
    }

    /// Starts the machine at the cartridge, in the state the BIOS would have
    /// left it.
    ///
    /// # Why not run the BIOS
    ///
    /// Because there may not be one, and a machine that only works with a file
    /// the user has to find is a machine that mostly does not work. What the
    /// BIOS leaves behind that a game depends on is small and known: the mode,
    /// and a stack pointer in each of the three banks that has one. So they are
    /// set here and the cartridge is entered directly.
    ///
    /// A real BIOS, if one was handed in, is still there to be *called* — games
    /// use its routines constantly — and [`Gba::boot`] will run it from the
    /// reset vector for whoever wants the logo and the two seconds it takes.
    pub fn reset(&mut self) {
        self.cpu = Cpu::new();
        // A stack for each bank that gets one. Supervisor and IRQ are entered
        // by the hardware itself, so a game that never sets them still expects
        // them to point somewhere.
        self.cpu.regs.set_mode(Mode::Supervisor);
        self.cpu.regs.set(13, 0x0300_7FE0);
        self.cpu.regs.set_mode(Mode::Irq);
        self.cpu.regs.set(13, 0x0300_7FA0);
        self.cpu.regs.set_mode(Mode::System);
        self.cpu.regs.set(13, 0x0300_7F00);
        self.cpu.regs.set_pc(ROM_BASE);
    }

    /// Starts from the reset vector instead, which is the BIOS's own entry
    /// point. Only useful with a real BIOS in place.
    pub fn boot(&mut self) {
        self.reset();
        self.cpu.regs.set_mode(Mode::Supervisor);
        self.cpu.regs.set_pc(0);
    }

    /// Runs until the picture unit has finished one more frame.
    ///
    /// Two boundaries and not one when starting mid-frame: the first only
    /// finishes whatever was already partly swept, which was drawn with the
    /// contents of memory as they were before this call.
    pub fn run_frame(&mut self) -> Result<(), Stopped> {
        let until = self.mem.ppu().frames() + 1;
        let mut steps = 0u64;
        while self.mem.ppu().frames() < until {
            if steps >= STEPS_PER_FRAME_CEILING {
                return Err(Stopped::Stuck);
            }
            self.cpu.step(&mut self.mem).map_err(Stopped::Faulted)?;
            steps += 1;
        }
        Ok(())
    }

    /// The picture as it stands: 15-bit colour, a pixel each, in rows of
    /// [`SCREEN_WIDTH`].
    pub fn frame(&self) -> &[u16] {
        self.mem.ppu().frame()
    }

    pub const fn screen_size(&self) -> (usize, usize) {
        (SCREEN_WIDTH, SCREEN_HEIGHT)
    }

    /// The samples made since this was last called, and it empties what it
    /// hands over.
    ///
    /// Whoever runs frames must call this or [`Gba::discard_audio`] after each
    /// one. There is no third option: samples are made whether or not anybody
    /// wants them — the mixer is driven by the same clock as the beam — and a
    /// frontend that never collected them would grow this by fifty thousand a
    /// second for as long as the game ran.
    pub fn take_audio(&mut self) -> Vec<StereoSample> {
        self.mem.sound_mut().drain()
    }

    /// Throws them away instead, for a frontend with no sound card.
    pub fn discard_audio(&mut self) {
        self.mem.sound_mut().discard();
    }

    /// Changes the rate the samples come out at, which is the sound card's to
    /// decide and not the machine's.
    pub fn set_sample_rate(&mut self, rate: u32) {
        self.mem.sound_mut().set_sample_rate(rate);
    }

    /// Presses or releases a button. Whoever calls this must do so *before*
    /// the frame the game reads it in.
    pub fn set_button(&mut self, button: Button, down: bool) {
        self.mem.keypad_mut().set(button, down);
    }

    pub fn release_all_buttons(&mut self) {
        self.mem.keypad_mut().release_all();
    }

    /// The name the cartridge gives itself: twelve bytes at `0x080000A0`,
    /// padded with zeros.
    ///
    /// Anything unprintable is dropped rather than shown as a replacement
    /// character. A blank answer means the field was blank, which some
    /// cartridges leave it.
    pub fn title(&self) -> String {
        let mut title = String::new();
        for offset in 0..12 {
            let byte = self.mem.peek8(ROM_BASE + 0xA0 + offset);
            if byte.is_ascii_graphic() || byte == b' ' {
                title.push(byte as char);
            }
        }
        title.trim().to_string()
    }

    /// How many frames have been swept since the machine started.
    pub fn frames(&self) -> u64 {
        self.mem.ppu().frames()
    }

    /// Which save chip this cartridge carries, which is worked out from the ROM
    /// and not declared anywhere in its header. See [`crate::save`].
    pub fn save_kind(&self) -> save::Kind {
        self.mem.save().kind()
    }

    /// The saved game as it stands, for whoever writes it to a file.
    ///
    /// Always something, because every cartridge is given a chip — a ROM that
    /// names no save library is taken to be using plain RAM, which is the one
    /// that needs no library. Whether any of it has been *written* is the
    /// caller's to notice, and worth noticing: a file created next to every ROM
    /// that was opened for a moment is a mess nobody asked for.
    pub fn save_data(&self) -> &[u8] {
        self.mem.save().data()
    }

    /// Restores one, and says whether it fitted.
    ///
    /// A file of the wrong length is refused rather than stretched. It is far
    /// more likely to be another cartridge's saved game than a damaged one, and
    /// the cost of guessing wrong is overwriting a real save.
    pub fn load_save(&mut self, data: &[u8]) -> bool {
        self.mem.save_mut().load(data)
    }

    pub fn memory(&self) -> &Memory {
        &self.mem
    }

    pub fn memory_mut(&mut self) -> &mut Memory {
        &mut self.mem
    }

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }

    pub fn cpu_mut(&mut self) -> &mut Cpu {
        &mut self.cpu
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ppu::PIXELS;

    /// A cartridge that does nothing but branch to itself, which is enough for
    /// the beam to sweep past it.
    fn spinning_rom() -> Vec<u8> {
        let mut rom = vec![0; 0x200];
        // `B .` at the entry point: EAFFFFFE.
        rom[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom
    }

    #[test]
    fn a_new_machine_starts_at_the_cartridge_with_a_stack() {
        let gba = Gba::new();
        assert_eq!(gba.cpu().regs.pc(), ROM_BASE, "straight into the cartridge");
        assert_ne!(gba.cpu().regs.sp(), 0, "with somewhere to push to");
    }

    /// Each of the three banks that the hardware enters on its own needs its
    /// own stack, and a game that never sets them expects them set. The ARM
    /// suite found this the hard way: an unset FIQ stack pushed to address zero
    /// and the machine rebooted.
    #[test]
    fn every_bank_the_hardware_enters_has_a_stack() {
        let mut gba = Gba::new();
        for mode in [Mode::Supervisor, Mode::Irq, Mode::System] {
            gba.cpu_mut().regs.set_mode(mode);
            assert_ne!(gba.cpu().regs.sp(), 0, "{mode:?}");
        }
    }

    #[test]
    fn booting_starts_at_the_reset_vector_instead() {
        let mut gba = Gba::new();
        gba.boot();
        assert_eq!(gba.cpu().regs.pc(), 0);
    }

    /// One call, one frame. This is the whole contract a frontend depends on.
    #[test]
    fn running_a_frame_sweeps_exactly_one() {
        let mut gba = Gba::with_rom(&spinning_rom());
        assert_eq!(gba.frames(), 0);

        for expected in 1..=3 {
            gba.run_frame().expect("a spinning cartridge sweeps fine");
            assert_eq!(gba.frames(), expected);
        }
    }

    #[test]
    fn the_frame_is_a_screenful_of_pixels() {
        let mut gba = Gba::with_rom(&spinning_rom());
        gba.run_frame().unwrap();
        assert_eq!(gba.frame().len(), PIXELS);
        assert_eq!(gba.screen_size(), (SCREEN_WIDTH, SCREEN_HEIGHT));
    }

    /// A machine with no cartridge at all still comes back — with a frame, as
    /// it happens, because the beam is swept by the clock and the clock is
    /// charged by every step whatever the step was doing. That is the property
    /// worth pinning: whoever calls this gets a picture or an error, never
    /// neither.
    #[test]
    fn a_machine_with_nothing_in_it_still_finishes_its_frames() {
        let mut gba = Gba::new();
        for expected in 1..=3 {
            assert_eq!(gba.run_frame(), Ok(()), "frame {expected}");
            assert_eq!(gba.frames(), expected);
        }
    }

    /// And a frame costs about what a frame should. A run that quietly took ten
    /// times as long would still return, and would still be wrong.
    #[test]
    fn a_frame_costs_about_a_frames_worth_of_clock() {
        let mut gba = Gba::with_rom(&spinning_rom());
        let before = gba.memory().cycles();
        gba.run_frame().unwrap();
        let spent = gba.memory().cycles() - before;

        assert!(spent <= u64::from(FRAME_CYCLES), "{spent} cycles is more than a frame");
        // Most of one, rather than a sliver: starting from a fresh machine the
        // beam is at the top, so a whole frame is owed.
        assert!(spent > u64::from(FRAME_CYCLES) / 2, "{spent} cycles is suspiciously few");
    }

    #[test]
    fn buttons_reach_the_machine() {
        let mut gba = Gba::with_rom(&spinning_rom());
        let buttons = |gba: &Gba| {
            let at = crate::keypad::KEYINPUT;
            u16::from(gba.memory().peek8(at)) | (u16::from(gba.memory().peek8(at + 1)) << 8)
        };
        assert_eq!(buttons(&gba), 0x03FF, "nobody is touching it");

        gba.set_button(Button::Start, true);
        assert_eq!(buttons(&gba), 0x03FF & !0x0008, "Start is bit 3");

        gba.release_all_buttons();
        assert_eq!(buttons(&gba), 0x03FF, "and everything comes back up");
    }

    /// The name is where the cartridge header says it is, and anything that is
    /// not a printable character is left out rather than shown as rubbish.
    #[test]
    fn the_title_comes_out_of_the_cartridge_header() {
        let mut rom = spinning_rom();
        rom[0xA0..0xA0 + 12].copy_from_slice(b"AKEBIA\0\0\0\0\0\0");
        let gba = Gba::with_rom(&rom);
        assert_eq!(gba.title(), "AKEBIA");
    }

    #[test]
    fn a_blank_header_gives_a_blank_title() {
        let gba = Gba::with_rom(&spinning_rom());
        assert_eq!(gba.title(), "");
    }
}
