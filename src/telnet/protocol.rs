//! RFC 854 framing and RFC 1143 §7 (Q method) option negotiation.
//!
//! Every octet is visited once, including incomplete subnegotiations. Memory
//! use is independent of peer-provided lengths. Local and remote options are
//! deliberately separate state machines.

pub mod op_command {
    pub const EOF: u8 = 236;
    pub const SUSP: u8 = 237;
    pub const ABORT: u8 = 238;
    pub const EOR: u8 = 239;
    pub const SE: u8 = 240;
    pub const NOP: u8 = 241;
    pub const DM: u8 = 242;
    pub const BRK: u8 = 243;
    pub const IP: u8 = 244;
    pub const AO: u8 = 245;
    pub const AYT: u8 = 246;
    pub const EC: u8 = 247;
    pub const EL: u8 = 248;
    pub const GA: u8 = 249;
    pub const SB: u8 = 250;
    pub const WILL: u8 = 251;
    pub const WONT: u8 = 252;
    pub const DO: u8 = 253;
    pub const DONT: u8 = 254;
    pub const IAC: u8 = 255;
    pub const IS: u8 = 0;
    pub const SEND: u8 = 1;
}

pub mod op_option {
    pub const BINARY: u8 = 0;
    pub const ECHO: u8 = 1;
    pub const SGA: u8 = 3;
    pub const STATUS: u8 = 5;
    pub const TTYPE: u8 = 24;
    pub const NAWS: u8 = 31;
    pub const TSPEED: u8 = 32;
    pub const LFLOW: u8 = 33;
    pub const LINEMODE: u8 = 34;
    pub const NEWENVIRON: u8 = 39;
    pub const CHARSET: u8 = 42;
}

use op_command::*;

pub const MAX_SUBNEGOTIATION: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    Negotiation(u8, u8),
    Subnegotiation(u8, Vec<u8>),
    Command(u8),
}

#[derive(Default)]
enum State {
    #[default]
    Data,
    Iac,
    Negotiation(u8),
    SubOption,
    SubData(u8),
    SubIac(u8),
}

#[derive(Default)]
pub struct Parser {
    state: State,
    sub: Vec<u8>,
}

impl Parser {
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, &'static str> {
        let mut frames = Vec::new();
        let mut data = Vec::new();
        for &byte in bytes {
            self.state = match std::mem::take(&mut self.state) {
                State::Data if byte == IAC => {
                    if !data.is_empty() {
                        frames.push(Frame::Data(std::mem::take(&mut data)));
                    }
                    State::Iac
                }
                State::Data => {
                    data.push(byte);
                    State::Data
                }
                State::Iac => match byte {
                    IAC => {
                        data.push(IAC);
                        State::Data
                    }
                    WILL | WONT | DO | DONT => State::Negotiation(byte),
                    SB => State::SubOption,
                    _ => {
                        // All other commands are TWO octets, even commands
                        // we do not recognize (RFC 856 §5).
                        frames.push(Frame::Command(byte));
                        State::Data
                    }
                },
                State::Negotiation(command) => {
                    frames.push(Frame::Negotiation(command, byte));
                    State::Data
                }
                State::SubOption => {
                    self.sub.clear();
                    State::SubData(byte)
                }
                State::SubData(option) if byte == IAC => State::SubIac(option),
                State::SubData(option) => {
                    self.push_sub(byte)?;
                    State::SubData(option)
                }
                State::SubIac(option) => match byte {
                    IAC => {
                        self.push_sub(IAC)?;
                        State::SubData(option)
                    }
                    SE => {
                        frames.push(Frame::Subnegotiation(option, std::mem::take(&mut self.sub)));
                        State::Data
                    }
                    _ => return Err("Malformed Telnet subnegotiation"),
                },
            };
        }
        if !data.is_empty() {
            frames.push(Frame::Data(data));
        }
        Ok(frames)
    }

    fn push_sub(&mut self, byte: u8) -> Result<(), &'static str> {
        if self.sub.len() == MAX_SUBNEGOTIATION {
            return Err("Telnet subnegotiation exceeds 16 KiB");
        }
        self.sub.push(byte);
        Ok(())
    }
}

pub fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &byte in data {
        out.push(byte);
        if byte == IAC {
            out.push(IAC);
        }
    }
    out
}

pub fn subnegotiation(option: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![IAC, SB, option];
    out.extend(escape(payload));
    out.extend([IAC, SE]);
    out
}

/// CR decoding is streaming across both network packets and Telnet commands.
#[derive(Default)]
pub struct Nvt {
    after_cr: bool,
}

impl Nvt {
    pub fn decode(&mut self, data: &[u8], binary: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &byte in data {
            if !binary && self.after_cr && byte == 0 {
                self.after_cr = false;
                continue;
            }
            out.push(byte);
            self.after_cr = !binary && byte == b'\r';
        }
        out
    }
}

/// An input command is an atomic application write. Canonicalize every CR at
/// this boundary; never delay a key waiting for a possible following LF.
/// CR LF / CR NUL supplied by paste and line editing remain unchanged.
pub fn encode(data: &[u8], binary: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for (i, &byte) in data.iter().enumerate() {
        out.push(byte);
        if byte == IAC {
            out.push(IAC);
        } else if !binary && byte == b'\r' && !matches!(data.get(i + 1), Some(b'\n' | 0)) {
            out.push(0);
        }
    }
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Q {
    #[default]
    No,
    Yes,
    WantNo(bool),
    WantYes(bool),
}

impl Q {
    pub fn enabled(self) -> bool {
        self == Self::Yes
    }

    /// Return the positive/negative response, if one must be sent.
    pub fn receive(&mut self, positive: bool, supported: bool) -> Option<bool> {
        use Q::*;
        let (state, reply) = match (*self, positive) {
            (No, true) if supported => (Yes, Some(true)),
            (No, true) => (No, Some(false)),
            (Yes, true) | (No, false) => (*self, None),
            (Yes, false) => (No, Some(false)),
            (WantNo(false), _) => (No, None),
            (WantNo(true), true) => (Yes, None),
            (WantNo(true), false) => (WantYes(false), Some(true)),
            (WantYes(false), true) => (Yes, None),
            (WantYes(true), true) => (WantNo(false), Some(false)),
            (WantYes(_), false) => (No, None),
        };
        *self = state;
        reply
    }

    pub fn request(&mut self, enable: bool) -> Option<bool> {
        use Q::*;
        let (state, reply) = match (*self, enable) {
            (No, true) => (WantYes(false), Some(true)),
            (Yes, false) => (WantNo(false), Some(false)),
            (WantNo(_), want) => (WantNo(want), None),
            (WantYes(_), want) => (WantYes(!want), None),
            _ => (*self, None),
        };
        *self = state;
        reply
    }
}

pub struct Options {
    pub local: [Q; 256],
    pub remote: [Q; 256],
}

impl Default for Options {
    fn default() -> Self {
        Self {
            local: [Q::No; 256],
            remote: [Q::No; 256],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_split(bytes: &[u8], split: usize) -> Vec<Frame> {
        let mut parser = Parser::default();
        let mut frames = parser.receive(&bytes[..split]).unwrap();
        frames.extend(parser.receive(&bytes[split..]).unwrap());
        frames
    }

    #[test]
    fn every_split_preserves_escaped_iac_and_two_octet_commands() {
        for cmd in [BRK, IP, AO, AYT, DM, NOP, GA, EOR, EOF, SUSP, ABORT, 200] {
            let wire = [b'A', IAC, IAC, b'B', IAC, cmd, b'C'];
            for split in 0..=wire.len() {
                let frames = parse_split(&wire, split);
                let data: Vec<u8> = frames
                    .iter()
                    .filter_map(|f| match f {
                        Frame::Data(d) => Some(d.as_slice()),
                        _ => None,
                    })
                    .flatten()
                    .copied()
                    .collect();
                assert_eq!(data, [b'A', IAC, b'B', b'C'], "{cmd}, split {split}");
                assert!(frames.contains(&Frame::Command(cmd)));
            }
        }
    }

    #[test]
    fn subnegotiation_unescapes_iac_and_does_not_end_on_literal_se() {
        let wire = subnegotiation(34, &[3, IAC, SE, b'A']);
        for split in 0..=wire.len() {
            assert_eq!(
                parse_split(&wire, split),
                [Frame::Subnegotiation(34, vec![3, IAC, SE, b'A'])]
            );
        }
    }

    #[test]
    fn unfinished_subnegotiation_is_bounded() {
        let mut parser = Parser::default();
        parser.receive(&[IAC, SB, 24]).unwrap();
        for _ in 0..MAX_SUBNEGOTIATION {
            parser.receive(b"x").unwrap();
        }
        assert!(parser.receive(b"x").is_err());
        assert_eq!(parser.sub.len(), MAX_SUBNEGOTIATION);
    }

    #[test]
    fn nvt_stream_and_binary_directions() {
        let mut nvt = Nvt::default();
        assert_eq!(nvt.decode(b"a\r", false), b"a\r");
        assert_eq!(nvt.decode(b"\0b\r\nc", false), b"b\r\nc");
        assert_eq!(nvt.decode(b"\r\0", true), b"\r\0");
        assert_eq!(encode(b"\r", false), b"\r\0");
        assert_eq!(encode(b"\r\0\r\n\xff", false), b"\r\0\r\n\xff\xff");
        assert_eq!(encode(b"\r\xff", true), b"\r\xff\xff");
    }

    #[test]
    fn q_method_crossed_requests_and_opposite_queue() {
        let mut q = Q::No;
        assert_eq!(q.request(true), Some(true));
        assert_eq!(q.request(false), None);
        assert_eq!(q, Q::WantYes(true));
        assert_eq!(q.receive(true, true), Some(false));
        assert_eq!(q, Q::WantNo(false));
        assert_eq!(q.request(true), None);
        assert_eq!(q.receive(false, true), Some(true));
        assert_eq!(q.receive(true, true), None);
        assert_eq!(q, Q::Yes);
        assert_eq!(q.receive(true, true), None);
        assert_eq!(q.receive(false, true), Some(false));
        assert_eq!(q, Q::No);
    }
}
