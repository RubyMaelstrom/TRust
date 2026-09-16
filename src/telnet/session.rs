//! Bounded, cancellation-safe Telnet transport. No frontend channel send or
//! socket write is awaited outside the central select: a slow painter/peer
//! cannot keep Close, resize, or the opposite socket direction from running.

use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

use super::protocol::{self, Frame, Nvt, Options, Parser, op_command::*, op_option::*};
use super::{Command, Event};

const QUEUED_INPUT_BYTES: usize = 1024 * 1024;
const QUEUED_OUTPUT_BYTES: usize = 256 * 1024;
const WIRE_HIGH_WATER: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Submission is immediate, including the async spelling used by the TUI.
/// Accepted input is ordered and lossless. A full queue is an explicit error,
/// never a reason to park the UI task that must drain incoming output.
pub struct CommandSender {
    tx: mpsc::Sender<Command>,
    queued: Arc<AtomicUsize>,
    resize: watch::Sender<(u16, u16)>,
    close: watch::Sender<bool>,
}

#[derive(Debug)]
pub struct SendError(pub Command, pub &'static str);

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}

impl CommandSender {
    pub fn try_send(&self, command: Command) -> Result<(), SendError> {
        match command {
            Command::Close => {
                self.close.send_replace(true);
                Ok(())
            }
            Command::Resize { cols, rows } => {
                self.resize.send_replace((cols, rows));
                Ok(())
            }
            command => {
                let cost = command_cost(&command);
                if self
                    .queued
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        n.checked_add(cost).filter(|&sum| sum <= QUEUED_INPUT_BYTES)
                    })
                    .is_err()
                {
                    return Err(SendError(
                        command,
                        "Telnet input queue is full; input was not sent",
                    ));
                }
                if let Err(error) = self.tx.try_send(command) {
                    self.queued.fetch_sub(cost, Ordering::AcqRel);
                    let message = if matches!(error, mpsc::error::TrySendError::Closed(_)) {
                        "Telnet connection is closed; input was not sent"
                    } else {
                        "Telnet input queue is full; input was not sent"
                    };
                    return Err(SendError(error.into_inner(), message));
                }
                Ok(())
            }
        }
    }

    pub async fn send(&self, command: Command) -> Result<(), SendError> {
        self.try_send(command)
    }
}

pub struct Handle {
    pub commands: CommandSender,
    task: Option<tokio::task::AbortHandle>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.commands.close.send_replace(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Handle {
    #[cfg(test)]
    pub(crate) fn for_test(tx: mpsc::Sender<Command>) -> Self {
        let (resize, _) = watch::channel((80, 24));
        let (close, _) = watch::channel(false);
        Self {
            commands: CommandSender {
                tx,
                queued: Arc::default(),
                resize,
                close,
            },
            task: None,
        }
    }
}

fn command_cost(command: &Command) -> usize {
    match command {
        Command::Send(bytes) => bytes.len().saturating_add(1),
        _ => 1,
    }
}

pub fn connect(
    host: String,
    port: u16,
    size: (u16, u16),
    use_tls: bool,
) -> (Handle, mpsc::Receiver<Event>) {
    let (tx, commands) = mpsc::channel(1024);
    let (events, receiver) = mpsc::channel(32);
    let (resize, sizes) = watch::channel(size);
    let (close, mut closing) = watch::channel(false);
    let queued = Arc::new(AtomicUsize::new(0));
    let task_queued = queued.clone();
    let task_events = events.clone();
    let task = tokio::spawn(async move {
        let work = async {
            let stream =
                tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), port)))
                    .await
                    .map_err(|_| "Telnet connection timed out".to_owned())?
                    .map_err(|error| error.to_string())?;
            stream
                .set_nodelay(true)
                .map_err(|error| error.to_string())?;
            let peer = stream.peer_addr().map_err(|error| error.to_string())?;
            if use_tls {
                let name = crate::tls::server_name(&host)?;
                let connector = crate::tls::connector(&host, port);
                let stream = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, stream))
                    .await
                    .map_err(|_| "Telnet TLS handshake timed out".to_owned())?
                    .map_err(|error| format!("TLS: {error}"))?;
                events
                    .send(Event::Connected { peer, tls: true })
                    .await
                    .map_err(|_| String::new())?;
                run_session(stream, size, commands, sizes, &events, task_queued).await
            } else {
                events
                    .send(Event::Connected { peer, tls: false })
                    .await
                    .map_err(|_| String::new())?;
                run_session(stream, size, commands, sizes, &events, task_queued).await
            }
        };
        // Covers DNS, TCP, TLS, and every pending I/O operation. Dropping the
        // receiver also closes the socket without waiting for another packet.
        let error = tokio::select! {
            _ = closing.changed() => None,
            _ = task_events.closed() => return,
            result = work => result.err().filter(|message| !message.is_empty()),
        };
        let _ = task_events.send(Event::Closed(error)).await;
    });
    (
        Handle {
            commands: CommandSender {
                tx,
                queued,
                resize,
                close,
            },
            task: Some(task.abort_handle()),
        },
        receiver,
    )
}

/// Fixed local characters are exported with CANTCHANGE (RFC 1184 §2.4).
/// Flush flags are not offered: sending an ordinary DM is not TCP SYNCH.
pub const LOCAL_SLC: &[(u8, u8)] = &[
    (3, 3),    // IP, ^C
    (7, 28),   // ABORT, ^\
    (9, 26),   // SUSP, ^Z
    (8, 4),    // EOF, ^D
    (10, 127), // EC, DEL
    (11, 21),  // EL, ^U
    (12, 23),  // EW, ^W
];

struct Protocol {
    parser: Parser,
    options: Options,
    nvt: Nvt,
    size: (u16, u16),
    mode: u8,
    ttype: usize,
    disabled_slc: u32,
}

impl Protocol {
    fn new(size: (u16, u16)) -> Self {
        Self {
            parser: Parser::default(),
            options: Options::default(),
            nvt: Nvt::default(),
            size,
            mode: 0,
            ttype: 0,
            disabled_slc: 0,
        }
    }

    fn local(&self, option: u8) -> bool {
        self.options.local[usize::from(option)].enabled()
    }
    fn remote(&self, option: u8) -> bool {
        self.options.remote[usize::from(option)].enabled()
    }

    fn receive(
        &mut self,
        bytes: &[u8],
        wire: &mut Vec<u8>,
        events: &mut VecDeque<Event>,
    ) -> Result<(), String> {
        for frame in self.parser.receive(bytes).map_err(str::to_owned)? {
            match frame {
                Frame::Data(data) => {
                    let data = self.nvt.decode(&data, self.remote(BINARY));
                    if !data.is_empty() {
                        events.push_back(Event::Data(data));
                    }
                }
                Frame::Negotiation(command, option) => {
                    self.negotiate(command, option, wire, events)
                }
                Frame::Subnegotiation(option, payload) if self.local(option) => {
                    self.suboption(option, &payload, wire, events)
                }
                Frame::Command(AYT) => wire.extend(protocol::encode(
                    b"\r\n[TRust is here]\r\n",
                    self.local(BINARY),
                )),
                _ => {}
            }
        }
        Ok(())
    }

    fn negotiate(
        &mut self,
        command: u8,
        option: u8,
        wire: &mut Vec<u8>,
        events: &mut VecDeque<Event>,
    ) {
        let local = matches!(command, DO | DONT);
        let positive = matches!(command, DO | WILL);
        // RFC 1372 requires an actual XON/XOFF implementation before WILL
        // LFLOW. Refuse it, along with unsupported terminal extensions.
        let supported = if local {
            matches!(
                option,
                BINARY | SGA | NAWS | TTYPE | STATUS | TSPEED | NEWENVIRON | LINEMODE
            )
        } else {
            matches!(option, BINARY | ECHO | SGA)
        };
        let state = if local {
            &mut self.options.local[usize::from(option)]
        } else {
            &mut self.options.remote[usize::from(option)]
        };
        let before = state.enabled();
        if let Some(yes) = state.receive(positive, supported) {
            wire.extend([
                IAC,
                match (local, yes) {
                    (true, true) => WILL,
                    (true, false) => WONT,
                    (false, true) => DO,
                    (false, false) => DONT,
                },
                option,
            ]);
        }
        let enabled = state.enabled();
        if before == enabled {
            return;
        }
        events.push_back(Event::Negotiation {
            command: match (local, enabled) {
                (true, true) => DO,
                (true, false) => DONT,
                (false, true) => WILL,
                (false, false) => WONT,
            },
            option,
        });
        if local && option == NAWS && enabled {
            wire.extend(self.naws());
        }
        if local && option == TTYPE {
            self.ttype = 0;
        }
        if local && option == LINEMODE {
            self.mode = 0; // RFC 1184 §3: EDIT starts OFF.
            self.disabled_slc = 0;
            events.push_back(Event::LineMode {
                active: enabled,
                mode: self.mode,
            });
            if enabled {
                let mut slc = vec![3];
                for &(function, value) in LOCAL_SLC {
                    slc.extend([function, 1, value]);
                }
                wire.extend(protocol::subnegotiation(LINEMODE, &slc));
            }
        }
    }

    fn naws(&self) -> Vec<u8> {
        let (cols, rows) = self.size;
        protocol::subnegotiation(
            NAWS,
            &[(cols >> 8) as u8, cols as u8, (rows >> 8) as u8, rows as u8],
        )
    }

    fn suboption(
        &mut self,
        option: u8,
        payload: &[u8],
        wire: &mut Vec<u8>,
        events: &mut VecDeque<Event>,
    ) {
        if option == LINEMODE {
            self.linemode(payload, wire, events);
            return;
        }
        if payload != [SEND] && option != NEWENVIRON {
            return;
        }
        if payload.first() != Some(&SEND) {
            return;
        }
        let mut reply = vec![IS];
        match option {
            TTYPE => {
                // RFC 1091: repeat the final synonym once, then restart.
                // Do not advertise full XTERM emulation that we do not have.
                const TYPES: [&[u8]; 3] = [b"ANSI", b"VT100", b"VT100"];
                reply.extend(TYPES[self.ttype]);
                self.ttype = (self.ttype + 1) % TYPES.len();
            }
            TSPEED => reply.extend(b"38400,38400"),
            NEWENVIRON => {} // Never disclose the process environment.
            STATUS => {
                for option in 0..=255 {
                    if self.local(option) {
                        reply.extend([WILL, option]);
                    }
                    if self.remote(option) {
                        reply.extend([DO, option]);
                    }
                }
            }
            _ => return,
        }
        wire.extend(protocol::subnegotiation(option, &reply));
    }

    fn linemode(&mut self, payload: &[u8], wire: &mut Vec<u8>, events: &mut VecDeque<Event>) {
        match payload {
            [1, mask] if mask & 4 == 0 && mask & !4 != self.mode => {
                // Honor EDIT/TRAPSIG; soft tabs and literal echo are supported
                // by the shared input model. Unknown bits are not acknowledged.
                self.mode = mask & 0x1b;
                let ack = if self.mode == *mask { 4 } else { 0 };
                wire.extend(protocol::subnegotiation(LINEMODE, &[1, self.mode | ack]));
                events.push_back(Event::LineMode {
                    active: true,
                    mode: self.mode,
                });
            }
            [3, triples @ ..] if triples.len().is_multiple_of(3) => {
                let mut reply = vec![3];
                for triple in triples.as_chunks::<3>().0 {
                    let (function, flags, value) = (triple[0], triple[1], triple[2]);
                    if flags & 0x80 != 0 {
                        continue;
                    }
                    if function == 0 {
                        continue;
                    } // Reserved for client requests.
                    if flags & 3 == 0 {
                        if function < 32 {
                            self.disabled_slc |= 1 << function;
                        }
                        reply.extend([function, 0x80, value]);
                    } else if let Some(&(_, local)) =
                        LOCAL_SLC.iter().find(|&&(f, _)| f == function)
                    {
                        self.disabled_slc &= !(1 << function);
                        let ack = if flags & 0x7f == 1 && value == local {
                            0x80
                        } else {
                            0
                        };
                        reply.extend([function, 1 | ack, local]);
                    } else {
                        reply.extend([function, 0, 0]);
                    }
                }
                if reply.len() > 1 {
                    wire.extend(protocol::subnegotiation(LINEMODE, &reply));
                }
                events.push_back(Event::Slc {
                    disabled: self.disabled_slc,
                });
            }
            [DO, 2, ..] => wire.extend(protocol::subnegotiation(LINEMODE, &[WONT, 2])),
            _ => {} // Unchanged MODE and all MODE_ACKs never generate replies.
        }
    }

    fn command(&mut self, command: Command, wire: &mut Vec<u8>) {
        match command {
            Command::Send(data) => wire.extend(protocol::encode(&data, self.local(BINARY))),
            Command::SendIac(command) => wire.extend([IAC, command]),
            Command::LineModeRequest { edit } if self.local(LINEMODE) => {
                let mode = (self.mode & !1) | u8::from(edit);
                if mode != self.mode {
                    self.mode = mode;
                    wire.extend(protocol::subnegotiation(LINEMODE, &[1, mode]));
                    // The shared terminal already applied this local request.
                    // Do not echo it into the receive backlog; that backlog is
                    // reserved for ordered events from the remote peer.
                }
            }
            _ => {}
        }
    }
}

async fn run_session<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    size: (u16, u16),
    mut commands: mpsc::Receiver<Command>,
    mut sizes: watch::Receiver<(u16, u16)>,
    events: &mpsc::Sender<Event>,
    queued: Arc<AtomicUsize>,
) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut protocol = Protocol::new(size);
    let mut buffer = [0; 8192];
    let mut wire = Vec::new();
    let mut written = 0;
    let mut pending = VecDeque::new();
    let mut pending_bytes = 0usize;
    let mut eof = false;
    loop {
        if written == wire.len() {
            wire.clear();
            written = 0;
        }
        if eof && pending.is_empty() && wire.is_empty() {
            return Ok(());
        }
        tokio::select! {
            permit = events.reserve(), if !pending.is_empty() => {
                let permit = permit.map_err(|_| String::new())?;
                if let Some(event) = pending.pop_front() {
                    pending_bytes = pending_bytes.saturating_sub(event_cost(&event));
                    permit.send(event);
                }
            }
            result = writer.write(&wire[written..]), if written < wire.len() => {
                let n = result.map_err(|error| error.to_string())?;
                if n == 0 { return Err("Telnet socket stopped accepting output".into()); }
                written += n;
                // Compact only after a substantial prefix has been sent.
                if written >= 64 * 1024 && written < wire.len() {
                    wire.drain(..written); written = 0;
                }
            }
            result = reader.read(&mut buffer), if !eof && pending_bytes < QUEUED_OUTPUT_BYTES && wire.len() - written < WIRE_HIGH_WATER => {
                let n = result.map_err(|error| error.to_string())?;
                if n == 0 { eof = true; continue; }
                let previous = pending.len();
                protocol.receive(&buffer[..n], &mut wire, &mut pending)?;
                pending_bytes += pending.iter().skip(previous).map(event_cost).sum::<usize>();
            }
            command = commands.recv(), if wire.len() - written < WIRE_HIGH_WATER => {
                let Some(command) = command else { return Ok(()); };
                queued.fetch_sub(command_cost(&command), Ordering::AcqRel);
                protocol.command(command, &mut wire);
            }
            result = sizes.changed(), if wire.len() - written < WIRE_HIGH_WATER => {
                if result.is_err() { return Ok(()); }
                protocol.size = *sizes.borrow_and_update();
                if protocol.local(NAWS) { wire.extend(protocol.naws()); }
            }
        }
    }
}

fn event_cost(event: &Event) -> usize {
    match event {
        Event::Data(data) => data.len() + 32,
        _ => 32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_and_sga_negotiate_independently_in_both_directions() {
        for option in [BINARY, SGA] {
            let mut protocol = Protocol::new((255, 511));
            let (mut wire, mut events) = (Vec::new(), VecDeque::new());
            protocol
                .receive(
                    &[IAC, DO, option, IAC, WILL, option],
                    &mut wire,
                    &mut events,
                )
                .unwrap();
            assert_eq!(wire, [IAC, WILL, option, IAC, DO, option]);
            assert!(protocol.local(option) && protocol.remote(option));
            wire.clear();
            protocol
                .receive(
                    &[IAC, DO, option, IAC, WILL, option],
                    &mut wire,
                    &mut events,
                )
                .unwrap();
            assert!(wire.is_empty());
        }
    }

    #[test]
    fn ttype_repeats_final_name_once_then_restarts() {
        let mut protocol = Protocol::new((80, 24));
        let (mut wire, mut events) = (Vec::new(), VecDeque::new());
        protocol
            .receive(&[IAC, DO, TTYPE], &mut wire, &mut events)
            .unwrap();
        for name in [b"ANSI".as_slice(), b"VT100", b"VT100", b"ANSI"] {
            wire.clear();
            protocol
                .receive(
                    &protocol::subnegotiation(TTYPE, &[SEND]),
                    &mut wire,
                    &mut events,
                )
                .unwrap();
            assert_eq!(
                wire,
                protocol::subnegotiation(TTYPE, &[&[IS], name].concat())
            );
        }
    }

    #[test]
    fn linemode_defaults_ack_rules_and_fixed_slc() {
        let mut protocol = Protocol::new((80, 24));
        let (mut wire, mut events) = (Vec::new(), VecDeque::new());
        protocol
            .receive(&[IAC, DO, LINEMODE], &mut wire, &mut events)
            .unwrap();
        assert_eq!(protocol.mode, 0);
        assert!(
            wire.windows(4)
                .any(|window| window == [IAC, SB, LINEMODE, 3])
        );
        wire.clear();
        protocol.linemode(&[1, 3], &mut wire, &mut events);
        assert_eq!(wire, protocol::subnegotiation(LINEMODE, &[1, 7]));
        assert_eq!(protocol.mode, 3);
        wire.clear();
        protocol.linemode(&[1, 4], &mut wire, &mut events);
        protocol.linemode(&[1, 3], &mut wire, &mut events);
        assert_eq!(protocol.mode, 3);
        assert!(wire.is_empty());
        protocol.linemode(&[3, 3, 3, 99, 99, 2, 42], &mut wire, &mut events);
        assert_eq!(
            wire,
            protocol::subnegotiation(LINEMODE, &[3, 3, 1, 3, 99, 0, 0])
        );
    }

    #[tokio::test]
    async fn output_backpressure_does_not_block_input_or_close() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (handle, mut events) = connect(
            "127.0.0.1".into(),
            listener.local_addr().unwrap().port(),
            (80, 24),
            false,
        );
        let (socket, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        assert!(matches!(events.recv().await, Some(Event::Connected { .. })));
        let flood = tokio::spawn(async move {
            let _ = writer.write_all(&vec![b'x'; 2 * 1024 * 1024]).await;
            writer
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        handle
            .commands
            .send(Command::Send(b"hello".to_vec()))
            .await
            .unwrap();
        let mut reply = [0; 5];
        tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply, b"hello");
        handle.commands.send(Command::Close).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(event, Event::Closed(_)) {
                    return;
                }
            }
        })
        .await
        .unwrap();
        flood.abort();
    }

    #[tokio::test]
    async fn drop_cancels_a_stalled_tls_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (handle, events) = connect(
            "127.0.0.1".into(),
            listener.local_addr().unwrap().port(),
            (80, 24),
            true,
        );
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        let n = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        drop(handle);
        drop(events);
        let n = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn telnet_queue_limits_preserve_rejected_input_and_coalesce_resize() {
        let (tx, _rx) = mpsc::channel(1);
        let (resize, mut sizes) = watch::channel((80, 24));
        let (close, closing) = watch::channel(false);
        let sender = CommandSender {
            tx,
            queued: Arc::default(),
            resize,
            close,
        };
        sender
            .try_send(Command::Send(vec![b'x'; QUEUED_INPUT_BYTES - 1]))
            .unwrap();
        let error = sender
            .try_send(Command::Send(b"keep".to_vec()))
            .unwrap_err();
        assert!(matches!(error.0, Command::Send(bytes) if bytes == b"keep"));
        for cols in 1..100 {
            sender.try_send(Command::Resize { cols, rows: 30 }).unwrap();
        }
        assert_eq!(*sizes.borrow_and_update(), (99, 30));
        sender.try_send(Command::Close).unwrap();
        assert!(*closing.borrow());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn telnet_loopback_input_latency_during_output_backpressure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (handle, mut events) = connect(
            "127.0.0.1".into(),
            listener.local_addr().unwrap().port(),
            (80, 24),
            false,
        );
        let (socket, _) = listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        let (mut reader, mut writer) = socket.into_split();
        assert!(matches!(events.recv().await, Some(Event::Connected { .. })));
        let flood = tokio::spawn(async move {
            let block = vec![b'x'; 8192];
            while writer.write_all(&block).await.is_ok() {}
        });
        // Intentionally stop consuming output: this is the old circular-wait
        // reproducer. Measure enqueue-to-peer-read, excluding rendering.
        tokio::time::sleep(Duration::from_millis(40)).await;
        let mut micros = Vec::new();
        for sample in 0..300 {
            let expected = format!("{sample:08}").into_bytes();
            let start = std::time::Instant::now();
            handle
                .commands
                .try_send(Command::Send(expected.clone()))
                .unwrap();
            let mut received = [0; 8];
            tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut received))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received.as_slice(), expected);
            micros.push(start.elapsed().as_micros());
        }
        micros.sort_unstable();
        eprintln!(
            "Telnet enqueue-to-peer-read under backpressure (300 samples): p50={}µs p95={}µs p99={}µs max={}µs",
            micros[150], micros[285], micros[297], micros[299]
        );
        assert!(
            micros[297] < 200_000,
            "input starved under output backpressure"
        );
        let start = std::time::Instant::now();
        drop(handle);
        let mut byte = [0];
        let result = tokio::time::timeout(Duration::from_secs(2), reader.read(&mut byte))
            .await
            .unwrap();
        assert!(
            matches!(result, Ok(0) | Err(_)),
            "cancellation did not close the socket"
        );
        eprintln!(
            "Telnet cancellation-to-peer-close: {}µs",
            start.elapsed().as_micros()
        );
        flood.abort();
    }
}
