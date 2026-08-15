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

/// Where a cartridge is mapped, and so where a machine with one starts.
const ROM_BASE: u32 = 0x0800_0000;

/// Long enough for a test suite to finish and short enough to give up in a
/// second or two.
const DEFAULT_STEPS: u64 = 50_000_000;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: run <rom.gba> [--bios FILE] [--boot] [--steps N] [--trace N] [--ppm FILE]");
        return ExitCode::FAILURE;
    };

    let mut limit = DEFAULT_STEPS;
    let mut trace = 0u64;
    let mut bios = None;
    let mut ppm = None;
    let mut boot = false;
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
            "--steps" | "--trace" => {
                let Some(n) = args.next().and_then(|v| v.parse().ok()) else {
                    eprintln!("{flag} wants a number");
                    return ExitCode::FAILURE;
                };
                if flag == "--steps" {
                    limit = n;
                } else {
                    trace = n;
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

    let outcome = run(&mut cpu, &mut mem, limit, trace);
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
        Outcome::Settled { .. } => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
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
    /// Neither happened in time, which tells us nothing.
    RanOn,
}

fn run(cpu: &mut Cpu, mem: &mut Memory, limit: u64, trace: u64) -> Outcome {
    for step in 0..limit {
        let before = cpu.regs.pc();

        if step < trace {
            println!("{:08X}  {:08X}", before, mem.peek32(before));
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

fn report(cpu: &Cpu, mem: &Memory, outcome: &Outcome) {
    let irq = mem.interrupts();
    println!();
    match outcome {
        Outcome::Faulted(fault, steps) => println!("stopped after {steps} steps: {fault}"),
        Outcome::Settled { at, steps } => {
            println!("settled at {at:08X} after {steps} steps");
            println!("  the instruction there: {:08X}", mem.peek32(*at));
        }
        Outcome::RanOn => println!("still running when the step limit ran out"),
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
        "  frames={}  line={}  mode={}{}  dispstat={:04X}",
        ppu.frames(),
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
