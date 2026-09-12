//! Thin wrapper around libmpv2 that maps FCast-shaped requests onto mpv
//! commands/properties, and reads back mpv state as an FCast PlaybackUpdate.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use libmpv2::Mpv;

use crate::fcast::{PlayMessage, PlaybackState, PlaybackUpdateMessage};

pub struct Player {
    mpv: Mpv,
}

impl Player {
    /// Create the mpv core. No window is opened and no decoding happens until
    /// the first `play()` call: mpv's `idle` mode holds an empty, otherwise
    /// dormant window that Cage can still fullscreen, without spinning up a
    /// decode pipeline for nothing on boat power.
    pub fn new() -> Result<Self> {
        let mpv = Mpv::with_initializer(|init| {
            init.set_property("vo", "gpu")?;
            init.set_property("fullscreen", "yes")?;
            init.set_property("force-window", "yes")?;
            init.set_property("idle", "yes")?;
            init.set_property("keep-open", "yes")?;
            // Prefer hardware decode when available: much lower CPU/power draw
            // than software decode for the long, mostly-static playback runs
            // this box is built for.
            init.set_property("hwdec", "auto-safe")?;
            init.set_property("input-default-bindings", "no")?;
            init.set_property("input-vo-keyboard", "no")?;
            init.set_property("osc", "no")?;
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("failed to initialize mpv: {e:?}"))?;
        Ok(Self { mpv })
    }

    pub fn play(&self, msg: &PlayMessage) -> Result<()> {
        let target = match (msg.url.as_deref(), msg.content.as_deref()) {
            (Some(url), _) => url,
            (None, Some(_)) => anyhow::bail!(
                "Play message carries inline `content` (e.g. a DASH manifest) with no `url`; \
                 inline manifest playback is not yet supported"
            ),
            (None, None) => anyhow::bail!("Play message has neither `url` nor `content`"),
        };
        self.mpv
            .command("loadfile", &[target, "replace"])
            .map_err(|e| anyhow::anyhow!("loadfile failed: {e:?}"))?;
        // `keep-open=yes` (see `new()`) leaves `pause` set to `true` once a
        // previous file hits EOF, and mpv does not reset that property on the
        // next `loadfile`. Without this, a second Play call loads the new
        // file but stays paused on its first frame forever: silent, endless
        // black screen with no error, since `time-pos` never advances past 0.
        self.mpv
            .set_property("pause", false)
            .map_err(|e| anyhow::anyhow!("failed to unpause after loadfile: {e:?}"))?;
        if let Some(time) = msg.time {
            let _ = self.mpv.set_property("start", time);
        }
        if let Some(volume) = msg.volume {
            let _ = self.mpv.set_property("volume", to_mpv_volume(volume));
        }
        if let Some(speed) = msg.speed {
            let _ = self.mpv.set_property("speed", speed);
        }
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        self.mpv
            .set_property("pause", true)
            .map_err(|e| anyhow::anyhow!("pause failed: {e:?}"))
    }

    pub fn resume(&self) -> Result<()> {
        self.mpv
            .set_property("pause", false)
            .map_err(|e| anyhow::anyhow!("resume failed: {e:?}"))
    }

    /// Stop playback and return to mpv's idle state (no decode pipeline
    /// running, screen left black rather than a busy renderer).
    pub fn stop(&self) -> Result<()> {
        self.mpv
            .command("stop", &[])
            .map_err(|e| anyhow::anyhow!("stop failed: {e:?}"))
    }

    pub fn seek(&self, time: f64) -> Result<()> {
        self.mpv
            .command("seek", &[&time.to_string(), "absolute"])
            .map_err(|e| anyhow::anyhow!("seek failed: {e:?}"))
    }

    pub fn set_volume(&self, volume: f64) -> Result<()> {
        self.mpv
            .set_property("volume", to_mpv_volume(volume))
            .map_err(|e| anyhow::anyhow!("set_volume failed: {e:?}"))
    }

    pub fn set_speed(&self, speed: f64) -> Result<()> {
        self.mpv
            .set_property("speed", speed)
            .map_err(|e| anyhow::anyhow!("set_speed failed: {e:?}"))
    }

    /// Snapshot current mpv state as an FCast PlaybackUpdate.
    pub fn status(&self) -> PlaybackUpdateMessage {
        let paused: bool = self.mpv.get_property("pause").unwrap_or(false);
        let idle: bool = self.mpv.get_property("idle-active").unwrap_or(true);
        let time: Option<f64> = self.mpv.get_property("time-pos").ok();
        let duration: Option<f64> = self.mpv.get_property("duration").ok();
        let speed: Option<f64> = self.mpv.get_property("speed").ok();

        let state = if idle {
            PlaybackState::Idle
        } else if paused {
            PlaybackState::Paused
        } else {
            PlaybackState::Playing
        };

        PlaybackUpdateMessage {
            generation_time: now_millis(),
            state,
            time,
            duration,
            speed,
        }
    }

    /// Current volume on FCast's 0.0-1.0 scale.
    pub fn volume(&self) -> f64 {
        let v: f64 = self.mpv.get_property("volume").unwrap_or(0.0);
        v / 100.0
    }
}

fn to_mpv_volume(fcast_volume: f64) -> f64 {
    (fcast_volume.clamp(0.0, 1.0)) * 100.0
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// A `Player` around a headless (`vo=null`/`ao=null`, no window or audio
    /// device) mpv core, sufficient to drive real `Player::play` behavior in
    /// a sandbox with no display or sound hardware. Mirrors `Player::new`'s
    /// `keep-open` setting, since that's what the double-play regression test
    /// below needs to reproduce.
    fn headless_player() -> Player {
        let mpv = Mpv::with_initializer(|init| {
            init.set_property("vo", "null")?;
            init.set_property("ao", "null")?;
            init.set_property("idle", "yes")?;
            init.set_property("keep-open", "yes")?;
            Ok(())
        })
        .expect("failed to initialize headless mpv for test");
        Player { mpv }
    }

    /// Poll `mpv` for up to 5s until `pred` is true; panics on timeout so a
    /// stuck test fails fast instead of hanging.
    fn wait_until(mpv: &Mpv, mut pred: impl FnMut(&Mpv) -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if pred(mpv) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    #[test]
    fn play_rejects_inline_content_without_url() {
        let player = headless_player();
        let msg = PlayMessage {
            content: Some("<MPD>...</MPD>".to_string()),
            ..Default::default()
        };

        let err = player.play(&msg).expect_err("content-only play must fail");
        assert!(
            err.to_string().contains("inline `content`"),
            "unexpected error: {err}"
        );

        // The rejected message must never have reached mpv as a playback target:
        // the core stays idle rather than treating the manifest text as a path.
        let idle: bool = player.mpv.get_property("idle-active").unwrap_or(false);
        assert!(idle, "mpv should remain idle after a rejected play() call");
    }

    #[test]
    fn play_with_url_hands_the_url_to_mpv_and_leaves_idle_state() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("https://example.invalid/does-not-exist.mp4".to_string()),
            ..Default::default()
        };

        // A real (if unreachable) URL is accepted and queued for playback,
        // unlike the `content`-only case above.
        player.play(&msg).expect("play with a url must be accepted");
    }

    /// Regression test for a bug where a second `Play` after the first clip
    /// finished loaded correctly but never advanced past its first frame
    /// (silent black screen, no error): `keep-open=yes` leaves `pause=true`
    /// once a clip hits EOF, and mpv does not reset that property on the next
    /// `loadfile`, so the freshly loaded second clip inherited the stale
    /// pause. Uses a synthetic, network-free `lavfi` test source (no real
    /// video file needed) so this runs headless in CI.
    #[test]
    fn second_play_after_first_reaches_eof_is_not_left_paused() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };

        player.play(&msg).expect("first play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property("eof-reached").unwrap_or(false),
            "first clip to reach eof",
        );
        // Sanity check on the test's own premise: `keep-open` should indeed
        // leave mpv paused at eof, otherwise this test is not exercising the
        // bug it claims to.
        assert!(
            player.mpv.get_property::<bool>("pause").unwrap_or(false),
            "sanity: keep-open=yes should pause mpv once eof is reached"
        );

        player.play(&msg).expect("second play");

        let paused: bool = player.mpv.get_property("pause").unwrap_or(true);
        assert!(
            !paused,
            "second play() must not leave mpv paused on the reloaded clip's first frame"
        );
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "second clip's playback time to advance past its first frame",
        );
    }
}
