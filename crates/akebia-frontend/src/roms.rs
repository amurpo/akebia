//! ROM discovery and memory of the last folder used.
//!
//! There is no interface here: only reading a directory and remembering which
//! one it was. It is kept apart from [`crate::app`] because it is the only part
//! of the selection screen that can be tested without opening a window.

use std::path::{Path, PathBuf};

use akebia_core::cartridge::{CgbSupport, Header};

/// Extensions considered to be ROMs. Both machines, in one list, because a
/// person keeps their games in one folder and does not sort them by console.
const EXTENSIONS: [&str; 3] = ["gb", "gbc", "gba"];

/// A ROM found in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// The file name without its extension: it is the one the user recognises
    /// and the one they see in their file manager. The header title comes
    /// clipped to fifteen letters and does not always resemble the name the game
    /// is known by.
    pub name: String,
    /// Mapper family, as the core names it, or `?` if it could not be read.
    pub mapper: String,
    pub cgb: CgbSupport,
}

/// Whether the file is one the list would offer.
fn is_rom(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Lists the ROMs in a folder, sorted by name.
///
/// Subdirectories are not walked: a folder is shown, not a tree.
pub fn scan(dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut roms: Vec<Entry> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| is_rom(p))
        .map(|path| {
            let name =
                path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
            let (mapper, cgb) = describe(&path);
            Entry { path, name, mapper, cgb }
        })
        .collect();

    // Case-insensitive: in an alphabetical list, "zelda" after "Wario" and
    // before "Zelda" makes no sense at all to whoever is looking at it.
    roms.sort_by_key(|e| (e.name.to_lowercase(), e.path.clone()));
    roms
}

/// Mapper and CGB compatibility, reading only the cartridge header.
///
/// The first 336 bytes are read and not the whole file: the folder may hold
/// dozens of multi-megabyte ROMs and the list has to appear instantly.
fn describe(path: &Path) -> (String, CgbSupport) {
    use std::io::Read;

    // An Advance cartridge has a header of its own with none of this in it: no
    // mapper family, and the colour question does not arise. Saying so is
    // better than reading the older machine's fields out of bytes that mean
    // something else entirely.
    if crate::console::is_advance(path) {
        return ("Advance".to_owned(), CgbSupport::None);
    }

    // A ROM too short to have a header is not going to boot, but it is listed
    // all the same and flagged: the user sees the file in their file manager,
    // and hiding it silently would only make them think the list is broken.
    let unknown = || ("?".to_owned(), CgbSupport::None);

    let mut header = [0u8; akebia_core::cartridge::header::HEADER_END];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header).map(|()| true))
        .unwrap_or(false);
    if !read {
        return unknown();
    }
    match Header::parse(&header) {
        Ok(header) => (header.cartridge_type.kind.name().to_owned(), header.cgb),
        Err(_) => unknown(),
    }
}

// ---- Walking folders --------------------------------------------------------

/// A subfolder, as the folder browser offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub path: PathBuf,
    pub name: String,
    /// How many ROMs it holds. It is what turns a column of folder names into a
    /// way of *finding* the games: the right folder is recognised by the number
    /// beside it, without having to go into each one in turn.
    pub roms: usize,
}

/// The subfolders of `dir`, sorted by name.
///
/// Hidden folders are left out: a home directory holds dozens of them and none
/// of them is where anybody keeps their games. The path box reaches them on the
/// rare occasion one is really wanted.
pub fn subfolders(dir: &Path) -> Vec<Folder> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut folders: Vec<Folder> = entries
        .flatten()
        .map(|e| e.path())
        // Symbolic links are followed on purpose: a link to the collection on
        // another drive is a perfectly ordinary way to arrange it.
        .filter(|p| p.is_dir())
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            (!name.starts_with('.')).then(|| Folder { roms: count(&path), name, path })
        })
        .collect();

    folders.sort_by_key(|f| (f.name.to_lowercase(), f.path.clone()));
    folders
}

/// How many ROMs a folder holds, without reading a single header.
///
/// Only the extension is looked at. This runs once per subfolder every time the
/// browser opens a directory, and opening every file inside every one of them
/// would be felt on a home folder.
pub fn count(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_rom(p)).count()
}

// ---- Where to look ---------------------------------------------------------

/// Folders searched when none is given or remembered.
///
/// Plain `roms` comes first because it is the project's own: whoever runs Akebia
/// from its working directory has it right there. `~/Juegos` is kept alongside
/// `~/Games` because a Spanish-language desktop creates it with that name.
const CANDIDATES: [&str; 5] =
    ["roms", "~/Juegos", "~/Games", "~/ROMs", "~/.local/share/akebia/roms"];

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| !h.as_os_str().is_empty())
}

fn expand(candidate: &str) -> Option<PathBuf> {
    match candidate.strip_prefix("~/") {
        Some(rest) => Some(home()?.join(rest)),
        None => Some(PathBuf::from(candidate)),
    }
}

/// File where the last folder used is remembered.
///
/// It goes into `XDG_STATE_HOME` and not into the configuration folder because
/// it is not a preference the user wrote: it is a trace of what they did last
/// time. Deleting it loses nothing but that convenience.
pub fn state_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home()?.join(".local/state"),
    };
    Some(base.join("akebia/last-folder"))
}

/// The last folder used, if it still exists.
pub fn remembered(state: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(state).ok()?;
    let dir = PathBuf::from(contents.trim_end_matches('\n'));
    dir.is_dir().then_some(dir)
}

/// Notes the folder the ROM just opened came from.
///
/// It is also called when the ROM came from the command line, and that is the
/// whole point: opening a game with "Open with Akebia" from the file manager
/// teaches the list where the rest of them are.
///
/// A write failure is swallowed silently. It is a convenience, not a saved game:
/// nagging with a warning every time a game is opened would be worse than losing
/// it.
pub fn remember(state: &Path, dir: &Path) {
    if let Some(parent) = state.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = std::fs::write(state, dir.to_string_lossy().as_bytes());
}

/// Which folder the list opens with.
///
/// The order runs from the most explicit to the most guessed: what was asked for
/// on the command line, the last one used, and only then the usual places. If
/// none of them exists the last of the list is returned, which is Akebia's own
/// folder: that way the empty list shows **where** to put the ROMs instead of
/// some arbitrary path.
pub fn initial_dir(requested: Option<&Path>, state: Option<&Path>) -> PathBuf {
    if let Some(dir) = requested {
        return dir.to_owned();
    }
    if let Some(dir) = state.and_then(remembered) {
        return dir;
    }
    let mut last = PathBuf::from(".");
    for candidate in CANDIDATES {
        let Some(path) = expand(candidate) else {
            continue;
        };
        if path.is_dir() {
            return path;
        }
        last = path;
    }
    last
}

/// The path as shown in the header, with `~` instead of `HOME`.
pub fn shorten(dir: &Path) -> String {
    match home().and_then(|h| dir.strip_prefix(h).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// The reverse of [`shorten`]: what somebody typed into the path box.
///
/// Unlike [`expand`], a `~` with no `HOME` behind it is left alone instead of
/// giving up. What was typed is then treated as a literal folder, which will not
/// exist and will be reported as such — better than a box that swallows the text
/// and does nothing.
pub fn expand_user(typed: &str) -> PathBuf {
    let typed = typed.trim();
    match typed.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => match home() {
            Some(home) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(typed),
        },
        _ => PathBuf::from(typed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temporary folder of its own for each test, as in `save`.
    fn temporary(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("akebia-roms-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Synthetic 32 KiB ROM with whatever cartridge type is asked for.
    fn rom(cart_type: u8, cgb: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 32 * 1024];
        rom[0x0143] = cgb;
        rom[0x0147] = cart_type;
        rom
    }

    #[test]
    fn it_lists_only_roms_and_in_order() {
        let dir = temporary("scan");
        std::fs::write(dir.join("zeta.gb"), rom(0x00, 0x00)).unwrap();
        std::fs::write(dir.join("alfa.GBC"), rom(0x1B, 0xC0)).unwrap();
        std::fs::write(dir.join("Beta.gb"), rom(0x13, 0x00)).unwrap();
        // What lives next to the ROMs and is not one.
        std::fs::write(dir.join("alfa.sav"), [0u8; 8]).unwrap();
        std::fs::write(dir.join("capture.ppm"), [0u8; 8]).unwrap();
        std::fs::create_dir(dir.join("subfolder.gb")).unwrap();

        let roms = scan(&dir);
        let names: Vec<&str> = roms.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alfa", "Beta", "zeta"], "sorted and without the .sav");
        assert_eq!(roms[0].mapper, "MBC5", "an uppercase extension counts too");
        assert_eq!(roms[0].cgb, CgbSupport::Only);
        assert_eq!(roms[2].mapper, "ROM ONLY");
    }

    #[test]
    fn a_rom_with_no_header_is_listed_and_flagged() {
        let dir = temporary("truncated");
        std::fs::write(dir.join("broken.gb"), [0u8; 16]).unwrap();

        let roms = scan(&dir);
        assert_eq!(roms.len(), 1, "it is listed all the same: the user sees the file");
        assert_eq!(roms[0].mapper, "?");
    }

    /// Both machines appear in one list, and an Advance cartridge is named as
    /// one rather than having the older machine's header read out of it.
    #[test]
    fn advance_cartridges_are_listed_and_named() {
        let dir = temporary("advance");
        std::fs::write(dir.join("alpha.gba"), [0u8; 512]).unwrap();
        std::fs::write(dir.join("beta.gb"), rom(0x00, 0x00)).unwrap();

        let roms = scan(&dir);
        assert_eq!(roms.len(), 2, "both machines, one list");
        assert_eq!(roms[0].name, "alpha");
        assert_eq!(roms[0].mapper, "Advance");
        assert_eq!(roms[1].mapper, "ROM ONLY");
    }

    #[test]
    fn a_folder_that_does_not_exist_gives_an_empty_list() {
        assert!(scan(Path::new("/does/not/exist/this/folder")).is_empty());
    }

    #[test]
    fn the_folder_is_remembered_from_one_session_to_the_next() {
        let dir = temporary("state");
        let state = dir.join("last-folder");
        let roms = dir.join("my roms");
        std::fs::create_dir(&roms).unwrap();

        assert_eq!(remembered(&state), None, "with no file there is nothing to remember");

        remember(&state, &roms);
        assert_eq!(remembered(&state), Some(roms.clone()), "spaces included");

        // What was asked for on the command line beats what was remembered.
        let requested = dir.join("other");
        assert_eq!(initial_dir(Some(&requested), Some(&state)), requested);
        assert_eq!(initial_dir(None, Some(&state)), roms);

        // And a folder that no longer exists is not offered.
        std::fs::remove_dir(&roms).unwrap();
        assert_eq!(remembered(&state), None);
    }

    #[test]
    fn remembering_creates_the_state_files_directory() {
        let dir = temporary("new-state");
        let state = dir.join("not/created/last-folder");
        remember(&state, &dir);
        assert_eq!(remembered(&state), Some(dir));
    }

    #[test]
    fn the_path_is_shortened_with_a_tilde() {
        let Some(home) = home() else { return };
        assert_eq!(shorten(&home), "~");
        assert_eq!(shorten(&home.join("Games")), "~/Games");
        assert_eq!(shorten(Path::new("/opt/roms")), "/opt/roms");
    }

    #[test]
    fn a_typed_path_comes_back_from_its_tilde() {
        let Some(home) = home() else { return };
        assert_eq!(expand_user("~"), home);
        assert_eq!(expand_user("~/Games"), home.join("Games"));
        assert_eq!(expand_user("  ~/Games  "), home.join("Games"), "trimmed");
        assert_eq!(expand_user("/opt/roms"), PathBuf::from("/opt/roms"));
        // Only a `~` on its own or followed by a separator is HOME. `~ana` is
        // another user's folder in the shell, and expanding it here would be a
        // lie: this does not read the password database.
        assert_eq!(expand_user("~ana/roms"), PathBuf::from("~ana/roms"));
        // What `shorten` writes, this reads back.
        assert_eq!(expand_user(&shorten(&home.join("a/b"))), home.join("a/b"));
    }

    #[test]
    fn subfolders_are_listed_with_how_many_games_each_holds() {
        let dir = temporary("browse");
        std::fs::create_dir(dir.join("Games")).unwrap();
        std::fs::create_dir(dir.join("empty")).unwrap();
        std::fs::create_dir(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("Games/one.gb"), rom(0x00, 0x00)).unwrap();
        std::fs::write(dir.join("Games/two.GBC"), rom(0x00, 0xC0)).unwrap();
        std::fs::write(dir.join("Games/one.sav"), [0u8; 8]).unwrap();
        // A file next to the folders is not one of them.
        std::fs::write(dir.join("loose.gb"), rom(0x00, 0x00)).unwrap();

        let folders = subfolders(&dir);
        let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["empty", "Games"], "sorted, and without the hidden one");
        assert_eq!(folders[1].roms, 2, "the .sav does not count");
        assert_eq!(folders[0].roms, 0);

        assert_eq!(count(&dir), 1, "only what is loose in the folder itself");
        assert!(subfolders(Path::new("/does/not/exist/this/folder")).is_empty());
        assert_eq!(count(Path::new("/does/not/exist/this/folder")), 0);
    }
}
