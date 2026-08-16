//! Argument parsing by hand.
//!
//! No `clap` or anything like it: these are twenty options with no subcommands
//! or completion, and the whole parse fits in one function. The project's rule
//! is that a dependency gets in when it solves something that would genuinely be
//! costly to write —`eframe` for the interface, `cpal` for the audio—, not for
//! convenience.

use std::path::PathBuf;

use akebia_core::Model;

/// What to do with the ROM.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Show the cartridge metadata and exit.
    Info,
    /// Run and draw in the terminal.
    Run { frames: Option<u64> },
    /// Run while dumping every instruction with the register state.
    Trace { instructions: u64 },
    /// Run N frames and dump the last one.
    ///
    /// Unlike `Run`, it uses neither ANSI nor the alternate screen. Without
    /// `--ppm` the output is plain redirectable text comparable with `diff`;
    /// with `--ppm` it is a colour image, which is the only way to check the
    /// CGB palettes.
    Dump { frames: u64 },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// Path to the ROM. Without it, the game list picks one.
    pub rom: Option<PathBuf>,
    pub command: Command,
    /// Folder the game list opens, instead of the usual one.
    pub roms_dir: Option<PathBuf>,
    /// Draw inside the terminal instead of opening a window.
    pub tui: bool,
    /// Window scale factor. 0 = fit to the screen.
    pub scale: u32,
    /// Halve the width so it fits in 80 columns (`--tui` only).
    pub narrow: bool,
    /// Dump to stderr whatever the game writes to the serial port.
    pub serial: bool,
    /// Use the greyscale instead of the DMG greens.
    pub grayscale: bool,
    /// With `--dump`, write a colour PPM image to this path.
    pub ppm: Option<PathBuf>,
    /// Force the console model instead of deducing it from the cartridge.
    pub force_model: Option<Model>,
    /// Run without opening the audio device.
    pub mute: bool,
    /// Skip the low-pass that stands in for the speaker, leaving the DAC's
    /// output as it comes: brighter, and harsher.
    pub raw_audio: bool,
    /// With `--dump`, write the generated audio as WAV to this path.
    pub wav: Option<PathBuf>,
    /// Path to the `.sav`. By default, the ROM's with a different extension.
    pub save: Option<PathBuf>,
    /// Play without loading or writing the saved game.
    pub no_save: bool,
    /// Dump the PPU registers line by line to stderr.
    pub debug: bool,
    /// Path to a Game Boy Advance BIOS image.
    ///
    /// Optional, and worth saying why. An Advance cartridge is entered directly
    /// without one, in the state the BIOS would have left the registers, and
    /// most games run that way — but they call BIOS routines constantly, and
    /// the real thing answers where an empty vector does not.
    pub bios: Option<PathBuf>,
}

/// Default window scale: 160×144 is tiny on a modern display.
const DEFAULT_SCALE: u32 = 4;

pub const USAGE: &str = "\
Usage: akebia [OPTIONS] [ROM]

With no ROM, the game list opens on the last folder used.

Commands:
  --run                 Run the game in a window (default)
  --info                Show the cartridge header and exit
  --trace <N>           Run N instructions dumping the registers
  --dump <N>            Run N frames and dump the last one as ASCII

Options:
  --roms <FOLDER>       Folder the game list opens
  --tui                 Draw in the terminal instead of opening a window
  --scale <N>           Window scale (1..32, or 0 to fit; default 4)
  --grayscale           Greyscale instead of the DMG greens (DMG only)
  --dmg                 Force original Game Boy mode
  --cgb                 Force Game Boy Color mode
  --ppm <FILE>          With --dump, write the frame as a colour PPM image
  --wav <FILE>          With --dump, write the generated audio as WAV
  --save <FILE>         Path to the saved game (default: the ROM with .sav)
  --bios <FILE>         Game Boy Advance BIOS image (optional)
  --no-save             Do not load or write the saved game
  --frames <N>          Stop after N frames
  --narrow              With --tui, draw at 80 columns instead of 160
  --mute                Run without sound
  --raw-audio           Skip the speaker low-pass (brighter, harsher)
  --serial              Dump the serial port to stderr (Blargg tests)
  --debug               Dump the PPU registers per line (D = capture)
  -h, --help            Show this help

Controls:
  Arrows                D-pad
  Z / X                 A / B buttons
  Enter / Backspace     Start / Select
  Escape                Back to the game list
  D                     With --debug, dump the frame and VRAM as images

In the list:
  Arrows / Home / End   Move the cursor
  Enter or double click Play
  Typing                Filter by name
";

/// Result of the parse.
///
/// Asking for the help is not an error: it goes to stdout and exits with code 0,
/// so that `akebia --help | grep ...` works as expected of any command line
/// tool.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Box<Args>),
    Help,
}

impl Args {
    pub fn parse(argv: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
        let mut rom = None;
        let mut command = None;
        let mut frames = None;
        let mut tui = false;
        let mut scale = DEFAULT_SCALE;
        let mut narrow = false;
        let mut serial = false;
        let mut grayscale = false;
        let mut ppm = None;
        let mut force_model = None;
        let mut mute = false;
        let mut raw_audio = false;
        let mut wav = None;
        let mut save = None;
        let mut no_save = false;
        let mut debug = false;
        let mut bios = None;
        let mut roms_dir = None;

        let mut it = argv.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(Parsed::Help),
                "--info" => command = Some(Command::Info),
                "--run" => command = Some(Command::Run { frames: None }),
                "--tui" => tui = true,
                "--grayscale" => grayscale = true,
                "--dmg" => force_model = Some(Model::Dmg),
                "--cgb" => force_model = Some(Model::Cgb),
                "--ppm" => {
                    ppm = Some(PathBuf::from(it.next().ok_or("--ppm needs a path")?));
                }
                "--scale" => {
                    let n = next_number(&mut it, "--scale")?;
                    if n > 32 {
                        return Err("--scale accepts at most 32".to_owned());
                    }
                    scale = n as u32;
                }
                "--trace" => {
                    let n = next_number(&mut it, "--trace")?;
                    command = Some(Command::Trace { instructions: n });
                }
                "--dump" => {
                    let n = next_number(&mut it, "--dump")?;
                    command = Some(Command::Dump { frames: n });
                }
                "--frames" => frames = Some(next_number(&mut it, "--frames")?),
                "--narrow" => narrow = true,
                "--mute" => mute = true,
                "--raw-audio" => raw_audio = true,
                "--wav" => {
                    wav = Some(PathBuf::from(it.next().ok_or("--wav needs a path")?));
                }
                "--serial" => serial = true,
                "--no-save" => no_save = true,
                "--debug" => debug = true,
                "--save" => {
                    save = Some(PathBuf::from(it.next().ok_or("--save needs a path")?));
                }
                "--bios" => {
                    bios = Some(PathBuf::from(it.next().ok_or("--bios needs a path")?));
                }
                "--roms" => {
                    roms_dir = Some(PathBuf::from(it.next().ok_or("--roms needs a folder")?));
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option: {other}"));
                }
                path => {
                    if rom.replace(PathBuf::from(path)).is_some() {
                        return Err("more than one ROM was given".to_owned());
                    }
                }
            }
        }

        let command = match command.unwrap_or(Command::Run { frames: None }) {
            Command::Run { .. } => Command::Run { frames },
            other => other,
        };

        // The menu only replaces the path when there is a window to draw it in
        // and a game to play. The debugging tools still require it: their output
        // has to depend on the ROM and nothing else, and picking it by hand
        // every time would break that comparison.
        if rom.is_none() {
            if command != (Command::Run { frames }) {
                return Err("the ROM path is missing".to_owned());
            }
            if tui {
                return Err("--tui needs the ROM path: the list is drawn in the window".to_owned());
            }
        }

        Ok(Parsed::Run(Box::new(Self {
            rom,
            command,
            roms_dir,
            tui,
            scale,
            narrow,
            serial,
            grayscale,
            ppm,
            force_model,
            mute,
            raw_audio,
            wav,
            save,
            bios,
            no_save,
            debug,
        })))
    }
}

fn next_number(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    it.next()
        .ok_or_else(|| format!("{flag} needs a number"))?
        .parse()
        .map_err(|_| format!("{flag} needs a valid number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses and unwraps the normal case; the help tests use [`Args::parse`]
    /// directly.
    fn parse(args: &[&str]) -> Result<Args, String> {
        match Args::parse(args.iter().map(|s| (*s).to_owned()))? {
            Parsed::Run(args) => Ok(*args),
            Parsed::Help => Err("the help was requested".to_owned()),
        }
    }

    #[test]
    fn the_help_is_not_an_error() {
        let r = Args::parse(["--help".to_owned()]);
        assert_eq!(r, Ok(Parsed::Help), "--help must exit successfully");
    }

    #[test]
    fn the_default_command_is_run() {
        let a = parse(&["game.gb"]).unwrap();
        assert_eq!(a.command, Command::Run { frames: None });
        assert_eq!(a.rom, Some(PathBuf::from("game.gb")));
    }

    #[test]
    fn it_accepts_options_before_and_after_the_rom() {
        let a = parse(&["--narrow", "game.gb", "--serial"]).unwrap();
        assert!(a.narrow && a.serial);
    }

    #[test]
    fn frames_binds_to_the_run_command() {
        let a = parse(&["--frames", "10", "game.gb"]).unwrap();
        assert_eq!(a.command, Command::Run { frames: Some(10) });
    }

    #[test]
    fn with_no_rom_the_game_is_picked_from_the_menu() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.rom, None, "the menu picks it");
        assert_eq!(a.command, Command::Run { frames: None });
    }

    #[test]
    fn the_tools_still_require_the_rom() {
        assert!(parse(&["--info"]).is_err());
        assert!(parse(&["--dump", "10"]).is_err());
        assert!(parse(&["--trace", "10"]).is_err());
        // And the menu cannot be drawn in the terminal.
        assert!(parse(&["--tui"]).is_err());
        assert!(parse(&["--tui", "game.gb"]).is_ok());
    }

    #[test]
    fn the_menu_folder_can_be_given() {
        let a = parse(&["--roms", "/games"]).unwrap();
        assert_eq!(a.roms_dir, Some(PathBuf::from("/games")));
        assert!(parse(&["--roms"]).is_err(), "--roms needs a folder");
    }

    #[test]
    fn it_rejects_unknown_options() {
        assert!(parse(&["--turbo", "game.gb"]).is_err());
    }

    #[test]
    fn the_window_is_the_default_mode() {
        let a = parse(&["game.gb"]).unwrap();
        assert!(!a.tui, "without --tui a window opens");
        assert_eq!(a.scale, DEFAULT_SCALE);
    }

    #[test]
    fn the_speaker_filter_is_on_unless_told_otherwise() {
        assert!(!parse(&["game.gb"]).unwrap().raw_audio, "the console sounds filtered by default");
        assert!(parse(&["--raw-audio", "game.gb"]).unwrap().raw_audio);

        // They are separate: muting says whether the card opens at all, and the
        // filter says how what comes out of it sounds.
        let a = parse(&["--raw-audio", "game.gb"]).unwrap();
        assert!(!a.mute, "--raw-audio does not silence anything");
    }

    #[test]
    fn the_game_is_saved_unless_told_otherwise() {
        let a = parse(&["game.gb"]).unwrap();
        assert!(!a.no_save && a.save.is_none(), "by default, next to the ROM");

        let a = parse(&["--save", "other.sav", "game.gb"]).unwrap();
        assert_eq!(a.save, Some(PathBuf::from("other.sav")));

        assert!(parse(&["--no-save", "game.gb"]).unwrap().no_save);
        assert!(parse(&["game.gb", "--save"]).is_err(), "--save needs a path");
    }

    /// The Advance BIOS is optional, because requiring a file the player has to
    /// find would mean an emulator that mostly does not run.
    #[test]
    fn the_advance_bios_is_optional_and_takes_a_path() {
        assert!(parse(&["game.gba"]).unwrap().bios.is_none());

        let a = parse(&["--bios", "gba_bios.bin", "game.gba"]).unwrap();
        assert_eq!(a.bios, Some(PathBuf::from("gba_bios.bin")));

        assert!(parse(&["game.gba", "--bios"]).is_err(), "--bios needs a path");
    }

    #[test]
    fn it_accepts_a_scale_and_rejects_out_of_range_values() {
        assert_eq!(parse(&["--scale", "2", "game.gb"]).unwrap().scale, 2);
        assert_eq!(parse(&["--scale", "0", "game.gb"]).unwrap().scale, 0);
        assert!(parse(&["--scale", "99", "game.gb"]).is_err());
    }
}
