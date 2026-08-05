//! The system bus: the memory map and the clock.
//!
//! It is the centre of the emulator. It performs two jobs that in hardware are
//! the same thing:
//!
//! 1. **Address decoding**: translating a 16-bit address into the device that
//!    answers at it.
//! 2. **Handing out time**: every CPU access consumes one M-cycle, and that
//!    M-cycle is delivered to the PPU, the timer and the DMA *before* the CPU
//!    sees the data. See [`crate::cpu::bus`] for why this matters.
//!
//! # Memory map
//!
//! ```text
//!  0x0000 ┌───────────────────────┐
//!         │ ROM bank 0            │ cartridge
//!  0x4000 ├───────────────────────┤
//!         │ ROM bank N            │ cartridge (switchable)
//!  0x8000 ├───────────────────────┤
//!         │ VRAM                  │ PPU
//!  0xA000 ├───────────────────────┤
//!         │ external SRAM         │ cartridge
//!  0xC000 ├───────────────────────┤
//!         │ WRAM                  │
//!  0xE000 ├───────────────────────┤
//!         │ Echo RAM              │ mirror of 0xC000
//!  0xFE00 ├───────────────────────┤
//!         │ OAM                   │ PPU
//!  0xFEA0 ├───────────────────────┤
//!         │ prohibited            │
//!  0xFF00 ├───────────────────────┤
//!         │ I/O registers         │
//!  0xFF80 ├───────────────────────┤
//!         │ HRAM                  │ reachable during the DMA
//!  0xFFFF ├───────────────────────┤
//!         │ IE                    │
//!         └───────────────────────┘
//! ```

mod hdma;

pub use hdma::Hdma;

use crate::apu::Apu;
use crate::cartridge::mapper::OPEN_BUS;
use crate::cartridge::{Cartridge, Mapper};
use crate::cpu::{Bus, InterruptController};
use crate::joypad::{Button, Joypad};
use crate::model::Model;
use crate::ppu::Ppu;
use crate::serial::Serial;
use crate::timer::Timer;

/// Size of a WRAM bank. The DMG has two fixed ones; the CGB, eight, and the
/// second one is switchable.
const WRAM_BANK_SIZE: usize = 0x1000;
/// Total WRAM. All 8 banks are always allocated even when emulating a DMG.
const WRAM_SIZE: usize = 8 * WRAM_BANK_SIZE;
const HRAM_SIZE: usize = 0x7F;

/// Bytes the OAM DMA transfers, one per M-cycle.
const DMA_LENGTH: u16 = 0xA0;

/// OAM transfer in progress.
///
/// The DMA copies 160 bytes at one per M-cycle. During that time the CPU can
/// only read HRAM safely; that is why every game's DMA routine is copied into
/// HRAM before being run.
#[derive(Debug, Clone, Copy)]
struct DmaTransfer {
    /// Base address: the value written into 0xFF46, multiplied by 0x100.
    source: u16,
    /// Next byte to copy.
    index: u16,
}

#[derive(Clone)]
pub struct SystemBus {
    pub cartridge: Cartridge,
    pub ppu: Ppu,
    pub apu: Apu,
    pub timer: Timer,
    pub joypad: Joypad,
    pub serial: Serial,

    wram: Box<[u8; WRAM_SIZE]>,
    hram: Box<[u8; HRAM_SIZE]>,
    interrupts: InterruptController,
    dma: Option<DmaTransfer>,

    model: Model,
    /// WRAM bank visible at `0xD000..0xE000` (`SVBK`, 0xFF70). Never 0: the
    /// hardware turns 0 into 1, because bank 0 is already fixed below.
    wram_bank: usize,
    /// The CGB's VRAM transfer.
    hdma: Hdma,
    /// `true` if the console is running at double speed.
    double_speed: bool,
    /// Bit 0 of `KEY1`: a speed switch is armed, waiting for `STOP`.
    speed_switch_armed: bool,

    /// `PC` of the instruction in progress, which the bus does not know by
    /// itself. `GameBoy` leaves it here before each step, and it is only used
    /// for the trace.
    current_pc: u16,
    /// Latest VRAM writes. `None` while nobody asks for them.
    ///
    /// I/O goes into its own ring and not the same one: games write to the APU
    /// constantly, and in a shared ring those writes would evict exactly the
    /// video ones worth keeping.
    vram_log: Option<std::collections::VecDeque<crate::debug::MemWrite>>,
    /// Latest writes to I/O registers.
    io_log: Option<std::collections::VecDeque<crate::debug::MemWrite>>,

    /// Last value written into 0xFF46, which is what it returns when read.
    dma_source_high: u8,

    /// T-cycles elapsed since boot. For diagnostics only.
    t_cycles: u64,
}

impl SystemBus {
    pub fn new(cartridge: Cartridge, model: Model) -> Self {
        Self {
            cartridge,
            ppu: Ppu::new(model),
            apu: Apu::new(crate::apu::DEFAULT_SAMPLE_RATE),
            timer: Timer::new(),
            joypad: Joypad::new(),
            serial: Serial::new(model),
            wram: Box::new([0; WRAM_SIZE]),
            hram: Box::new([0; HRAM_SIZE]),
            interrupts: InterruptController::new(),
            dma: None,
            model,
            wram_bank: 1,
            hdma: Hdma::new(),
            double_speed: false,
            speed_switch_armed: false,
            current_pc: 0,
            vram_log: None,
            io_log: None,
            dma_source_high: 0,
            t_cycles: 0,
        }
    }

    /// Turns the write log on or off. See [`crate::debug`].
    pub fn set_write_log_enabled(&mut self, enabled: bool) {
        self.vram_log = enabled.then(std::collections::VecDeque::new);
        self.io_log = enabled.then(std::collections::VecDeque::new);
    }

    /// Recorded VRAM writes, from oldest to newest.
    pub fn vram_log(&self) -> Vec<crate::debug::MemWrite> {
        Self::dump(&self.vram_log)
    }

    /// Recorded writes to I/O registers.
    pub fn io_log(&self) -> Vec<crate::debug::MemWrite> {
        Self::dump(&self.io_log)
    }

    fn dump(
        log: &Option<std::collections::VecDeque<crate::debug::MemWrite>>,
    ) -> Vec<crate::debug::MemWrite> {
        log.as_ref().map(|l| l.iter().copied().collect()).unwrap_or_default()
    }

    /// `GameBoy` leaves the `PC` here before each instruction.
    pub fn set_current_pc(&mut self, pc: u16) {
        self.current_pc = pc;
    }

    /// Notes a VRAM write together with the `PC` that ordered it.
    ///
    /// It is a ring: once full it drops the oldest. What matters is always the
    /// stretch right before the capture, not the game's boot.
    fn log_write(&mut self, addr: u16, value: u8, io: bool) {
        /// Writes kept in the ring. At ~120 per frame, more than a hundred
        /// frames of history.
        const MAX: usize = 16384;

        let entry = crate::debug::MemWrite {
            pc: self.current_pc,
            addr,
            value,
            bank: self.ppu.vram_bank() as u8,
            ly: self.ppu.ly(),
            mode: self.ppu.mode() as u8,
        };
        let log = if io { &mut self.io_log } else { &mut self.vram_log };
        if let Some(log) = log {
            if log.len() == MAX {
                log.pop_front();
            }
            log.push_back(entry);
        }
    }

    pub fn t_cycles(&self) -> u64 {
        self.t_cycles
    }

    /// See [`GameBoy::offset_clock`]. Only this counter moves: nothing that runs
    /// off it —timer, PPU, APU— is told, which is precisely what keeps the
    /// console's own rhythm intact while its place on the shared clock shifts.
    ///
    /// [`GameBoy::offset_clock`]: crate::GameBoy::offset_clock
    pub fn offset_clock(&mut self, t_cycles: u64) {
        self.t_cycles += t_cycles;
    }

    pub fn model(&self) -> Model {
        self.model
    }

    pub fn double_speed(&self) -> bool {
        self.double_speed
    }

    pub fn set_button(&mut self, button: Button, down: bool) {
        self.joypad.set_button(button, down, &mut self.interrupts);
    }

    /// Hands the serial port the byte the other console was sending. See
    /// [`Serial::complete`].
    ///
    /// It goes through the bus for the same reason the buttons do: the
    /// interrupt controller is not the peripheral's to keep.
    pub fn link_complete(&mut self, received: u8) {
        self.serial.complete(received, &mut self.interrupts);
    }

    /// Clocks a byte in from the console driving the clock. See
    /// [`Serial::clock_in`].
    pub fn link_clock_in(&mut self, incoming: u8) -> Option<u8> {
        self.serial.clock_in(incoming, &mut self.interrupts)
    }

    /// Advances every peripheral by one CPU M-cycle.
    ///
    /// # The two clock domains
    ///
    /// At double speed only the CPU speeds up; the PPU keeps producing its
    /// 59.7 frames per second no matter what. That is why an M-cycle is always
    /// worth 4 T-cycles to the timer and the cartridge —which run with the CPU—
    /// but only 2 to the PPU: in the same real time, the CPU runs twice as many
    /// instructions and the screen never notices.
    fn tick_m_cycle(&mut self) {
        const T: u32 = crate::T_CYCLES_PER_M_CYCLE;
        let video_cycles = if self.double_speed { T / 2 } else { T };
        self.t_cycles += u64::from(T);

        self.step_dma();
        self.ppu.tick(video_cycles, &mut self.interrupts);
        self.step_hdma();
        self.timer.tick(T, &mut self.interrupts);
        // The serial shift clock is divided down from the same counter as the
        // timer, so it gets the full four T-cycles as well: at double speed a
        // transfer really does take half the real time.
        self.serial.tick(T, &mut self.interrupts);

        // The APU sequencer is derived from the timer's counter, so this order
        // matters: the clock advances first, and only then is it queried.
        let bit = Apu::sequencer_div_bit(self.model, self.double_speed);
        self.apu.tick(T, self.timer.counter_bit(bit));

        self.cartridge.tick(T);
    }

    /// Copies whatever the HDMA is due on this M-cycle.
    ///
    /// # Why the general mode goes all at once
    ///
    /// In hardware the GDMA **freezes the CPU** until it finishes, so the game
    /// cannot touch anything between blocks. Spreading it over M-cycles opened a
    /// window that does not exist, and games walked right into it: one that
    /// fires a GDMA and immediately changes `VBK` —which is utterly normal,
    /// because CGB maps are written in two passes, indices into bank 0 and
    /// attributes into bank 1— saw its transfer **split across the two banks**
    /// halfway through. It showed up as horizontal bands: the first blocks in
    /// the right bank and the rest in the other one.
    ///
    /// HBlank mode does go block by block, which is what the hardware does:
    /// there the CPU stays alive between blocks on purpose.
    ///
    /// The time the transfer steals from the CPU **is** charged, in
    /// [`SystemBus::stall_for_gdma`].
    fn step_hdma(&mut self) {
        if !self.hdma.is_active() {
            return;
        }
        let atomic = self.hdma.mode() == Some(hdma::Mode::General);
        let mut blocks = 0u32;

        while let hdma::Step::Copy { source, dest } = self.hdma.step(self.ppu.in_hblank()) {
            for offset in 0..hdma::BLOCK_SIZE {
                let byte = self.read_raw(source.wrapping_add(offset));
                self.ppu.write_vram_dma(dest.wrapping_add(offset), byte);
            }
            self.ppu.note_hdma_block();
            blocks += 1;

            if !atomic || !self.hdma.is_active() {
                break;
            }
        }

        if atomic {
            self.stall_for_gdma(blocks);
        }
    }

    /// Freezes the CPU for as long as a GDMA lasts.
    ///
    /// Copying for free does not falsify *what* ends up in VRAM, but it does
    /// hand the game a few cycles it does not have on the console, and there are
    /// routines that count on that pause to line up with the screen's scan.
    ///
    /// A 16-byte block takes about 8 µs of real time **at both speeds**. Measured
    /// in CPU M-cycles that is 8 at single speed and 16 at double: the CPU runs
    /// twice as fast, but the bus moving the bytes does not, so twice as many
    /// M-cycles fit in the same span.
    ///
    /// Advancing the peripherals here is safe and does not recurse: the transfer
    /// is already finished, so the `step_hdma` of each M-cycle returns
    /// immediately.
    fn stall_for_gdma(&mut self, blocks: u32) {
        const M_CYCLES_PER_BLOCK: u32 = 8;

        let per_block = if self.double_speed { M_CYCLES_PER_BLOCK * 2 } else { M_CYCLES_PER_BLOCK };
        for _ in 0..blocks * per_block {
            self.tick_m_cycle();
        }
    }

    /// Index within WRAM for a memory-map address.
    ///
    /// The low half (`0xC000..0xD000`) is always bank 0. The high one is bank 1
    /// on DMG and whatever `SVBK` says on CGB.
    fn wram_index(&self, addr: u16) -> usize {
        let offset = (addr as usize) & 0x1FFF;
        if offset < WRAM_BANK_SIZE {
            offset
        } else {
            self.wram_bank * WRAM_BANK_SIZE + (offset - WRAM_BANK_SIZE)
        }
    }

    /// Copies one byte of the OAM transfer in progress, if there is one.
    fn step_dma(&mut self) {
        let Some(dma) = self.dma else { return };

        let value = self.read_raw(dma.source + dma.index);
        self.ppu.write_oam_dma(dma.index as usize, value);

        let index = dma.index + 1;
        self.dma = (index < DMA_LENGTH).then_some(DmaTransfer { source: dma.source, index });
    }

    /// Read that consumes no time, used by the DMA and by [`Bus::peek`].
    fn read_raw(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.cartridge.read_rom(addr),
            0x8000..=0x9FFF => self.ppu.peek_vram(addr),
            0xA000..=0xBFFF => self.cartridge.read_ram(addr),
            // Echo RAM: the hardware repeats WRAM at 0xE000. Nintendo forbade it
            // and several games use it anyway.
            0xC000..=0xFDFF => self.wram[self.wram_index(addr)],
            0xFE00..=0xFE9F => self.ppu.peek_oam(addr),
            0xFEA0..=0xFEFF => 0x00,
            0xFF00..=0xFF7F => self.read_io(addr),
            0xFF80..=0xFFFE => self.hram[addr as usize - 0xFF80],
            0xFFFF => self.interrupts.read_enable(),
        }
    }

    /// The I/O page.
    ///
    /// **Everything not implemented reads as `0xFF`**, not as zero, and that is
    /// not a cosmetic detail: a real DMG does not have the Game Boy Color
    /// exclusive registers, and software detects them by reading. With `KEY1`
    /// (0xFF4D) returning 0x00, Blargg's `cpu_instrs` suite believes it is on a
    /// CGB at single speed, tries to switch to double speed and runs `STOP`,
    /// which it never comes out of.
    fn read_io(&self, addr: u16) -> u8 {
        match addr {
            0xFF00 => self.joypad.read(),
            0xFF01 | 0xFF02 => self.serial.read(addr),
            0xFF04..=0xFF07 => self.timer.read(addr),
            0xFF0F => self.interrupts.read_flag(),
            0xFF10..=0xFF3F => self.apu.read(addr),
            0xFF40..=0xFF45 | 0xFF47..=0xFF4B => self.ppu.read_register(addr),
            0xFF46 => self.dma_source_high,
            0xFF4F | 0xFF68..=0xFF6C => self.ppu.read_register(addr),
            // From here on, everything is CGB.
            0xFF4D if self.model.is_cgb() => {
                // Bit 7: current speed. Bit 0: switch armed. The rest does not
                // exist. That this returns 0xFF on a DMG is what keeps software
                // from trying to switch on a console that cannot.
                0x7E | (u8::from(self.double_speed) << 7) | u8::from(self.speed_switch_armed)
            }
            0xFF55 if self.model.is_cgb() => self.hdma.read_status(),
            0xFF70 if self.model.is_cgb() => self.wram_bank as u8 | 0xF8,
            _ => OPEN_BUS,
        }
    }

    fn write_io(&mut self, addr: u16, value: u8) {
        match addr {
            0xFF00 => self.joypad.write(value),
            0xFF01 | 0xFF02 => self.serial.write(addr, value),
            0xFF04..=0xFF07 => self.timer.write(addr, value),
            0xFF0F => self.interrupts.write_flag(value),
            0xFF10..=0xFF3F => self.apu.write(addr, value),
            0xFF46 => {
                self.dma_source_high = value;
                // The written value is the high byte of the source address.
                self.dma = Some(DmaTransfer { source: u16::from(value) << 8, index: 0 });
            }
            0xFF40..=0xFF45 | 0xFF47..=0xFF4B => self.ppu.write_register(addr, value),
            0xFF4F | 0xFF68..=0xFF6C => self.ppu.write_register(addr, value),
            0xFF4D if self.model.is_cgb() => {
                // Writing here does not change the speed: it only arms it. The
                // switch happens when the CPU runs `STOP`.
                self.speed_switch_armed = value & 0x01 != 0;
            }
            0xFF51..=0xFF55 if self.model.is_cgb() => self.hdma.write_register(addr, value),
            0xFF70 if self.model.is_cgb() => {
                // Bank 0 is not selectable: it becomes bank 1, because 0 is
                // already fixed in the low half.
                self.wram_bank = usize::from(value & 0x07).max(1);
            }
            // Nonexistent registers: the write is lost, as in the hardware.
            // Storing it and returning it later would be pretending they exist.
            _ => {}
        }
    }
}

impl Bus for SystemBus {
    fn read(&mut self, addr: u16) -> u8 {
        // Time advances first: the CPU observes the system state *after* this
        // M-cycle has elapsed.
        self.tick_m_cycle();

        match addr {
            // VRAM and OAM are read through the PPU because they may be locked
            // depending on the mode it is in.
            0x8000..=0x9FFF => self.ppu.read_vram(addr),
            0xFE00..=0xFE9F => self.ppu.read_oam(addr),
            _ => self.read_raw(addr),
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.tick_m_cycle();

        match addr {
            // Writing to the ROM range configures the mapper, not the memory.
            0x0000..=0x7FFF => self.cartridge.write_rom(addr, value),
            0x8000..=0x9FFF => {
                self.log_write(addr, value, false);
                self.ppu.write_vram(addr, value)
            }
            0xA000..=0xBFFF => self.cartridge.write_ram(addr, value),
            0xC000..=0xFDFF => {
                let index = self.wram_index(addr);
                self.wram[index] = value;
            }
            0xFE00..=0xFE9F => self.ppu.write_oam(addr, value),
            0xFEA0..=0xFEFF => {}
            0xFF00..=0xFF7F => {
                self.log_write(addr, value, true);
                self.write_io(addr, value)
            }
            0xFF80..=0xFFFE => self.hram[addr as usize - 0xFF80] = value,
            0xFFFF => self.interrupts.write_enable(value),
        }
    }

    fn tick(&mut self) {
        self.tick_m_cycle();
    }

    fn interrupts(&self) -> &InterruptController {
        &self.interrupts
    }

    fn interrupts_mut(&mut self) -> &mut InterruptController {
        &mut self.interrupts
    }

    fn peek(&self, addr: u16) -> u8 {
        self.read_raw(addr)
    }

    fn perform_speed_switch(&mut self) -> bool {
        if !self.model.is_cgb() || !self.speed_switch_armed {
            return false;
        }
        self.double_speed = !self.double_speed;
        self.speed_switch_armed = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bus() -> SystemBus {
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0147] = 0x00; // ROM ONLY
        rom[0x0100] = 0x00;
        SystemBus::new(Cartridge::load(rom).unwrap(), Model::Dmg)
    }

    #[test]
    fn the_echo_ram_mirrors_the_wram() {
        let mut bus = test_bus();
        bus.write(0xC000, 0x42);
        assert_eq!(bus.read(0xE000), 0x42);
        bus.write(0xE100, 0x99);
        assert_eq!(bus.read(0xC100), 0x99);
    }

    #[test]
    fn every_access_consumes_one_m_cycle() {
        let mut bus = test_bus();
        let before = bus.t_cycles();
        bus.read(0xC000);
        assert_eq!(bus.t_cycles() - before, u64::from(crate::T_CYCLES_PER_M_CYCLE));
    }

    #[test]
    fn writing_to_rom_does_not_modify_the_rom() {
        let mut bus = test_bus();
        let before = bus.read(0x0100);
        bus.write(0x0100, 0xFF);
        assert_eq!(bus.read(0x0100), before);
    }

    #[test]
    fn the_dma_copies_160_bytes_to_oam() {
        let mut bus = test_bus();
        for i in 0..0xA0u16 {
            bus.write(0xC000 + i, i as u8);
        }
        // Turn the LCD off so OAM can be read without the lock.
        bus.write(0xFF40, 0x00);
        bus.write(0xFF46, 0xC0); // source 0xC000

        // The transfer takes 160 M-cycles.
        for _ in 0..0xA0 {
            bus.tick();
        }

        assert_eq!(bus.read(0xFE00), 0x00);
        assert_eq!(bus.read(0xFE9F), 0x9F);
    }

    /// Regression from `blargg/cpu_instrs`: the registers a DMG does not have
    /// must read as 0xFF. With 0x00, software believes it is on a CGB.
    #[test]
    fn nonexistent_registers_read_as_open_bus() {
        let mut bus = test_bus();
        // KEY1 (double speed), HDMA, colour palettes, SVBK: all CGB.
        for addr in [0xFF03, 0xFF4C, 0xFF4D, 0xFF51, 0xFF55, 0xFF68, 0xFF70, 0xFF7F] {
            assert_eq!(bus.read(addr), 0xFF, "0x{addr:04X} must be open bus");
            bus.write(addr, 0x00);
            assert_eq!(bus.read(addr), 0xFF, "0x{addr:04X} must not store anything");
        }
    }

    #[test]
    fn the_apu_is_connected_to_the_bus() {
        let mut bus = test_bus();
        assert_eq!(bus.read(0xFF26) & 0x80, 0, "the APU starts powered off");

        bus.write(0xFF26, 0x80);
        assert_eq!(bus.read(0xFF26) & 0x80, 0x80);
        bus.write(0xFF25, 0xF3);
        assert_eq!(bus.read(0xFF25), 0xF3, "NR51 reads back as written");
        // The wave RAM lives in the same range and really is memory.
        bus.write(0xFF30, 0xAB);
        assert_eq!(bus.read(0xFF30), 0xAB);
    }

    #[test]
    fn the_audio_sequencer_advances_with_the_timer() {
        let mut bus = test_bus();
        bus.write(0xFF26, 0x80); // APU powered on
        bus.write(0xFF12, 0xF0); // channel 1: DAC on
        bus.write(0xFF11, 0x3F); // length 63 → 1 step left
        bus.write(0xFF14, 0xC0); // trigger with length enabled
        assert_eq!(bus.read(0xFF26) & 0x01, 1);

        // One sequencer step is 8192 T-cycles = 2048 M-cycles.
        for _ in 0..2048 * 2 {
            bus.tick();
        }
        assert_eq!(bus.read(0xFF26) & 0x01, 0, "the length turned it off by itself");
    }

    // ---- Game Boy Color mode -----------------------------------------------

    fn cgb_bus() -> SystemBus {
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0143] = 0xC0; // CGB exclusive
        rom[0x0147] = 0x00;
        SystemBus::new(Cartridge::load(rom).unwrap(), Model::Cgb)
    }

    #[test]
    fn the_high_half_of_the_wram_is_switchable() {
        let mut bus = cgb_bus();
        for bank in 1..8u8 {
            bus.write(0xFF70, bank);
            bus.write(0xD000, bank);
        }
        for bank in 1..8u8 {
            bus.write(0xFF70, bank);
            assert_eq!(bus.read(0xD000), bank, "bank {bank} must be independent");
        }
    }

    #[test]
    fn the_low_half_of_the_wram_does_not_move() {
        let mut bus = cgb_bus();
        bus.write(0xC000, 0x42);
        bus.write(0xFF70, 5);
        assert_eq!(bus.read(0xC000), 0x42, "0xC000 is always bank 0");
    }

    #[test]
    fn wram_bank_zero_becomes_one() {
        let mut bus = cgb_bus();
        bus.write(0xFF70, 1);
        bus.write(0xD000, 0xAB);
        bus.write(0xFF70, 0);
        assert_eq!(bus.read(0xD000), 0xAB, "writing 0 selects bank 1");
        assert_eq!(bus.read(0xFF70) & 0x07, 1);
    }

    #[test]
    fn the_speed_switch_needs_arming_and_running_stop() {
        let mut bus = cgb_bus();
        assert!(!bus.double_speed());

        // Unarmed, `STOP` switches nothing.
        assert!(!bus.perform_speed_switch());

        bus.write(0xFF4D, 0x01);
        assert_eq!(bus.read(0xFF4D) & 0x81, 0x01, "the switch is left armed");
        assert!(bus.perform_speed_switch());
        assert!(bus.double_speed());
        assert_eq!(bus.read(0xFF4D) & 0x81, 0x80, "now it reports double speed");
    }

    #[test]
    fn at_double_speed_the_ppu_advances_half_as_much() {
        let mut bus = cgb_bus();
        let count_lines = |bus: &mut SystemBus| {
            let start = bus.ppu.read_register(0xFF44);
            for _ in 0..114 {
                bus.tick(); // 114 M-cycles = 456 T-cycles = one line
            }
            bus.ppu.read_register(0xFF44).wrapping_sub(start)
        };

        assert_eq!(count_lines(&mut bus), 1, "at single speed, one line");

        bus.write(0xFF4D, 0x01);
        bus.perform_speed_switch();
        assert_eq!(count_lines(&mut bus), 0, "at double, the PPU takes twice the M-cycles");
    }

    #[test]
    fn the_hdma_copies_from_wram_to_vram() {
        let mut bus = cgb_bus();
        for i in 0..0x20u16 {
            bus.write(0xC000 + i, i as u8);
        }
        bus.write(0xFF40, 0x00); // turn the LCD off so VRAM can be read

        // Two 16-byte blocks from 0xC000 to 0x8000, general mode.
        bus.write(0xFF51, 0xC0);
        bus.write(0xFF52, 0x00);
        bus.write(0xFF53, 0x00);
        bus.write(0xFF54, 0x00);
        bus.write(0xFF55, 0x01); // (1 + 1) blocks, bit 7 at 0 = immediate

        bus.tick();
        bus.tick();

        assert_eq!(bus.read(0x8000), 0x00);
        assert_eq!(bus.read(0x800F), 0x0F);
        assert_eq!(bus.read(0x801F), 0x1F, "the second block too");
        assert_eq!(bus.read(0xFF55), 0xFF, "and the transfer finished");
    }

    #[test]
    fn the_hdma_writes_into_the_selected_vram_bank() {
        let mut bus = cgb_bus();
        bus.write(0xC000, 0x99);
        bus.write(0xFF40, 0x00);
        bus.write(0xFF4F, 1); // VRAM bank 1

        bus.write(0xFF51, 0xC0);
        bus.write(0xFF52, 0x00);
        bus.write(0xFF53, 0x00);
        bus.write(0xFF54, 0x00);
        bus.write(0xFF55, 0x00);
        bus.tick();

        assert_eq!(bus.read(0x8000), 0x99);
        bus.write(0xFF4F, 0);
        assert_eq!(bus.read(0x8000), 0x00, "bank 0 was left untouched");
    }

    #[test]
    fn the_cgb_registers_do_not_exist_on_a_dmg() {
        let mut bus = test_bus();
        // KEY1, VBK, HDMA, palettes and SVBK: all must be open bus.
        for addr in [0xFF4D, 0xFF4F, 0xFF55, 0xFF68, 0xFF69, 0xFF6B, 0xFF70] {
            bus.write(addr, 0x01);
            assert_eq!(bus.read(addr), 0xFF, "0x{addr:04X} does not exist on a DMG");
        }
        assert!(!bus.perform_speed_switch(), "a DMG cannot switch speed");
    }

    #[test]
    fn ie_and_if_are_addressable() {
        let mut bus = test_bus();
        bus.write(0xFFFF, 0x1F);
        assert_eq!(bus.read(0xFFFF), 0x1F);
        bus.write(0xFF0F, 0x04);
        assert_eq!(bus.read(0xFF0F), 0xE4, "the high bits of IF read as 1");
    }
}
