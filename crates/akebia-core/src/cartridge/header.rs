//! Cartridge header parsing (range 0x0100..0x0150).
//!
//! It is a *value object*: built once, immutable, and with no behaviour beyond
//! describing itself.

use crate::{Error, Result};

/// Offset and length of the header fields we care about.
mod offset {
    pub const TITLE: core::ops::Range<usize> = 0x0134..0x0144;
    pub const CGB_FLAG: usize = 0x0143;
    pub const NEW_LICENSEE: core::ops::Range<usize> = 0x0144..0x0146;
    pub const SGB_FLAG: usize = 0x0146;
    pub const CART_TYPE: usize = 0x0147;
    pub const ROM_SIZE: usize = 0x0148;
    pub const RAM_SIZE: usize = 0x0149;
    pub const DESTINATION: usize = 0x014A;
    pub const OLD_LICENSEE: usize = 0x014B;
    pub const VERSION: usize = 0x014C;
    pub const HEADER_CHECKSUM: usize = 0x014D;
    pub const GLOBAL_CHECKSUM: core::ops::Range<usize> = 0x014E..0x0150;
}

/// Minimum size of a valid ROM: it must contain the complete header.
pub const HEADER_END: usize = 0x0150;

/// Game Boy Color compatibility declared by the cartridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgbSupport {
    /// DMG only; on a real CGB it runs in monochrome compatibility mode.
    None,
    /// Uses colour features but still boots on a DMG.
    Enhanced,
    /// CGB exclusive; it does not boot on a DMG.
    Only,
}

/// Hardware present in the cartridge, decoded from byte 0x0147.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CartridgeType {
    pub raw: u8,
    pub kind: MapperKind,
    pub has_ram: bool,
    pub has_battery: bool,
    pub has_rtc: bool,
    pub has_rumble: bool,
}

/// Memory bank controller family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapperKind {
    /// No mapper: 32 KiB straight.
    RomOnly,
    Mbc1,
    Mbc2,
    Mbc3,
    Mbc5,
    Mbc6,
    Mbc7,
    /// Any exotic variant (HuC1, TAMA5, camera…).
    Other(&'static str),
}

impl MapperKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::RomOnly => "ROM ONLY",
            Self::Mbc1 => "MBC1",
            Self::Mbc2 => "MBC2",
            Self::Mbc3 => "MBC3",
            Self::Mbc5 => "MBC5",
            Self::Mbc6 => "MBC6",
            Self::Mbc7 => "MBC7",
            Self::Other(n) => n,
        }
    }
}

impl CartridgeType {
    /// Decodes byte 0x0147 according to the Pan Docs table.
    fn from_byte(raw: u8) -> Self {
        use MapperKind::*;
        // (family, ram, battery, rtc, rumble)
        let (kind, ram, bat, rtc, rumble) = match raw {
            0x00 => (RomOnly, false, false, false, false),
            0x01 => (Mbc1, false, false, false, false),
            0x02 => (Mbc1, true, false, false, false),
            0x03 => (Mbc1, true, true, false, false),
            0x05 => (Mbc2, false, false, false, false),
            0x06 => (Mbc2, false, true, false, false),
            0x08 => (RomOnly, true, false, false, false),
            0x09 => (RomOnly, true, true, false, false),
            0x0B..=0x0D => (Other("MMM01"), raw != 0x0B, raw == 0x0D, false, false),
            0x0F => (Mbc3, false, true, true, false),
            0x10 => (Mbc3, true, true, true, false),
            0x11 => (Mbc3, false, false, false, false),
            0x12 => (Mbc3, true, false, false, false),
            0x13 => (Mbc3, true, true, false, false),
            0x19 => (Mbc5, false, false, false, false),
            0x1A => (Mbc5, true, false, false, false),
            0x1B => (Mbc5, true, true, false, false),
            0x1C => (Mbc5, false, false, false, true),
            0x1D => (Mbc5, true, false, false, true),
            0x1E => (Mbc5, true, true, false, true),
            0x20 => (Mbc6, true, true, false, false),
            0x22 => (Mbc7, true, true, false, true),
            0xFC => (Other("POCKET CAMERA"), true, true, false, false),
            0xFD => (Other("BANDAI TAMA5"), true, true, false, false),
            0xFE => (Other("HuC3"), true, true, true, false),
            0xFF => (Other("HuC1"), true, true, false, false),
            _ => (Other("UNKNOWN"), false, false, false, false),
        };
        Self { raw, kind, has_ram: ram, has_battery: bat, has_rtc: rtc, has_rumble: rumble }
    }
}

/// Read-only view over the cartridge metadata.
#[derive(Debug, Clone)]
pub struct Header {
    pub title: String,
    pub cartridge_type: CartridgeType,
    /// ROM size in bytes, according to byte 0x0148.
    pub rom_size: usize,
    /// SRAM size in bytes, according to byte 0x0149.
    pub ram_size: usize,
    pub cgb: CgbSupport,
    pub sgb: bool,
    pub licensee: String,
    /// `true` if the declared destination is Japan.
    pub japanese: bool,
    pub version: u8,
    /// Stored and computed header checksum; they must match or the real BootROM
    /// halts the boot.
    pub header_checksum: (u8, u8),
    pub global_checksum: u16,
}

impl Header {
    /// Reads the header from the raw ROM bytes.
    ///
    /// It does not fail on incorrect checksums: many homebrew and test ROMs come
    /// with them wrong and are still perfectly runnable.
    pub fn parse(rom: &[u8]) -> Result<Self> {
        if rom.len() < HEADER_END {
            return Err(Error::RomTooSmall { len: rom.len() });
        }

        let cgb_byte = rom[offset::CGB_FLAG];
        let cgb = match cgb_byte {
            0x80 => CgbSupport::Enhanced,
            0xC0 => CgbSupport::Only,
            _ => CgbSupport::None,
        };

        // On CGB cartridges the title was shortened to make room for the
        // manufacturer code and the CGB flag.
        let title_end = if cgb == CgbSupport::None { 16 } else { 15 };
        let title = rom[offset::TITLE]
            .iter()
            .take(title_end)
            .take_while(|&&b| b != 0)
            .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
            .collect::<String>()
            .trim_end()
            .to_owned();

        let rom_size = rom_size_from_byte(rom[offset::ROM_SIZE]);
        let ram_size = ram_size_from_byte(rom[offset::RAM_SIZE]);

        let old_licensee = rom[offset::OLD_LICENSEE];
        let licensee = if old_licensee == 0x33 {
            String::from_utf8_lossy(&rom[offset::NEW_LICENSEE]).into_owned()
        } else {
            format!("{old_licensee:02X}")
        };

        Ok(Self {
            title,
            cartridge_type: CartridgeType::from_byte(rom[offset::CART_TYPE]),
            rom_size,
            ram_size,
            cgb,
            sgb: rom[offset::SGB_FLAG] == 0x03,
            licensee,
            japanese: rom[offset::DESTINATION] == 0x00,
            version: rom[offset::VERSION],
            header_checksum: (rom[offset::HEADER_CHECKSUM], compute_header_checksum(rom)),
            global_checksum: u16::from_be_bytes([
                rom[offset::GLOBAL_CHECKSUM.start],
                rom[offset::GLOBAL_CHECKSUM.start + 1],
            ]),
        })
    }

    /// `true` if the stored checksum matches the computed one.
    pub fn header_checksum_ok(&self) -> bool {
        self.header_checksum.0 == self.header_checksum.1
    }

    /// Number of 16 KiB ROM banks.
    pub fn rom_banks(&self) -> usize {
        (self.rom_size / crate::cartridge::ROM_BANK_SIZE).max(2)
    }

    /// Number of 8 KiB SRAM banks.
    pub fn ram_banks(&self) -> usize {
        self.ram_size / crate::cartridge::RAM_BANK_SIZE
    }
}

/// Byte 0x0148 encodes `32 KiB << n`.
fn rom_size_from_byte(b: u8) -> usize {
    match b {
        0x00..=0x08 => (32 * 1024) << b,
        // Unofficial values that show up on some cartridges.
        0x52 => 72 * 16 * 1024,
        0x53 => 80 * 16 * 1024,
        0x54 => 96 * 16 * 1024,
        _ => 32 * 1024,
    }
}

/// Byte 0x0149 is not linear; it is a table.
fn ram_size_from_byte(b: u8) -> usize {
    match b {
        0x02 => 8 * 1024,
        0x03 => 32 * 1024,
        0x04 => 128 * 1024,
        0x05 => 64 * 1024,
        // 0x00 = no RAM, 0x01 = unused value.
        _ => 0,
    }
}

/// Header checksum algorithm exactly as the BootROM runs it:
/// `x = 0; for a in 0x0134..=0x014C { x = x - rom[a] - 1 }`.
fn compute_header_checksum(rom: &[u8]) -> u8 {
    rom[0x0134..=0x014C].iter().fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic 32 KiB ROM with a coherent header.
    fn fake_rom(title: &str, cart_type: u8, rom_size: u8, ram_size: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 32 * 1024];
        for (i, b) in title.bytes().take(16).enumerate() {
            rom[0x0134 + i] = b;
        }
        rom[0x0147] = cart_type;
        rom[0x0148] = rom_size;
        rom[0x0149] = ram_size;
        rom[0x014D] = compute_header_checksum(&rom);
        rom
    }

    #[test]
    fn it_parses_the_title_and_the_type() {
        let rom = fake_rom("SAMPLE ROM", 0x00, 0x00, 0x00);
        let h = Header::parse(&rom).unwrap();
        assert_eq!(h.title, "SAMPLE ROM");
        assert_eq!(h.cartridge_type.kind, MapperKind::RomOnly);
        assert_eq!(h.rom_size, 32 * 1024);
        assert_eq!(h.rom_banks(), 2);
        assert!(h.header_checksum_ok());
    }

    #[test]
    fn it_decodes_mbc3_with_battery_and_rtc() {
        let rom = fake_rom("RTC CART", 0x10, 0x05, 0x03);
        let h = Header::parse(&rom).unwrap();
        let t = h.cartridge_type;
        assert_eq!(t.kind, MapperKind::Mbc3);
        assert!(t.has_ram && t.has_battery && t.has_rtc);
        assert_eq!(h.rom_size, 1024 * 1024);
        assert_eq!(h.ram_banks(), 4);
    }

    #[test]
    fn it_detects_the_cgb_flag() {
        let mut rom = fake_rom("COLOR CART", 0x1B, 0x05, 0x03);
        rom[0x0143] = 0xC0;
        rom[0x014D] = compute_header_checksum(&rom);
        assert_eq!(Header::parse(&rom).unwrap().cgb, CgbSupport::Only);
    }

    #[test]
    fn it_rejects_a_truncated_rom() {
        assert!(matches!(Header::parse(&[0u8; 0x100]), Err(Error::RomTooSmall { .. })));
    }
}
