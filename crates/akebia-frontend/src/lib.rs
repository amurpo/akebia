//! Adapters between the outside world and `akebia-core`.
//!
//! Everything here **translates**: files, window, keyboard, sound card and
//! system clock on one side; the surface `akebia-core` exposes on the other. All
//! the emulation logic lives in the core; there is not a single Game Boy detail
//! here.
//!
//! # Why this is a library and not only a binary
//!
//! Android does not run binaries: it loads a `cdylib` out of the APK and calls
//! `android_main` inside it. That entry point cannot live in a `main.rs`, so
//! everything needed to put a window up lives here, where both the command line
//! ([`crate::main`], `src/main.rs`) and the Android entry point
//! (`crates/akebia-android`) can reach it.
//!
//! What stays behind in the binary is what a telephone has no use for: parsing
//! arguments, drawing in the terminal and the diagnostic commands.

pub mod console;
pub mod app;
pub mod args;
pub mod audio;
pub mod bios;
pub mod debug;
pub mod net;
pub mod remote;
pub mod roms;
pub mod save;

use std::io::Write;
use std::path::Path;

use akebia_core::cpu::Fault;
use akebia_core::ports::Palette;
use akebia_core::GameBoy;

use args::Args;

/// Reads the ROM and powers the console on.
pub fn load(path: &Path, args: &Args) -> Result<GameBoy, String> {
    let rom = std::fs::read(path)
        .map_err(|e| format!("could not read {}: {e}{}", path.display(), suggestions(path)))?;

    // The model comes from the header unless it is forced on the command line.
    let mut gb = match args.force_model {
        Some(model) => GameBoy::new_as(rom, model),
        None => GameBoy::new(rom),
    }
    .map_err(|e| e.to_string())?;
    apply_palette(&mut gb, args);
    // Here and not in the window's settings, so that `--dump --wav` writes the
    // same audio that would have been played.
    gb.set_speaker_filter(!args.raw_audio);
    Ok(gb)
}

/// Loads whichever machine the file is for.
///
/// The extension decides, which is what a file manager and a person both go by.
/// An Advance cartridge that is misnamed will be handed to the Game Boy and
/// rejected there, and that is the right way round: a wrong name is a mistake
/// worth reporting rather than quietly working around.
pub fn load_console(path: &Path, args: &Args) -> Result<console::Console, String> {
    if !console::is_advance(path) {
        return load(path, args).map(|gb| console::Console::Gb(Box::new(gb)));
    }

    let rom = std::fs::read(path)
        .map_err(|e| format!("could not read {}: {e}{}", path.display(), suggestions(path)))?;
    let mut gba = akebia_gba::Gba::new();
    gba.load_rom(&rom);

    // The one that was named, or the one that can be found where they are kept.
    // Games do not merely *call* BIOS routines: every interrupt they take goes
    // through it, so a machine without one runs a game as far as its first
    // interrupt and no further. Looking is what keeps that from being a black
    // window nobody can explain — see [`bios`], and [`bios::ADVICE`] for what
    // is said when the looking turns up nothing.
    if let Some(image) = bios::image(args.bios.as_deref(), path)? {
        gba.load_bios(&image);
    }
    gba.reset();
    Ok(console::Console::Gba(Box::new(gba)))
}

/// Notes which folder the ROM came from, so the list opens there next time.
///
/// It is done even when the path came from the command line, and that is the
/// useful part: opening a game with "Open with Akebia" from the file manager
/// leaves the list pointing at the folder where the rest of them are.
pub fn remember_dir(rom: &Path) {
    let Ok(absolute) = std::fs::canonicalize(rom) else {
        return;
    };
    if let Some(dir) = absolute.parent() {
        remember_folder(dir);
    }
}

/// Notes a folder chosen outright, without any ROM having been opened from it.
///
/// This is what the folder browser calls, and it is why the browser exists: when
/// none of the guessed folders held any games there was no way to correct the
/// guess that survived closing the window, because the only thing that taught
/// Akebia a folder was opening a game from it.
pub fn remember_folder(dir: &Path) {
    let Some(state) = roms::state_path() else {
        return;
    };
    // The absolute path is stored: recording a bare "roms" would point somewhere
    // else depending on where Akebia is run from next time.
    let Ok(absolute) = std::fs::canonicalize(dir) else {
        return;
    };
    roms::remember(&state, &absolute);
}

/// Looks for ROMs whose name starts like the path that was not found.
///
/// ROM names carry spaces and parentheses, so they get written in quotes and it
/// is easy to lose a piece at the end —the extension, above all—. When that
/// happens, the bare error says nothing useful and one has to go and look at the
/// directory; with the full name right there, it gets fixed in one go.
fn suggestions(rom: &std::path::Path) -> String {
    const MAX: usize = 3;

    let dir = rom.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or(std::path::Path::new("."));
    let Some(wanted) = rom.file_name().and_then(|n| n.to_str()) else {
        return String::new();
    };
    // Without the extension, which is exactly the part usually missing.
    let stem = wanted.trim_end_matches('.').rsplit_once('.').map_or(wanted, |(a, _)| a);

    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut similar: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(stem) && n != wanted)
        // Next to the ROM live its `.sav` and its captures, which cannot be
        // opened.
        .filter(|n| n.ends_with(".gb") || n.ends_with(".gbc"))
        .take(MAX)
        .collect();
    similar.sort();

    if similar.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\nDid you mean?");
    for n in similar {
        s.push_str(&format!("\n  {}", dir.join(n).display()));
    }
    s
}

/// Pushes the four DMG-mode shades chosen on the command line into the core.
///
/// In CGB mode it has no effect: the game supplies the colours.
fn apply_palette(gb: &mut GameBoy, args: &Args) {
    let palette = if args.grayscale { Palette::GRAYSCALE } else { Palette::DMG };
    gb.set_dmg_shades(palette.shades);
}

/// Opens the audio, or warns on stderr and carries on in silence.
///
/// Having no sound card is no reason to stop someone from playing.
pub fn open_audio(gb: &mut GameBoy, enabled: bool) -> Option<audio::AudioOutput> {
    if !enabled {
        return None;
    }
    match audio::AudioOutput::new() {
        Ok(output) => {
            gb.set_sample_rate(output.sample_rate());
            Some(output)
        }
        Err(message) => {
            eprintln!("warning: no sound ({message})");
            None
        }
    }
}

/// Hands the frame's audio to the device, or discards it if there is none.
pub fn drain_audio(gb: &mut GameBoy, output: Option<&audio::AudioOutput>) {
    match output {
        Some(output) => {
            let samples = gb.take_audio();
            output.push(&samples);
        }
        // Uncollected, the APU's buffer would grow for the whole session.
        None => gb.discard_audio(),
    }
}

/// Spills to stderr whatever the game wrote to the serial port.
pub fn drain_serial(gb: &mut GameBoy, enabled: bool) {
    if !enabled {
        return;
    }
    let bytes = gb.take_serial_output();
    if !bytes.is_empty() {
        let mut err = std::io::stderr();
        let _ = err.write_all(&bytes);
        let _ = err.flush();
    }
}

/// Translates a [`Fault`] into an actionable message.
///
/// While the instruction set is incomplete, this message is the work loop: it
/// says exactly which opcode is missing and where.
pub fn describe_fault(fault: Fault) -> String {
    match fault {
        Fault::Unimplemented { opcode, prefixed, pc } => {
            let prefix = if prefixed { "CB " } else { "" };
            format!(
                "opcode {prefix}0x{opcode:02X} not implemented (PC=0x{pc:04X})\n\
                 Implement it in crates/akebia-core/src/cpu/execute.rs"
            )
        }
        Fault::Illegal { opcode, pc } => {
            format!(
                "illegal opcode 0x{opcode:02X} at PC=0x{pc:04X}\n\
                 On real hardware this hangs the console; it almost always means \
                 the CPU went off the rails earlier."
            )
        }
    }
}
