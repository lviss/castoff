//! Wire format and message types for the FCast local control protocol.
//!
//! Framing (FCast protocol v2, <https://docs.fcast.org/protocol/v2>):
//! a 4-byte little-endian length prefix, followed by a 1-byte opcode,
//! followed by an optional UTF-8 JSON body. `length` counts the opcode
//! byte plus the body, so a body-less message has `length == 1`.
//! Maximum packet size is 32 KiB.

use std::collections::HashMap;
use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const DEFAULT_PORT: u16 = 46899;
pub const MAX_PACKET_SIZE: usize = 32 * 1024;
/// FCast protocol version this daemon implements (see docs.fcast.org/protocol/v2).
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8,
    PlaybackError = 9,
    SetSpeed = 10,
    Version = 11,
    Ping = 12,
    Pong = 13,
}

impl Opcode {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Opcode::None,
            1 => Opcode::Play,
            2 => Opcode::Pause,
            3 => Opcode::Resume,
            4 => Opcode::Stop,
            5 => Opcode::Seek,
            6 => Opcode::PlaybackUpdate,
            7 => Opcode::VolumeUpdate,
            8 => Opcode::SetVolume,
            9 => Opcode::PlaybackError,
            10 => Opcode::SetSpeed,
            11 => Opcode::Version,
            12 => Opcode::Ping,
            13 => Opcode::Pong,
            _ => return None,
        })
    }
}

/// A decoded frame: an opcode plus its raw (still-encoded) JSON body, if any.
pub struct Frame {
    pub opcode: Opcode,
    pub body: Vec<u8>,
}

/// Read one length-prefixed frame from `stream`. Returns `Ok(None)` on clean EOF
/// before any bytes of a new frame arrive.
pub async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R) -> io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid FCast frame length {len}"),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    let opcode = Opcode::from_u8(payload[0]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown opcode {}", payload[0]),
        )
    })?;
    Ok(Some(Frame {
        opcode,
        body: payload[1..].to_vec(),
    }))
}

/// Write one frame: `opcode` plus an already-serialized JSON `body` (empty for
/// body-less messages like Pause/Resume/Stop/Ping/Pong).
pub async fn write_frame<W: AsyncWrite + Unpin>(
    stream: &mut W,
    opcode: Opcode,
    body: &[u8],
) -> io::Result<()> {
    let len = 1 + body.len();
    if len > MAX_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.push(opcode as u8);
    out.extend_from_slice(body);
    stream.write_all(&out).await
}

pub async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut W,
    opcode: Opcode,
    msg: &T,
) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write_frame(stream, opcode, &body).await
}

pub async fn write_empty<W: AsyncWrite + Unpin>(stream: &mut W, opcode: Opcode) -> io::Result<()> {
    write_frame(stream, opcode, &[]).await
}

/// Sender -> receiver: start playback.
///
/// `volume`/`speed` accommodate FCast's existing knobs; there is deliberately
/// no `quality` field yet (see README roadmap: playback-quality selection is
/// a later, additive field on this same message, not a breaking rework).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayMessage {
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub time: Option<f64>,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub speed: Option<f64>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeekMessage {
    pub time: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetVolumeMessage {
    pub volume: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSpeedMessage {
    pub speed: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionMessage {
    pub version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum PlaybackState {
    Idle = 0,
    Playing = 1,
    Paused = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackUpdateMessage {
    pub generation_time: u64,
    pub state: PlaybackState,
    #[serde(default)]
    pub time: Option<f64>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub speed: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackErrorMessage {
    pub message: String,
}
