//! MBC3: up to 2 MiB of ROM, 32 KiB of SRAM and a real-time clock.
//!
//! After the MBC1 it is a relief: the ROM bank is 7 straight bits, with no modes
//! or bits split across registers. What makes it special is the **RTC**, a clock
//! that on the real cartridge runs off its own crystal and a button cell, even
//! with the console powered off.
//!
//! | Write            | Effect                                              |
//! |------------------|-----------------------------------------------------|
//! | `0x0000..0x2000` | enables SRAM **and** the RTC registers               |
//! | `0x2000..0x4000` | ROM bank (7 bits; 0 is promoted to 1)                |
//! | `0x4000..0x6000` | 0x00-0x07: SRAM bank · 0x08-0x0C: RTC register       |
//! | `0x6000..0x8000` | latches the RTC on writing 0x00 and then 0x01        |
//!
//! # The latch
//!
//! The five RTC registers are not read directly: they have to be **latched**
//! first by writing `0x00` and then `0x01` into `0x6000..0x8000`. That copies
//! the live clock into a parallel set of registers, which is the one the CPU
//! sees.
//!
//! It is not a whim. Reading the five bytes takes several instructions, and
//! without latching one could read `23:59:59` and, three instructions later, a
//! day that has already rolled over: a 24-hour jump. The latch gives a coherent
//! reading.

use super::{ram::CartRam, Mapper, OPEN_BUS, RTC_SAVE_LEN};
use crate::cartridge::ROM_BANK_SIZE;

/// Codes in `0x4000..0x6000` that select an RTC register instead of an SRAM
/// bank.
const RTC_SELECT: core::ops::RangeInclusive<u8> = 0x08..=0x0C;

/// Days that fit in the counter before it overflows: 9 bits.
const MAX_DAYS: u16 = 0x1FF;

/// The five clock registers.
///
/// They are stored separately —and not as an instant— because the hardware
/// allows writing impossible values into them (`0x3B` seconds, for instance) and
/// the clock keeps counting from there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RtcRegisters {
    seconds: u8,
    minutes: u8,
    hours: u8,
    /// 9-bit day counter.
    days: u16,
    /// Bit 6 of `0x0C`: with the clock halted, time stops advancing.
    halted: bool,
    /// Bit 7 of `0x0C`: the day counter overflowed. **It is sticky**: it is only
    /// cleared by writing it by hand, so that a game can detect that more than
    /// 512 days went by even if it was not run in all that time.
    day_carry: bool,
}

impl RtcRegisters {
    fn advance_one_second(&mut self) {
        if self.halted {
            return;
        }
        self.seconds += 1;
        if self.seconds < 60 {
            return;
        }
        self.seconds = 0;

        self.minutes += 1;
        if self.minutes < 60 {
            return;
        }
        self.minutes = 0;

        self.hours += 1;
        if self.hours < 24 {
            return;
        }
        self.hours = 0;

        self.days += 1;
        if self.days > MAX_DAYS {
            self.days = 0;
            self.day_carry = true;
        }
    }

    /// Advances the clock by `seconds` seconds in one go.
    ///
    /// With arithmetic and not in a loop: when loading a save from a month ago
    /// this is millions of seconds, and counting them one by one would take
    /// longer than booting the game.
    fn advance(&mut self, seconds: u64) {
        if self.halted || seconds == 0 {
            return;
        }
        let total_s = u64::from(self.seconds) + seconds;
        self.seconds = (total_s % 60) as u8;

        let total_m = u64::from(self.minutes) + total_s / 60;
        self.minutes = (total_m % 60) as u8;

        let total_h = u64::from(self.hours) + total_m / 60;
        self.hours = (total_h % 24) as u8;

        let period = u64::from(MAX_DAYS) + 1;
        let total_d = u64::from(self.days) + total_h / 24;
        if total_d >= period {
            self.day_carry = true;
        }
        self.days = (total_d % period) as u16;
    }

    /// The five values that go into the `.sav`, in the format's order.
    fn to_save(self) -> [u32; 5] {
        [
            u32::from(self.seconds),
            u32::from(self.minutes),
            u32::from(self.hours),
            u32::from(self.days & 0xFF),
            // The fifth groups bit 8 of the days, the halt flag and the
            // overflow one, just like the 0x0C register the game sees.
            u32::from(self.read(0x0C)),
        ]
    }

    fn from_save(v: [u32; 5]) -> Self {
        let control = v[4] as u8;
        Self {
            seconds: v[0] as u8,
            minutes: v[1] as u8,
            hours: v[2] as u8,
            days: (v[3] as u16 & 0xFF) | (u16::from(control & 0x01) << 8),
            halted: control & 0x40 != 0,
            day_carry: control & 0x80 != 0,
        }
    }

    fn read(&self, register: u8) -> u8 {
        match register {
            0x08 => self.seconds,
            0x09 => self.minutes,
            0x0A => self.hours,
            0x0B => self.days as u8,
            0x0C => {
                (self.days >> 8) as u8 & 0x01
                    | (u8::from(self.halted) << 6)
                    | (u8::from(self.day_carry) << 7)
            }
            _ => OPEN_BUS,
        }
    }

    fn write(&mut self, register: u8, value: u8) {
        match register {
            0x08 => self.seconds = value & 0x3F,
            0x09 => self.minutes = value & 0x3F,
            0x0A => self.hours = value & 0x1F,
            0x0B => self.days = (self.days & 0x100) | u16::from(value),
            0x0C => {
                self.days = (self.days & 0x0FF) | (u16::from(value & 0x01) << 8);
                self.halted = value & 0x40 != 0;
                self.day_carry = value & 0x80 != 0;
            }
            _ => {}
        }
    }
}

/// The clock: the live registers, their latched copy and the cycle divider.
///
/// # Time source
///
/// The clock advances by counting **emulated T-cycles**, not by querying the
/// system clock. That is consistent with an I/O-free core, and it makes the
/// emulation reproducible: the same sequence of inputs always produces the same
/// result. The difference from the hardware is that here time stops when the
/// emulator is closed, whereas the real cartridge keeps counting.
struct Rtc {
    live: RtcRegisters,
    latched: RtcRegisters,
    /// T-cycles accumulated towards the next second.
    cycles: u32,
    /// `true` after writing `0x00` into the latch register, waiting for the
    /// `0x01` that completes the sequence.
    latch_armed: bool,
}

impl Rtc {
    const fn new() -> Self {
        Self {
            live: RtcRegisters {
                seconds: 0,
                minutes: 0,
                hours: 0,
                days: 0,
                halted: false,
                day_carry: false,
            },
            latched: RtcRegisters {
                seconds: 0,
                minutes: 0,
                hours: 0,
                days: 0,
                halted: false,
                day_carry: false,
            },
            cycles: 0,
            latch_armed: false,
        }
    }

    fn tick(&mut self, t_cycles: u32) {
        self.cycles += t_cycles;
        while self.cycles >= crate::CLOCK_HZ {
            self.cycles -= crate::CLOCK_HZ;
            self.live.advance_one_second();
        }
    }

    /// Handles a write to the latch register (`0x6000..0x8000`).
    fn write_latch(&mut self, value: u8) {
        if value == 0x01 && self.latch_armed {
            self.latched = self.live;
        }
        self.latch_armed = value == 0x00;
    }
}

pub struct Mbc3 {
    rom: Vec<u8>,
    ram: CartRam,
    rtc: Option<Rtc>,

    rom_bank_mask: u8,
    /// ROM bank active at `0x4000..0x8000`. It is never 0.
    rom_bank: u8,
    /// Last value written into `0x4000..0x6000`: SRAM bank or RTC register.
    ram_select: u8,
}

impl Mbc3 {
    pub fn new(mut rom: Vec<u8>, ram_size: usize, battery: bool, rtc: bool) -> Self {
        rom.resize(rom.len().max(2 * ROM_BANK_SIZE), OPEN_BUS);
        let banks = rom.len() / ROM_BANK_SIZE;
        Self {
            rom,
            ram: CartRam::new(ram_size, battery, false),
            rtc: rtc.then(Rtc::new),
            rom_bank_mask: (banks.next_power_of_two().max(2) - 1) as u8,
            rom_bank: 1,
            ram_select: 0,
        }
    }

    fn rom_byte(&self, bank: u8, offset: usize) -> u8 {
        let index = bank as usize * ROM_BANK_SIZE + offset;
        self.rom.get(index).copied().unwrap_or(OPEN_BUS)
    }

    /// `Some(register)` if `0x4000..0x6000` selected the RTC instead of SRAM.
    fn selected_rtc_register(&self) -> Option<u8> {
        (self.rtc.is_some() && RTC_SELECT.contains(&self.ram_select)).then_some(self.ram_select)
    }
}

impl Mapper for Mbc3 {
    fn read_rom(&self, addr: u16) -> u8 {
        match addr {
            // Unlike the MBC1, the low region is always bank 0.
            0x0000..=0x3FFF => self.rom_byte(0, addr as usize),
            0x4000..=0x7FFF => self.rom_byte(self.rom_bank, addr as usize - 0x4000),
            _ => OPEN_BUS,
        }
    }

    fn write_rom(&mut self, addr: u16, value: u8) {
        match addr {
            // The same register enables the SRAM and the RTC registers.
            0x0000..=0x1FFF => self.ram.set_enable_register(value),
            // 7 bits, and 0 is promoted to 1: bank 0 is already fixed below.
            0x2000..=0x3FFF => self.rom_bank = (value & 0x7F).max(1) & self.rom_bank_mask,
            0x4000..=0x5FFF => self.ram_select = value,
            0x6000..=0x7FFF => {
                if let Some(rtc) = &mut self.rtc {
                    rtc.write_latch(value);
                }
            }
            _ => {}
        }
    }

    fn read_ram(&self, addr: u16) -> u8 {
        if !self.ram.is_enabled() {
            return OPEN_BUS;
        }
        match self.selected_rtc_register() {
            // The latched copy is read, not the live clock.
            Some(reg) => self.rtc.as_ref().map_or(OPEN_BUS, |r| r.latched.read(reg)),
            None => self.ram.read(self.ram_select as usize, addr),
        }
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        if !self.ram.is_enabled() {
            return;
        }
        match self.selected_rtc_register() {
            Some(reg) => {
                if let Some(rtc) = &mut self.rtc {
                    // Writing adjusts the live clock; the latched copy is
                    // updated too so that an immediate re-read is coherent with
                    // what was just set.
                    rtc.live.write(reg, value);
                    rtc.latched.write(reg, value);
                    // Setting the clock resets the fraction of a second.
                    rtc.cycles = 0;
                }
            }
            None => self.ram.write(self.ram_select as usize, addr, value),
        }
    }

    fn tick(&mut self, t_cycles: u32) {
        if let Some(rtc) = &mut self.rtc {
            rtc.tick(t_cycles);
        }
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.ram.save()
    }

    fn load_save_ram(&mut self, data: &[u8]) -> bool {
        self.ram.load(data)
    }

    fn rtc_save(&self, now_unix: u64) -> Option<[u8; RTC_SAVE_LEN]> {
        let rtc = self.rtc.as_ref()?;

        let mut out = [0u8; RTC_SAVE_LEN];
        let mut write = |i: usize, v: u32| {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        };
        for (i, v) in rtc.live.to_save().into_iter().enumerate() {
            write(i, v);
        }
        for (i, v) in rtc.latched.to_save().into_iter().enumerate() {
            write(5 + i, v);
        }
        out[40..].copy_from_slice(&now_unix.to_le_bytes());
        Some(out)
    }

    fn rtc_load(&mut self, data: &[u8], now_unix: u64) -> bool {
        let Some(rtc) = &mut self.rtc else {
            return false;
        };
        if data.len() < RTC_SAVE_LEN {
            return false;
        }

        let read = |i: usize| {
            u32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]])
        };
        let five = |base: usize| [0, 1, 2, 3, 4].map(|i| read(base + i));
        rtc.live = RtcRegisters::from_save(five(0));
        rtc.latched = RtcRegisters::from_save(five(5));
        rtc.cycles = 0;

        // The cartridge clock runs with the console powered off, so whatever
        // elapsed since the save is added to it. A timestamp from the future —a
        // system clock running behind, or a `.sav` brought from another machine—
        // is ignored instead of subtracting time, which would leave the clock
        // worse off than it was.
        let saved = u64::from_le_bytes(data[40..48].try_into().unwrap());
        rtc.live.advance(now_unix.saturating_sub(saved));
        true
    }

    fn name(&self) -> &'static str {
        if self.rtc.is_some() {
            "MBC3+RTC"
        } else {
            "MBC3"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::mapper::ram::RAM_ENABLE_MAGIC;

    fn marked_rom(banks: usize) -> Vec<u8> {
        let mut rom = vec![0u8; banks * ROM_BANK_SIZE];
        for b in 0..banks {
            rom[b * ROM_BANK_SIZE] = b as u8;
        }
        rom
    }

    fn mbc3(rtc: bool) -> Mbc3 {
        let mut m = Mbc3::new(marked_rom(128), 32 * 1024, true, rtc);
        m.write_rom(0x0000, RAM_ENABLE_MAGIC);
        m
    }

    /// Advances the clock by `secs` emulated seconds.
    fn advance(m: &mut Mbc3, secs: u32) {
        for _ in 0..secs {
            m.tick(crate::CLOCK_HZ);
        }
    }

    /// Latches the clock and returns the five registers.
    fn read_rtc(m: &mut Mbc3) -> (u8, u8, u8, u8, u8) {
        m.write_rom(0x6000, 0x00);
        m.write_rom(0x6000, 0x01);
        let mut regs = [0u8; 5];
        for (i, reg) in (0x08..=0x0Cu8).enumerate() {
            m.write_rom(0x4000, reg);
            regs[i] = m.read_ram(0xA000);
        }
        (regs[0], regs[1], regs[2], regs[3], regs[4])
    }

    #[test]
    fn the_low_region_is_always_bank_zero() {
        let mut m = mbc3(false);
        m.write_rom(0x2000, 0x40);
        assert_eq!(m.read_rom(0x0000), 0, "the MBC3 does not move the low region");
        assert_eq!(m.read_rom(0x4000), 0x40);
    }

    #[test]
    fn bank_zero_is_promoted_to_one() {
        let mut m = mbc3(false);
        m.write_rom(0x2000, 0x00);
        assert_eq!(m.read_rom(0x4000), 1);
    }

    #[test]
    fn it_uses_seven_bank_bits() {
        let mut m = mbc3(false);
        m.write_rom(0x2000, 0x7F);
        assert_eq!(m.read_rom(0x4000), 0x7F, "127 addressable banks");
    }

    #[test]
    fn the_sram_banks_are_independent() {
        let mut m = mbc3(false);
        m.write_rom(0x4000, 0x00);
        m.write_ram(0xA000, 0x11);
        m.write_rom(0x4000, 0x02);
        m.write_ram(0xA000, 0x22);

        assert_eq!(m.read_ram(0xA000), 0x22);
        m.write_rom(0x4000, 0x00);
        assert_eq!(m.read_ram(0xA000), 0x11);
    }

    #[test]
    fn the_clock_advances_one_second_per_clock_cycle_count() {
        let mut m = mbc3(true);
        advance(&mut m, 1);
        assert_eq!(read_rtc(&mut m).0, 1);
    }

    #[test]
    fn the_clock_carries_from_seconds_to_minutes_to_hours() {
        let mut m = mbc3(true);
        advance(&mut m, 3661); // 1 h, 1 min, 1 s
        let (s, min, h, dl, _) = read_rtc(&mut m);
        assert_eq!((s, min, h, dl), (1, 1, 1, 0));
    }

    #[test]
    fn the_day_counter_uses_nine_bits() {
        let mut m = mbc3(true);
        // Set the clock to day 511 at 23:59:59.
        m.write_rom(0x4000, 0x0B);
        m.write_ram(0xA000, 0xFF);
        m.write_rom(0x4000, 0x0C);
        m.write_ram(0xA000, 0x01); // bit 8 of the days
        m.write_rom(0x4000, 0x0A);
        m.write_ram(0xA000, 23);
        m.write_rom(0x4000, 0x09);
        m.write_ram(0xA000, 59);
        m.write_rom(0x4000, 0x08);
        m.write_ram(0xA000, 59);

        advance(&mut m, 1);
        let (_, _, _, days_low, control) = read_rtc(&mut m);
        assert_eq!(days_low, 0, "the counter goes back to zero");
        assert_eq!(control & 0x01, 0);
        assert_eq!(control & 0x80, 0x80, "and it sets the overflow bit");
    }

    #[test]
    fn the_halt_bit_freezes_the_clock() {
        let mut m = mbc3(true);
        m.write_rom(0x4000, 0x0C);
        m.write_ram(0xA000, 0x40); // halt

        advance(&mut m, 100);
        assert_eq!(read_rtc(&mut m).0, 0, "with the clock halted no time passes");
    }

    #[test]
    fn without_latching_the_registers_do_not_change() {
        let mut m = mbc3(true);
        read_rtc(&mut m); // latches at 0

        advance(&mut m, 30);
        m.write_rom(0x4000, 0x08);
        assert_eq!(m.read_ram(0xA000), 0, "the latched copy does not move on its own");

        // Only the 0x00 → 0x01 sequence updates it.
        m.write_rom(0x6000, 0x00);
        m.write_rom(0x6000, 0x01);
        assert_eq!(m.read_ram(0xA000), 30);
    }

    #[test]
    fn an_incomplete_latch_sequence_does_nothing() {
        let mut m = mbc3(true);
        advance(&mut m, 5);
        m.write_rom(0x6000, 0x01); // without the preceding 0x00
        m.write_rom(0x4000, 0x08);
        assert_eq!(m.read_ram(0xA000), 0);
    }

    #[test]
    fn without_an_rtc_the_high_codes_fall_into_the_sram() {
        let mut m = mbc3(false);
        m.write_rom(0x4000, 0x08); // on a cartridge with RTC this would be a register
        m.write_ram(0xA000, 0x99);
        // 0x08 % 4 banks = bank 0.
        m.write_rom(0x4000, 0x00);
        assert_eq!(m.read_ram(0xA000), 0x99);
    }

    // ---- Clock persistence -------------------------------------------------

    /// Sets the clock to a specific time through the registers.
    fn set_time(m: &mut Mbc3, d: u16, h: u8, min: u8, s: u8) {
        m.write_rom(0x0000, RAM_ENABLE_MAGIC);
        for (reg, value) in
            [(0x08, s), (0x09, min), (0x0A, h), (0x0B, d as u8), (0x0C, (d >> 8) as u8)]
        {
            m.write_rom(0x4000, reg);
            m.write_ram(0xA000, value);
        }
    }

    /// Latches the clock, which is what the game does before reading it: the
    /// registers being read are the snapshot, not the live clock.
    fn latch(m: &mut Mbc3) {
        m.write_rom(0x6000, 0x00);
        m.write_rom(0x6000, 0x01);
    }

    fn read_time(m: &mut Mbc3) -> (u8, u8, u8, u8) {
        m.write_rom(0x0000, RAM_ENABLE_MAGIC);
        latch(m);
        let mut out = [0u8; 4];
        for (i, reg) in [0x0B, 0x0A, 0x09, 0x08].into_iter().enumerate() {
            m.write_rom(0x4000, reg);
            out[i] = m.read_ram(0xA000);
        }
        (out[0], out[1], out[2], out[3])
    }

    #[test]
    fn the_clock_survives_a_save_and_load() {
        let mut m = mbc3(true);
        set_time(&mut m, 300, 13, 45, 30);

        // Saved and loaded at the same instant: nothing must move.
        let saved = m.rtc_save(1_000_000).unwrap();
        let mut other = mbc3(true);
        assert!(other.rtc_load(&saved, 1_000_000));

        assert_eq!(read_time(&mut other), (300u16 as u8, 13, 45, 30));
    }

    #[test]
    fn the_clock_keeps_running_with_the_emulator_closed() {
        let mut m = mbc3(true);
        set_time(&mut m, 0, 0, 0, 0);
        let saved = m.rtc_save(1_000).unwrap();

        // A good hour and a half later: 1 h 30 min 15 s.
        let mut other = mbc3(true);
        assert!(other.rtc_load(&saved, 1_000 + 3600 + 30 * 60 + 15));

        assert_eq!(read_time(&mut other), (0, 1, 30, 15));
    }

    #[test]
    fn a_halted_clock_does_not_advance_while_closed() {
        let mut m = mbc3(true);
        set_time(&mut m, 0, 5, 0, 0);
        // Bit 6 of 0x0C: halt the clock.
        m.write_rom(0x4000, 0x0C);
        m.write_ram(0xA000, 0x40);

        let saved = m.rtc_save(0).unwrap();
        let mut other = mbc3(true);
        assert!(other.rtc_load(&saved, 100_000));

        assert_eq!(read_time(&mut other).1, 5, "halted means halted");
    }

    #[test]
    fn a_timestamp_from_the_future_does_not_set_the_clock_back() {
        // It happens with a system clock running behind or a .sav from another
        // machine.
        let mut m = mbc3(true);
        set_time(&mut m, 0, 8, 0, 0);
        let saved = m.rtc_save(500_000).unwrap();

        let mut other = mbc3(true);
        assert!(other.rtc_load(&saved, 1_000));
        assert_eq!(read_time(&mut other).1, 8, "no time is subtracted");
    }

    #[test]
    fn the_day_overflow_is_flagged_on_load() {
        let mut m = mbc3(true);
        set_time(&mut m, 511, 23, 59, 59); // the counter's last second
        let saved = m.rtc_save(0).unwrap();

        let mut other = mbc3(true);
        assert!(other.rtc_load(&saved, 1));

        other.write_rom(0x0000, RAM_ENABLE_MAGIC);
        latch(&mut other);
        other.write_rom(0x4000, 0x0C);
        assert_ne!(other.read_ram(0xA000) & 0x80, 0, "the overflow bit sticks");
    }

    #[test]
    fn a_cartridge_with_no_clock_saves_no_trailer() {
        assert!(mbc3(false).rtc_save(0).is_none());
        assert!(!mbc3(false).rtc_load(&[0u8; RTC_SAVE_LEN], 0));
    }

    #[test]
    fn a_short_trailer_is_rejected_instead_of_reading_garbage() {
        assert!(!mbc3(true).rtc_load(&[0u8; RTC_SAVE_LEN - 1], 0));
    }

    #[test]
    fn the_rtc_requires_the_ram_enable() {
        let mut m = Mbc3::new(marked_rom(4), 8 * 1024, true, true);
        m.write_rom(0x4000, 0x08);
        assert_eq!(m.read_ram(0xA000), OPEN_BUS, "without enabling, neither SRAM nor RTC");
    }
}
