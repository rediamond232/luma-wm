//! Wire protocol for Luma's opt-in direct game capture path.
//!
//! The OpenGL hook or external Vulkan GPU receiver sends encoded H.264 access
//! units to a local muxer. All fixed-width fields in both the 16-byte packet
//! header and message payloads use big-endian (network) byte order; H.264 AU
//! bytes are opaque and are not byte-swapped. This crate intentionally transports *bytes only*:
//! it does not claim DMA-BUF support because that would require SCM_RIGHTS and
//! explicit fence/lifetime handling.  A Unix stream is used rather than a
//! datagram socket so a large IDR access unit is never silently truncated.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

pub const PROTOCOL_VERSION: u16 = 1;
pub const TOKEN_LEN: usize = 32;
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;
pub const MAX_ACCESS_UNIT_BYTES: usize = 64 * 1024 * 1024;

const MAGIC: u32 = 0x4c47_4350; // "LGCP"
const HEADER_LEN: usize = 16;

/// A random, per-recorder-launch secret supplied out-of-band (normally via a
/// protected inherited file descriptor or environment only visible to the
/// launcher and hook). It authenticates the local hook to the muxer.
pub type SessionToken = [u8; TOKEN_LEN];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    H264,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameStats {
    pub submitted: u64,
    pub encoded: u64,
    pub dropped_before_encode: u64,
    pub dropped_send_backpressure: u64,
    pub encode_latency_us: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessUnitFlags(u32);

impl AccessUnitFlags {
    pub const KEYFRAME: Self = Self(1);
    pub const CONFIG: Self = Self(2);
    pub const EMPTY: Self = Self(0);

    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn from_bits(bits: u32) -> Result<Self, ProtocolError> {
        if bits & !0x3 != 0 {
            return Err(ProtocolError::InvalidPayload("unknown access-unit flags"));
        }
        Ok(Self(bits))
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessUnit {
    /// Nanoseconds on the hook's monotonic clock. The muxer maps this to the
    /// recording timeline; it must not be wall-clock time.
    pub pts_ns: u64,
    pub dts_ns: u64,
    pub duration_ns: u64,
    pub flags: AccessUnitFlags,
    /// One complete Annex-B H.264 access unit. No file/container framing.
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Message {
    ClientHello {
        token: SessionToken,
        pid: u32,
        capabilities: u32,
    },
    ServerHello {
        session_id: u64,
    },
    Start(VideoConfig),
    AccessUnit(AccessUnit),
    Stats(FrameStats),
    Stop,
    Error {
        code: u32,
        message: String,
    },
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    UnsupportedVersion(u16),
    BadMagic,
    UnknownMessage(u16),
    Oversized {
        message: &'static str,
        length: usize,
        maximum: usize,
    },
    Sequence {
        expected: u32,
        actual: u32,
    },
    AuthenticationFailed,
    ServerRejected {
        code: u32,
        message: String,
    },
    InvalidPayload(&'static str),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IPC I/O error: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported game-capture protocol version {version}")
            }
            Self::BadMagic => f.write_str("invalid game-capture IPC magic"),
            Self::UnknownMessage(kind) => write!(f, "unknown game-capture IPC message {kind}"),
            Self::Oversized {
                message,
                length,
                maximum,
            } => write!(f, "{message} payload {length} exceeds {maximum}"),
            Self::Sequence { expected, actual } => write!(
                f,
                "out-of-order game-capture packet {actual}; expected {expected}"
            ),
            Self::AuthenticationFailed => f.write_str("game-capture hook token was rejected"),
            Self::ServerRejected { code, message } => {
                write!(f, "game-capture server rejected hook ({code}): {message}")
            }
            Self::InvalidPayload(reason) => write!(f, "invalid game-capture IPC payload: {reason}"),
        }
    }
}
impl std::error::Error for ProtocolError {}
impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Ordered framed transport over a local [`UnixStream`]. A stream may split or
/// coalesce writes, so this always writes and reads the complete fixed header
/// and payload. Callers should put the stream in nonblocking mode only when
/// they are prepared to handle `WouldBlock` without losing encoder output.
pub struct Channel {
    stream: UnixStream,
    next_send: u32,
    next_receive: u32,
}

impl Channel {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            next_send: 0,
            next_receive: 0,
        }
    }
    pub fn into_inner(self) -> UnixStream {
        self.stream
    }
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.stream.set_nonblocking(nonblocking)
    }

    pub fn send(&mut self, message: &Message) -> Result<(), ProtocolError> {
        let (kind, payload) = encode_message(message)?;
        let max = maximum_for(kind);
        if payload.len() > max {
            return Err(ProtocolError::Oversized {
                message: "game-capture",
                length: payload.len(),
                maximum: max,
            });
        }
        let mut header = [0_u8; HEADER_LEN];
        put_u32(&mut header[0..4], MAGIC);
        put_u16(&mut header[4..6], PROTOCOL_VERSION);
        put_u16(&mut header[6..8], kind);
        put_u32(&mut header[8..12], payload.len() as u32);
        put_u32(&mut header[12..16], self.next_send);
        self.stream.write_all(&header)?;
        self.stream.write_all(&payload)?;
        self.next_send = self.next_send.wrapping_add(1);
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Message, ProtocolError> {
        let mut header = [0_u8; HEADER_LEN];
        self.stream.read_exact(&mut header)?;
        if read_u32(&header[0..4]) != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let version = read_u16(&header[4..6]);
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let kind = read_u16(&header[6..8]);
        let length = read_u32(&header[8..12]) as usize;
        let sequence = read_u32(&header[12..16]);
        if sequence != self.next_receive {
            return Err(ProtocolError::Sequence {
                expected: self.next_receive,
                actual: sequence,
            });
        }
        let max = maximum_for(kind);
        if length > max {
            return Err(ProtocolError::Oversized {
                message: "game-capture",
                length,
                maximum: max,
            });
        }
        let mut payload = vec![0_u8; length];
        self.stream.read_exact(&mut payload)?;
        self.next_receive = self.next_receive.wrapping_add(1);
        decode_message(kind, &payload)
    }

    /// Perform the hook-side authentication exchange.
    pub fn client_handshake(
        &mut self,
        token: SessionToken,
        pid: u32,
        capabilities: u32,
    ) -> Result<u64, ProtocolError> {
        self.send(&Message::ClientHello {
            token,
            pid,
            capabilities,
        })?;
        match self.recv()? {
            Message::ServerHello { session_id } => Ok(session_id),
            Message::Error { code, message } => {
                Err(ProtocolError::ServerRejected { code, message })
            }
            _ => Err(ProtocolError::InvalidPayload("expected server hello")),
        }
    }

    /// Validate the hook's token and acknowledge it. `constant_time_token_eq`
    /// avoids turning this local socket into an easy token oracle.
    pub fn server_handshake(
        &mut self,
        expected: &SessionToken,
        session_id: u64,
    ) -> Result<(u32, u32), ProtocolError> {
        match self.recv()? {
            Message::ClientHello {
                token,
                pid,
                capabilities,
            } if constant_time_token_eq(expected, &token) => {
                self.send(&Message::ServerHello { session_id })?;
                Ok((pid, capabilities))
            }
            Message::ClientHello { .. } => Err(ProtocolError::AuthenticationFailed),
            _ => Err(ProtocolError::InvalidPayload("expected client hello")),
        }
    }
}

/// Compare fixed-size secrets without early exit.
pub fn constant_time_token_eq(left: &SessionToken, right: &SessionToken) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

const CLIENT_HELLO: u16 = 1;
const SERVER_HELLO: u16 = 2;
const START: u16 = 3;
const ACCESS_UNIT: u16 = 4;
const STATS: u16 = 5;
const STOP: u16 = 6;
const ERROR: u16 = 7;

fn maximum_for(kind: u16) -> usize {
    if kind == ACCESS_UNIT {
        MAX_ACCESS_UNIT_BYTES
    } else {
        MAX_CONTROL_BYTES
    }
}

fn encode_message(message: &Message) -> Result<(u16, Vec<u8>), ProtocolError> {
    let mut out = Vec::new();
    let kind = match message {
        Message::ClientHello {
            token,
            pid,
            capabilities,
        } => {
            out.extend_from_slice(token);
            push_u32(&mut out, *pid);
            push_u32(&mut out, *capabilities);
            CLIENT_HELLO
        }
        Message::ServerHello { session_id } => {
            push_u64(&mut out, *session_id);
            SERVER_HELLO
        }
        Message::Start(config) => {
            out.push(match config.codec {
                Codec::H264 => 1,
            });
            push_u32(&mut out, config.width);
            push_u32(&mut out, config.height);
            push_u32(&mut out, config.fps_num);
            push_u32(&mut out, config.fps_den);
            START
        }
        Message::AccessUnit(au) => {
            push_u64(&mut out, au.pts_ns);
            push_u64(&mut out, au.dts_ns);
            push_u64(&mut out, au.duration_ns);
            push_u32(&mut out, au.flags.bits());
            out.extend_from_slice(&au.data);
            ACCESS_UNIT
        }
        Message::Stats(stats) => {
            for value in [
                stats.submitted,
                stats.encoded,
                stats.dropped_before_encode,
                stats.dropped_send_backpressure,
                stats.encode_latency_us,
            ] {
                push_u64(&mut out, value);
            }
            STATS
        }
        Message::Stop => STOP,
        Message::Error { code, message } => {
            push_u32(&mut out, *code);
            let bytes = message.as_bytes();
            if bytes.len() > MAX_CONTROL_BYTES - 4 {
                return Err(ProtocolError::Oversized {
                    message: "error",
                    length: bytes.len(),
                    maximum: MAX_CONTROL_BYTES - 4,
                });
            }
            out.extend_from_slice(bytes);
            ERROR
        }
    };
    Ok((kind, out))
}

fn decode_message(kind: u16, payload: &[u8]) -> Result<Message, ProtocolError> {
    let mut cursor = Cursor::new(payload);
    let message = match kind {
        CLIENT_HELLO => {
            let token = cursor.array()?;
            let pid = cursor.u32()?;
            let capabilities = cursor.u32()?;
            Message::ClientHello {
                token,
                pid,
                capabilities,
            }
        }
        SERVER_HELLO => Message::ServerHello {
            session_id: cursor.u64()?,
        },
        START => {
            let codec = match cursor.byte()? {
                1 => Codec::H264,
                _ => return Err(ProtocolError::InvalidPayload("unknown codec")),
            };
            let config = VideoConfig {
                codec,
                width: cursor.u32()?,
                height: cursor.u32()?,
                fps_num: cursor.u32()?,
                fps_den: cursor.u32()?,
            };
            if config.width == 0 || config.height == 0 || config.fps_num == 0 || config.fps_den == 0
            {
                return Err(ProtocolError::InvalidPayload(
                    "zero video dimension or rate",
                ));
            }
            Message::Start(config)
        }
        ACCESS_UNIT => {
            let pts_ns = cursor.u64()?;
            let dts_ns = cursor.u64()?;
            let duration_ns = cursor.u64()?;
            let flags = AccessUnitFlags::from_bits(cursor.u32()?)?;
            let data = cursor.rest().to_vec();
            if data.is_empty() {
                return Err(ProtocolError::InvalidPayload("empty access unit"));
            }
            Message::AccessUnit(AccessUnit {
                pts_ns,
                dts_ns,
                duration_ns,
                flags,
                data,
            })
        }
        STATS => Message::Stats(FrameStats {
            submitted: cursor.u64()?,
            encoded: cursor.u64()?,
            dropped_before_encode: cursor.u64()?,
            dropped_send_backpressure: cursor.u64()?,
            encode_latency_us: cursor.u64()?,
        }),
        STOP => Message::Stop,
        ERROR => {
            let code = cursor.u32()?;
            let message = std::str::from_utf8(cursor.rest())
                .map_err(|_| ProtocolError::InvalidPayload("non-UTF8 error"))?
                .to_owned();
            Message::Error { code, message }
        }
        other => return Err(ProtocolError::UnknownMessage(other)),
    };
    if !cursor.is_empty() && kind != ACCESS_UNIT && kind != ERROR {
        return Err(ProtocolError::InvalidPayload("trailing bytes"));
    }
    Ok(message)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(ProtocolError::InvalidPayload("length overflow"))?;
        let part = self
            .bytes
            .get(self.offset..end)
            .ok_or(ProtocolError::InvalidPayload("truncated payload"))?;
        self.offset = end;
        Ok(part)
    }
    fn byte(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(read_u32(self.take(4)?))
    }
    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(read_u64(self.take(8)?))
    }
    fn array(&mut self) -> Result<SessionToken, ProtocolError> {
        let mut token = [0; TOKEN_LEN];
        token.copy_from_slice(self.take(TOKEN_LEN)?);
        Ok(token)
    }
    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }
    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}
fn put_u16(dst: &mut [u8], value: u16) {
    dst.copy_from_slice(&value.to_be_bytes());
}
fn put_u32(dst: &mut [u8], value: u32) {
    dst.copy_from_slice(&value.to_be_bytes());
}
fn push_u32(dst: &mut Vec<u8>, value: u32) {
    dst.extend_from_slice(&value.to_be_bytes());
}
fn push_u64(dst: &mut Vec<u8>, value: u64) {
    dst.extend_from_slice(&value.to_be_bytes());
}
fn read_u16(src: &[u8]) -> u16 {
    u16::from_be_bytes(src.try_into().expect("fixed slice"))
}
fn read_u32(src: &[u8]) -> u32 {
    u32::from_be_bytes(src.try_into().expect("fixed slice"))
}
fn read_u64(src: &[u8]) -> u64 {
    u64::from_be_bytes(src.try_into().expect("fixed slice"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn authenticated_handshake_and_encoded_access_unit_round_trip() {
        let (hook_stream, muxer_stream) = UnixStream::pair().unwrap();
        let token = [0x5a; TOKEN_LEN];
        let muxer = thread::spawn(move || {
            let mut channel = Channel::new(muxer_stream);
            assert_eq!(channel.server_handshake(&token, 99).unwrap(), (4242, 7));
            assert_eq!(
                channel.recv().unwrap(),
                Message::Start(VideoConfig {
                    codec: Codec::H264,
                    width: 2560,
                    height: 1440,
                    fps_num: 480,
                    fps_den: 1
                })
            );
            match channel.recv().unwrap() {
                Message::AccessUnit(au) => {
                    assert!(au.flags.contains(AccessUnitFlags::KEYFRAME));
                    assert_eq!(au.data, vec![0, 0, 0, 1, 0x67]);
                }
                other => panic!("unexpected {other:?}"),
            }
            channel
                .send(&Message::Stats(FrameStats {
                    submitted: 2,
                    encoded: 1,
                    ..FrameStats::default()
                }))
                .unwrap();
        });
        let mut hook = Channel::new(hook_stream);
        assert_eq!(hook.client_handshake(token, 4242, 7).unwrap(), 99);
        hook.send(&Message::Start(VideoConfig {
            codec: Codec::H264,
            width: 2560,
            height: 1440,
            fps_num: 480,
            fps_den: 1,
        }))
        .unwrap();
        hook.send(&Message::AccessUnit(AccessUnit {
            pts_ns: 10,
            dts_ns: 10,
            duration_ns: 2_083_333,
            flags: AccessUnitFlags::KEYFRAME,
            data: vec![0, 0, 0, 1, 0x67],
        }))
        .unwrap();
        assert_eq!(
            hook.recv().unwrap(),
            Message::Stats(FrameStats {
                submitted: 2,
                encoded: 1,
                ..FrameStats::default()
            })
        );
        muxer.join().unwrap();
    }

    #[test]
    fn incorrect_token_is_rejected() {
        let (hook_stream, muxer_stream) = UnixStream::pair().unwrap();
        let server =
            thread::spawn(move || Channel::new(muxer_stream).server_handshake(&[1; TOKEN_LEN], 1));
        let mut hook = Channel::new(hook_stream);
        hook.send(&Message::ClientHello {
            token: [2; TOKEN_LEN],
            pid: 1,
            capabilities: 0,
        })
        .unwrap();
        assert!(matches!(
            server.join().unwrap(),
            Err(ProtocolError::AuthenticationFailed)
        ));
    }

    #[test]
    fn rejects_oversized_header_before_allocation() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut header = [0_u8; HEADER_LEN];
        put_u32(&mut header[0..4], MAGIC);
        put_u16(&mut header[4..6], PROTOCOL_VERSION);
        put_u16(&mut header[6..8], ACCESS_UNIT);
        put_u32(&mut header[8..12], (MAX_ACCESS_UNIT_BYTES as u32) + 1);
        writer.write_all(&header).unwrap();
        assert!(matches!(
            Channel::new(reader).recv(),
            Err(ProtocolError::Oversized { .. })
        ));
    }
}
