//! Interrupt controller: the `IE` (0xFFFF) and `IF` (0xFF0F) registers.
//!
//! Peripherals do not call into the CPU: they publish a request by raising
//! their bit in `IF`. The CPU checks that register between instructions. It is
//! an *observer* degenerated into a 5-bit register, and that is how the real
//! hardware works.

/// The five interrupt sources, in priority order (VBlank is the highest). The
/// discriminant is the bit number within `IE`/`IF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Interrupt {
    /// Start of the vertical blanking period (line 144).
    VBlank = 0,
    /// Configurable PPU condition: LYC=LY match or mode change.
    LcdStat = 1,
    /// `TIMA` overflow.
    Timer = 2,
    /// Serial port transfer completed.
    Serial = 3,
    /// Falling edge on any selected joypad line.
    Joypad = 4,
}

impl Interrupt {
    /// All five, already sorted by priority.
    pub const ALL: [Interrupt; 5] =
        [Self::VBlank, Self::LcdStat, Self::Timer, Self::Serial, Self::Joypad];

    pub const fn mask(self) -> u8 {
        1 << (self as u8)
    }

    /// Address the CPU jumps to when servicing this interrupt.
    pub const fn vector(self) -> u16 {
        0x0040 + (self as u16) * 8
    }
}

/// State of the `IE`/`IF` pair.
///
/// The top 3 bits do not exist in hardware; they always read as 1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterruptController {
    /// Interrupt Enable (0xFFFF).
    enable: u8,
    /// Interrupt Flag (0xFF0F): pending requests.
    flag: u8,
}

impl InterruptController {
    const UNUSED_BITS: u8 = 0b1110_0000;

    pub const fn new() -> Self {
        Self { enable: 0, flag: 0 }
    }

    /// A peripheral requests attention.
    pub fn request(&mut self, int: Interrupt) {
        self.flag |= int.mask();
    }

    /// The CPU accepts the interrupt and clears its pending bit.
    pub fn acknowledge(&mut self, int: Interrupt) {
        self.flag &= !int.mask();
    }

    /// Highest-priority interrupt that is both requested and enabled. `None` if
    /// there is none.
    ///
    /// Note that it does not depend on `IME`: this same computation is what
    /// pulls the CPU out of `HALT` even when interrupts are globally disabled.
    pub fn pending(&self) -> Option<Interrupt> {
        let active = self.enable & self.flag & !Self::UNUSED_BITS;
        (active != 0).then(|| {
            let bit = active.trailing_zeros() as u8;
            Interrupt::ALL[bit as usize]
        })
    }

    /// `true` if there is any enabled request, without identifying which.
    pub fn any_pending(&self) -> bool {
        self.enable & self.flag & !Self::UNUSED_BITS != 0
    }

    pub fn read_enable(&self) -> u8 {
        self.enable
    }

    pub fn write_enable(&mut self, value: u8) {
        // IE does store all 8 bits, including the unused ones.
        self.enable = value;
    }

    pub fn read_flag(&self) -> u8 {
        self.flag | Self::UNUSED_BITS
    }

    pub fn write_flag(&mut self, value: u8) {
        self.flag = value & !Self::UNUSED_BITS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_are_correct() {
        assert_eq!(Interrupt::VBlank.vector(), 0x0040);
        assert_eq!(Interrupt::LcdStat.vector(), 0x0048);
        assert_eq!(Interrupt::Timer.vector(), 0x0050);
        assert_eq!(Interrupt::Serial.vector(), 0x0058);
        assert_eq!(Interrupt::Joypad.vector(), 0x0060);
    }

    #[test]
    fn priority_is_respected() {
        let mut ic = InterruptController::new();
        ic.write_enable(0xFF);
        ic.request(Interrupt::Joypad);
        ic.request(Interrupt::Timer);
        assert_eq!(ic.pending(), Some(Interrupt::Timer), "Timer has higher priority");
        ic.acknowledge(Interrupt::Timer);
        assert_eq!(ic.pending(), Some(Interrupt::Joypad));
    }

    #[test]
    fn a_request_that_is_not_enabled_is_not_pending() {
        let mut ic = InterruptController::new();
        ic.request(Interrupt::VBlank);
        assert_eq!(ic.pending(), None);
        ic.write_enable(Interrupt::VBlank.mask());
        assert_eq!(ic.pending(), Some(Interrupt::VBlank));
    }

    #[test]
    fn the_high_bits_of_if_read_as_one() {
        let mut ic = InterruptController::new();
        ic.write_flag(0x00);
        assert_eq!(ic.read_flag(), 0xE0);
    }
}
