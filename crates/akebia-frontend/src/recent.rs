//! The games opened lately, so that going back to one is not a search.
//!
//! # Why this is not the folder Akebia already remembered
//!
//! It remembered one *folder*, which is enough to find the list again and no
//! help at all once the list is long. A collection is hundreds of files in one
//! directory — that is how people keep them — and the four or five being played
//! this month are scattered through it alphabetically. Getting back to one meant
//! typing enough of its name into the search box every time.
//!
//! So this remembers the files, and the folder memory stays as it is: they
//! answer different questions. The folder says *where the games are*, which is
//! still what the list needs when nothing has been played yet.
//!
//! # A trace and not a preference
//!
//! It goes beside the folder in `XDG_STATE_HOME` for the same reason: nobody
//! wrote it, it is a record of what they did. Deleting the file loses a
//! convenience and nothing else, and every failure here is swallowed — a game
//! that opens but cannot be written to a list is a game that opened.
//!
//! # What it is not
//!
//! A list of favourites. It has no pinning and no ordering by hand, because
//! the moment it had either it would need to be kept rather than rebuilt, and
//! a file the user has curated cannot be quietly pruned when a path stops
//! existing — which is the one thing this must do, since a collection moves.

use std::path::{Path, PathBuf};

/// How many are kept.
///
/// Short on purpose. The point is to skip the search for the handful of games
/// actually being played; a list long enough to need scrolling is a second list
/// to look through, which is what this exists to avoid.
const KEEP: usize = 10;

/// Where the list lives, beside the remembered folder.
pub fn path() -> Option<PathBuf> {
    crate::roms::state_path().map(|state| state.with_file_name("recent"))
}

/// The games last opened, most recent first.
///
/// Anything that is no longer there is left out. A collection gets moved and
/// renamed, and offering a game that cannot be opened is worse than not
/// offering it: the entry looks like a bug in the emulator rather than a file
/// that went away.
pub fn load(list: &Path) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(list) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .take(KEEP)
        .collect()
}

/// Puts a game at the top of the list.
///
/// Opening one already in it moves it up rather than adding it twice, which is
/// what makes the order mean "lately" instead of "the first ten ever opened".
pub fn remember(list: &Path, rom: &Path) {
    // The absolute path, because the list outlives the working directory Akebia
    // happened to be started from.
    let Ok(rom) = std::fs::canonicalize(rom) else {
        return;
    };
    let mut paths = load(list);
    paths.retain(|path| path != &rom);
    paths.insert(0, rom);
    paths.truncate(KEEP);
    write(list, &paths);
}

/// Empties it. Offered because a list of what somebody has been playing is the
/// kind of thing they may want gone, and hunting for the file to delete is not
/// an answer.
pub fn clear(list: &Path) {
    let _ = std::fs::remove_file(list);
}

fn write(list: &Path, paths: &[PathBuf]) {
    if let Some(parent) = list.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let mut text = String::new();
    for path in paths {
        text.push_str(&path.to_string_lossy());
        text.push('\n');
    }
    let _ = std::fs::write(list, text);
}

/// What a menu entry says: the game's name, and the folder only when two of
/// them share a name.
///
/// Showing the whole path would make the menu as wide as the screen, and
/// showing the bare name would offer two identical-looking entries to somebody
/// who keeps the Japanese and the American release of the same game.
pub fn labels(paths: &[PathBuf]) -> Vec<String> {
    let name = |path: &Path| {
        path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned())
    };
    let names: Vec<String> = paths.iter().map(|path| name(path)).collect();
    paths
        .iter()
        .zip(&names)
        .map(|(path, own)| {
            if names.iter().filter(|other| *other == own).count() == 1 {
                return own.clone();
            }
            let folder = path
                .parent()
                .and_then(Path::file_name)
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
            format!("{own}  —  {folder}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own, with real files in it: everything here
    /// drops paths that are not there, so nothing can be tested with names
    /// alone.
    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("akebia-recent-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rom(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, [0u8; 4]).unwrap();
        path
    }

    #[test]
    fn the_last_opened_comes_first() {
        let dir = dir("order");
        let list = dir.join("recent");
        let (one, two) = (rom(&dir, "one.gb"), rom(&dir, "two.gba"));

        remember(&list, &one);
        remember(&list, &two);

        let loaded = load(&list);
        assert_eq!(loaded.len(), 2);
        assert!(loaded[0].ends_with("two.gba"), "the newest is at the top");
    }

    /// Opening a game twice moves it up rather than listing it twice. Without
    /// this the list fills with one game and stops being a list.
    #[test]
    fn opening_one_again_moves_it_up_instead_of_repeating_it() {
        let dir = dir("again");
        let list = dir.join("recent");
        let (one, two) = (rom(&dir, "one.gb"), rom(&dir, "two.gb"));

        remember(&list, &one);
        remember(&list, &two);
        remember(&list, &one);

        let loaded = load(&list);
        assert_eq!(loaded.len(), 2, "two games, not three entries");
        assert!(loaded[0].ends_with("one.gb"));
    }

    #[test]
    fn it_keeps_only_the_last_ten() {
        let dir = dir("cap");
        let list = dir.join("recent");
        for index in 0..15 {
            remember(&list, &rom(&dir, &format!("game{index}.gb")));
        }
        let loaded = load(&list);
        assert_eq!(loaded.len(), KEEP);
        assert!(loaded[0].ends_with("game14.gb"), "and the newest survived");
    }

    /// A collection gets moved. An entry pointing at a file that is gone is
    /// left out rather than offered — chosen, it would look like a fault in the
    /// emulator instead of a file that is not there.
    #[test]
    fn a_game_that_is_no_longer_there_is_dropped() {
        let dir = dir("missing");
        let list = dir.join("recent");
        let (one, two) = (rom(&dir, "one.gb"), rom(&dir, "two.gb"));
        remember(&list, &one);
        remember(&list, &two);

        std::fs::remove_file(&one).unwrap();
        let loaded = load(&list);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].ends_with("two.gb"));
    }

    #[test]
    fn a_list_that_was_never_written_is_empty_rather_than_an_error() {
        assert!(load(&dir("none").join("recent")).is_empty());
    }

    #[test]
    fn clearing_it_leaves_nothing_behind() {
        let dir = dir("clear");
        let list = dir.join("recent");
        remember(&list, &rom(&dir, "one.gb"));
        clear(&list);
        assert!(load(&list).is_empty());
        assert!(!list.exists());
    }

    /// Two releases of the same game live in different folders under the same
    /// name, and a menu offering the name twice is a menu that cannot be used.
    #[test]
    fn two_games_of_the_same_name_are_told_apart_by_their_folder() {
        let paths =
            [PathBuf::from("/roms/jp/Zelda.gb"), PathBuf::from("/roms/us/Zelda.gb"), PathBuf::from("/roms/Metroid.gba")];
        let labels = labels(&paths);
        assert_eq!(labels[0], "Zelda  —  jp");
        assert_eq!(labels[1], "Zelda  —  us");
        assert_eq!(labels[2], "Metroid", "and a name of its own stays a name");
    }
}
