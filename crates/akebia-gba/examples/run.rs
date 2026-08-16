//! Runs a cartridge headless and says where the processor ended up.
//!
//! ```text
//! cargo run --release -p akebia-gba --example run -- arm.gba
//! cargo run --release -p akebia-gba --example run -- game.gba --bios gba_bios.bin
//! cargo run --release -p akebia-gba --example run -- arm.gba --trace 40
//! cargo run --release -p akebia-gba --example run -- hello.gba --ppm out.ppm
//! ```
//!
//! # Where it starts, and why not at the beginning
//!
//! With a BIOS given, `--boot` starts the machine where the hardware does, at
//! address zero, and lets the sixteen kibibytes run: the logo check, the
//! animation, the jump into the cartridge. That wants a picture unit and timers
//! to answer, because the animation waits on them, so it is not the default.
//!
//! Without `--boot` the machine starts in the cartridge with the registers the
//! BIOS would have left. A BIOS given is still loaded and still used — it is
//! only the *boot* that is skipped, and every `SWI` a game makes from then on
//! goes to the real handler.
//!
//! # What this is for
//!
//! Measuring the processor against a test ROM, which is the only way to know
//! whether it is right. A suite like `jsmolka/gba-tests` runs its checks in
//! order and, when one fails, stops dead in a branch to itself with the number
//! of the failing check left in a register. That is the shape this looks for:
//! **a branch to its own address** is not a hang to be reported as a timeout,
//! it is the ROM's way of saying it has finished and has something to tell.
//!
//! So the three ways out are all worth telling apart, and all three are
//! printed: the processor faulted, the ROM settled, or neither happened before
//! the step limit — which is the only one that means nothing at all.
//!
//! That shape is not the only way a suite finishes. Some run their checks and
//! then wait for the beam in the BIOS's own loop, which never returns and is
//! not a branch to itself — so a run that reaches the step limit is not a
//! failure by itself. The way to tell is to run twice with very different
//! limits: identical registers and identical video memory mean it is done and
//! is only waiting.
//!
//! # Seeing what it drew
//!
//! `--ppm` writes the picture out, which is the only way to check a renderer
//! and the only way to read a suite that reports on screen — which is how
//! `jsmolka/gba-tests` reports, in words rather than in a register.

use std::process::ExitCode;

use akebia_gba::bus::Memory;
use akebia_gba::cpu::{Bus, Cpu, Fault};
use akebia_gba::keypad::Button;
use akebia_gba::ppu::FRAME_CYCLES;
use akebia_gba::CLOCK_HZ;

/// Where a cartridge is mapped, and so where a machine with one starts.
const ROM_BASE: u32 = 0x0800_0000;

/// How long a run goes for when nothing was asked for, in frames.
///
/// # Why this is counted in frames, and why it is this many
///
/// Because the two things this is pointed at want opposite budgets, and only
/// one of them is expensive. A test suite branches to itself the moment it
/// finishes and stops the run there — a fifth of a second, whatever the budget
/// is. A cartridge never stops, so it runs the budget out every time, and
/// anything under about nine hundred frames is a run that ends before the game
/// has finished showing its publisher's logo.
///
/// So a budget generous enough to be useful costs the suites nothing at all and
/// costs a cartridge fifteen seconds. That is the right way round: the default
/// should be the one that answers a question, and the flag should be for the
/// person in a hurry.
const DEFAULT_FRAMES: u64 = 900;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: run <rom.gba> [--bios FILE] [--boot] [--steps N] [--frames N]");
        eprintln!("                       [--trace N] [--from N]");
        eprintln!("                       [--press BUTTONS] [--press-at N] [--ppm FILE]");
        eprintln!("  BUTTONS: comma-separated, from a b select start right left up down r l");
        return ExitCode::FAILURE;
    };

    // Only what was asked for. A frame count sets its own step budget, and
    // that cannot be worked out until the arguments are all read.
    let mut limit: Option<u64> = None;
    let mut until_frame: Option<u64> = None;
    let mut trace = 0u64;
    // Where to begin tracing. A fault a million steps in cannot be seen from
    // the first forty instructions, and printing all million is not reading.
    let mut from = 0u64;
    let mut bios = None;
    let mut ppm = None;
    let mut boot = false;
    // Which buttons to press, and when. A title screen waits for a person, and
    // without this there is no way to be one.
    let mut press: Vec<Button> = Vec::new();
    let mut press_at = 0u64;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--boot" => boot = true,
            "--bios" | "--ppm" => match args.next() {
                Some(path) => {
                    if flag == "--bios" {
                        bios = Some(path);
                    } else {
                        ppm = Some(path);
                    }
                }
                None => {
                    eprintln!("{flag} wants a file");
                    return ExitCode::FAILURE;
                }
            },
            "--steps" | "--frames" | "--trace" | "--from" | "--press-at" => {
                let Some(n) = args.next().and_then(|v| v.parse().ok()) else {
                    eprintln!("{flag} wants a number");
                    return ExitCode::FAILURE;
                };
                match flag.as_str() {
                    "--steps" => limit = Some(n),
                    "--frames" => until_frame = Some(n),
                    "--trace" => trace = n,
                    "--from" => from = n,
                    _ => press_at = n,
                }
            }
            "--press" => {
                let Some(names) = args.next() else {
                    eprintln!("--press wants a button or a comma-separated list of them");
                    return ExitCode::FAILURE;
                };
                for name in names.split(',') {
                    match button_named(name) {
                        Some(button) => press.push(button),
                        None => {
                            eprintln!("no button called {name:?}");
                            return ExitCode::FAILURE;
                        }
                    }
                }
            }
            _ => {
                eprintln!("unrecognised option: {flag}");
                return ExitCode::FAILURE;
            }
        }
    }

    if boot && bios.is_none() {
        eprintln!("--boot wants a BIOS to boot from");
        return ExitCode::FAILURE;
    }

    let image = match std::fs::read(&path) {
        Ok(image) => image,
        Err(why) => {
            eprintln!("{path}: {why}");
            return ExitCode::FAILURE;
        }
    };

    let mut mem = Memory::new();
    mem.load_rom(&image);
    println!("{path}: {} bytes", mem.rom_len());
    // Which save chip was found, because a cartridge answered by the wrong one
    // fails in a way that looks nothing like saving: it says on screen that the
    // board is not installed and never reaches a menu.
    println!("  save: {}", mem.save().kind());

    if let Some(path) = &bios {
        match std::fs::read(path) {
            Ok(image) => {
                // Sixteen kibibytes exactly. Anything else is not this BIOS and
                // saying so beats loading a prefix of it and wondering later.
                if image.len() != akebia_gba::bus::BIOS_LEN {
                    eprintln!("{path}: {} bytes, expected {}", image.len(), akebia_gba::bus::BIOS_LEN);
                    return ExitCode::FAILURE;
                }
                mem.load_bios(&image);
                println!("{path}: loaded");
            }
            Err(why) => {
                eprintln!("{path}: {why}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("no BIOS: a call into it will go to an empty vector");
    }

    let mut cpu = Cpu::new();
    // Straight into the cartridge, in the mode and with the stacks the BIOS
    // would have left. A real machine runs sixteen kibibytes of BIOS first; a
    // test ROM does not care what happened before it, only that the registers
    // it is about to use point somewhere.
    cpu.regs.set_mode(akebia_gba::Mode::Supervisor);
    cpu.regs.set(13, 0x0300_7FE0);
    cpu.regs.set_mode(akebia_gba::Mode::Irq);
    cpu.regs.set(13, 0x0300_7FA0);
    cpu.regs.set_mode(akebia_gba::Mode::System);
    cpu.regs.set(13, 0x0300_7F00);

    if boot {
        // Where the hardware starts. Everything above was the BIOS's to set and
        // it is about to set it again.
        cpu.regs.set_mode(akebia_gba::Mode::Supervisor);
        cpu.regs.set_pc(0);
        println!("booting from the BIOS");
    } else {
        cpu.regs.set_pc(ROM_BASE);
    }

    // A frame count pays for itself in steps. One step charges one cycle, so a
    // frame is `FRAME_CYCLES` of them at the most — at the most, because any
    // honest timing later can only make an instruction cost more than one, and
    // so make a frame cost fewer steps than this.
    // Frames unless a step budget was named outright, and a frame count pays
    // for itself in steps. One step charges one cycle, so a frame costs at most
    // `FRAME_CYCLES` of them — at most, because any honest timing later can only
    // make an instruction cost more than one, and so make a frame cost fewer
    // steps than this.
    let until_frame = match (limit, until_frame) {
        (_, Some(frames)) => Some(frames),
        // A bare `--steps` means that and nothing else.
        (Some(_), None) => None,
        (None, None) => Some(DEFAULT_FRAMES),
    };
    let limit = limit.unwrap_or_else(|| {
        (until_frame.unwrap_or(DEFAULT_FRAMES) + 1).saturating_mul(u64::from(FRAME_CYCLES))
    });

    let plan = Plan { limit, until_frame, from, trace, press, press_at };
    let outcome = run(&mut cpu, &mut mem, &plan);
    report(&cpu, &mem, &outcome);

    if let Some(path) = &ppm {
        let lines = draw_one_more_frame(&mut cpu, &mut mem);
        println!("\n  drew a whole frame in {lines} further steps");
        if let Err(why) = write_ppm(path, mem.ppu().frame()) {
            eprintln!("{path}: {why}");
            return ExitCode::FAILURE;
        }
        println!("\n  wrote {path}");
    }

    match outcome {
        // Both of these are the run doing what it was asked. A cartridge that
        // is still going after the frames it was given has not failed at
        // anything — it is a game, and games do not stop.
        Outcome::Settled { .. } | Outcome::Reached { .. } => ExitCode::SUCCESS,
        Outcome::Faulted(..) | Outcome::RanOn => ExitCode::FAILURE,
    }
}

enum Outcome {
    /// The processor could not carry on.
    ///
    /// The step count comes with it because it is the useful half: a fault at
    /// the first instruction and a fault at the six-hundredth mean very
    /// different things about what is working.
    Faulted(Fault, u64),
    /// The ROM branched to itself, which is how a test suite says it is done.
    Settled { at: u32, steps: u64 },
    /// The frame count asked for was swept.
    Reached { steps: u64 },
    /// The step budget ran out with the machine still going.
    ///
    /// On a test suite this is a hang. On a game it is usually just the budget:
    /// a cartridge needs hundreds of frames to get through its opening, and the
    /// default is worth about three seconds. Which of the two it is cannot be
    /// told from here, so the report says how far it got and leaves the reading
    /// to whoever asked.
    RanOn,
}

/// How long a press lasts, in steps: about ten frames' worth.
///
/// A button held for ever is not a person, and a game that reads one is a game
/// that never sees it come up again — a title screen waiting for Start would
/// take it and then find Start still down on the menu behind it. A pulse is
/// what a person is.
const PRESS_STEPS: u64 = 280_896 * 10;

/// The button a name means, for the command line.
fn button_named(name: &str) -> Option<Button> {
    Some(match name {
        "a" => Button::A,
        "b" => Button::B,
        "select" => Button::Select,
        "start" => Button::Start,
        "right" => Button::Right,
        "left" => Button::Left,
        "up" => Button::Up,
        "down" => Button::Down,
        "r" => Button::R,
        "l" => Button::L,
        _ => return None,
    })
}

/// What a run has been asked to do.
///
/// Together rather than as seven arguments, because they are one idea: the
/// budget, where to stop, what to print on the way and what to press.
struct Plan {
    /// The most steps to take, whatever else happens.
    limit: u64,
    /// Stop once this many frames have been swept, if a count was given. The
    /// natural unit for a game, where the step budget is the natural unit for a
    /// test suite that finishes in a fraction of one frame.
    until_frame: Option<u64>,
    /// Where the trace begins, and how much of it.
    from: u64,
    trace: u64,
    /// What to press, and when.
    press: Vec<Button>,
    press_at: u64,
}

fn run(cpu: &mut Cpu, mem: &mut Memory, plan: &Plan) -> Outcome {
    let Plan { limit, until_frame, from, trace, press, press_at } = plan;
    let (limit, from, trace, press_at) = (*limit, *from, *trace, *press_at);

    for step in 0..limit {
        // Down at the given step, up ten frames later. Both edges matter: a
        // game watches for the button coming up as often as for it going down.
        if !press.is_empty() && (step == press_at || step == press_at + PRESS_STEPS) {
            let down = step == press_at;
            for button in press {
                mem.keypad_mut().set(*button, down);
            }
        }

        let before = cpu.regs.pc();

        if step >= from && step < from + trace {
            let flag = |on: bool, name: char| if on { name } else { '-' };
            println!(
                "{step:9}  {before:08X}  {:08X}  {}{}{}{}  {:?}{}  r0={:08X} r1={:08X} r12={:08X}",
                mem.peek32(before),
                flag(cpu.regs.n(), 'N'),
                flag(cpu.regs.z(), 'Z'),
                flag(cpu.regs.c(), 'C'),
                flag(cpu.regs.v(), 'V'),
                cpu.regs.mode(),
                if cpu.regs.thumb() { " T" } else { "  " },
                cpu.regs.get(0),
                cpu.regs.get(1),
                cpu.regs.get(12),
            );
        }

        if let Err(fault) = cpu.step(mem) {
            return Outcome::Faulted(fault, step);
        }

        // A branch to its own address. Nothing else can move the counter back
        // to where it already was — except a processor that has been halted,
        // which fetches nothing and so leaves the counter exactly where a
        // branch to itself would. Telling the two apart matters: a halt is a
        // game waiting for the picture unit and about to carry on, and calling
        // it a finish stops the run three frames into a cartridge that was
        // working.
        if cpu.regs.pc() == before && !mem.interrupts().halted() {
            return Outcome::Settled { at: before, steps: step + 1 };
        }

        if let Some(target) = until_frame.as_ref() {
            if mem.ppu().frames() >= *target {
                return Outcome::Reached { steps: step + 1 };
            }
        }
    }
    Outcome::RanOn
}

/// Keeps the machine running until it has swept one whole frame, and says how
/// many steps that took.
///
/// # Why a capture needs this
///
/// Because a line is drawn as the beam passes it and is never revisited, so the
/// picture at any moment is made of whatever each line's registers said at
/// different times. A ROM that fills video memory and then stops has been
/// drawing blank lines the entire time it was working: every one of the 160 was
/// swept before the contents arrived, and capturing there gives an empty screen
/// that looks exactly like a renderer that does not work.
///
/// This is not a fudge to make the picture look better. It is the difference
/// between photographing the screen while a game is still loading and
/// photographing it afterwards.
fn draw_one_more_frame(cpu: &mut Cpu, mem: &mut Memory) -> u64 {
    // Two boundaries and not one: starting mid-frame, the first only finishes
    // the frame that was already partly swept with the old contents.
    let until = mem.ppu().frames() + 2;
    let mut steps = 0;
    while mem.ppu().frames() < until {
        if cpu.step(mem).is_err() {
            break;
        }
        steps += 1;
    }
    steps
}

/// Writes the picture out as an image, which is the only way to check a
/// renderer.
///
/// # Turning five bits into eight
///
/// The top three bits of the result are the bottom three of the source rather
/// than zeros. Shifting alone would make the brightest a channel can be 248
/// instead of 255, so nothing would ever be quite white and every picture would
/// come out slightly dark — a difference small enough to look like a bad
/// palette and never like an arithmetic mistake.
fn write_ppm(path: &str, frame: &[u16]) -> std::io::Result<()> {
    use std::io::Write;

    let width = akebia_gba::SCREEN_WIDTH;
    let mut out = Vec::with_capacity(frame.len() * 3 + 32);
    write!(out, "P6\n{width} {}\n255\n", akebia_gba::SCREEN_HEIGHT)?;
    for &colour in frame {
        for channel in 0..3 {
            let five = (colour >> (channel * 5)) & 0x1F;
            out.push(((five << 3) | (five >> 2)) as u8);
        }
    }
    std::fs::write(path, out)
}

/// How long the machine thinks it has been running.
///
/// Steps are the emulator's unit and seconds are the game's, and the gap
/// between them is the whole reason a run that looks stuck usually is not: the
/// default budget is three seconds of machine time, which is not enough for a
/// cartridge to finish showing its publisher's logo.
fn machine_time(mem: &Memory) -> String {
    let seconds = mem.cycles() as f64 / f64::from(CLOCK_HZ);
    if seconds < 1.0 {
        format!("{:.0} ms", seconds * 1000.0)
    } else {
        format!("{seconds:.1} s")
    }
}

fn report(cpu: &Cpu, mem: &Memory, outcome: &Outcome) {
    let irq = mem.interrupts();
    println!();
    match outcome {
        Outcome::Faulted(fault, steps) => println!("stopped after {steps} steps: {fault}"),
        Outcome::Settled { at, steps } => {
            println!("settled at {at:08X} after {steps} steps");
            println!("  the instruction there: {:08X}", mem.peek32(*at));
        }
        Outcome::Reached { steps } => {
            println!("swept {} frames in {steps} steps", mem.ppu().frames());
        }
        // Worth spelling out, because the state below reads like a hang and
        // usually is not one. A game asleep in the BIOS waiting for the beam is
        // a game doing exactly what a game does; it just has not been given
        // long enough to get anywhere.
        Outcome::RanOn => {
            println!("the step limit ran out with the machine still running");
            println!("  {} frames is about {}", mem.ppu().frames(), machine_time(mem));
        }
    }

    println!();
    for row in 0..4 {
        let cells: Vec<String> = (0..4)
            .map(|column| {
                let index = row * 4 + column;
                format!("r{index:<2}={:08X}", cpu.regs.get(index))
            })
            .collect();
        println!("  {}", cells.join("  "));
    }

    let flag = |on: bool, name: char| if on { name } else { '-' };
    println!();
    println!(
        "  cpsr={:08X}  {}{}{}{}  {:?}{}",
        cpu.regs.cpsr(),
        flag(cpu.regs.n(), 'N'),
        flag(cpu.regs.z(), 'Z'),
        flag(cpu.regs.c(), 'C'),
        flag(cpu.regs.v(), 'V'),
        cpu.regs.mode(),
        if cpu.regs.thumb() { " THUMB" } else { "" },
    );

    // Where the beam got to, which is the difference between a machine that is
    // waiting for something and one that has stopped. A cartridge sitting in a
    // loop with the frame count climbing is a cartridge waiting on something
    // else; with the frame count at zero it is waiting on this.
    let ppu = mem.ppu();
    println!();
    println!(
        "  frames={} ({})  line={}  mode={}{}  dispstat={:04X}",
        ppu.frames(),
        machine_time(mem),
        ppu.vcount(),
        ppu.mode(),
        if ppu.forced_blank() { " (held blank)" } else { "" },
        ppu.status(),
    );
    println!(
        "  ie={:04X}  if={:04X}  ime={}{}",
        irq.enabled(),
        irq.requested(),
        u8::from(irq.master()),
        if irq.halted() { "  halted" } else { "" },
    );
    // Where the BIOS jumps when something interrupts: a game leaves the address
    // of its own handler in the last word of internal RAM. Worth printing
    // because a wild value there is not a processor bug — it means whatever was
    // supposed to put the handler in place did not run.
    println!("  handler={:08X}", mem.peek32(0x0300_7FFC));
    // The word beside it, which is how a sleeping game is told it may wake.
    // `IntrWait` halts and then asks *this*, not `IF`: the handler is expected
    // to set the bit for whatever it dealt with. A game asleep for ever with
    // its handler plainly running is this word staying zero.
    println!("  biosif={:04X}", mem.peek32(0x0300_7FF8) & 0xFFFF);

    // How much of each video memory has been filled in. Nothing here can say
    // whether a picture is *right*, but it can say whether there is one to
    // draw at all, which is the difference between a renderer that is wrong
    // and a game that never got as far as putting anything there.
    println!();
    for (name, base, len) in [
        ("palette", 0x0500_0000u32, akebia_gba::bus::PRAM_LEN),
        ("video  ", 0x0600_0000, akebia_gba::bus::VRAM_LEN),
        ("sprites", 0x0700_0000, akebia_gba::bus::OAM_LEN),
    ] {
        let words = len as u32 / 4;
        let filled = (0..words).filter(|index| mem.peek32(base + index * 4) != 0).count();
        println!("  {name}  {filled:6} of {words:6} words written");
    }
}
