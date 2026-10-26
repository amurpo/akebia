//! A byte has to land where the other console's clock says it did.
//!
//! Two consoles talking over a cable do not just swap bytes: they swap them at
//! an *instant*, and a game is only listening for part of the time. Between one
//! byte and the next every game has a moment —its interrupt handler, its
//! bookkeeping— where the register is not armed, and on real hardware the other
//! console's byte never lands in it, because the two clocks are the same clock.
//!
//! The three runs are the same pair of consoles over the cables Akebia has: the
//! one in this process, a socket, and a socket with a few milliseconds in it.
//! What they measure is how many of the master's bytes found nobody listening.

use akebia_core::link;
use akebia_core::ports::NullOutput;
use akebia_core::serial::LinkEvent;
use akebia_core::GameBoy;
use akebia_frontend::net::Wire;
use akebia_frontend::remote::{Remote, Role};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Frames each run is given. Enough for a few hundred bytes to cross.
const FRAMES: usize = 120;

/// A console that drives the clock: a byte every so often, counting up, and
/// `gap` loops of quiet between one and the next. Zero is the longest gap there
/// is —`DEC C` from zero comes round— and not the shortest.
fn ticker(gap: u8) -> Vec<u8> {
    let mut rom = vec![0u8; 32 * 1024];
    let program: &[u8] = &[
        0x06, 0x01, // LD B,1
        0x78, //       LD A,B          <- round again
        0xE0, 0x01, // LDH (SB),A
        0x3E, 0x81, // LD A,$81        start, and this end drives the clock
        0xE0, 0x02, // LDH (SC),A
        0xF0, 0x02, // LDH A,(SC)      <- until the start bit clears
        0xCB, 0x7F, // BIT 7,A
        0x20, 0xFA, // JR NZ,-6
        0x04, //       INC B
        0x0E, gap, //  LD C,gap
        0x0D, //       DEC C           <- the quiet between bytes
        0x20, 0xFD, // JR NZ,-3
        0x18, 0xEB, // JR -21
    ];
    rom[0x0100..0x0100 + program.len()].copy_from_slice(program);
    rom
}

/// A console that listens, and that for `window` loops after every byte is not
/// listening: the moment a game spends putting its register back.
fn listener(window: u8) -> Vec<u8> {
    let mut rom = vec![0u8; 32 * 1024];
    let program: &[u8] = &[
        0x3E, 0x80, //    LD A,$80     <- armed, waiting for somebody else's clock
        0xE0, 0x02, //    LDH (SC),A
        0xF0, 0x02, //    LDH A,(SC)   <- until it has been clocked
        0xCB, 0x7F, //    BIT 7,A
        0x20, 0xFA, //    JR NZ,-6
        0x0E, window, //  LD C,window
        0x0D, //          DEC C        <- not listening
        0x20, 0xFD, //    JR NZ,-3
        0x18, 0xEF, //    JR -17
    ];
    rom[0x0100..0x0100 + program.len()].copy_from_slice(program);
    rom
}

/// Bytes this console clocked out. Every one of them is an instant at which the
/// other end was supposed to be listening.
fn clocked(events: &[LinkEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, LinkEvent::Transferred { internal: true, .. }))
        .count()
}

/// Of those, the ones the partner never took in.
///
/// It is read off the answers and not off a counter in the other console,
/// because the cable already says it: an armed partner gives up whatever its
/// register holds, which is the byte it was sent last. So an answer that is not
/// the byte before it is a byte that never reached the game at the other end —
/// `0xFF` for a partner that was not listening, and a stale byte for one that
/// was clocked twice while it was.
fn missed(events: &[LinkEvent]) -> usize {
    let mut misses = 0;
    let mut last: Option<u8> = None;
    for event in events {
        if let LinkEvent::Transferred { sent, received, internal: true, .. } = *event {
            // The first answer is whatever the partner had in its register
            // before any of this started, and it is nobody's byte.
            if last.is_some_and(|before| before != received) {
                misses += 1;
            }
            last = Some(sent);
        }
    }
    misses
}

/// The pair on the cable that goes no further than this process.
fn over_one_cable() -> Vec<LinkEvent> {
    let mut a = GameBoy::new(ticker(0x00)).unwrap();
    let mut b = GameBoy::new(listener(0x80)).unwrap();
    a.set_link_log_enabled(true);
    link::connect(&mut a, &mut b);

    let mut log = Vec::new();
    for _ in 0..FRAMES {
        link::run_frame(&mut a, &mut NullOutput, &mut b, &mut NullOutput).unwrap();
        log.extend(a.take_link_log());
    }
    log
}

/// A listener that passes packets on to `onward`, `delay` late in each
/// direction.
///
/// A loopback answers in microseconds, which is a link no player has: the phone
/// on the other side of the house is milliseconds away, and the whole question
/// here is what a console does with the time it is allowed to run while it
/// waits. Without this the test is one machine talking to itself.
fn slow_relay(door: TcpListener, onward: u16, delay: Duration) {
    std::thread::spawn(move || {
        let Ok((near, _)) = door.accept() else { return };
        let Ok(far) = TcpStream::connect(("127.0.0.1", onward)) else { return };
        for (from, to) in [(&near, &far), (&far, &near)] {
            let (mut from, mut to) = (from.try_clone().unwrap(), to.try_clone().unwrap());
            std::thread::spawn(move || {
                let mut packet = [0u8; 8];
                while from.read_exact(&mut packet).is_ok() {
                    std::thread::sleep(delay);
                    if to.write_all(&packet).is_err() {
                        return;
                    }
                }
            });
        }
    });
}

/// The same pair with a socket in the middle, `delay` away in each direction.
fn over_a_socket(frames: usize, delay: Duration) -> Vec<LinkEvent> {
    let door = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut port = door.local_addr().unwrap().port();
    if !delay.is_zero() {
        // The console that dials is pointed at the relay instead, and the relay
        // at the console that waited.
        let front = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let relayed = front.local_addr().unwrap().port();
        slow_relay(front, port, delay);
        port = relayed;
    }

    let driving = std::thread::spawn(move || {
        let mut gb = GameBoy::new(ticker(0x00)).unwrap();
        gb.set_link_log_enabled(true);
        let wire = Wire::accept_from(&door).unwrap();
        let mut remote = Remote::new(wire, &mut gb, Role::Waited);
        let mut log = Vec::new();
        let mut done = 0;
        // The bound is the runaway case: a link that stops moving would
        // otherwise sit here for ever.
        for _ in 0..frames * 400 {
            match remote.run_frame(&mut gb, &mut NullOutput) {
                Ok(true) => done += 1,
                Ok(false) => {}
                Err(_) => break,
            }
            log.extend(gb.take_link_log());
            if done == frames {
                break;
            }
        }
        log
    });

    let following = std::thread::spawn(move || {
        let mut gb = GameBoy::new(listener(0x80)).unwrap();
        let wire = Wire::dial(&format!("127.0.0.1:{port}")).unwrap();
        let mut remote = Remote::new(wire, &mut gb, Role::Dialled);
        // Longer than the other end, so that the one being measured is never
        // the one left waiting for a partner that has already gone.
        for _ in 0..frames * 800 {
            if remote.run_frame(&mut gb, &mut NullOutput).is_err() {
                break;
            }
        }
    });

    let log = driving.join().unwrap();
    let _ = following.join();
    log
}

/// Where in the run the misses fall, for a failing run to be readable.
fn where_missed(label: &str, events: &[LinkEvent]) {
    let mut n = 0usize;
    let mut last: Option<u8> = None;
    for event in events {
        if let LinkEvent::Transferred { at, sent, received, internal: true, .. } = *event {
            n += 1;
            if let Some(before) = last {
                if before != received {
                    println!("{label}: byte {n} at {at}: {before:02X} out, {received:02X} back");
                }
            }
            last = Some(sent);
        }
    }
}

#[test]
fn on_one_cable_every_byte_finds_the_partner_listening() {
    let log = over_one_cable();
    where_missed("cable", &log);
    assert!(clocked(&log) > 200, "barely any bytes crossed: {}", clocked(&log));
    assert_eq!(missed(&log), 0, "of {} bytes", clocked(&log));
}

#[test]
fn over_a_socket_every_byte_finds_the_partner_listening() {
    let log = over_a_socket(FRAMES, Duration::ZERO);
    where_missed("socket", &log);
    assert!(clocked(&log) > 200, "barely any bytes crossed: {}", clocked(&log));
    assert_eq!(missed(&log), 0, "of {} bytes", clocked(&log));
}

/// The same, with the other console a few milliseconds away.
///
/// This is the link people actually have —a phone across the room— and it is a
/// different question from the one above: what a console does with the running
/// it is allowed while an answer is on its way. Fewer frames because every byte
/// costs a round trip, which is the price of the cable and not a fault.
#[test]
fn a_slow_link_does_not_cost_bytes_either() {
    let log = over_a_socket(30, Duration::from_millis(5));
    where_missed("slow socket", &log);
    assert!(clocked(&log) > 200, "barely any bytes crossed: {}", clocked(&log));
    assert_eq!(missed(&log), 0, "of {} bytes", clocked(&log));
}
