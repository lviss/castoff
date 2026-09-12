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

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
