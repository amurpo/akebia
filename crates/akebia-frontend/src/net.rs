//! Packets over a socket: the cable when the other console is somewhere else.
//!
//! The format and the pacing are the core's —[`akebia_core::link::bgb`] and
//! [`akebia_core::link::session`]— and neither of them knows what a socket is.
//! What is here is the I/O, like every other adapter in this crate.
//!
//! # Threads, and why there are two of them
//!
//! Reading a socket blocks, and the thread that must never block is the one
//! drawing the window. So each connection gets a reader and a writer of its own,
//! and what crosses between them and the emulator is a channel of whole packets:
//! by the time one reaches the console it is eight bytes that arrived complete,
//! and nobody had to wait for them.
//!
//! Two threads and not one because a write can block as well —a peer that stops
//! reading fills the buffers— and a write stuck behind a read would be a link
//! that stops for a reason neither end can see.
//!
//! # No async runtime
//!
//! For two sockets that carry eight bytes at a time it would be a dependency and
//! a colour on every function that touches them, in exchange for nothing: there
//! is no fan-out here to be worth an executor. It is also the same reason the
//! folder chooser is Akebia's own; see the README.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use akebia_core::link::bgb::{Packet, DEFAULT_PORT, PACKET_LEN};

/// How long a connection is given to be made before it is called off.
///
/// Long enough for a phone on the far side of a house, short enough that a typed
/// address with a digit wrong says so instead of hanging.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Sent every so often with nothing to say, so that a peer which has gone away
/// is noticed rather than waited on for ever.
///
/// TCP on its own will not tell: a cable pulled out of a switch looks exactly
/// like a console whose player has gone to make tea.
const KEEPALIVE: Duration = Duration::from_secs(2);

/// A connection being made, without the window stopping while it is.
///
/// Both ends of the operation block —`connect` waits for an answer, `accept`
/// waits for somebody to arrive— so both happen on a thread and the result is
/// picked up whenever the interface next asks.
pub struct Pending {
    answer: Receiver<io::Result<Wire>>,
    /// Raised when this is dropped, to get the thread to give up.
    ///
    /// Without it, calling off a wait would leave a thread sitting in `accept`
    /// holding the port, and the next attempt to listen would be told the
    /// address is already in use — by a listener nobody can see any more.
    cancelled: Arc<AtomicBool>,
    /// What to say while it has not answered.
    pub what: String,
    /// The address the other machine has to be given, when this end is the one
    /// waiting. `None` when this end went looking instead —there is nobody to
    /// tell anything to— and when it could not be worked out.
    pub here: Option<String>,
    /// Whether this end went looking rather than waited. It is the only
    /// asymmetry the two have, and something has to use it: see
    /// [`crate::remote::Role`].
    pub dialled: bool,
}

/// Giving up on a connection tells the thread making it to give up too.
impl Drop for Pending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Pending {
    /// Waits for the other console to connect here.
    pub fn listen(port: u16) -> Self {
        let (tell, answer) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let watching = Arc::clone(&cancelled);
        thread::spawn(move || {
            let _ = tell.send(accept_one(port, &watching));
        });
        Self {
            answer,
            cancelled,
            what: format!("Waiting on port {port}"),
            here: this_machine().map(|address| format!("{address}:{port}")),
            dialled: false,
        }
    }

    /// Goes looking for a console that is already listening.
    ///
    /// A bare host takes [the usual port](DEFAULT_PORT); `host:port` is honoured
    /// as written.
    pub fn connect(address: String) -> Self {
        let (tell, answer) = mpsc::channel();
        let what = format!("Connecting to {address}");
        thread::spawn(move || {
            let _ = tell.send(Wire::dial(&address));
        });
        // Nothing to raise here: `connect_timeout` gives up on its own, and the
        // thread goes with it.
        Self { answer, cancelled: Arc::new(AtomicBool::new(false)), what, here: None, dialled: true }
    }

    /// The connection, once there is one. `None` while it is still being made.
    pub fn poll(&mut self) -> Option<io::Result<Wire>> {
        match self.answer.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            // The thread is gone without an answer, which it cannot normally be.
            Err(TryRecvError::Disconnected) => {
                Some(Err(io::Error::other("the connection was given up on")))
            }
        }
    }
}

/// The address of this machine on the network, as the other end would have to
/// type it.
///
/// It is asked of a UDP socket and not of a list of interfaces, because the
/// standard library has no such list and a machine has more than one address
/// anyway: loopback, whatever a container or a virtual machine left behind, and
/// the one a telephone on the same house network would actually reach. Which is
/// which is a question about routing, so routing is asked: connecting a UDP
/// socket sends nothing at all —there is no handshake in UDP— and only fixes
/// which interface a packet for that address would leave by. The address it is
/// pointed at is documentation's own and is not routed anywhere on purpose.
///
/// `None` when there is no route out, which on a machine with no network is the
/// honest answer: there is no address to give anybody.
fn this_machine() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// A connection carrying packets, in both directions, without blocking anybody.
pub struct Wire {
    incoming: Receiver<Packet>,
    outgoing: Sender<Packet>,
    /// Kept for two things neither channel can do: shut the connection down, and
    /// be shut down by dropping this. A reader sitting in `read_exact` does not
    /// notice its channel being dropped, so without this a link let go of would
    /// hold its socket —and its thread— for as long as the program ran.
    socket: TcpStream,
    /// Cleared by whichever thread finds the connection gone.
    ///
    /// Asked instead of the channel because asking the channel means calling
    /// `try_recv`, and that takes a packet out of it: a link would lose a byte
    /// every time somebody wondered whether it was still there.
    alive: Arc<AtomicBool>,
    /// Who is at the other end, to be able to say so.
    pub peer: String,
}

/// Closing a link closes the socket, which is what ends the two threads.
impl Drop for Wire {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

impl Wire {
    /// Puts the reader and the writer to work on a socket already connected.
    fn over(stream: TcpStream) -> io::Result<Self> {
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        // Eight bytes at a time and every one of them urgent: waiting to gather
        // a bigger write is exactly wrong here, because the thing being waited
        // for is what the other console is waiting on.
        stream.set_nodelay(true)?;
        let reading = stream.try_clone()?;
        let writing = stream.try_clone()?;
        let alive = Arc::new(AtomicBool::new(true));

        let (deliver, incoming) = mpsc::channel();
        let ours = Arc::clone(&alive);
        thread::spawn(move || {
            read_packets(reading, &deliver);
            ours.store(false, Ordering::Relaxed);
        });

        let (outgoing, to_send) = mpsc::channel();
        let ours = Arc::clone(&alive);
        thread::spawn(move || {
            write_packets(writing, &to_send);
            ours.store(false, Ordering::Relaxed);
        });

        Ok(Self { incoming, outgoing, socket: stream, alive, peer })
    }

    /// The next packet that arrived, if one has.
    pub fn try_recv(&mut self) -> Option<Packet> {
        self.incoming.try_recv().ok()
    }

    /// Waits a moment for a packet rather than returning empty-handed.
    ///
    /// It is what a console stalled on the other end does: coming back to the
    /// interface only to be asked again a frame later would put sixteen
    /// milliseconds of waiting behind a link that answered in one.
    pub fn recv_timeout(&mut self, patience: Duration) -> Option<Packet> {
        self.incoming.recv_timeout(patience).ok()
    }

    /// Queues a packet. It goes out on the writer's thread.
    pub fn send(&mut self, packet: Packet) {
        // An error means the writer is gone, which `is_up` is about to report.
        // There is nothing useful to do about it one packet at a time.
        let _ = self.outgoing.send(packet);
    }

    /// Whether the connection is still there.
    pub fn is_up(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Takes the next console to arrive on a listener already bound.
    ///
    /// It blocks, so it is for a caller that has somewhere to block: a thread of
    /// its own, or a test. The interface uses [`Pending::listen`].
    pub fn accept_from(listener: &TcpListener) -> io::Result<Self> {
        let (stream, _) = listener.accept()?;
        Self::over(stream)
    }

    /// Goes to an address and connects. Blocks; see [`Wire::accept_from`].
    pub fn dial(address: &str) -> io::Result<Self> {
        let target = resolve(address)?;
        let stream = TcpStream::connect_timeout(&target, CONNECT_TIMEOUT)?;
        Self::over(stream)
    }
}

/// Accepts exactly one console and then stops listening.
///
/// The socket is closed as soon as somebody is on it, on purpose: a second
/// player arriving at a cable with two ends already in it has nowhere to go, and
/// a listener left open would take the connection and then ignore it.
fn accept_one(port: u16, cancelled: &AtomicBool) -> io::Result<Wire> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    // Asked rather than waited on, so that calling the wait off actually ends
    // it. The alternative is a thread stuck in `accept` until somebody
    // connects to a port nobody is offering any more.
    listener.set_nonblocking(true)?;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::other("the wait was called off"));
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Wire::over(stream);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Turns what somebody typed into an address.
fn resolve(address: &str) -> io::Result<SocketAddr> {
    let address = address.trim();
    // Where a port would be, if one was written. An IPv6 address is all colons,
    // so the last one only means "port" outside the brackets — and an IPv6 with
    // no brackets cannot carry a port at all, which is the whole reason the
    // brackets exist.
    let port = match address.rfind(']') {
        Some(bracket) => address[bracket + 1..].strip_prefix(':'),
        None if address.matches(':').count() > 1 => None,
        None => address.rsplit_once(':').map(|(_, port)| port),
    };
    let with_port = match port {
        Some(port) if is_port(port) => address.to_owned(),
        _ => format!("{address}:{DEFAULT_PORT}"),
    };
    with_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("{address} does not resolve to anything")))
}

fn is_port(tail: &str) -> bool {
    !tail.is_empty() && tail.parse::<u16>().is_ok()
}

/// Reads eight bytes at a time until the connection ends.
fn read_packets(mut stream: TcpStream, deliver: &Sender<Packet>) {
    let mut buffer = [0u8; PACKET_LEN];
    loop {
        if stream.read_exact(&mut buffer).is_err() {
            return;
        }
        if deliver.send(Packet::decode(buffer)).is_err() {
            // Nobody is listening any more: the session was closed.
            return;
        }
    }
}

/// Writes what the emulator queues, and a keepalive when it queues nothing.
fn write_packets(mut stream: TcpStream, to_send: &Receiver<Packet>) {
    loop {
        let packet = match to_send.recv_timeout(KEEPALIVE) {
            Ok(packet) => packet,
            // Nothing to say for a while. A status packet is the harmless thing
            // to say instead: it carries no timestamp, so it cannot move the
            // other console, and a write that fails is how a peer that has gone
            // away gets noticed.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Packet::Status { running: true, paused: false, reconnect: false }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if stream.write_all(&packet.encode()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akebia_core::link::bgb::{Control, Stamp};

    /// A listener and a caller joined over the loopback, so the tests exercise
    /// real sockets rather than a stand-in for them.
    fn pair() -> (Wire, Wire) {
        // Port zero: the system picks a free one, so tests can run at the same
        // time as each other and as a real session.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepting = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            Wire::over(stream).unwrap()
        });
        let caller = Wire::dial(&format!("127.0.0.1:{port}")).unwrap();
        (accepting.join().unwrap(), caller)
    }

    /// Long enough that a loopback which is working never reaches it, short
    /// enough that one which is not does not hold the suite up.
    const PATIENCE: Duration = Duration::from_secs(2);

    #[test]
    fn a_packet_comes_out_the_other_end_as_it_went_in() {
        let (mut server, mut client) = pair();
        let sent = Packet::Master {
            data: 0x60,
            control: Control::new(false, false),
            at: Stamp::from_t_cycles(70_224),
        };
        client.send(sent);
        assert_eq!(server.recv_timeout(PATIENCE), Some(sent));
    }

    #[test]
    fn packets_arrive_in_the_order_they_were_sent() {
        let (mut server, mut client) = pair();
        let stream = [
            Packet::version(),
            Packet::Status { running: true, paused: false, reconnect: false },
            Packet::Slave { data: 0x02 },
            Packet::NotListening,
            Packet::Reached(Stamp::from_t_cycles(140_448)),
        ];
        for packet in stream {
            client.send(packet);
        }
        for packet in stream {
            assert_eq!(server.recv_timeout(PATIENCE), Some(packet), "out of order");
        }
    }

    #[test]
    fn it_carries_both_ways_at_once() {
        let (mut server, mut client) = pair();
        client.send(Packet::Slave { data: 0xAA });
        server.send(Packet::Slave { data: 0xBB });

        assert_eq!(server.recv_timeout(PATIENCE), Some(Packet::Slave { data: 0xAA }));
        assert_eq!(client.recv_timeout(PATIENCE), Some(Packet::Slave { data: 0xBB }));
    }

    #[test]
    fn nothing_arrives_before_it_is_sent() {
        let (mut server, _client) = pair();
        assert_eq!(server.try_recv(), None);
    }

    /// A console whose partner has gone has to find out. Over TCP that is not
    /// automatic: a socket nobody writes to looks exactly like a quiet one.
    #[test]
    fn a_peer_that_goes_away_is_noticed() {
        let (server, client) = pair();
        drop(client);

        // The reader ends when the socket does, and its channel goes with it.
        let deadline = std::time::Instant::now() + PATIENCE;
        while server.is_up() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!server.is_up(), "the link should have been given up as gone");
    }

    /// A wait called off has to let the port go. Leaving a thread in `accept`
    /// would make the next attempt fail with "address already in use", against a
    /// listener nobody can see any more.
    #[test]
    fn calling_off_a_wait_frees_the_port() {
        // A port picked by the system, then released, so the test does not fight
        // with anything else on the machine for a fixed number.
        let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();

        let waiting = Pending::listen(port);
        // Long enough for the thread to have bound it.
        thread::sleep(Duration::from_millis(100));
        drop(waiting);

        // And now it must be possible to wait on it again.
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            match TcpListener::bind(("0.0.0.0", port)) {
                Ok(_) => return,
                Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("the port was never let go of: {e}"),
            }
        }
    }

    #[test]
    fn a_host_with_no_port_gets_the_usual_one() {
        let resolved = resolve("127.0.0.1").unwrap();
        assert_eq!(resolved.port(), DEFAULT_PORT);
    }

    #[test]
    fn a_port_that_was_written_out_is_honoured() {
        assert_eq!(resolve("127.0.0.1:9000").unwrap().port(), 9000);
        assert_eq!(resolve("  127.0.0.1:9000  ").unwrap().port(), 9000, "and trimmed");
    }

    /// An IPv6 address is all colons, so the last one is not a port and taking
    /// it for one would turn a valid address into a refusal.
    #[test]
    fn an_ipv6_address_keeps_its_colons() {
        assert_eq!(resolve("[::1]").unwrap().port(), DEFAULT_PORT);
        assert_eq!(resolve("[::1]:9000").unwrap().port(), 9000);
        // Without brackets there is nowhere to put a port, so the last group is
        // part of the address and not one.
        assert_eq!(resolve("::1").unwrap().port(), DEFAULT_PORT);
    }

    #[test]
    fn an_address_that_means_nothing_says_so_instead_of_hanging() {
        assert!(resolve("not a host at all").is_err());
    }
}
