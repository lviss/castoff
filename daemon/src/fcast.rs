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
use serde_repr::{Deserialize_repr, Serialize_repr};
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
    // --- castoff private extension: the play queue. FCast v2 itself has no
    // queue concept; these opcodes live beyond its reserved 0-13 range, on
    // this daemon's own single-user control API (see README's "Queueing
    // (private extension)"). Every message shape is documented on its own
    // struct below.
    /// sender -> receiver: ask for the current queue; replies `QueueState`.
    RequestQueue = 14,
    /// receiver -> sender: the full queue and the current position in it.
    /// Sent both as `RequestQueue`'s reply and, unprompted, to every
    /// connected sender whenever the queue changes.
    QueueState = 15,
    /// sender -> receiver: move to the next queue item, if any; replies
    /// `QueueState`. A no-op (still replies) when already on the last item.
    QueueJumpForward = 16,
    /// sender -> receiver: move to the previous queue item, if any; replies
    /// `QueueState`. A no-op (still replies) when already on the first item.
    QueueJumpBackward = 17,
    /// sender -> receiver: empty the play queue, stopping playback first if
    /// its current item is actively playing; replies `QueueState`.
    ClearQueue = 18,
    /// sender -> receiver: move straight to an arbitrary queue index
    /// (`QueueJumpToIndexMessage`), if in range; replies `QueueState`. A
    /// no-op (still replies) for an out-of-range index.
    QueueJumpToIndex = 19,
    // --- castoff private extension: image wallpaper tagging. An uploaded
    // image (see `upload.rs`'s `/images` HTTP endpoint -- a separate
    // transport from this TCP framing, since FCast's own frame cap is 32 KiB,
    // nowhere near enough for a phone photo) is tagged/untagged for
    // idle-screen wallpaper rotation independent of the play queue; see
    // README's "Image uploads (private extension)".
    /// sender -> receiver: tag or untag a previously uploaded image (by the
    /// id the upload endpoint returned) for idle-screen wallpaper rotation.
    /// Replies with `ImageWallpaperUpdate`.
    SetImageWallpaper = 20,
    /// receiver -> sender: confirms the wallpaper tag `SetImageWallpaper`
    /// just set.
    ImageWallpaperUpdate = 21,
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
            14 => Opcode::RequestQueue,
            15 => Opcode::QueueState,
            16 => Opcode::QueueJumpForward,
            17 => Opcode::QueueJumpBackward,
            18 => Opcode::ClearQueue,
            19 => Opcode::QueueJumpToIndex,
            20 => Opcode::SetImageWallpaper,
            21 => Opcode::ImageWallpaperUpdate,
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
///
/// `container` is FCast's MIME-type field (docs.fcast.org/protocol/v2 calls
/// it "The MIME type (video/mp4)"). castoff reuses it to route between its
/// two content types -- mpv (media) and the browser engine (web pages) --
/// exactly as a MIME type is meant to be routed on, so no new opcode, field
/// or protocol version is needed (see `PlayMessage::explicit_target`).
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

/// `Play.container` MIME types that mean "render this URL as a web page in a
/// browser engine" rather than "play this URL as media in mpv". Case
/// insensitive; `text/html` is what a dashboard/document sender should use.
const WEBPAGE_CONTAINERS: [&str; 2] = ["text/html", "application/xhtml+xml"];

/// What a `Play` should be handed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayTarget {
    /// mpv: a media file, stream, or a source `yt-dlp` can resolve (e.g. a
    /// YouTube watch URL).
    Media,
    /// The browser engine: a web page.
    Webpage,
}

impl PlayMessage {
    /// The target the sender *explicitly* asked for through FCast's
    /// `container` MIME-type field, or `None` when it did not say (no
    /// `container`, an empty one, or only MIME parameters).
    ///
    /// `container` is FCast's own MIME field
    /// ([docs.fcast.org/protocol/v2](https://docs.fcast.org/protocol/v2)
    /// documents it as "The MIME type (video/mp4)"), so a sender that sets it
    /// stays in control: a web MIME type ([`WEBPAGE_CONTAINERS`]) means the
    /// browser, anything else means mpv. A `None` return means the daemon
    /// decides for itself (see README's routing rules and `Player::play`) --
    /// which is what lets a sender that only knows a URL cast media, YouTube
    /// and web pages alike.
    pub fn explicit_target(&self) -> Option<PlayTarget> {
        // A MIME type may carry parameters (`text/html; charset=utf-8`), so
        // route on the media type alone.
        let media_type = self.container.as_deref()?.split(';').next()?.trim();
        if media_type.is_empty() {
            return None;
        }
        if WEBPAGE_CONTAINERS
            .iter()
            .any(|webpage| media_type.eq_ignore_ascii_case(webpage))
        {
            Some(PlayTarget::Webpage)
        } else {
            Some(PlayTarget::Media)
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
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

/// castoff private extension (see `Opcode::QueueState`): one item in the play
/// queue, as shown to a sender (e.g. the Android app's queue list).
/// Deliberately smaller than what the daemon actually replays a queued item
/// with -- the queue itself stores the original `PlayMessage` (`queue.rs`) --
/// since a sender's list only needs enough to display and identify each
/// entry, not `volume`/`speed`/`headers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItemMessage {
    pub url: String,
    #[serde(default)]
    pub container: Option<String>,
    /// The video's title, if known. Resolved asynchronously in the
    /// background for YouTube/yt-dlp-style URLs (see `metadata.rs`) and left
    /// absent -- rather than blocking the item from appearing in the queue
    /// at all -- until that lookup resolves, or forever if it fails. A
    /// client should show a placeholder until an update with this field
    /// present arrives, the same push-on-change model documented on
    /// `QueueStateMessage`.
    #[serde(default)]
    pub title: Option<String>,
    /// The video's length in seconds, if known. Same async/optional
    /// contract as `title`.
    #[serde(default)]
    pub duration_secs: Option<f64>,
}

/// castoff private extension: the full play queue and the sender's position
/// within it. Sent both as `RequestQueue`'s reply and, unprompted, to every
/// connected sender whenever the queue changes (an item is added, playback
/// auto-advances past one, or a client jumps forward/backward) -- the same
/// push-on-change model `PlaybackUpdate` already uses (see README's Design
/// principles and `daemon/src/main.rs`'s `push_updates`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueStateMessage {
    pub generation_time: u64,
    pub items: Vec<QueueItemMessage>,
    /// Index into `items` of the current (playing, paused, or
    /// most-recently-played) item. `None` when the queue is empty or nothing
    /// has ever played from it yet.
    #[serde(default)]
    pub current_index: Option<usize>,
}

/// castoff private extension (see `Opcode::QueueJumpToIndex`): the queue
/// index a sender wants to jump straight to, e.g. a tap on an item in the
/// Android app's queue list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueJumpToIndexMessage {
    pub index: usize,
}

/// castoff private extension (see `Opcode::SetImageWallpaper`): tag or untag
/// an uploaded image (`id` is the id `upload.rs`'s `/images` endpoint
/// returned) for idle-screen wallpaper rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetImageWallpaperMessage {
    pub id: String,
    pub wallpaper: bool,
}

/// castoff private extension: `SetImageWallpaper`'s reply, confirming the tag
/// that was just set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageWallpaperUpdateMessage {
    pub generation_time: u64,
    pub id: String,
    pub wallpaper: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FCast v2 (docs.fcast.org/protocol/v2) specifies `PlaybackUpdate.state`
    /// as an integer enum, not a string. A real sender/receiver on the wire
    /// only ever sees the JSON produced here, so assert on that JSON directly.
    #[test]
    fn playback_update_state_serializes_as_numeric_on_the_wire() {
        let msg = PlaybackUpdateMessage {
            generation_time: 1234,
            state: PlaybackState::Playing,
            time: Some(1.5),
            duration: Some(10.0),
            speed: Some(1.0),
        };
        let value: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["state"], serde_json::json!(1));

        for (state, expected) in [
            (PlaybackState::Idle, 0),
            (PlaybackState::Playing, 1),
            (PlaybackState::Paused, 2),
        ] {
            let encoded = serde_json::to_string(&state).unwrap();
            assert_eq!(encoded, expected.to_string());
        }
    }

    #[test]
    fn playback_state_round_trips_through_numeric_wire_values() {
        for (raw, expected) in [
            ("0", PlaybackState::Idle),
            ("1", PlaybackState::Playing),
            ("2", PlaybackState::Paused),
        ] {
            let decoded: PlaybackState = serde_json::from_str(raw).unwrap();
            assert_eq!(decoded, expected);
        }
    }

    /// The `container` MIME type is what routes a `Play` between mpv (media)
    /// and the browser engine (web pages); see `explicit_target`'s doc
    /// comment. `None` means the daemon decides for itself.
    #[test]
    fn play_container_mime_type_is_an_explicit_target_override() {
        for container in [
            "text/html",
            "TEXT/HTML",
            " text/html ",
            "application/xhtml+xml",
            "text/html; charset=utf-8",
            "text/html ;charset=UTF-8",
            "application/xhtml+xml;charset=utf-8",
        ] {
            let msg = PlayMessage {
                container: Some(container.to_string()),
                url: Some("http://example.invalid/dashboard".to_string()),
                ..Default::default()
            };
            assert_eq!(
                msg.explicit_target(),
                Some(PlayTarget::Webpage),
                "container {container:?} must select a web page"
            );
        }

        for container in [
            "video/mp4",
            "audio/mpeg",
            "text/plain",
            "video/mp4; codecs=\"avc1.42E01E\"",
        ] {
            let msg = PlayMessage {
                container: Some(container.to_string()),
                url: Some("https://example.invalid/watch.mp4".to_string()),
                ..Default::default()
            };
            assert_eq!(
                msg.explicit_target(),
                Some(PlayTarget::Media),
                "container {container:?} must stay on the media path"
            );
        }

        // No `container`, an empty one, or nothing but MIME parameters: the
        // sender did not say, so the daemon decides (see `Player::play`).
        for container in [None, Some(""), Some("   "), Some("; charset=utf-8")] {
            let msg = PlayMessage {
                container: container.map(str::to_string),
                url: Some("https://example.invalid/unknown".to_string()),
                ..Default::default()
            };
            assert_eq!(
                msg.explicit_target(),
                None,
                "container {container:?} must leave the decision to the daemon"
            );
        }
    }
}
