//! The four memory movers, which are how anything large gets anywhere.
//!
//! # Why a game cannot simply copy it itself
//!
//! It can, and it would be several times slower. These channels move a
//! halfword or a word per cycle with no instructions fetched in between, and
//! the processor is stopped while they run — so a screen's worth of tiles
//! arrives in the gap at the bottom of a frame, which is the only time video
//! memory is free to receive it.
//!
//! That makes them not an optimisation but a prerequisite. A machine without
//! them does not run games slowly; it runs them wrongly, and the way it goes
//! wrong is misleading. Both cartridges tried here programmed a channel, waited
//! for the copy that never happened, and then jumped through a function pointer
//! that was still whatever had been in memory before — landing somewhere
//! arbitrary and faulting on a byte pattern that was never an instruction. The
//! symptom looked like a broken processor and was a missing mover.
//!
//! # Four channels, and why the order they run in is fixed
//!
//! Lower numbers win. It is not arbitrary: channel 0 is the one a game gives
//! work that must land inside a specific gap, channel 3 is the general-purpose
//! one, and the two in between feed sound. If a channel could be overtaken the
//! guarantee that makes 0 worth having would be gone.
//!
//! # The registers a game writes and the values a channel uses
//!
//! They are not the same, and the difference is the whole of the design. What a
//! game writes is a *setting*, kept until it is written again. What a transfer
//! walks is a copy taken when the channel was switched on. A channel set to
//! repeat runs at every gap without the game touching it again, and it can do
//! that only because the settings were never consumed.
//!
//! # What is not here
//!
//! The two start conditions that are not a blanking period: sound wants a
//! channel to run when a queue empties, and there is no sound; the video
//! capture mode wants one per line of an external feed there is no hardware
//! for. A channel asking for either is switched on and then never triggered,
//! which is at least a silence rather than a wrong picture.
//!
//! Nor do the transfers cost anything. On hardware the processor is stopped for
//! the duration, and here the whole copy happens between two instructions. The
//! shape a game can observe — that the bytes are there afterwards and not
//! before — is right, and the time it took is one more thing the clock does not
//! yet count.

use crate::interrupts::{Interrupts, Source};
use crate::ppu::Crossed;

/// The first byte of the four channels' registers, and the last.
pub const BASE: u32 = 0x0400_00B0;
pub const LAST: u32 = 0x0400_00DF;

/// Each channel has twelve bytes: two addresses, a count and a control.
const STRIDE: u32 = 12;

const COUNT_AT: u32 = 8;
const CONTROL_AT: u32 = 10;

/// Control bits. The two pairs say how each address moves between units.
const DEST_STEP: u16 = 0x0060;
const SOURCE_STEP: u16 = 0x0180;
const REPEAT: u16 = 1 << 9;
/// Set for words, clear for halfwords. There is no other width.
const WIDE: u16 = 1 << 10;
const START: u16 = 0x3000;
const IRQ_AT_END: u16 = 1 << 14;
const ENABLE: u16 = 1 << 15;

/// The four values the start field can hold.
const IMMEDIATELY: u16 = 0 << 12;
const AT_VBLANK: u16 = 1 << 12;
const AT_HBLANK: u16 = 2 << 12;

/// How an address moves after each unit.
const STEP_UP: u16 = 0;
const STEP_DOWN: u16 = 1;
const STEP_FIXED: u16 = 2;
/// Up, and back to where it started when the channel repeats. Destination only.
const STEP_UP_AND_RELOAD: u16 = 3;

/// One channel's settings and its place in whatever it is doing.
#[derive(Clone, Copy, Default)]
struct Channel {
    /// What the game wrote. Kept, because a repeating channel needs it again.
    source: u32,
    dest: u32,
    count: u16,
    control: u16,
    /// Where the transfer has reached. Taken from the settings when the channel
    /// is switched on, and walked from there.
    src_now: u32,
    dst_now: u32,
}

impl Channel {
    fn enabled(&self) -> bool {
        self.control & ENABLE != 0
    }

    fn repeats(&self) -> bool {
        self.control & REPEAT != 0
    }

    fn wide(&self) -> bool {
        self.control & WIDE != 0
    }

    fn start(&self) -> u16 {
        self.control & START
    }
}

/// The four of them.
#[derive(Clone, Default)]
pub struct Dma {
    channels: [Channel; 4],
    /// Which channels are ready to move something now, a bit each. A transfer
    /// is not performed inside the write that arms it: the game is midway
    /// through an instruction, and the hardware takes a couple of cycles to get
    /// going. It happens at the next tick of the clock.
    pending: u8,
}

impl Dma {
    pub fn new() -> Self {
        Self::default()
    }

    /// One byte of the four channels' registers.
    ///
    /// The two addresses do not read back — they are write-only on this
    /// hardware, and so is the count. Only the control halfword answers, which
    /// is how a game finds out that a transfer has finished: the enable bit
    /// clears itself.
    pub fn read8(&self, addr: u32) -> u8 {
        let (channel, offset) = locate(addr);
        match offset & !1 {
            CONTROL_AT => (self.channels[channel].control >> ((offset & 1) * 8)) as u8,
            _ => 0,
        }
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        let (index, offset) = locate(addr);
        let shift = (offset & 1) * 8;
        let byte = |existing: u32, at: u32| -> u32 {
            (existing & !(0xFFu32 << (at * 8))) | (u32::from(value) << (at * 8))
        };
        let half = |existing: u16| -> u16 {
            (existing & !(0xFFu16 << shift)) | (u16::from(value) << shift)
        };

        let channel = &mut self.channels[index];
        match offset {
            0..=3 => channel.source = byte(channel.source, offset),
            4..=7 => channel.dest = byte(channel.dest, offset - 4),
            COUNT_AT..CONTROL_AT => channel.count = half(channel.count),
            _ => {
                let was_on = channel.enabled();
                channel.control = half(channel.control);
                // Switching a channel on is the moment the settings are copied.
                // Writing the register again while it is already on changes what
                // the *next* run will do and leaves this one alone.
                if !was_on && channel.enabled() {
                    channel.src_now = channel.source & source_mask(index);
                    channel.dst_now = channel.dest & dest_mask(index);
                    if channel.start() == IMMEDIATELY {
                        self.pending |= 1 << index;
                    }
                }
            }
        }
    }

    /// Tells the channels a blanking period has begun, which is what most of
    /// them are waiting for.
    pub fn at_blanking(&mut self, crossed: Crossed) {
        for index in 0..4 {
            let channel = &mut self.channels[index];
            if !channel.enabled() {
                continue;
            }
            let due = match channel.start() {
                AT_VBLANK => crossed.vblank,
                AT_HBLANK => crossed.hblank,
                // Immediately, which already ran, or one of the two conditions
                // this machine cannot produce yet.
                _ => false,
            };
            if due {
                self.pending |= 1 << index;
            }
        }
    }

    /// The next channel with work to do, lowest number first.
    pub fn next_ready(&self) -> Option<usize> {
        (0..4).find(|index| self.pending & (1 << index) != 0)
    }

    /// What a ready channel is about to do, taken as one piece so the caller
    /// can move the bytes without holding a borrow on this.
    pub fn job(&self, index: usize) -> Job {
        let channel = &self.channels[index];
        let width = if channel.wide() { 4 } else { 2 };
        Job {
            source: channel.src_now,
            dest: channel.dst_now,
            units: units(channel.count, index),
            width,
            source_step: step(channel.control & SOURCE_STEP, 7, width),
            dest_step: step(channel.control & DEST_STEP, 5, width),
        }
    }

    /// Puts a finished transfer's addresses back and decides whether the
    /// channel stays on.
    pub fn finished(&mut self, index: usize, source: u32, dest: u32, irq: &mut Interrupts) {
        self.pending &= !(1 << index);
        let channel = &mut self.channels[index];
        channel.src_now = source;
        channel.dst_now = dest;

        if channel.control & IRQ_AT_END != 0 {
            irq.raise(SOURCES[index]);
        }

        // A channel started immediately has nothing left to wait for, so
        // repeating it would mean running for ever. Hardware treats the
        // combination as a one-off and so does this.
        if channel.repeats() && channel.start() != IMMEDIATELY {
            if channel.control & DEST_STEP == (STEP_UP_AND_RELOAD << 5) {
                channel.dst_now = channel.dest & dest_mask(index);
            }
        } else {
            // The bit clears itself, which is how a game learns it is done.
            channel.control &= !ENABLE;
        }
    }
}

/// A transfer, as a value: everything needed to move the bytes and nothing
/// that borrows the channel it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    pub source: u32,
    pub dest: u32,
    /// How many units, never zero.
    pub units: u32,
    /// Two bytes or four.
    pub width: u32,
    /// Added to each address after every unit. Wrapping, because a channel
    /// stepping down through zero is a thing a game may ask for.
    pub source_step: u32,
    pub dest_step: u32,
}

/// Which interrupt each channel raises when it is told to.
const SOURCES: [Source; 4] = [Source::Dma0, Source::Dma1, Source::Dma2, Source::Dma3];

/// Which channel an address belongs to, and how far into its twelve bytes.
fn locate(addr: u32) -> (usize, u32) {
    let offset = (addr - BASE) % (STRIDE * 4);
    ((offset / STRIDE) as usize, offset % STRIDE)
}

/// How far each address may reach. Channel 0 cannot see the cartridge at all —
/// it is the one meant for work that must not wait on slow memory — and only
/// channel 3 may write there.
fn source_mask(index: usize) -> u32 {
    if index == 0 { 0x07FF_FFFF } else { 0x0FFF_FFFF }
}

fn dest_mask(index: usize) -> u32 {
    if index == 3 { 0x0FFF_FFFF } else { 0x07FF_FFFF }
}

/// How many units a count means. Zero is not nothing: it is the most the field
/// can hold, plus one — which is the only way to ask for a whole 64 KiB.
fn units(count: u16, index: usize) -> u32 {
    let width = if index == 3 { 16 } else { 14 };
    let held = u32::from(count) & ((1 << width) - 1);
    if held == 0 { 1 << width } else { held }
}

/// What an address does between units, as the number to add to it.
fn step(field: u16, shift: u16, width: u32) -> u32 {
    match field >> shift {
        STEP_DOWN => width.wrapping_neg(),
        STEP_FIXED => 0,
        // Up, and up-and-reload, which differ only in what happens afterwards.
        STEP_UP | STEP_UP_AND_RELOAD => width,
        _ => unreachable!("a two-bit field has four values"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Channel 3's registers, which is the one a game uses for general work.
    const THREE: u32 = BASE + STRIDE * 3;

    fn arm(dma: &mut Dma, at: u32, source: u32, dest: u32, count: u16, control: u16) {
        for byte in 0..4 {
            dma.write8(at + byte, (source >> (byte * 8)) as u8);
            dma.write8(at + 4 + byte, (dest >> (byte * 8)) as u8);
        }
        dma.write8(at + COUNT_AT, count as u8);
        dma.write8(at + COUNT_AT + 1, (count >> 8) as u8);
        dma.write8(at + CONTROL_AT, control as u8);
        dma.write8(at + CONTROL_AT + 1, (control >> 8) as u8);
    }

    /// Each channel's twelve bytes, and no channel able to reach another's.
    #[test]
    fn the_four_channels_have_twelve_bytes_each_and_do_not_overlap() {
        for index in 0..4u32 {
            for offset in 0..STRIDE {
                let (channel, at) = locate(BASE + index * STRIDE + offset);
                assert_eq!(channel, index as usize, "0x{:08X}", BASE + index * STRIDE + offset);
                assert_eq!(at, offset);
            }
        }
        assert_eq!(BASE + STRIDE * 4 - 1, LAST, "and between them they fill the block");
    }

    /// Switching a channel on with nothing to wait for makes it ready at once.
    #[test]
    fn a_channel_told_to_go_now_is_ready_at_once() {
        let mut dma = Dma::new();
        assert_eq!(dma.next_ready(), None);

        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 16, ENABLE | WIDE);
        assert_eq!(dma.next_ready(), Some(3));

        let job = dma.job(3);
        assert_eq!(job.source, 0x0200_0000);
        assert_eq!(job.dest, 0x0600_0000);
        assert_eq!(job.units, 16);
        assert_eq!(job.width, 4);
        assert_eq!(job.source_step, 4, "both addresses climb by default");
        assert_eq!(job.dest_step, 4);
    }

    /// A channel waiting for a gap is not ready until the gap arrives, and then
    /// is ready for that gap only.
    #[test]
    fn a_channel_waiting_for_a_gap_goes_when_the_gap_comes() {
        let mut dma = Dma::new();
        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 8, ENABLE | AT_VBLANK);
        assert_eq!(dma.next_ready(), None, "not until the beam reaches the bottom");

        dma.at_blanking(Crossed { vblank: false, hblank: true });
        assert_eq!(dma.next_ready(), None, "and not for the wrong gap");

        dma.at_blanking(Crossed { vblank: true, hblank: false });
        assert_eq!(dma.next_ready(), Some(3));
    }

    /// Lower numbers go first, whatever order they became ready in. Channel 0
    /// exists precisely so that it cannot be overtaken.
    #[test]
    fn the_lowest_numbered_channel_ready_is_the_one_that_goes() {
        let mut dma = Dma::new();
        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE);
        arm(&mut dma, BASE, 0x0200_0000, 0x0600_0000, 4, ENABLE);
        arm(&mut dma, BASE + STRIDE * 2, 0x0200_0000, 0x0600_0000, 4, ENABLE);

        assert_eq!(dma.next_ready(), Some(0));
        let mut irq = Interrupts::new();
        dma.finished(0, 0, 0, &mut irq);
        assert_eq!(dma.next_ready(), Some(2));
        dma.finished(2, 0, 0, &mut irq);
        assert_eq!(dma.next_ready(), Some(3));
    }

    /// A count of zero is the largest transfer the field can ask for, not the
    /// smallest. It is the only way to move a whole 64 KiB in one go.
    #[test]
    fn a_count_of_zero_asks_for_the_most_and_not_the_least() {
        assert_eq!(units(0, 3), 0x1_0000, "the general-purpose channel's whole range");
        assert_eq!(units(0, 0), 0x4000, "and the others' smaller one");
        assert_eq!(units(1, 3), 1, "while one is one");

        // The narrower channels ignore the bits they do not have.
        assert_eq!(units(0x4001, 0), 1, "a count wider than the field is masked");
        assert_eq!(units(0x4001, 3), 0x4001, "and not on the wide one");
    }

    /// Each address can climb, fall, or stay put, and the amount is the width
    /// of a unit.
    #[test]
    fn an_address_can_climb_fall_or_stay_where_it_is() {
        let mut dma = Dma::new();

        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE | (STEP_DOWN << 7));
        assert_eq!(dma.job(3).source_step, 2u32.wrapping_neg(), "a halfword back");
        assert_eq!(dma.job(3).dest_step, 2);

        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE | WIDE | (STEP_FIXED << 5));
        assert_eq!(dma.job(3).dest_step, 0, "a destination that does not move");
        assert_eq!(dma.job(3).source_step, 4, "and a source that climbs by a word");
    }

    /// Reaching further than a channel is wired for gets the address folded,
    /// not refused: channel 0 cannot see the cartridge at all.
    #[test]
    fn a_channel_cannot_reach_past_what_it_is_wired_for() {
        let mut dma = Dma::new();
        arm(&mut dma, BASE, 0x0800_0000, 0x0600_0000, 4, ENABLE);
        assert_eq!(dma.job(0).source, 0, "the cartridge is out of channel 0's reach");

        arm(&mut dma, THREE, 0x0800_0000, 0x0600_0000, 4, ENABLE);
        assert_eq!(dma.job(3).source, 0x0800_0000, "and within channel 3's");
    }

    /// A one-off switches itself off when it is done, which is how a game that
    /// polls finds out. And it says so through the register, which is the only
    /// part of a channel that reads back.
    #[test]
    fn a_channel_that_does_not_repeat_switches_itself_off() {
        let mut dma = Dma::new();
        let mut irq = Interrupts::new();
        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE);
        assert_ne!(dma.read8(THREE + CONTROL_AT + 1) & 0x80, 0, "on");

        dma.finished(3, 0x0200_0008, 0x0600_0008, &mut irq);
        assert_eq!(dma.read8(THREE + CONTROL_AT + 1) & 0x80, 0, "and off again");
        assert_eq!(dma.next_ready(), None);
    }

    /// A repeating channel stays on and runs again at the next gap, without the
    /// game touching it.
    #[test]
    fn a_repeating_channel_runs_again_at_the_next_gap() {
        let mut dma = Dma::new();
        let mut irq = Interrupts::new();
        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE | REPEAT | AT_VBLANK);

        dma.at_blanking(Crossed { vblank: true, hblank: false });
        assert_eq!(dma.next_ready(), Some(3));
        dma.finished(3, 0x0200_0008, 0x0600_0008, &mut irq);
        assert_eq!(dma.next_ready(), None, "done for this frame");

        dma.at_blanking(Crossed { vblank: true, hblank: false });
        assert_eq!(dma.next_ready(), Some(3), "and again the next");
        assert_eq!(dma.job(3).source, 0x0200_0008, "carrying on where it left off");
    }

    /// The one destination setting that reloads: it is how a sound queue or a
    /// scroll table is refilled from the top every gap.
    #[test]
    fn the_reloading_destination_goes_back_to_where_it_started() {
        let mut dma = Dma::new();
        let mut irq = Interrupts::new();
        let control = ENABLE | REPEAT | AT_VBLANK | (STEP_UP_AND_RELOAD << 5);
        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, control);

        dma.at_blanking(Crossed { vblank: true, hblank: false });
        dma.finished(3, 0x0200_0008, 0x0600_0008, &mut irq);

        dma.at_blanking(Crossed { vblank: true, hblank: false });
        assert_eq!(dma.job(3).dest, 0x0600_0000, "the destination went back");
        assert_eq!(dma.job(3).source, 0x0200_0008, "and the source did not");
    }

    /// Repeating with nothing to wait for would be a channel that never stops,
    /// so the combination is treated as a one-off.
    #[test]
    fn repeating_with_nothing_to_wait_for_is_a_one_off() {
        let mut dma = Dma::new();
        let mut irq = Interrupts::new();
        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE | REPEAT | IMMEDIATELY);

        dma.finished(3, 0x0200_0008, 0x0600_0008, &mut irq);
        assert_eq!(dma.read8(THREE + CONTROL_AT + 1) & 0x80, 0, "off, not looping for ever");
    }

    /// A channel can ask to be told when it is done.
    #[test]
    fn a_channel_can_raise_an_interrupt_when_it_finishes() {
        let mut dma = Dma::new();
        let mut irq = Interrupts::new();

        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE);
        dma.finished(3, 0, 0, &mut irq);
        assert_eq!(irq.requested(), 0, "not unless it was asked for");

        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE | IRQ_AT_END);
        dma.finished(3, 0, 0, &mut irq);
        assert_eq!(irq.requested(), Source::Dma3.bit(), "and each channel has its own");
    }

    /// Writing the register of a channel that is already on changes what the
    /// next run does and leaves this one alone. The settings are never
    /// consumed, which is what makes repeating possible at all.
    #[test]
    fn rewriting_a_running_channel_does_not_restart_it() {
        let mut dma = Dma::new();
        arm(&mut dma, THREE, 0x0200_0000, 0x0600_0000, 4, ENABLE | AT_VBLANK);
        dma.at_blanking(Crossed { vblank: true, hblank: false });
        assert_eq!(dma.job(3).source, 0x0200_0000);

        // The enable bit was already set, so this is not a switch-on.
        dma.write8(THREE, 0xFF);
        assert_eq!(dma.job(3).source, 0x0200_0000, "the running transfer is untouched");
    }

    /// The addresses and the count are write-only, and only the control
    /// halfword answers a read. A game that read the count back would be
    /// reading a number this hardware does not report.
    #[test]
    fn only_the_control_register_reads_back() {
        let mut dma = Dma::new();
        arm(&mut dma, THREE, 0x0201_0203, 0x0604_0506, 0x1234, ENABLE | WIDE);

        for offset in 0..COUNT_AT + 2 {
            assert_eq!(dma.read8(THREE + offset), 0, "offset {offset} does not read back");
        }
        assert_eq!(dma.read8(THREE + CONTROL_AT), (ENABLE | WIDE) as u8);
        assert_eq!(dma.read8(THREE + CONTROL_AT + 1), ((ENABLE | WIDE) >> 8) as u8);
    }
}
