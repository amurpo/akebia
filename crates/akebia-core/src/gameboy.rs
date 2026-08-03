//! The complete console.
//!
//! **Facade pattern.** It joins CPU and bus and exposes the minimum surface a
//! frontend needs: load a ROM, advance a frame, press buttons. Everything else
//! is internal detail the frontend must not know about.

use crate::bus::SystemBus;
use crate::cartridge::{Cartridge, Header};
use crate::cpu::{Bus, Cpu, Fault};
use crate::joypad::Button;
use crate::model::Model;
use crate::ports::VideoOutput;
use crate::{Result, CLOCK_HZ};

/// T-cycles in a complete frame: 154 lines × 456 T-cycles.
pub const T_CYCLES_PER_FRAME: u32 = 70_224;

/// The hardware's real frames per second: ≈59.7275, not 60.
pub const FRAMES_PER_SECOND: f64 = CLOCK_HZ as f64 / T_CYCLES_PER_FRAME as f64;

pub struct GameBoy {
    cpu: Cpu,
    bus: SystemBus,
}

impl GameBoy {
    /// Loads a ROM, choosing the console model from what the cartridge declares,
    /// and leaves the machine in the state the BootROM hands over to the game.
    ///
    /// The BootROM is not emulated: the Nintendo logo does not show up, but
    /// neither is the file needed, which is copyrighted anyway.
    pub fn new(rom: Vec<u8>) -> Result<Self> {
        let cartridge = Cartridge::load(rom)?;
        let model = Model::detect(cartridge.header().cgb);
        Ok(Self::with_model(cartridge, model))
    }

    /// The same, but forcing the model.
    ///
    /// Useful for seeing a colour game in black and white, or for checking that
    /// an "enhanced" cartridge still works on the original console.
    pub fn new_as(rom: Vec<u8>, model: Model) -> Result<Self> {
        Ok(Self::with_model(Cartridge::load(rom)?, model))
    }

    fn with_model(cartridge: Cartridge, model: Model) -> Self {
        // Register `A` after boot is what the game checks to find out which
        // console it is on: 0x01 on DMG, 0x11 on CGB.
        let cpu = match model {
            Model::Dmg => Cpu::new_dmg(),
            Model::Cgb => Cpu::new_cgb(),
        };
        Self { cpu, bus: SystemBus::new(cartridge, model) }
    }

    pub fn model(&self) -> Model {
        self.bus.model()
    }

    /// Sets the four shades of DMG mode, from lightest to darkest. In CGB mode
    /// it has no effect: the game supplies the colours.
    pub fn set_dmg_shades(&mut self, shades: [crate::ppu::Rgb555; 4]) {
        self.bus.ppu.set_dmg_shades(shades);
    }

    /// Executes one instruction. Returns the M-cycles consumed.
    ///
    /// It is the entry point of a step-by-step debugger.
    pub fn step(&mut self) -> core::result::Result<u32, Fault> {
        self.bus.set_current_pc(self.cpu.regs.pc);
        self.cpu.step(&mut self.bus)
    }

    /// Runs until the PPU delivers a frame and sends it to the video port.
    ///
    /// The frame is the natural unit of synchronisation: the frontend calls
    /// this, presents the result and sleeps whatever is left of the 16.74 ms.
    pub fn run_frame(&mut self, video: &mut impl VideoOutput) -> core::result::Result<(), Fault> {
        // Safety bound: if the game turns the LCD off, the PPU never produces
        // frames and without this the loop would not terminate.
        let limit = self.bus.t_cycles() + u64::from(T_CYCLES_PER_FRAME) * 2;

        while self.bus.t_cycles() < limit {
            self.bus.set_current_pc(self.cpu.regs.pc);
            self.cpu.step(&mut self.bus)?;
            if self.bus.ppu.take_frame_ready() {
                video.present(self.bus.ppu.framebuffer());
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn set_button(&mut self, button: Button, down: bool) {
        self.bus.set_button(button, down);
    }

    /// Audio samples generated since the last call.
    ///
    /// They have to be drained every frame: if nobody collects them, the
    /// internal buffer grows without bound. A frontend with no audio should call
    /// [`GameBoy::discard_audio`] instead.
    pub fn take_audio(&mut self) -> Vec<crate::StereoSample> {
        self.bus.apu.drain()
    }

    /// Discards the pending audio without copying it.
    pub fn discard_audio(&mut self) {
        self.bus.apu.discard();
    }

    /// Sets the rate at which the APU generates samples.
    ///
    /// The output device sets it, not the emulator: generating at 48 kHz to then
    /// resample down to the 44.1 kHz the card asks for would be doing the work
    /// twice and worse.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.bus.apu.set_sample_rate(sample_rate);
    }

    /// Turns the speaker low-pass on or off. See [`crate::apu::SPEAKER_CUTOFF_HZ`].
    pub fn set_speaker_filter(&mut self, enabled: bool) {
        self.bus.apu.set_speaker_filter(enabled);
    }

    /// Bytes the game wrote to the serial port since the last call.
    ///
    /// It is where Blargg's test suites report their results.
    pub fn take_serial_output(&mut self) -> Vec<u8> {
        self.bus.serial.take_output()
    }

    pub fn header(&self) -> &Header {
        self.bus.cartridge.header()
    }

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }

    pub fn bus(&self) -> &SystemBus {
        &self.bus
    }

    /// Reads memory without altering the state or the clock. For debugging only.
    pub fn peek(&self, addr: u16) -> u8 {
        self.bus.peek(addr)
    }

    /// Contents of the battery-backed SRAM, to dump into a `.sav`.
    pub fn save_ram(&self) -> Option<&[u8]> {
        self.bus.cartridge.save_ram()
    }

    /// Restores a saved game. `false` if the file does not match.
    pub fn load_save_ram(&mut self, data: &[u8]) -> bool {
        self.bus.cartridge.load_save_ram(data)
    }

    /// State of the cartridge clock, if it has one, to write after the SRAM in
    /// the `.sav`.
    ///
    /// `now_unix` is supplied by the frontend: the core does not query the
    /// system clock because that is I/O.
    pub fn rtc_save(&self, now_unix: u64) -> Option<[u8; crate::cartridge::mapper::RTC_SAVE_LEN]> {
        self.bus.cartridge.rtc_save(now_unix)
    }

    /// Restores the cartridge clock and advances it by the time the console
    /// spent powered off. `false` if the cartridge carries no clock or it does
    /// not fit.
    pub fn rtc_load(&mut self, data: &[u8], now_unix: u64) -> bool {
        self.bus.cartridge.rtc_load(data, now_unix)
    }

    /// Turns on the log of writes to VRAM and I/O. See [`crate::debug`].
    pub fn set_write_log_enabled(&mut self, enabled: bool) {
        self.bus.set_write_log_enabled(enabled);
    }

    /// The latest VRAM writes, with the `PC` that ordered them.
    pub fn vram_log(&self) -> Vec<crate::debug::MemWrite> {
        self.bus.vram_log()
    }

    /// The latest writes to I/O registers, with their `PC`.
    pub fn io_log(&self) -> Vec<crate::debug::MemWrite> {
        self.bus.io_log()
    }

    /// Turns on the PPU's per-line instrumentation. See [`crate::debug`].
    pub fn set_trace_enabled(&mut self, enabled: bool) {
        self.bus.ppu.set_trace_enabled(enabled);
    }

    /// Summary of the last drawn frame: on which lines the drawing changed and
    /// how much HDMA there was. Empty if the instrumentation is off.
    pub fn take_frame_trace(&mut self) -> crate::debug::FrameTrace {
        let mut trace = crate::debug::FrameTrace::from_lines(&self.bus.ppu.take_trace());
        let (writes, blocked) = self.bus.ppu.take_vram_counters();
        trace.vram_writes = writes;
        trace.vram_blocked = blocked;
        // The frame total, not the per-line sum: that one misses the blocks
        // happening during VBlank, which are exactly the general-mode ones.
        trace.hdma_blocks = self.bus.ppu.take_hdma_blocks() as u16;
        trace
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::NullOutput;

    /// Minimal ROM that runs an infinite loop at 0x0100.
    fn loop_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0100] = 0x18; // JR -2
        rom[0x0101] = 0xFE;
        rom
    }

    #[test]
    fn the_write_log_records_the_address_and_the_pc() {
        // LD A,0x42 ; LD (0x9C00),A ; JR -2
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0100..0x0106].copy_from_slice(&[0x3E, 0x42, 0xEA, 0x00, 0x9C, 0x18]);
        rom[0x0106] = 0xFE;

        let mut gb = GameBoy::new(rom).unwrap();
        gb.set_write_log_enabled(true);
        gb.step().unwrap(); // LD A,0x42
        gb.step().unwrap(); // LD (0x9C00),A

        let log = gb.vram_log();
        assert_eq!(log.len(), 1, "only the VRAM write, not the register load");
        assert_eq!(log[0].addr, 0x9C00);
        assert_eq!(log[0].value, 0x42);
        assert_eq!(log[0].pc, 0x0102, "the PC is that of the writing instruction");
    }

    /// The bug that split CGB maps across the two banks.
    ///
    /// A game writes the map in two passes —indices with `VBK=0`, attributes
    /// with `VBK=1`— and fires a GDMA for each. In hardware the GDMA freezes the
    /// CPU, so the bank change cannot slip in halfway. Spreading the copy over
    /// M-cycles let it slip in, and the transfer ended up split: the first
    /// blocks in one bank and the rest in the other.
    #[test]
    fn the_gdma_is_not_split_if_the_cpu_changes_bank_right_after() {
        let mut rom = vec![0u8; 32 * 1024];
        // Transfer source: 32 recognisable bytes at 0x0200.
        rom[0x0200..0x02C0].fill(0xAA);

        let program: &[u8] = &[
            0x3E, 0x00, 0xE0, 0x4F, // LD A,0 ; LDH (VBK),A     → bank 0
            0x3E, 0x02, 0xE0, 0x51, // source 0x0200
            0x3E, 0x00, 0xE0, 0x52, //
            0x3E, 0x9C, 0xE0, 0x53, // destination 0x9C00
            0x3E, 0x00, 0xE0, 0x54, //
            0x3E, 0x0B, 0xE0, 0x55, // GDMA (bit 7 at 0) of 12 blocks, 192 bytes
            0x3E, 0x01, 0xE0, 0x4F, // LD A,1 ; LDH (VBK),A     → bank 1
            0x18, 0xFE, // JR -2
        ];
        rom[0x0100..0x0100 + program.len()].copy_from_slice(program);

        let mut gb = GameBoy::new_as(rom, Model::Cgb).unwrap();
        for _ in 0..program.len() {
            gb.step().unwrap();
        }

        // 12 blocks are 192 bytes: long enough for the CPU to get to change
        // banks before the transfer finishes.
        let bank0 = &gb.bus().ppu.vram_bank_snapshot(0)[0x1C00..0x1CC0];
        let bank1 = &gb.bus().ppu.vram_bank_snapshot(1)[0x1C00..0x1CC0];
        assert_eq!(bank0, [0xAA; 192], "all 12 blocks go entirely into bank 0");
        assert_eq!(bank1, [0x00; 192], "and not one byte strays into bank 1");
    }

    /// The GDMA freezes the CPU while it copies, and that is charged for.
    ///
    /// A 16-byte block is 8 M-cycles. The pause arrives one M-cycle late —the
    /// bus advances the clock *before* executing the write, so when that M-cycle
    /// ends the transfer is not armed yet and starts on the next one—, so the
    /// cost is charged to the following instruction. No game notices one M-cycle
    /// of skew; what was noticeable was not charging the 96 the whole transfer
    /// lasts.
    #[test]
    fn the_gdma_charges_the_time_it_freezes_the_cpu() {
        const BLOCKS: u64 = 12;
        /// `LDH (n),A` is 3 M-cycles and `JR` another 3; each M-cycle, 4 T-cycles.
        const TRIGGER: u64 = (3 + 3) * 4;

        let mut rom = vec![0u8; 32 * 1024];
        let program: &[u8] = &[
            0x3E, 0x02, 0xE0, 0x51, // source 0x0200
            0x3E, 0x00, 0xE0, 0x52, //
            0x3E, 0x9C, 0xE0, 0x53, // destination 0x9C00
            0x3E, 0x00, 0xE0, 0x54, //
            0x3E, 0x0B, 0xE0, 0x55, // GDMA of 12 blocks
            0x18, 0xFE, // JR -2
        ];
        rom[0x0100..0x0100 + program.len()].copy_from_slice(program);

        let mut gb = GameBoy::new_as(rom, Model::Cgb).unwrap();
        // The first nine instructions only program the registers.
        for _ in 0..9 {
            gb.step().unwrap();
        }

        let before = gb.bus().t_cycles();
        gb.step().unwrap(); // LDH (0xFF55),A → arms the transfer
        gb.step().unwrap(); // JR -2          → here it copies and freezes
        let cost = gb.bus().t_cycles() - before;

        assert_eq!(
            cost,
            TRIGGER + BLOCKS * 8 * 4,
            "the two instructions cost their own plus 8 M-cycles per block"
        );
    }

    #[test]
    fn at_double_speed_the_gdma_freezes_twice_the_m_cycles() {
        // The block takes the same real time, but at double speed twice as many
        // CPU M-cycles fit into that span.
        let mut rom = vec![0u8; 32 * 1024];
        let program: &[u8] = &[
            0x3E, 0x01, 0xE0, 0x4D, // KEY1: arms the speed switch
            0x10, 0x00, // STOP: performs it
            0x3E, 0x02, 0xE0, 0x51, //
            0x3E, 0x00, 0xE0, 0x52, //
            0x3E, 0x9C, 0xE0, 0x53, //
            0x3E, 0x00, 0xE0, 0x54, //
            0x3E, 0x00, 0xE0, 0x55, // GDMA of 1 block
            0x18, 0xFE, //
        ];
        rom[0x0100..0x0100 + program.len()].copy_from_slice(program);

        let mut gb = GameBoy::new_as(rom, Model::Cgb).unwrap();
        for _ in 0..12 {
            gb.step().unwrap();
        }
        assert!(gb.bus().double_speed(), "the STOP should have switched the speed");

        let before = gb.bus().t_cycles();
        gb.step().unwrap();
        gb.step().unwrap();
        let cost = gb.bus().t_cycles() - before;

        assert_eq!(cost, (3 + 3) * 4 + 16 * 4, "16 M-cycles per block, not 8");
    }

    #[test]
    fn without_turning_it_on_nothing_is_logged() {
        let mut gb = GameBoy::new(loop_rom()).unwrap();
        gb.step().unwrap();
        assert!(gb.vram_log().is_empty());
    }

    #[test]
    fn it_boots_at_0x0100() {
        let gb = GameBoy::new(loop_rom()).unwrap();
        assert_eq!(gb.cpu().regs.pc, 0x0100);
        assert_eq!(gb.cpu().regs.a, 0x01, "A=0x01 identifies the DMG");
    }

    #[test]
    fn between_two_frames_70224_t_cycles_pass() {
        let mut gb = GameBoy::new(loop_rom()).unwrap();

        // The first frame is shorter: the console starts at LY=0 and the frame
        // is delivered on entering VBlank (LY=144), not when line 153 ends. The
        // interval between two consecutive VBlanks is the full period.
        gb.run_frame(&mut NullOutput).unwrap();
        let start = gb.bus().t_cycles();
        gb.run_frame(&mut NullOutput).unwrap();
        let spent = gb.bus().t_cycles() - start;

        // It is not exact: the last instruction can overshoot by a few cycles.
        let expected = u64::from(T_CYCLES_PER_FRAME);
        assert!(
            spent.abs_diff(expected) < 100,
            "a frame spent {spent} T-cycles, ~{expected} were expected"
        );
    }

    #[test]
    fn the_frame_rate_is_not_exactly_60() {
        assert!((FRAMES_PER_SECOND - 59.727).abs() < 0.001);
    }
}
