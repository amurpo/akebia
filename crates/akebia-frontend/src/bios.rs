//! Finding the Advance's BIOS.
//!
//! # Why a missing one is not a small loss
//!
//! The BIOS of the Advance is not just a boot logo. Two things games depend on
//! live inside it. The first is the interrupt path: the hardware vectors to
//! `0x18`, which is BIOS, and it is the BIOS that reads the handler the game
//! left at `0x03007FFC` and calls it. The second is a couple of dozen routines
//! games call the way a program calls a library — decompression, division, wait
//! for the next frame.
//!
//! So a machine without one does not run a game slightly worse. Both cartridges
//! this was tried against take the first interrupt they enable, walk forward
//! through sixteen kilobytes of zeros — which decode to something, and that
//! something does nothing — and step off the end of the memory map having drawn
//! not one pixel. The window stays black and there is nothing on it to say why.
//!
//! # Why looking is worth the code
//!
//! Naming the file on the command line was enough while the only caller was a
//! diagnostic harness. It stops being enough the moment a game is started by
//! clicking it in a list: there is no command line there to put it on, and the
//! result of leaving it out is a black window rather than a complaint.
//!
//! So the usual places are looked at, nearest first, and whoever keeps the file
//! anywhere sensible never has to say where. Whoever has not got one is *told*
//! — see [`ADVICE`] — because "there is no BIOS" is an answer and a black
//! screen is not.

use std::path::{Path, PathBuf};

/// How big a BIOS image is: sixteen kilobytes, exactly.
///
/// The size is the whole test applied to a candidate. Checking the contents
/// would mean picking a revision to be right about, and there are replacement
/// BIOSes that work and hash to something else; checking nothing would mean
/// loading the first file that happened to be called `bios.bin`.
pub const SIZE: usize = 16 * 1024;

/// What the file tends to be called.
const NAMES: [&str; 2] = ["gba_bios.bin", "bios.bin"];

/// Where to look after the folder the game itself came from.
///
/// The same places and the same reasoning as the ROM folders in [`crate::roms`]:
/// the application's own data folder is where it belongs, `roms` is there for
/// whoever runs Akebia out of its working directory, and the current directory
/// is the last guess.
const FOLDERS: [&str; 3] = ["~/.local/share/akebia", "roms", "."];

/// What to tell someone whose Advance game came up black.
pub const ADVICE: &str = "no Advance BIOS found: the game may draw nothing. \
     Put gba_bios.bin beside the game or in ~/.local/share/akebia";

/// Whether this is a file the right size to be a BIOS.
fn is_image(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() == SIZE as u64)
}

/// The BIOS to use with this cartridge, if one can be found.
pub fn find(rom: &Path) -> Option<PathBuf> {
    let folders: Vec<PathBuf> =
        FOLDERS.iter().filter_map(|folder| crate::roms::expand(folder)).collect();
    find_in(rom, &folders)
}

/// The search itself, with the list of folders handed in.
///
/// It is split out from [`find`] so that a test can say where to look. A test
/// that went by the real list would be asking about the home directory of
/// whoever ran it, and would pass or fail on whether that person happens to
/// own a BIOS.
///
/// The game's own folder comes before any of them, because it is the one place
/// the person actually chose: a collection with the BIOS sitting in it is what
/// somebody who has both files has.
fn find_in(rom: &Path, folders: &[PathBuf]) -> Option<PathBuf> {
    let beside = rom.parent().filter(|dir| !dir.as_os_str().is_empty()).map(Path::to_path_buf);
    beside
        .into_iter()
        .chain(folders.iter().cloned())
        .flat_map(|dir| NAMES.iter().map(move |name| dir.join(name)))
        .find(|candidate| is_image(candidate))
}

/// The image to put in the machine, if there is to be one.
///
/// A path given by name is an error when it cannot be used: it was asked for,
/// and carrying on without it silently is how one ends up debugging a machine
/// other than the one that was meant. A path nobody asked for and that is not
/// there is not an error — there was nothing to be wrong about — and `None`
/// says so, leaving the caller to decide what that costs.
pub fn image(named: Option<&Path>, rom: &Path) -> Result<Option<Vec<u8>>, String> {
    match named.map(Path::to_path_buf).or_else(|| find(rom)) {
        Some(path) => read(&path).map(Some),
        None => Ok(None),
    }
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    let image =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if image.len() != SIZE {
        return Err(format!(
            "{} is {} bytes and an Advance BIOS is {SIZE}",
            path.display(),
            image.len()
        ));
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own per test, so that two of them running at once do not
    /// find each other's files.
    fn temporary(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("akebia-bios-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("could not create the test folder");
        dir
    }

    fn write(path: &Path, len: usize) {
        std::fs::write(path, vec![0u8; len]).expect("could not write the test file");
    }

    #[test]
    fn the_bios_beside_the_game_is_the_one_found() {
        let dir = temporary("beside");
        write(&dir.join("gba_bios.bin"), SIZE);
        assert_eq!(find_in(&dir.join("game.gba"), &[]), Some(dir.join("gba_bios.bin")));
    }

    /// The game's own folder beats the list, which is what makes a collection
    /// with its own BIOS in it work without anybody being asked anything.
    #[test]
    fn the_game_s_own_folder_comes_first() {
        let beside = temporary("first-beside");
        let listed = temporary("first-listed");
        write(&beside.join("gba_bios.bin"), SIZE);
        write(&listed.join("gba_bios.bin"), SIZE);
        assert_eq!(
            find_in(&beside.join("game.gba"), std::slice::from_ref(&listed)),
            Some(beside.join("gba_bios.bin"))
        );
    }

    /// A game whose folder has nothing in it falls through to the list.
    #[test]
    fn the_listed_folders_are_the_fallback() {
        let beside = temporary("fallback-beside");
        let listed = temporary("fallback-listed");
        write(&listed.join("gba_bios.bin"), SIZE);
        assert_eq!(
            find_in(&beside.join("game.gba"), std::slice::from_ref(&listed)),
            Some(listed.join("gba_bios.bin"))
        );
    }

    /// The commoner name wins over the vaguer one, so that a folder holding both
    /// gives the same answer every time rather than whichever the filesystem
    /// listed first.
    #[test]
    fn the_advance_name_is_preferred_to_the_plain_one() {
        let dir = temporary("names");
        write(&dir.join("gba_bios.bin"), SIZE);
        write(&dir.join("bios.bin"), SIZE);
        assert_eq!(find_in(&dir.join("game.gba"), &[]), Some(dir.join("gba_bios.bin")));
    }

    /// But the plain one is taken when it is what there is.
    #[test]
    fn the_plain_name_is_taken_when_it_is_the_only_one() {
        let dir = temporary("plain");
        write(&dir.join("bios.bin"), SIZE);
        assert_eq!(find_in(&dir.join("game.gba"), &[]), Some(dir.join("bios.bin")));
    }

    /// The size is the whole test, and it is there to keep some other machine's
    /// BIOS out: a folder of mixed emulation is exactly where a `bios.bin` that
    /// is not this one lives.
    #[test]
    fn a_file_of_the_wrong_size_is_not_a_bios() {
        let dir = temporary("size");
        write(&dir.join("gba_bios.bin"), 256);
        assert_eq!(find_in(&dir.join("game.gba"), &[]), None);
    }

    /// Nothing found is not a failure. It is an answer, and the caller is the
    /// one that decides what it costs.
    #[test]
    fn nothing_found_is_not_an_error() {
        let dir = temporary("empty");
        assert_eq!(find_in(&dir.join("game.gba"), &[]), None);
    }

    /// A file asked for by name and not usable is a failure, because whoever
    /// named it is expecting *that* machine and no other.
    #[test]
    fn a_named_bios_that_is_not_there_is_an_error() {
        let dir = temporary("named-missing");
        assert!(read(&dir.join("nowhere.bin")).is_err());
    }

    #[test]
    fn a_named_bios_of_the_wrong_size_is_an_error() {
        let dir = temporary("named-short");
        let named = dir.join("short.bin");
        write(&named, 1024);
        let failure = read(&named).unwrap_err();
        assert!(failure.contains("1024"), "it says what was wrong with it: {failure}");
    }

    #[test]
    fn a_bios_the_right_size_is_read_whole() {
        let dir = temporary("named-good");
        let named = dir.join("gba_bios.bin");
        write(&named, SIZE);
        assert_eq!(read(&named).map(|image| image.len()), Ok(SIZE));
    }
}
