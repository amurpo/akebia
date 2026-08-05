//! Drives two linked consoles from a script and prints what crosses the cable.
//!
//! It exists to answer one question the window cannot: a trade that fails says
//! "your friend is not ready" and nothing else, and from outside there is no
//! telling whether the two games never agreed on who leads, agreed and then lost
//! each other, or are talking perfectly to a partner that is not listening.
//!
//! It drives the two consoles from the same script, which is a thing no keyboard
//! can do —press on both at once— and that is the point: it takes the player's
//! timing out of the question and leaves only the cable's. `--lag` puts it back.
//!
//! ```text
//! # both consoles here, one cable, no sockets
//! cargo run --release --example link_trace -- <rom> <sav> [script] [--frames N]
//!
//! # one console here and one on the other end of a socket
//! cargo run --release --example link_trace -- <rom> <sav> --serve 8765 [script]
//! cargo run --release --example link_trace -- <rom> <sav> --join host [script]
//! ```
//!
//! The script is a word per action: `w30` waits thirty frames, `a2` holds A for
//! two, `up8` walks up for eight. Frames are dumped as text every `--every`
//! frames so it is possible to see where the game actually is.

use akebia_core::joypad::Button;
use akebia_core::link;
use akebia_core::ports::VideoOutput;
use akebia_core::ppu::{FrameBuffer, SCREEN_HEIGHT, SCREEN_WIDTH};
use akebia_core::serial::LinkEvent;
use akebia_core::GameBoy;
use akebia_frontend::net::Wire;
use akebia_frontend::remote::{Remote, Role};

/// Keeps the last frame each console produced, to print or write out.
struct Screen {
    pixels: Vec<[u8; 3]>,
}

impl Screen {
    fn new() -> Self {
        Self { pixels: vec![[0; 3]; SCREEN_WIDTH * SCREEN_HEIGHT] }
    }

    /// Rough luminance, enough to tell text from background.
    fn luma(&self, x: usize, y: usize) -> u8 {
        let [r, g, b] = self.pixels[y * SCREEN_WIDTH + x];
        ((u32::from(r) * 30 + u32::from(g) * 59 + u32::from(b) * 11) / 100) as u8
    }
}

impl VideoOutput for Screen {
    fn present(&mut self, frame: &FrameBuffer) {
        for (target, color) in self.pixels.iter_mut().zip(frame.as_slice()) {
            *target = color.to_rgb888();
        }
    }
}

/// Writes the two screens side by side as a binary PPM, which any viewer reads.
fn write_ppm(path: &str, screens: [&Screen; 2]) {
    let width = SCREEN_WIDTH * 2 + 8;
    let mut out = format!("P6\n{width} {SCREEN_HEIGHT}\n255\n").into_bytes();
    for y in 0..SCREEN_HEIGHT {
        for x in 0..SCREEN_WIDTH {
            out.extend_from_slice(&screens[0].pixels[y * SCREEN_WIDTH + x]);
        }
        out.extend_from_slice(&[255, 0, 0].repeat(8));
        for x in 0..SCREEN_WIDTH {
            out.extend_from_slice(&screens[1].pixels[y * SCREEN_WIDTH + x]);
        }
    }
    std::fs::write(path, out).expect("the frame");
}

/// The two screens side by side, one text row per two scanlines.
fn dump(label: &str, screens: [&Screen; 2]) {
    const SHADES: &[u8] = b"@%#*+=-:. ";
    println!("\n===== {label} =====");
    for y in (0..SCREEN_HEIGHT).step_by(4) {
        let mut line = String::new();
        for (i, screen) in screens.iter().enumerate() {
            if i == 1 {
                line.push_str(" | ");
            }
            for x in (0..SCREEN_WIDTH).step_by(2) {
                let v = screen.luma(x, y);
                let shade = SHADES[(usize::from(v) * (SHADES.len() - 1)) / 255];
                line.push(shade as char);
            }
        }
        println!("{line}");
    }
}

/// One instruction of the script: a button (or none) held for `frames`.
struct Action {
    button: Option<Button>,
    frames: u32,
}

fn parse_script(words: &[String]) -> Vec<Action> {
    let mut actions = Vec::new();
    for word in words {
        let split = word.find(|c: char| c.is_ascii_digit()).unwrap_or(word.len());
        let (name, count) = word.split_at(split);
        let frames: u32 = count.parse().unwrap_or(1);
        let button = match name {
            "w" | "wait" => None,
            "a" => Some(Button::A),
            "b" => Some(Button::B),
            "start" => Some(Button::Start),
            "select" => Some(Button::Select),
            "up" => Some(Button::Up),
            "down" => Some(Button::Down),
            "left" => Some(Button::Left),
            "right" => Some(Button::Right),
            other => panic!("unknown action: {other}"),
        };
        actions.push(Action { button, frames });
    }
    actions
}

/// Prints a console's share of the trace, one line per event.
fn report(side: char, events: &[LinkEvent]) {
    for event in events {
        match *event {
            LinkEvent::Armed { at, sb, internal } => {
                let role = if internal { "master" } else { "slave " };
                println!("{side} {at:>12} arm  {role} sb={sb:02X}");
            }
            LinkEvent::Transferred { at, sent, received, internal, armed } => {
                let role = if internal { "master" } else { "slave " };
                let armed = if armed { "" } else { "  UNARMED" };
                println!("{side} {at:>12} xfer {role} out={sent:02X} in={received:02X}{armed}");
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut limit = 3600u32;
    let mut every = 0u32;
    let mut ppm = String::from("link");
    let mut lag = 0usize;
    let mut over_the_wire: Option<Wire> = None;
    let mut waiting: Option<u16> = None;
    let mut calling: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--frames" => {
                i += 1;
                limit = args[i].parse().unwrap();
            }
            "--every" => {
                i += 1;
                every = args[i].parse().unwrap();
            }
            "--ppm" => {
                i += 1;
                ppm.clone_from(&args[i]);
            }
            // Frames console B gets the script late by. It is how one keyboard
            // between two consoles really behaves: whatever the player does on
            // one of them, the other one gets seconds later, if at all.
            "--lag" => {
                i += 1;
                lag = args[i].parse().unwrap();
            }
            "--serve" => {
                i += 1;
                waiting = Some(args[i].parse().unwrap());
            }
            "--join" => {
                i += 1;
                calling = Some(args[i].clone());
            }
            other => positional.push(other.to_owned()),
        }
        i += 1;
    }

    let rom = std::fs::read(&positional[0]).expect("the ROM");
    let mut a = GameBoy::new(rom).expect("a supported cartridge");

    if positional.len() > 1 && !positional[1].is_empty() {
        let save = std::fs::read(&positional[1]).expect("the saved game");
        let sram = a.save_ram().expect("a cartridge with a battery").len();
        assert!(a.load_save_ram(&save[..sram]), "the cartridge rejected the saved game");
        if save.len() > sram {
            a.rtc_load(&save[sram..], 0);
        }
        eprintln!("loaded {} bytes of SRAM", sram);
    }

    let script = parse_script(&positional[2..]);
    a.set_link_log_enabled(true);

    // One console here and the other somewhere else. There is nothing to copy:
    // the partner is a second run of this same program.
    let mut role = Role::Waited;
    if let Some(port) = waiting {
        eprintln!("waiting on port {port}");
        let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("the port");
        over_the_wire = Some(Wire::accept_from(&listener).expect("a console"));
    } else if let Some(address) = calling {
        eprintln!("connecting to {address}");
        role = Role::Dialled;
        over_the_wire = Some(Wire::dial(&address).expect("a console"));
    }

    if let Some(wire) = over_the_wire {
        eprintln!("linked to {}", wire.peer);
        a.set_link_connected(true);
        let mut remote = Remote::new(wire, &mut a, role);
        return over_a_socket(a, &mut remote, script, limit, every, &ppm);
    }

    // Both consoles here, on the shortest cable there is. The copy is taken
    // before anything is pressed, exactly as the window does it.
    let mut b = a.clone();
    b.set_link_log_enabled(true);
    link::connect(&mut a, &mut b);

    let (mut screen_a, mut screen_b) = (Screen::new(), Screen::new());
    let mut frame = 0u32;
    let mut actions = script.into_iter();
    let mut current = actions.next();
    let mut held: [Option<Button>; 2] = [None, None];
    let mut delayed: std::collections::VecDeque<Option<Button>> = std::collections::VecDeque::new();

    while frame < limit {
        // The script drives both consoles, B `lag` frames behind A. With no lag
        // it is a thing no keyboard can do —press on two consoles at once— and
        // that is the point: it takes the player's timing out of the question
        // and leaves only the cable's. With lag it puts it back in.
        let want = current.as_ref().and_then(|action| action.button);
        delayed.push_back(want);
        let late = if delayed.len() > lag { delayed.pop_front().flatten() } else { None };

        for (gb, (now, was)) in
            [&mut a, &mut b].into_iter().zip([want, late].into_iter().zip(held.iter_mut()))
        {
            if now != *was {
                if let Some(button) = *was {
                    gb.set_button(button, false);
                }
                if let Some(button) = now {
                    gb.set_button(button, true);
                }
                *was = now;
            }
        }

        link::run_frame(&mut a, &mut screen_a, &mut b, &mut screen_b).expect("no fault");
        frame += 1;

        if let Some(action) = current.as_mut() {
            action.frames -= 1;
            if action.frames == 0 {
                current = actions.next();
            }
        }

        let events_a = a.take_link_log();
        let events_b = b.take_link_log();
        if !events_a.is_empty() || !events_b.is_empty() {
            println!("-- frame {frame}");
            report('A', &events_a);
            report('B', &events_b);
        }
        if every > 0 && frame % every == 0 {
            write_ppm(&format!("{ppm}-{frame:05}.ppm"), [&screen_a, &screen_b]);
        }
    }

    dump("final", [&screen_a, &screen_b]);
    write_ppm(&format!("{ppm}-final.ppm"), [&screen_a, &screen_b]);
}

/// The same run with the other console on the far end of a socket.
///
/// One screen, one script, and whatever the far end says about when this one may
/// move. What it is really testing is the pacing: a byte crossing a socket is
/// already covered by the tests, but a real game deadlocking against the rule
/// that lets it run is not the sort of thing a synthetic ROM finds.
fn over_a_socket(
    mut gb: GameBoy,
    remote: &mut Remote,
    script: Vec<Action>,
    limit: u32,
    every: u32,
    ppm: &str,
) {
    let mut screen = Screen::new();
    let blank = Screen::new();
    let mut frame = 0u32;
    let mut actions = script.into_iter();
    let mut current = actions.next();
    let mut held: Option<Button> = None;
    let mut stalls = 0u32;

    while frame < limit {
        let want = current.as_ref().and_then(|action| action.button);
        if want != held {
            if let Some(button) = held {
                gb.set_button(button, false);
            }
            if let Some(button) = want {
                gb.set_button(button, true);
            }
            held = want;
        }

        match remote.run_frame(&mut gb, &mut screen) {
            Ok(true) => frame += 1,
            // Waiting on the other console. It is normal and it is the whole
            // point; only counted, so a run that spends its life waiting says so.
            Ok(false) => {
                stalls += 1;
                continue;
            }
            Err(trouble) => {
                eprintln!("the link ended: {trouble}");
                break;
            }
        }

        if let Some(action) = current.as_mut() {
            action.frames -= 1;
            if action.frames == 0 {
                current = actions.next();
            }
        }

        let events = gb.take_link_log();
        if !events.is_empty() {
            println!("-- frame {frame}");
            report('L', &events);
        }
        if every > 0 && frame % every == 0 {
            write_ppm(&format!("{ppm}-{frame:05}.ppm"), [&screen, &blank]);
        }
    }

    eprintln!("{frame} frames, {stalls} waits on the other console");
    dump("final", [&screen, &blank]);
    write_ppm(&format!("{ppm}-final.ppm"), [&screen, &blank]);
}
