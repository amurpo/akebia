//! The four counters.
//!
//! # What they are for
//!
//! A timer is the only clock a game has that is not the beam. The beam gives it
//! sixty ticks a second and nothing finer, and plenty of things need finer:
//! a sound sample every few hundred cycles, a fade that moves on its own
//! schedule, a stopwatch that keeps running while the screen does something
//! else. A timer counts machine cycles, divided down, and interrupts when it
//! comes round.
//!
//! # Counting up rather than down, and why the reload is the interesting number
//!
//! The counter only ever runs up to `0xFFFF` and wraps. That means the way to
//! ask for an interrupt every *N* cycles is to start at `0x10000 - N` — so the
//! useful register is not the count but the **reload**, the value the counter
//! goes back to when it comes round. It is also why the two are different
//! numbers at the same address: reading gives the count, writing sets the
//! reload, and a game that expected to read back what it wrote would be reading
//! a counter that has moved on since.
//!
//! # Cascading
//!
//! A timer can be told to count the one below it coming round instead of
//! counting cycles. Four of them chained that way is a 64-bit counter, which is
//! how a game measures something longer than the 65536 ticks one of them holds.
//! Timer 0 has nothing below it, so it cannot do this, and the hardware ignores
//! the bit rather than doing something surprising with it.
//!
//! # What is deliberately not here
//!
//! A timer on the hardware does not start counting for two cycles after it is
//! switched on. Nothing here charges those two cycles, and it is written down
//! rather than pretended away — with the clock this emulator keeps, a two-cycle
//! error is far below the error already there.

use crate::interrupts::{Interrupts, Source};

/// The four of them, two registers each.
pub const BASE: u32 = 0x0400_0100;
pub const LAST: u32 = 0x0400_010F;
const STRIDE: u32 = 4;
pub const COUNT: usize = 4;

/// The control register.
const PRESCALER: u16 = 0x0003;
const CASCADES: u16 = 1 << 2;
const IRQ_ENABLED: u16 = 1 << 6;
const ENABLED: u16 = 1 << 7;
/// What a game can read back. The rest of the halfword is not wired to
/// anything.
const CONTROL_USED: u16 = PRESCALER | CASCADES | IRQ_ENABLED | ENABLED;

/// How many machine cycles one tick of the counter costs, by prescaler.
///
/// One, or a division. The three divisions are what let a 16-bit counter
/// measure something longer than four milliseconds.
const PERIODS: [u32; 4] = [1, 64, 256, 1024];

/// The interrupt each timer raises.
const SOURCES: [Source; COUNT] = [Source::Timer0, Source::Timer1, Source::Timer2, Source::Timer3];

/// One counter.
#[derive(Clone, Copy, Default)]
struct Timer {
    /// What the counter goes back to when it comes round — and what a write to
    /// the low register sets, which is not what a read of it gives.
    reload: u16,
    counter: u16,
    control: u16,
    /// Cycles counted that were not yet enough to move the counter on. Without
    /// this a prescaler of 1024 fed one cycle at a time would never move at
    /// all, since every tick would divide to zero and the remainder be thrown
    /// away.
    spare: u32,
}

impl Timer {
    const fn enabled(self) -> bool {
        self.control & ENABLED != 0
    }

    const fn period(self) -> u32 {
        PERIODS[(self.control & PRESCALER) as usize]
    }

    /// Whether this one counts the timer below it coming round rather than
    /// counting cycles. Timer 0 has nothing below it and never does.
    const fn cascades(self, index: usize) -> bool {
        index > 0 && self.control & CASCADES != 0
    }

    /// Moves the counter on, and says how many times it came round.
    ///
    /// The count is worked out rather than stepped, because the number of steps
    /// is unbounded — a caller may hand over a whole frame at once — and
    /// looping would make the cost of a tick depend on how long since the last
    /// one.
    fn advance(&mut self, steps: u32) -> u32 {
        if steps == 0 {
            return 0;
        }
        // How many steps this counter has left before it comes round.
        let room = u32::from(u16::MAX - self.counter) + 1;
        if steps < room {
            self.counter += steps as u16;
            return 0;
        }
        // The first time round costs `room`; every time after that costs a
        // whole span from the reload, which is why a reload of 0xFFFF comes
        // round on every single step.
        let span = u32::from(u16::MAX - self.reload) + 1;
        let after = steps - room;
        self.counter = self.reload.wrapping_add((after % span) as u16);
        1 + after / span
    }
}

/// All four.
#[derive(Default)]
pub struct Timers {
    channels: [Timer; COUNT],
}

impl Timers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves every running counter on by that many machine cycles.
    ///
    /// In order, and that matters: a cascading timer counts the one below it
    /// coming round, so the one below has to have been moved first. Going the
    /// other way would delay every cascade by a tick.
    pub fn tick(&mut self, cycles: u32, irq: &mut Interrupts) {
        let mut from_below = 0;
        for (index, (timer, source)) in self.channels.iter_mut().zip(SOURCES).enumerate() {
            if !timer.enabled() {
                from_below = 0;
                continue;
            }

            let steps = if timer.cascades(index) {
                from_below
            } else {
                // The spare cycles are what makes a slow prescaler work when
                // the clock arrives one cycle at a time.
                timer.spare += cycles;
                let period = timer.period();
                let steps = timer.spare / period;
                timer.spare %= period;
                steps
            };

            let came_round = timer.advance(steps);
            if came_round > 0 && timer.control & IRQ_ENABLED != 0 {
                irq.raise(source);
            }
            from_below = came_round;
        }
    }

    pub fn read8(&self, addr: u32) -> u8 {
        let index = ((addr - BASE) / STRIDE) as usize;
        let timer = self.channels[index & 3];
        // Reading the low register gives the count, not the reload that was
        // written there. They are two registers sharing an address.
        let value = if addr & 2 == 0 { timer.counter } else { timer.control };
        (value >> ((addr & 1) * 8)) as u8
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        let index = (((addr - BASE) / STRIDE) as usize) & 3;
        let timer = &mut self.channels[index];
        let shift = (addr & 1) * 8;
        let widened =
            |existing: u16| (existing & !(0xFFu16 << shift)) | (u16::from(value) << shift);

        if addr & 2 == 0 {
            // Writing the low register sets the reload and leaves the counter
            // alone. The counter takes it at the next time round — or at once,
            // if this write is the one that switches the timer on.
            timer.reload = widened(timer.reload);
            return;
        }

        let was_enabled = timer.enabled();
        timer.control = widened(timer.control) & CONTROL_USED;
        // Switching a timer on loads the counter from the reload and starts the
        // division afresh. Only on the *change*: a game that rewrites the
        // control register of a running timer — to change the prescaler, say —
        // does not restart it, and treating every write as a start would reset
        // a counter that was in the middle of something.
        if !was_enabled && timer.enabled() {
            timer.counter = timer.reload;
            timer.spare = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reload that makes a timer come round every `n` ticks.
    ///
    /// The counter runs up and wraps, so asking for an interrupt every `n` is
    /// asking it to start `n` short of the top. This is the arithmetic every
    /// game does, and writing it out is what keeps the tests about the timer
    /// rather than about hexadecimal.
    const fn every(n: u32) -> u16 {
        (0x1_0000 - n) as u16
    }

    /// A control value: prescaler, and whichever flags.
    const fn control(prescaler: u16, flags: u16) -> u16 {
        prescaler | flags
    }

    fn write16(timers: &mut Timers, addr: u32, value: u16) {
        timers.write8(addr, value as u8);
        timers.write8(addr + 1, (value >> 8) as u8);
    }

    fn read16(timers: &Timers, addr: u32) -> u16 {
        u16::from(timers.read8(addr)) | (u16::from(timers.read8(addr + 1)) << 8)
    }

    /// The address of one timer's pair.
    const fn at(index: u32) -> u32 {
        BASE + index * STRIDE
    }

    fn tick(timers: &mut Timers, cycles: u32) -> Interrupts {
        let mut irq = Interrupts::new();
        timers.tick(cycles, &mut irq);
        irq
    }

    #[test]
    fn a_timer_that_was_never_switched_on_does_not_count() {
        let mut timers = Timers::new();
        tick(&mut timers, 10_000);
        assert_eq!(read16(&timers, at(0)), 0);
    }

    #[test]
    fn a_running_timer_counts_one_for_every_cycle() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0) + 2, control(0, ENABLED));

        tick(&mut timers, 5);
        assert_eq!(read16(&timers, at(0)), 5);

        tick(&mut timers, 3);
        assert_eq!(read16(&timers, at(0)), 8, "and picks up where it left off");
    }

    /// Switching it on is what loads the counter. Until then the reload is just
    /// a number sitting in a register.
    #[test]
    fn switching_a_timer_on_loads_the_counter_from_the_reload() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), 0xFF00);
        assert_eq!(read16(&timers, at(0)), 0, "the count has not been touched");

        write16(&mut timers, at(0) + 2, control(0, ENABLED));
        assert_eq!(read16(&timers, at(0)), 0xFF00, "and now it has");
    }

    /// The two registers at one address. A game that read back what it wrote
    /// would be reading a counter that has moved on.
    #[test]
    fn the_low_register_is_written_as_a_reload_and_read_as_a_count() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0) + 2, control(0, ENABLED));
        tick(&mut timers, 100);

        write16(&mut timers, at(0), 0x1234);
        assert_eq!(read16(&timers, at(0)), 100, "still the count, not the reload");

        // And the reload it was given is what it comes back to.
        tick(&mut timers, 0x1_0000 - 100);
        assert_eq!(read16(&timers, at(0)), 0x1234);
    }

    /// Rewriting the control register of a running timer must not restart it.
    /// A game changes the prescaler of a timer that is in the middle of
    /// something, and a reset counter there would be a silent loss of time.
    #[test]
    fn writing_the_control_of_a_running_timer_does_not_restart_it() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), 0xFF00);
        write16(&mut timers, at(0) + 2, control(0, ENABLED));
        tick(&mut timers, 5);
        assert_eq!(read16(&timers, at(0)), 0xFF05);

        // Still enabled, now with the interrupt asked for as well.
        write16(&mut timers, at(0) + 2, control(0, ENABLED | IRQ_ENABLED));
        assert_eq!(read16(&timers, at(0)), 0xFF05, "the count was not reloaded");
    }

    /// Coming round is what a timer is for, and the interrupt only goes out if
    /// it was asked for.
    #[test]
    fn coming_round_reloads_and_interrupts_if_asked() {
        let mut timers = Timers::new();
        // Sixteen cycles from the top.
        write16(&mut timers, at(0), every(16));
        write16(&mut timers, at(0) + 2, control(0, ENABLED | IRQ_ENABLED));

        let irq = tick(&mut timers, 15);
        assert_eq!(irq.requested(), 0, "one short");
        assert_eq!(read16(&timers, at(0)), 0xFFFF);

        let irq = tick(&mut timers, 1);
        assert_ne!(irq.requested(), 0, "and there it goes");
        assert_eq!(read16(&timers, at(0)), every(16), "back to the reload");
    }

    #[test]
    fn coming_round_without_asking_raises_nothing() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), 0xFFFF);
        write16(&mut timers, at(0) + 2, control(0, ENABLED));

        let irq = tick(&mut timers, 4);
        assert_eq!(irq.requested(), 0, "it came round four times and said nothing");
        assert_eq!(read16(&timers, at(0)), 0xFFFF);
    }

    /// The whole point of the prescalers: a 16-bit counter that ticks every
    /// cycle covers four milliseconds, and one that ticks every 1024 covers
    /// four seconds.
    #[test]
    fn the_prescalers_divide_the_clock() {
        for (prescaler, period) in PERIODS.iter().enumerate() {
            let mut timers = Timers::new();
            write16(&mut timers, at(0) + 2, control(prescaler as u16, ENABLED));

            tick(&mut timers, period - 1);
            assert_eq!(read16(&timers, at(0)), 0, "prescaler {prescaler}: not yet");

            tick(&mut timers, 1);
            assert_eq!(read16(&timers, at(0)), 1, "prescaler {prescaler}: one tick");
        }
    }

    /// And the division has to survive being fed one cycle at a time, which is
    /// how the clock actually arrives. Throwing away the remainder of each
    /// division would leave a slow timer stopped for ever.
    #[test]
    fn a_divided_timer_still_counts_when_the_clock_arrives_a_cycle_at_a_time() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0) + 2, control(3, ENABLED));
        for _ in 0..1024 {
            tick(&mut timers, 1);
        }
        assert_eq!(read16(&timers, at(0)), 1);
    }

    /// A whole frame handed over at once must land where the same cycles handed
    /// over singly would. The count is worked out rather than stepped, and this
    /// is what says the arithmetic agrees with the loop it replaced.
    #[test]
    fn one_big_tick_lands_where_many_small_ones_would() {
        for prescaler in 0..4 {
            let (mut all_at_once, mut one_by_one) = (Timers::new(), Timers::new());
            for timers in [&mut all_at_once, &mut one_by_one] {
                write16(timers, at(0), 0xFF00);
                write16(timers, at(0) + 2, control(prescaler, ENABLED));
            }

            tick(&mut all_at_once, 5000);
            for _ in 0..5000 {
                tick(&mut one_by_one, 1);
            }
            assert_eq!(
                read16(&all_at_once, at(0)),
                read16(&one_by_one, at(0)),
                "prescaler {prescaler}"
            );
        }
    }

    /// Many times round in one tick, which is what a reload near the top of the
    /// range does. Counting one interrupt where several were due would drift.
    #[test]
    fn a_tick_long_enough_to_come_round_twice_says_so() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), every(10));
        write16(&mut timers, at(0) + 2, control(0, ENABLED | IRQ_ENABLED));

        // Twenty-five cycles is two whole times round and five over.
        let irq = tick(&mut timers, 25);
        assert_ne!(irq.requested(), 0);
        assert_eq!(read16(&timers, at(0)), every(10) + 5);
    }

    /// The chain: a timer counting the one below it coming round, which is how
    /// a game measures something longer than one of them holds.
    #[test]
    fn a_cascading_timer_counts_the_one_below_it_coming_round() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), 0xFFFF);
        write16(&mut timers, at(0) + 2, control(0, ENABLED));
        write16(&mut timers, at(1) + 2, control(0, ENABLED | CASCADES));

        tick(&mut timers, 3);
        assert_eq!(read16(&timers, at(1)), 3, "three times round below is three here");
        // And it is not counting cycles of its own on top of that.
        assert_eq!(read16(&timers, at(0)), 0xFFFF);
    }

    /// A cascading timer ignores its own prescaler: what it counts is the timer
    /// below, and dividing that would be dividing the wrong thing.
    #[test]
    fn a_cascading_timer_ignores_its_prescaler() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), 0xFFFF);
        write16(&mut timers, at(0) + 2, control(0, ENABLED));
        write16(&mut timers, at(1) + 2, control(3, ENABLED | CASCADES));

        tick(&mut timers, 2);
        assert_eq!(read16(&timers, at(1)), 2);
    }

    /// Timer 0 has nothing below it. The hardware ignores the bit rather than
    /// doing something surprising, and so a timer 0 with it set still counts
    /// cycles.
    #[test]
    fn the_first_timer_cannot_cascade_and_counts_cycles_regardless() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0) + 2, control(0, ENABLED | CASCADES));
        tick(&mut timers, 7);
        assert_eq!(read16(&timers, at(0)), 7);
    }

    /// A chain is only a chain while every link is running. A stopped timer
    /// passes nothing on, and the one above it must not carry on from whatever
    /// the last link happened to report.
    #[test]
    fn a_stopped_timer_passes_nothing_up_the_chain() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), 0xFFFF);
        write16(&mut timers, at(0) + 2, control(0, ENABLED));
        // Timer 1 is off; timer 2 cascades from it.
        write16(&mut timers, at(2) + 2, control(0, ENABLED | CASCADES));

        tick(&mut timers, 5);
        assert_eq!(read16(&timers, at(2)), 0, "nothing came through the gap");
    }

    /// Four of them chained is a 64-bit counter, which is the arrangement the
    /// cascade exists for.
    #[test]
    fn all_four_can_be_chained_together() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0), 0xFFFF);
        write16(&mut timers, at(0) + 2, control(0, ENABLED));
        for index in 1..4 {
            write16(&mut timers, at(index), 0xFFFF);
            write16(&mut timers, at(index) + 2, control(0, ENABLED | CASCADES));
        }

        // Every step comes round at every level, so the top moves once for
        // every cycle that reaches the bottom.
        tick(&mut timers, 4);
        assert_eq!(read16(&timers, at(3)), 0xFFFF, "the top came round too");
    }

    #[test]
    fn the_control_register_reads_back_only_the_bits_that_exist() {
        let mut timers = Timers::new();
        write16(&mut timers, at(0) + 2, 0xFFFF);
        assert_eq!(read16(&timers, at(0) + 2), CONTROL_USED);
    }

    /// Each of the four is its own timer, at its own address, with its own
    /// interrupt.
    #[test]
    fn the_four_are_separate() {
        for index in 0..COUNT as u32 {
            let mut timers = Timers::new();
            write16(&mut timers, at(index), 0xFFFF);
            write16(&mut timers, at(index) + 2, control(0, ENABLED | IRQ_ENABLED));

            let irq = tick(&mut timers, 1);
            assert_eq!(irq.requested(), 1 << (SOURCES[index as usize] as u16), "timer {index}");

            for other in 0..COUNT as u32 {
                if other != index {
                    assert_eq!(read16(&timers, at(other)), 0, "timer {other} stayed put");
                }
            }
        }
    }
}
