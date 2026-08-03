//! Desktop and terminal frontends.
//!
//! They are **adapters**: they translate between the outside world (files,
//! windows, terminal, keyboard, system clock) and the surface `akebia-core`
//! exposes. All the emulation logic lives in the core; there is not a single
//! Game Boy detail here.
//!
//! There are three video adapters, interchangeable at run time because the core
//! never met any of them:
//!
//! | Adapter | When | Input |
//! |---|---|---|
//! | `app::FrameSink` | default | real keyboard |
//! | [`terminal::TerminalVideo`] | `--tui` | none yet |
//! | `LastFrame` | `--dump` | none |

mod app;
mod args;
mod audio;
mod debug;
mod roms;
mod save;
mod terminal;

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use akebia_core::cartridge::{CgbSupport, Header};
use akebia_core::cpu::Fault;
use akebia_core::gameboy::FRAMES_PER_SECOND;
use akebia_core::ports::Palette;
use akebia_core::GameBoy;

use args::{Args, Command};

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args::Parsed::Run(args)) => *args,
        Ok(args::Parsed::Help) => {
            print!("{}", args::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("error: {message}\n\n{}", args::USAGE);
            return ExitCode::FAILURE;
        }
    };

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    match args.command {
        // Playing is the only thing that can start without a ROM: the list shown
        // in the window picks it, and that is why `app::run` takes the whole
        // argument set.
        Command::Run { frames } if args.tui => play_tui(&args, frames),
        Command::Run { frames } => app::run(args, frames),
        Command::Info => {
            print_header(load_from_args(&args)?.header());
            Ok(())
        }
        Command::Trace { instructions } => trace(&mut load_from_args(&args)?, instructions),
        Command::Dump { frames } => dump(&mut load_from_args(&args)?, frames, &args),
    }
}

/// The ROM for the commands that require one.
fn load_from_args(args: &Args) -> Result<GameBoy, String> {
    let path = args.rom.as_deref().ok_or("the ROM path is missing")?;
    load(path, args)
}

/// Reads the ROM and powers the console on.
fn load(path: &Path, args: &Args) -> Result<GameBoy, String> {
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

/// Plays drawing in the terminal, the only mode that opens no window.
fn play_tui(args: &Args, frames: Option<u64>) -> Result<(), String> {
    let path = args.rom.as_deref().ok_or("the ROM path is missing")?;
    let mut gb = load(path, args)?;
    gb.set_trace_enabled(args.debug);
    gb.set_write_log_enabled(args.debug);
    remember_dir(path);

    let mut save = if args.no_save {
        None
    } else {
        let sav = args.save.clone().unwrap_or_else(|| save::default_path(path));
        save::SaveFile::open(&mut gb, sav)
    };

    eprintln!("{}  —  Ctrl+C to quit", gb.header().title);
    let result = play_terminal(&mut gb, frames, args, &mut save);

    // It is saved even if the loop ended with a CPU fault: the SRAM is still
    // valid and throwing it away does not help debug anything.
    if let Some(save) = &mut save {
        save.flush_final(&gb);
    }
    result
}

/// Notes which folder the ROM came from, so the list opens there next time.
///
/// It is done even when the path came from the command line, and that is the
/// useful part: opening a game with "Open with Akebia" from the file manager
/// leaves the list pointing at the folder where the rest of them are.
fn remember_dir(rom: &Path) {
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
fn remember_folder(dir: &Path) {
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
fn open_audio(gb: &mut GameBoy, enabled: bool) -> Option<audio::AudioOutput> {
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
fn drain_audio(gb: &mut GameBoy, output: Option<&audio::AudioOutput>) {
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
fn drain_serial(gb: &mut GameBoy, enabled: bool) {
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

// ---- Commands ---------------------------------------------------------------

fn print_header(h: &Header) {
    let ct = h.cartridge_type;
    let yes = |b: bool| if b { "yes" } else { "no" };

    println!("Title           {}", h.title);
    println!("Licensee        {}", h.licensee);
    println!("Version         {}", h.version);
    println!("Type            {} (0x{:02X})", ct.kind.name(), ct.raw);
    println!(
        "  RAM {}   battery {}   RTC {}   rumble {}",
        yes(ct.has_ram),
        yes(ct.has_battery),
        yes(ct.has_rtc),
        yes(ct.has_rumble)
    );
    println!("ROM             {} KiB ({} banks)", h.rom_size / 1024, h.rom_banks());
    println!("SRAM            {} KiB ({} banks)", h.ram_size / 1024, h.ram_banks());
    println!(
        "Game Boy Color  {}",
        match h.cgb {
            CgbSupport::None => "no (DMG only)",
            CgbSupport::Enhanced => "compatible",
            CgbSupport::Only => "CGB exclusive",
        }
    );
    println!("Super Game Boy  {}", yes(h.sgb));
    println!("Region          {}", if h.japanese { "Japan" } else { "international" });
    println!(
        "Header checksum 0x{:02X} computed 0x{:02X} → {}",
        h.header_checksum.0,
        h.header_checksum.1,
        if h.header_checksum_ok() { "correct" } else { "INCORRECT" }
    );
}

/// Dumps instructions with the register state.
///
/// It is the format the community's reference logs use, so a `diff` against the
/// log of a known emulator pinpoints the exact instruction where the CPU
/// diverges.
fn trace(gb: &mut GameBoy, instructions: u64) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    for _ in 0..instructions {
        let pc = gb.cpu().regs.pc;
        // Besides the registers, the interrupt state is dumped: without it there
        // is no telling a real hang from a CPU waiting in HALT for an interrupt
        // nobody is going to request.
        let _ = writeln!(
            out,
            "{}  [{:02X} {:02X} {:02X}]  IME:{} IE:{:02X} IF:{:02X} {:?}",
            gb.cpu().regs,
            gb.peek(pc),
            gb.peek(pc.wrapping_add(1)),
            gb.peek(pc.wrapping_add(2)),
            u8::from(gb.cpu().ime()),
            gb.peek(0xFFFF),
            gb.peek(0xFF0F),
            gb.cpu().power,
        );

        if let Err(fault) = gb.step() {
            let _ = out.flush();
            return Err(describe_fault(fault));
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Runs `frames` frames and writes the last one as plain text.
///
/// It is also the fastest way to run a test ROM: with no window or ANSI
/// encoding, `--serial` sends whatever the ROM reports to stderr. Blargg's
/// suites write their results there.
fn dump(gb: &mut GameBoy, frames: u64, args: &Args) -> Result<(), String> {
    // `--debug` here writes the same capture as F2 in the window, on finishing.
    // It serves to diagnose without depending on reaching the scene by hand.
    if args.debug {
        gb.set_trace_enabled(true);
        gb.set_write_log_enabled(true);
    }

    /// Character ramp, from lightest to darkest.
    ///
    /// With real colour there are no longer "four shades" to map, so each
    /// pixel's luminance is computed and quantised onto the ramp. On a DMG ROM
    /// the result is identical to before.
    const RAMP: [u8; 4] = *b" .+#";

    let mut sink = LastFrame::default();
    let mut audio = Vec::new();
    for _ in 0..frames {
        let result = gb.run_frame(&mut sink);
        match &args.wav {
            Some(_) => audio.extend(gb.take_audio()),
            None => gb.discard_audio(),
        }
        drain_serial(gb, args.serial);
        result.map_err(describe_fault)?;
    }

    if let Some(path) = &args.wav {
        write_wav(path, &audio, akebia_core::apu::DEFAULT_SAMPLE_RATE)?;
    }

    if args.debug {
        let path = debug::snapshot(gb, std::path::Path::new("."), 0)?;
        eprintln!("capture written next to {}", path.display());
    }

    if let Some(path) = &args.ppm {
        return debug::write_ppm(
            path,
            akebia_core::SCREEN_WIDTH,
            akebia_core::SCREEN_HEIGHT,
            &sink.pixels,
        );
    }

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let mut row = Vec::with_capacity(akebia_core::SCREEN_WIDTH + 1);

    for y in 0..akebia_core::SCREEN_HEIGHT {
        row.clear();
        for x in 0..akebia_core::SCREEN_WIDTH {
            let [r, g, b] = sink.pixels[y * akebia_core::SCREEN_WIDTH + x].to_rgb888();
            // Perceptual luminance, in integers: 30 % red, 59 % green, 11 % blue.
            let luma = (r as u32 * 30 + g as u32 * 59 + b as u32 * 11) / 100;
            row.push(RAMP[3 - (luma * 4 / 256) as usize]);
        }
        row.push(b'\n');
        let _ = out.write_all(&row);
    }
    out.flush().map_err(|e| e.to_string())
}

/// Writes the samples as a 16-bit stereo WAV.
///
/// The RIFF format fits in twenty lines and needs no dependency. It is the audio
/// equivalent of `--ppm`: the only way to check the APU without depending on
/// there being speakers.
fn write_wav(
    path: &std::path::Path,
    samples: &[akebia_core::StereoSample],
    sample_rate: u32,
) -> Result<(), String> {
    let data_bytes = (samples.len() * 4) as u32;
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // size of the fmt block
    wav.extend_from_slice(&1u16.to_le_bytes()); // uncompressed PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 4).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&4u16.to_le_bytes()); // bytes per stereo sample
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per channel
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());

    for s in samples {
        for channel in [s.left, s.right] {
            let v = (channel.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            wav.extend_from_slice(&v.to_le_bytes());
        }
    }

    std::fs::write(path, wav).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Keeps a copy of the last presented frame.
struct LastFrame {
    pixels: Vec<akebia_core::Rgb555>,
}

impl Default for LastFrame {
    fn default() -> Self {
        Self {
            pixels: vec![
                akebia_core::Rgb555::WHITE;
                akebia_core::SCREEN_WIDTH * akebia_core::SCREEN_HEIGHT
            ],
        }
    }
}

impl akebia_core::ports::VideoOutput for LastFrame {
    fn present(&mut self, frame: &akebia_core::FrameBuffer) {
        self.pixels.copy_from_slice(frame.as_slice());
    }
}

/// Keeps the pace of 59.73 frames per second.
///
/// The target instant advances in fixed steps instead of being computed as
/// "now plus 16.74 ms". The difference matters: with the second form, the time
/// each frame takes to emulate is **added** to the period and the game runs
/// slow cumulatively.
struct FramePacer {
    frame_time: Duration,
    next: Instant,
}

impl FramePacer {
    fn new() -> Self {
        Self { frame_time: Duration::from_secs_f64(1.0 / FRAMES_PER_SECOND), next: Instant::now() }
    }

    fn wait(&mut self) {
        self.next += self.frame_time;
        match self.next.checked_duration_since(Instant::now()) {
            Some(wait) => std::thread::sleep(wait),
            // We are running late: the debt is dropped instead of speeding up
            // afterwards.
            None => self.next = Instant::now(),
        }
    }
}

/// Main loop drawing inside the terminal. No keyboard input.
fn play_terminal(
    gb: &mut GameBoy,
    max_frames: Option<u64>,
    args: &Args,
    save: &mut Option<save::SaveFile>,
) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut video = terminal::TerminalVideo::new(stdout.lock(), args.narrow)
        .map_err(|e| format!("could not set up the terminal: {e}"))?;
    let audio = open_audio(gb, !args.mute);
    let mut pacer = FramePacer::new();
    let mut trace = args.debug.then(debug::LiveTrace::new);

    loop {
        let result = gb.run_frame(&mut video);
        drain_audio(gb, audio.as_ref());
        drain_serial(gb, args.serial);
        // It has to be drained every frame even if nothing is printed: otherwise
        // the core's capture would pile up 144 lines per frame without end.
        if let Some(trace) = trace.as_mut() {
            trace.frame(gb.take_frame_trace());
        }
        // A Ctrl+C here kills the process without going through the final save,
        // so the autosave is the only thing protecting the game in this mode.
        if let Some(save) = save.as_mut() {
            save.tick(gb);
        }
        result.map_err(describe_fault)?;

        if max_frames.is_some_and(|max| video.frames() >= max) {
            return Ok(());
        }
        pacer.wait();
    }
}

/// Translates a [`Fault`] into an actionable message.
///
/// While the instruction set is incomplete, this message is the work loop: it
/// says exactly which opcode is missing and where.
fn describe_fault(fault: Fault) -> String {
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
