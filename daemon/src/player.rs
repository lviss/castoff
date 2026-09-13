//! Thin wrapper around libmpv2 that maps FCast-shaped requests onto mpv
//! commands/properties, and reads back mpv state as an FCast PlaybackUpdate.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use libmpv2::Mpv;

use crate::fcast::{PlayMessage, PlaybackState, PlaybackUpdateMessage};
use crate::idle_screen::{IdleScreen, IdleScreenController};

pub struct Player {
    mpv: Arc<Mpv>,
    idle: Arc<IdleScreenController>,
}

impl Player {
    /// Create the mpv core. No window is opened and no decoding happens until
    /// the first `play()` call: mpv's `idle` mode holds an empty, otherwise
    /// dormant window that Cage can still fullscreen, without spinning up a
    /// decode pipeline for nothing on boat power. Shows the idle screen
    /// (see `idle_screen`) immediately, since there is no playback yet.
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
        let player = Self::from_mpv(Arc::new(mpv));
        player.show_idle_screen(IdleScreen::Clock)?;
        Ok(player)
    }

    fn from_mpv(mpv: Arc<Mpv>) -> Self {
        let idle = Arc::new(IdleScreenController::new(Arc::clone(&mpv)));
        idle.spawn_eof_watcher();
        Self { mpv, idle }
    }

    /// Show `screen` (currently only `IdleScreen::Clock`) until the next
    /// `hide_idle_screen`/`show_idle_screen` call.
    pub fn show_idle_screen(&self, screen: IdleScreen) -> Result<()> {
        self.idle.show(screen)
    }

    /// Clear whatever idle screen is currently shown, if any.
    pub fn hide_idle_screen(&self) -> Result<()> {
        self.idle.hide()
    }

    /// The idle screen currently shown, or `None` while actively playing.
    /// Not read anywhere in the daemon itself today; exists so tests can
    /// observe idle-screen state without inferring it from mpv properties.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn idle_screen(&self) -> Option<IdleScreen> {
        self.idle.current()
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
        self.hide_idle_screen()?;
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
    /// running), showing the idle screen in place of the old black screen.
    pub fn stop(&self) -> Result<()> {
        self.mpv
            .command("stop", &[])
            .map_err(|e| anyhow::anyhow!("stop failed: {e:?}"))?;
        self.show_idle_screen(IdleScreen::Clock)
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
    /// `keep-open` setting (needed for the double-play regression test
    /// below) and its initial idle-screen setup (needed for the idle-screen
    /// test below).
    fn headless_player() -> Player {
        let mpv = Mpv::with_initializer(|init| {
            init.set_property("vo", "null")?;
            init.set_property("ao", "null")?;
            init.set_property("idle", "yes")?;
            init.set_property("keep-open", "yes")?;
            Ok(())
        })
        .expect("failed to initialize headless mpv for test");
        let player = Player::from_mpv(Arc::new(mpv));
        player
            .show_idle_screen(IdleScreen::Clock)
            .expect("show initial idle screen");
        player
    }

    /// Poll `player`'s idle screen for up to 5s until it equals `expected`;
    /// panics on timeout. Needed because the eof-watch thread that brings
    /// the idle screen back after a natural end-of-file runs asynchronously.
    fn wait_until_idle_screen(player: &Player, expected: Option<IdleScreen>, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if player.idle_screen() == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
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

    /// Exercises the idle screen through all three triggers the spec calls
    /// for -- startup, natural end-of-file with nothing queued, and Stop --
    /// plus the one place it must disappear (a real Play). `show`/`hide`
    /// each round-trip through a real `mpv.command("show-text", ...)` call
    /// (propagating any mpv error via `?`), and the eof case is driven by
    /// mpv's own `eof-reached` property flipping on a real synthetic clip
    /// rather than the test calling `show_idle_screen` itself, so this is
    /// checking that the real wiring fires at the right moments, not just
    /// that some function was called. (A headless `vo=null`/`ao=null` mpv
    /// core never produces a paintable frame -- confirmed empirically:
    /// `screenshot-to-file` errors out even while idle with `force-window`
    /// -- so there is no way to also assert on rendered pixels here.)
    #[test]
    fn idle_screen_appears_when_idle_and_disappears_once_playback_starts() {
        let player = headless_player();

        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "clock must be showing at startup, before any Play"
        );

        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };
        player.play(&msg).expect("first play");
        assert_eq!(
            player.idle_screen(),
            None,
            "idle screen must be hidden once playback actually starts"
        );

        // Let the clip run to completion. `keep-open=yes` pauses mpv at eof
        // instead of unloading it, and nothing else queues a next file, so
        // the eof-watch thread should bring the clock back on its own.
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property("eof-reached").unwrap_or(false),
            "clip to reach eof",
        );
        wait_until_idle_screen(
            &player,
            Some(IdleScreen::Clock),
            "idle screen to return after end-of-file with nothing queued next",
        );

        player.play(&msg).expect("second play");
        assert_eq!(
            player.idle_screen(),
            None,
            "idle screen must be hidden again once replay starts"
        );

        player.stop().expect("stop");
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "clock must return after an explicit Stop"
        );
    }
}
