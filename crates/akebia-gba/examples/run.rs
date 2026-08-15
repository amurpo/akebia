//! Runs a cartridge headless and says where the processor ended up.
//!
//! ```text
//! cargo run --release -p akebia-gba --example run -- arm.gba
//! cargo run --release -p akebia-gba --example run -- arm.gba --trace 40
//! ```
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
//! There is no BIOS here and no picture, so a ROM that wants either will not
//! get far. That is expected: this is for the ones that check the processor.

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
        eprintln!("usage: run <rom.gba> [--steps N] [--trace N]");
        return ExitCode::FAILURE;
    };

    let mut limit = DEFAULT_STEPS;
    let mut trace = 0u64;
    while let Some(flag) = args.next() {
        let value = args.next().and_then(|v| v.parse().ok());
        match (flag.as_str(), value) {
            ("--steps", Some(n)) => limit = n,
            ("--trace", Some(n)) => trace = n,
            _ => {
                eprintln!("unrecognised option: {flag}");
                return ExitCode::FAILURE;
            }
        }
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
    cpu.regs.set_pc(ROM_BASE);

    let outcome = run(&mut cpu, &mut mem, limit, trace);
    report(&cpu, &mem, &outcome);

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
        // to where it already was, so this is not a guess.
        if cpu.regs.pc() == before {
            return Outcome::Settled { at: before, steps: step + 1 };
        }
    }
    Outcome::RanOn
}

fn report(cpu: &Cpu, mem: &Memory, outcome: &Outcome) {
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
}
