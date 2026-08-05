//! Persistence of the battery-backed SRAM into a `.sav` file.
//!
//! The core does not touch the disk: it exposes the SRAM with
//! [`GameBoy::save_ram`] and restores it with [`GameBoy::load_save_ram`]. All
//! the I/O lives here, like the rest of this crate's adapters.
//!
//! Only play mode uses the `.sav`. `--trace` and `--dump` neither read nor write
//! it on purpose: they are debugging tools and their output has to depend on the
//! ROM alone, or it would stop being comparable between runs.

use std::path::{Path, PathBuf};

use akebia_core::GameBoy;

/// How many frames pass between checks for SRAM changes.
///
/// At 59.73 fps that is a bit under a second, and that is exactly the worst case
/// of what is lost if the process dies outright: a Ctrl+C in `--tui` does not go
/// through the orderly shutdown and never gets to run the final save.
const AUTOSAVE_EVERY: u64 = 60;

/// Default save path: the ROM's with the extension changed.
pub fn default_path(rom: &Path) -> PathBuf {
    rom.with_extension("sav")
}

/// Where the second console of a linked pair saves when it is running the very
/// same file as the first.
///
/// Two consoles are two players, and two players do not share one saved game.
/// Left alone they would both autosave over the same `.sav` a second apart, and
/// what survived would be whichever wrote last — over a real saved game, with
/// the trade that was just made in it.
///
/// The second console still *starts* from the first one's saved game —it is a
/// copy of it, right down to where the player is standing— because that is what
/// makes a trade testable at all: a blank game has nothing to trade. From the
/// first write on, the two go their own ways.
pub fn linked_path(rom: &Path) -> PathBuf {
    rom.with_extension("link.sav")
}

pub struct SaveFile {
    path: PathBuf,
    /// Copy of the last thing written, so as not to rewrite an unchanged file.
    written: Vec<u8>,
    frames: u64,
    /// A full disk or missing permissions would fail once a second; the warning
    /// is given only once.
    warned: bool,
}

impl SaveFile {
    /// Sets up persistence and restores the previous game if there is one.
    ///
    /// Returns `None` when there is nothing to save (a cartridge with no
    /// battery) or when the existing file cannot be used. That second case does
    /// **not** degrade into "start from scratch and overwrite": if there is a
    /// `.sav` we do not understand, it most likely is a real saved game, and
    /// losing it is far worse than playing one session without saving.
    pub fn open(gb: &mut GameBoy, path: PathBuf) -> Option<Self> {
        let expected = gb.save_ram()?.len();

        let previous = match std::fs::read(&path) {
            Ok(data) => data,
            // There is no saved game yet: that is normal the first time.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                eprintln!("warning: could not read {}: {e}", path.display());
                eprintln!("warning: the game will not be saved");
                return None;
            }
        };

        // With no previous file, the starting point is the blank SRAM. That way
        // no `.sav` full of zeros is created next to every ROM opened for a
        // moment: the file appears when the game actually writes something.
        let mut written = gb.save_ram()?.to_vec();
        if !previous.is_empty() {
            let (data, trailer) = fit(previous, expected).or_else(|| {
                eprintln!(
                    "warning: {} does not match this cartridge ({expected} bytes were expected)",
                    path.display()
                );
                eprintln!("warning: the game will not be saved, so as not to overwrite it");
                None
            })?;
            if !gb.load_save_ram(&data) {
                eprintln!("warning: the cartridge rejected {}", path.display());
                return None;
            }
            // The trailer `fit` clipped is the clock state, if there is one.
            if !trailer.is_empty() && gb.rtc_load(&trailer, now()) {
                eprintln!("cartridge clock restored from {}", path.display());
            }
            // We have just loaded it, so the file is already up to date.
            written = data;
        }

        Some(Self { path, written, frames: 0, warned: false })
    }

    /// Persistence for a console that was duplicated from another one.
    ///
    /// Nothing is read here, and that is the whole difference from [`open`]: the
    /// SRAM this console came up with **is** the live one it was copied from,
    /// and loading a file over it would drag the copy back to whatever the
    /// autosave happened to have written last.
    ///
    /// [`open`]: Self::open
    pub fn copied(gb: &GameBoy, path: PathBuf) -> Option<Self> {
        gb.save_ram()?;
        // Nothing written yet as far as this file is concerned, so the first
        // autosave creates it even if the game has not touched the SRAM since.
        Some(Self { path, written: Vec::new(), frames: 0, warned: false })
    }

    /// Bytes to be written: the SRAM and, after it, the cartridge clock.
    fn contents(gb: &GameBoy) -> Option<Vec<u8>> {
        let mut data = gb.save_ram()?.to_vec();
        if let Some(clock) = gb.rtc_save(now()) {
            data.extend_from_slice(&clock);
        }
        Some(data)
    }

    /// Advances one frame and saves every so often.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sends the autosave to another file from now on, leaving the one it was
    /// writing to exactly as it is.
    ///
    /// What was last written is forgotten along with the old path: measured
    /// against the new file nothing has been saved yet, and without clearing it
    /// the first flush would compare the SRAM against the *other* file's
    /// contents, find them equal and write nothing at all.
    pub fn redirect(&mut self, path: PathBuf) {
        self.path = path;
        self.written.clear();
    }

    pub fn tick(&mut self, gb: &GameBoy) {
        self.frames += 1;
        if self.frames % AUTOSAVE_EVERY == 0 {
            self.flush(gb);
        }
    }

    /// Writes if the SRAM changed since the last time.
    ///
    /// The comparison looks at **the SRAM only**, not the whole file: the
    /// clock's timestamp changes every second and comparing it would rewrite the
    /// `.sav` on every autosave even if the game had touched nothing.
    pub fn flush(&mut self, gb: &GameBoy) {
        let Some(current) = gb.save_ram() else { return };
        if current == self.written {
            return;
        }
        self.write(gb);
    }

    /// Writes on shutdown, even if the SRAM did not change.
    ///
    /// On a cartridge with a clock it is needed: the game may have halted or
    /// adjusted it without touching the SRAM, and without refreshing the
    /// timestamp the next load would advance the clock from a stale state.
    pub fn flush_final(&mut self, gb: &GameBoy) {
        if gb.rtc_save(0).is_some() {
            self.write(gb);
        } else {
            self.flush(gb);
        }
    }

    fn write(&mut self, gb: &GameBoy) {
        let Some(data) = Self::contents(gb) else {
            return;
        };
        match write_atomic(&self.path, &data) {
            Ok(()) => {
                // Only the SRAM is remembered, which is what gets compared.
                let sram = gb.save_ram().unwrap_or(&[]);
                self.written.clear();
                self.written.extend_from_slice(sram);
            }
            Err(e) if !self.warned => {
                eprintln!("warning: could not save to {}: {e}", self.path.display());
                self.warned = true;
            }
            Err(_) => {}
        }
    }
}

/// Splits what was read from the file into the SRAM and whatever follows.
///
/// What follows is the cartridge clock state, in the format BGB and VBA write. A
/// `.sav` with no trailer is just as valid —written by an emulator without a
/// clock, or by a cartridge that carries none— and loads without further ado.
///
/// If it is **missing**, on the other hand, there is no way to invent the bytes:
/// padding with zeros would give a half-baked save that looks valid. That is a
/// `None`.
fn fit(mut data: Vec<u8>, expected: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    if data.len() < expected {
        return None;
    }
    let trailer = data.split_off(expected);
    Some((data, trailer))
}

/// Seconds elapsed since the Unix epoch.
///
/// It is the only thing saving needs from the system clock, and that is why it
/// lives here and not in the core: a clock set before 1970 would give 0, which
/// the core reads as "no time has passed" instead of subtracting it.
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Writes the whole file or leaves it alone.
///
/// Overwriting the good `.sav` in place and dying halfway would leave it
/// truncated, and the moment of saving is precisely when the user is closing the
/// emulator. The temporary file goes into the same directory because the rename
/// is only atomic within the same filesystem.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("sav.tmp");
    std::fs::write(&tmp, data)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic 32 KiB ROM with whatever type and RAM size are asked for.
    fn rom(cart_type: u8, ram_size: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0134..0x013A].copy_from_slice(b"TESTER");
        rom[0x0147] = cart_type;
        rom[0x0149] = ram_size;
        rom
    }

    /// ROM ONLY + RAM + battery, with 8 KiB of SRAM.
    fn with_battery() -> GameBoy {
        GameBoy::new(rom(0x09, 0x02)).unwrap()
    }

    fn temporary(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gb-save-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("game.sav")
    }

    #[test]
    fn the_sav_goes_next_to_the_rom() {
        assert_eq!(default_path(Path::new("roms/game.gb")), PathBuf::from("roms/game.sav"));
        assert_eq!(default_path(Path::new("/x/game.gbc")), PathBuf::from("/x/game.sav"));
    }

    #[test]
    fn a_cartridge_with_no_battery_saves_nothing() {
        // Bare ROM ONLY: there is no SRAM to back up, so no file is opened.
        let mut gb = GameBoy::new(rom(0x00, 0x00)).unwrap();
        assert!(SaveFile::open(&mut gb, temporary("no-battery")).is_none());
    }

    #[test]
    fn the_first_save_needs_no_previous_file() {
        let path = temporary("first");
        let _ = std::fs::remove_file(&path);

        let mut gb = with_battery();
        let mut save = SaveFile::open(&mut gb, path.clone()).unwrap();
        assert!(!path.exists(), "nothing is written until there is something to save");

        // And it stays unwritten while the game does not touch the SRAM: opening
        // a ROM for a moment must not leave a `.sav` of zeros next to it.
        save.flush(&gb);
        assert!(!path.exists(), "a blank SRAM is not a saved game");
    }

    #[test]
    fn it_restores_the_saved_game_on_open() {
        let path = temporary("restore");
        std::fs::write(&path, vec![0xA5; 8 * 1024]).unwrap();

        let mut gb = with_battery();
        assert!(SaveFile::open(&mut gb, path).is_some());
        assert!(gb.save_ram().unwrap().iter().all(|&b| b == 0xA5));
    }

    /// MBC3 + RAM + RTC + battery, with 8 KiB of SRAM.
    fn with_clock() -> GameBoy {
        GameBoy::new(rom(0x10, 0x02)).unwrap()
    }

    #[test]
    fn the_sav_of_a_cartridge_with_a_clock_carries_the_trailer() {
        let path = temporary("clock");
        let _ = std::fs::remove_file(&path);

        let mut gb = with_clock();
        let mut save = SaveFile::open(&mut gb, path.clone()).unwrap();
        gb.load_save_ram(&vec![0x5A; 8 * 1024]);
        save.flush(&gb);

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 8 * 1024 + 48, "the SRAM and behind it the clock's 48 bytes");
        assert_eq!(&data[..8 * 1024], &[0x5A; 8 * 1024]);
    }

    #[test]
    fn a_cartridge_with_no_clock_writes_no_trailer() {
        let path = temporary("no-clock");
        let _ = std::fs::remove_file(&path);

        let mut gb = with_battery(); // ROM ONLY: carries no clock
        let mut save = SaveFile::open(&mut gb, path.clone()).unwrap();
        gb.load_save_ram(&vec![0x11; 8 * 1024]);
        save.flush(&gb);

        assert_eq!(std::fs::read(&path).unwrap().len(), 8 * 1024);
    }

    #[test]
    fn a_sav_with_a_trailer_is_reread_without_complaint() {
        let path = temporary("round-trip");
        let _ = std::fs::remove_file(&path);

        let mut gb = with_clock();
        let mut save = SaveFile::open(&mut gb, path.clone()).unwrap();
        gb.load_save_ram(&vec![0xC3; 8 * 1024]);
        save.flush(&gb);

        // A fresh console reads the same file: SRAM and clock must both get in.
        let mut other = with_clock();
        assert!(SaveFile::open(&mut other, path).is_some());
        assert!(other.save_ram().unwrap().iter().all(|&b| b == 0xC3));
    }

    #[test]
    fn a_sav_with_an_rtc_trailer_is_clipped() {
        // Other emulators leave the clock state behind the SRAM.
        let path = temporary("with-rtc");
        let mut data = vec![0x5A; 8 * 1024];
        data.extend_from_slice(&[0xFF; 48]);
        std::fs::write(&path, data).unwrap();

        let mut gb = with_battery();
        assert!(SaveFile::open(&mut gb, path).is_some());
        assert!(gb.save_ram().unwrap().iter().all(|&b| b == 0x5A));
    }

    #[test]
    fn a_sav_that_is_too_short_is_not_overwritten() {
        let path = temporary("short");
        std::fs::write(&path, vec![0x11; 1024]).unwrap();

        let mut gb = with_battery();
        assert!(SaveFile::open(&mut gb, path.clone()).is_none(), "this session is not saved");
        assert_eq!(std::fs::read(&path).unwrap().len(), 1024, "the file is left intact");
    }

    #[test]
    fn flush_writes_the_changes_and_leaves_no_temporaries() {
        let path = temporary("flush");
        let _ = std::fs::remove_file(&path);

        let mut gb = with_battery();
        let mut save = SaveFile::open(&mut gb, path.clone()).unwrap();

        // `load_save_ram` is the only way to touch the SRAM without running code.
        gb.load_save_ram(&vec![0x42; 8 * 1024]);
        save.flush(&gb);

        assert_eq!(std::fs::read(&path).unwrap(), vec![0x42; 8 * 1024]);
        assert!(!path.with_extension("sav.tmp").exists(), "the temporary was renamed");
    }

    #[test]
    fn flush_with_no_changes_does_not_rewrite() {
        let path = temporary("no-changes");
        std::fs::write(&path, vec![0x77; 8 * 1024]).unwrap();

        let mut gb = with_battery();
        let mut save = SaveFile::open(&mut gb, path.clone()).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        save.flush(&gb);

        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "the file was not even touched");
    }

    #[test]
    fn the_autosave_waits_for_the_period_to_complete() {
        let path = temporary("autosave");
        let _ = std::fs::remove_file(&path);

        let mut gb = with_battery();
        let mut save = SaveFile::open(&mut gb, path.clone()).unwrap();
        gb.load_save_ram(&vec![0x99; 8 * 1024]);

        for _ in 0..AUTOSAVE_EVERY - 1 {
            save.tick(&gb);
        }
        assert!(!path.exists(), "not time yet");

        save.tick(&gb);
        assert_eq!(std::fs::read(&path).unwrap(), vec![0x99; 8 * 1024]);
    }
}
