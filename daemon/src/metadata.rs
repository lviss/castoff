//! Background video-metadata lookup for the play queue (see `queue.rs`'s
//! `QueueEntry` and `player.rs`'s `spawn_metadata_lookup`).
//!
//! The daemon never shells out to `yt-dlp` for *playback* -- mpv's bundled
//! `ytdl_hook` Lua script does that internally (see README's "How YouTube
//! playback works"). Metadata is a separate concern: getting a title/length
//! to show in the queue does not need mpv to have loaded anything, so this
//! runs `yt-dlp` directly, the same already-assumed-present runtime
//! dependency `webpage.rs` treats `chromium` as (see `flake.nix`'s
//! `makeWrapper` PATH and `devShells.default`).

use std::process::{Command, Stdio};

use tracing::{debug, warn};

/// `yt-dlp`'s program name, or `CASTOFF_YTDLP` if set -- the same
/// env-override pattern `webpage.rs`'s `CASTOFF_BROWSER` uses, for a host
/// that wants to point at another `yt-dlp` build without rebuilding.
pub(crate) fn ytdlp_program() -> String {
    std::env::var("CASTOFF_YTDLP").unwrap_or_else(|_| "yt-dlp".to_string())
}

/// Hosts `yt-dlp`'s lookup is worth attempting for. Deliberately narrow
/// (YouTube only, matching the captain's ask) rather than "anything
/// `yt-dlp` might resolve": a Jellyfin stream or a local-file queue entry is
/// also handed to mpv on the media path, but shelling out to `yt-dlp` for
/// those would just be a wasted subprocess/network round-trip on every
/// queued item.
const YOUTUBE_HOSTS: [&str; 4] = ["youtube.com", "m.youtube.com", "music.youtube.com", "youtu.be"];

/// Whether `url` looks like a YouTube URL worth resolving title/length for
/// via `fetch_metadata`. A plain string/host check, not a network probe --
/// scoping only, see `YOUTUBE_HOSTS`.
pub(crate) fn is_youtube_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let host_end = rest.find(['/', '?', '#', ':']).unwrap_or(rest.len());
    let host = rest[..host_end].to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    YOUTUBE_HOSTS.contains(&host)
}

/// A queued video's title and length, as resolved by `fetch_metadata`.
pub(crate) struct VideoMetadata {
    pub title: String,
    pub duration_secs: f64,
}

/// Run `yt-dlp -j` (JSON metadata dump, no download) on `url` and pull out
/// its `title`/`duration`. `None` on any failure -- `program` missing,
/// non-zero exit, unparseable/incomplete JSON -- since a failed lookup must
/// leave the queue entry intact with the fields merely absent (see
/// `player.rs`'s `spawn_metadata_lookup`), never fail the enqueue itself.
pub(crate) fn fetch_metadata(program: &str, url: &str) -> Option<VideoMetadata> {
    let output = Command::new(program)
        .args(["-j", "--no-playlist", "--no-warnings", "--skip-download"])
        .arg(url)
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) => {
            warn!(error = %e, program, url, "failed to run yt-dlp for queue metadata lookup");
            return None;
        }
    };
    if !output.status.success() {
        debug!(
            status = ?output.status,
            stderr = %String::from_utf8_lossy(&output.stderr),
            url,
            "yt-dlp metadata lookup failed"
        );
        return None;
    }
    let value: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(e) => {
            warn!(error = %e, url, "yt-dlp metadata output did not parse as JSON");
            return None;
        }
    };
    let title = value.get("title")?.as_str()?.to_string();
    let duration_secs = value.get("duration")?.as_f64()?;
    Some(VideoMetadata {
        title,
        duration_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_youtube_urls_regardless_of_host_variant() {
        for url in [
            "https://www.youtube.com/watch?v=jNQXAC9IVRw",
            "https://youtube.com/watch?v=jNQXAC9IVRw",
            "https://m.youtube.com/watch?v=jNQXAC9IVRw",
            "https://music.youtube.com/watch?v=jNQXAC9IVRw",
            "https://youtu.be/jNQXAC9IVRw",
            "http://www.youtube.com/watch?v=jNQXAC9IVRw",
            "https://WWW.YOUTUBE.COM/watch?v=jNQXAC9IVRw",
        ] {
            assert!(is_youtube_url(url), "{url:?} must be recognized as YouTube");
        }
    }

    #[test]
    fn does_not_recognize_non_youtube_urls() {
        for url in [
            "https://jellyfin.example.invalid/stream/1",
            "file:///media/movie.mkv",
            "https://example.invalid/watch?v=jNQXAC9IVRw",
            "https://notyoutube.com/watch?v=jNQXAC9IVRw",
            "not a url",
        ] {
            assert!(
                !is_youtube_url(url),
                "{url:?} must not be recognized as YouTube"
            );
        }
    }
}
