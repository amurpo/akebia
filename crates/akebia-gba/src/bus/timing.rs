//! What an access costs, which is not the same anywhere twice.
//!
//! # Why the clock could not simply count instructions
//!
//! Until now every step of the processor charged one cycle, which was a floor
//! and not a measurement: it said so where it was written. The effect is that
//! this machine ran between two and four times as much code per line of the
//! screen as the hardware does — which is visible as an effect landing at the
//! wrong moment, and expensive, because every one of those cycles drags the
//! whole machine forward behind it.
//!
//! An instruction's real cost is almost entirely the memory it touches, and on
//! this machine the memories differ by a factor of six. Internal RAM answers in
//! one cycle on a 32-bit bus; external RAM takes three and is half as wide, so a
//! word out of it costs six; the cartridge is slower still and **the game
//! chooses how much slower** at runtime. That is why a game copies the code it
//! cares about into internal RAM before running it, and why an emulator that
//! charges one cycle for everything cannot tell the difference between the two.
//!
//! # Sequential and non-sequential
//!
//! An access that follows on from the last one is cheaper than one that jumps.
//! The cartridge and external RAM keep a row open and reading the next address
//! along costs less than opening another; the hardware calls the two S and N.
//! It is why straight-line code is faster than the same code with a branch in
//! it, and it is not a detail here: on a cartridge at its default settings the
//! two differ by nearly a factor of two.
//!
//! Whether an access is sequential is not something the processor announces. It
//! is a property of the address: the one after the last one, at the same width.
//! So it is worked out here rather than passed in, which also means the DMA gets
//! it right without being taught anything.
//!
//! # What is charged and what is not
//!
//! The processor's accesses are. The cycles an instruction spends on its own
//! internal work — the one a load takes to write its register back, the several
//! a multiply takes — are not, yet: they are one or two cycles against a memory
//! access that costs three to six, and adding them means teaching every
//! instruction to report, which is a change of a different size.
//!
//! Nor are the memory movers'. A transfer stops the processor for as long as it
//! takes and ought to cost that, but it runs from inside the clock's own tick,
//! so its cycles cannot move a clock that is already moving. Charging them to
//! the next instruction instead would jump the beam a whole transfer's worth in
//! one go — past the interrupt the game is waiting on. That wants the clock
//! driven by something other than the processor, which is the next piece of
//! work.
//!
//! The clock is short by both, and the direction is known.

/// The register a game sets its cartridge's speed with.
pub const WAITCNT: u32 = 0x0400_0204;

/// How many cycles each region answers in, and how wide its bus is.
///
/// The width matters as much as the count: a 16-bit bus serving a 32-bit read
/// does it twice, so external RAM costs six cycles for a word and three for a
/// halfword. Nothing else here is as slow, and it is where a game puts what it
/// does not mind being slow.
struct Region {
    /// Cycles for an access the bus can do in one go.
    cycles: u32,
    /// Whether a 32-bit access has to be done as two.
    narrow: bool,
}

/// `WAITCNT`'s prefetch switch.
///
/// With it on, the cartridge reads ahead of the processor whenever the bus is
/// idle, so code running straight through comes out of a buffer that is already
/// full instead of out of the chip. Games switch it on and leave it on, because
/// it is most of what makes running code from a cartridge bearable at all.
const PREFETCH: u16 = 1 << 14;

/// What a prefetched access costs: the buffer is already holding it.
const PREFETCHED: u32 = 1;

/// The `WAITCNT` settings a cartridge access is charged by.
///
/// The first-access counts are the four the register offers; the following-on
/// count is one of two. Both are *extra* cycles on top of the one every access
/// costs, which is why every table here is read as "one plus".
const FIRST: [u32; 4] = [4, 3, 2, 8];
const FOLLOWING: [[u32; 2]; 3] = [[2, 1], [4, 1], [8, 1]];

/// Which of the three cartridge windows an address is in. They are the same
/// chip and differ only in how long it takes to read, which is the whole reason
/// a game reads its code through one and its data through another.
fn window(addr: u32) -> usize {
    ((addr >> 25) & 3) as usize % 3
}

/// What one access costs, in cycles.
///
/// `wide` is a 32-bit access; `sequential` says it follows straight on from the
/// last one.
pub fn access(addr: u32, wide: bool, sequential: bool, waitcnt: u16) -> u32 {
    let region = match addr >> 24 {
        // The BIOS, both internal memories and the registers all answer in one
        // cycle on a full-width bus.
        0x00 | 0x03 | 0x04 | 0x07 => Region { cycles: 1, narrow: false },
        // External RAM: three cycles, and half as wide.
        0x02 => Region { cycles: 3, narrow: true },
        // Palette and video memory answer at once but are half as wide. The
        // picture unit also takes the bus away while it is drawing, which is
        // not modelled: it would cost a game a few cycles a line and nothing
        // here can see the difference yet.
        0x05 | 0x06 => Region { cycles: 1, narrow: true },
        0x08..=0x0D => return cartridge(addr, wide, sequential, waitcnt),
        // Save memory is on an eight-bit bus and always a first access: there
        // is no row to keep open.
        0x0E | 0x0F => return 1 + FIRST[(waitcnt >> 8) as usize & 3],
        // Not backed by anything. It answers at once because there is nothing
        // to wait for.
        _ => Region { cycles: 1, narrow: false },
    };
    if wide && region.narrow { region.cycles * 2 } else { region.cycles }
}

/// The cartridge, whose speed the game chooses.
///
/// A 32-bit read of it is two halfword reads, and the second of them always
/// follows on from the first — so a word costs a first access and a
/// following-on one, never two first ones. Charging two would make the
/// cartridge half as fast as it is and every game with it.
fn cartridge(addr: u32, wide: bool, sequential: bool, waitcnt: u16) -> u32 {
    let at = window(addr);
    let first = 1 + FIRST[(waitcnt >> (at * 3)) as usize & 3];
    // An access that follows on comes out of the prefetch buffer when the game
    // has asked for one, and out of the chip when it has not.
    let following = if waitcnt & PREFETCH != 0 {
        PREFETCHED
    } else {
        1 + FOLLOWING[at][(waitcnt >> (at * 3 + 2)) as usize & 1]
    };
    let one = if sequential { following } else { first };
    if wide { one + following } else { one }
}

/// Whether an access follows straight on from the one before it.
///
/// The rule is the address alone: the next one along, at the same width. A
/// processor running straight through memory produces those and a branch does
/// not, which is the whole of why straight-line code is faster.
pub fn follows_on(previous: Option<(u32, bool)>, addr: u32, wide: bool) -> bool {
    let step = if wide { 4 } else { 2 };
    matches!(previous, Some((last, was_wide))
        if was_wide == wide && addr == last.wrapping_add(step))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IWRAM: u32 = 0x0300_0000;
    const EWRAM: u32 = 0x0200_0000;
    const VRAM: u32 = 0x0600_0000;
    const ROM: u32 = 0x0800_0000;

    /// The two internal memories are the fast one and the slow one, and the
    /// difference is the reason a game copies code into the first.
    #[test]
    fn the_two_rams_differ_by_six_times_for_a_word() {
        assert_eq!(access(IWRAM, true, false, 0), 1, "on the chip, full width");
        assert_eq!(access(EWRAM, true, false, 0), 6, "off it, and half as wide");
        assert_eq!(access(EWRAM, false, false, 0), 3, "a halfword is one pass");
    }

    /// Video memory answers at once but is half as wide, so a word is two
    /// passes and a halfword is one.
    #[test]
    fn video_memory_is_narrow_rather_than_slow() {
        assert_eq!(access(VRAM, false, false, 0), 1);
        assert_eq!(access(VRAM, true, false, 0), 2);
    }

    /// A cartridge at its default settings: five cycles to open a row and three
    /// to read the next along. It is the slowest thing a game runs code out of
    /// and the reason the difference between the two matters at all.
    #[test]
    fn a_cartridge_costs_less_when_the_address_follows_on() {
        assert_eq!(access(ROM, false, false, 0), 5, "opening a row");
        assert_eq!(access(ROM, false, true, 0), 3, "and the next along");
    }

    /// A word out of the cartridge is two halfwords, and the second of them
    /// always follows the first. Charging two first accesses would make every
    /// cartridge half as fast as it is.
    #[test]
    fn a_word_from_the_cartridge_is_one_first_access_and_one_after_it() {
        assert_eq!(access(ROM, true, false, 0), 5 + 3);
        assert_eq!(access(ROM, true, true, 0), 3 + 3);
    }

    /// The game chooses. Setting the fastest first-access value takes a
    /// cartridge read from five cycles to three, which is what a game does when
    /// it knows its own cartridge is quick enough.
    #[test]
    fn the_game_can_make_its_cartridge_faster() {
        assert_eq!(access(ROM, false, false, 0b10), 3, "the fastest setting");
        assert_eq!(access(ROM, false, true, 0b100), 2, "and the fast following one");
    }

    /// Prefetch is most of what makes running code out of a cartridge bearable:
    /// with it on, code going straight through costs a cycle a halfword instead
    /// of three, because the buffer read it while the processor was busy. A
    /// machine that ignored the switch would tax every game that set it — and
    /// they all set it — three times over on its own code.
    #[test]
    fn a_cartridge_that_reads_ahead_costs_a_cycle_for_what_it_already_has() {
        let waits = PREFETCH;
        assert_eq!(access(ROM, false, true, waits), 1, "already in the buffer");
        assert_eq!(access(ROM, true, true, waits), 2, "a word is two halfwords of it");
        assert_eq!(access(ROM, false, false, waits), 5, "a jump still opens a row");
    }

    /// The three windows are the same chip with three settings, and they are
    /// read out of three different fields.
    #[test]
    fn each_of_the_three_windows_has_its_own_setting() {
        assert_eq!(window(0x0800_0000), 0);
        assert_eq!(window(0x0A00_0000), 1);
        assert_eq!(window(0x0C00_0000), 2);

        // The middle window set to its fastest, the others left alone.
        let waits = 0b10 << 3;
        assert_eq!(access(0x0A00_0000, false, false, waits), 3);
        assert_eq!(access(0x0800_0000, false, false, waits), 5, "the first is untouched");
    }

    #[test]
    fn an_access_follows_on_when_it_is_the_next_address_at_the_same_width() {
        assert!(follows_on(Some((0x100, false)), 0x102, false));
        assert!(follows_on(Some((0x100, true)), 0x104, true));
        assert!(!follows_on(Some((0x100, false)), 0x200, false), "a jump");
        assert!(!follows_on(Some((0x100, false)), 0x104, true), "a different width");
        assert!(!follows_on(None, 0x102, false), "nothing before it");
    }
}
