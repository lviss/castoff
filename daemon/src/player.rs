//! Thin wrapper around libmpv2 that maps FCast-shaped requests onto mpv
//! commands/properties, and reads back mpv state as an FCast PlaybackUpdate.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use libmpv2::events::Event;
use libmpv2::Mpv;
use tracing::error;

use crate::fcast::{PlayMessage, PlaybackState, PlaybackUpdateMessage};
use crate::idle_screen::{IdleScreen, IdleScreenController};
use crate::overlay::PlaybackOverlay;

pub struct Player {
    mpv: Arc<Mpv>,
    idle: Arc<IdleScreenController>,
    /// Loading spinner and start/stop fade drawn on top of whatever mpv is
    /// showing (see `overlay.rs`).
    overlay: Arc<PlaybackOverlay>,
    /// The most recently requested Play `url`, kept only so the background
    /// error listener (`spawn_error_logger`) can name which target a later
    /// async mpv error most likely belongs to. Attribution is best-effort
    /// toward the most recently submitted URL: an error already queued by
    /// mpv can be drained after a newer `play()` has replaced the slot, so
    /// the logged URL may be the newer request rather than the one that
    /// actually failed.
    last_target: Arc<Mutex<String>>,
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
            // Print mpv's own warning/error log lines to the daemon's stderr
            // (journald/console on the appliance). libmpv defaults to
            // `terminal=no`, so without this a failed asynchronous load --
            // e.g. `[ffmpeg] https: HTTP error 403 Forbidden` from a YouTube
            // stream URL yt-dlp resolved, or a `ytdl_hook`/`yt-dlp`
            // resolution failure -- was only visible to mpv's internal log
            // and never reached the console, leaving a silent black screen.
            // `all=warn` keeps this to actionable lines rather than
            // info-level chatter on every load.
            init.set_property("terminal", "yes")?;
            init.set_property("msg-level", "all=warn")?;
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("failed to initialize mpv: {e:?}"))?;
        let mpv = Arc::new(mpv);
        let last_target = Arc::new(Mutex::new(String::new()));
        spawn_error_logger(&mpv, Arc::clone(&last_target))?;
        let player = Self::from_mpv(mpv, last_target)?;
        player.show_idle_screen(IdleScreen::Clock)?;
        Ok(player)
    }

    fn from_mpv(mpv: Arc<Mpv>, last_target: Arc<Mutex<String>>) -> Result<Self> {
        let idle = Arc::new(IdleScreenController::new(Arc::clone(&mpv)));
        let overlay = Arc::new(PlaybackOverlay::new(Arc::clone(&mpv)));
        idle.spawn_eof_watcher();
        spawn_lifecycle_watcher(&mpv, Arc::clone(&idle), Arc::clone(&overlay))?;
        Ok(Self {
            mpv,
            idle,
            overlay,
            last_target,
        })
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

    /// Whether the loading spinner is currently up. Not read by the daemon
    /// itself; exists so tests can observe loading state directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn loading_overlay_active(&self) -> bool {
        self.overlay.is_active()
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
        // Fade the old content (previous video or the idle clock) out to
        // black, then put the spinner up over it, *before* submitting the
        // load: the spinner must be visible for the whole wait, so it can't
        // be raced by an instant `PlaybackRestart` from a fast load. It
        // stays up until `spawn_lifecycle_watcher` sees playback genuinely
        // restart (or the load fail), and redraws only until then. Any failure
        // here rolls the overlay back to the idle clock, so a partial setup
        // can't leave an opaque overlay with no spinner thread and no watcher
        // event coming to clear it.
        let begin_loading = || -> Result<()> {
            self.overlay.conceal()?;
            self.hide_idle_screen()?;
            self.overlay.spawn_spinner()
        };
        if let Err(e) = begin_loading() {
            let _ = self.abort_loading_to_idle();
            return Err(e);
        }
        // Held across both the write and the `loadfile` submission so two
        // concurrent `play()` calls (one per FCast connection, see main.rs)
        // can't interleave: without this, connection B could set
        // `last_target` between connection A's write and A's `loadfile`
        // call, so an async error later attributed to A's in-flight load
        // would wrongly blame B's url. Serializing the pair keeps
        // `last_target` in the same order as submission to mpv, which is
        // what the background error listener (see `spawn_error_logger`)
        // relies on to attribute an error that arrives while
        // resolution/opening is still in flight (e.g. a slow or failing
        // `ytdl_hook`/`yt-dlp` YouTube lookup).
        let mut last_target = self.last_target.lock().unwrap();
        *last_target = target.to_string();
        if let Err(e) = self.mpv.command("loadfile", &[target, "replace"]) {
            drop(last_target);
            // Nothing will load, so no async error/restart is coming to take
            // the spinner down; do it here and fall back to the idle clock.
            let _ = self.abort_loading_to_idle();
            return Err(anyhow::anyhow!("loadfile failed for url {target:?}: {e:?}"));
        }
        drop(last_target);
        // `keep-open=yes` (see `new()`) leaves `pause` set to `true` once a
        // previous file hits EOF, and mpv does not reset that property on the
        // next `loadfile`. Without this, a second Play call loads the new
        // file but stays paused on its first frame forever: silent, endless
        // black screen with no error, since `time-pos` never advances past 0.
        if let Err(e) = self.mpv.set_property("pause", false) {
            // The load is queued but nothing guarantees a `PlaybackRestart`
            // (playback is still paused), so don't leave the spinner up over
            // the opaque fade: clear it and fall back to the idle clock.
            let _ = self.abort_loading_to_idle();
            return Err(anyhow::anyhow!("failed to unpause after loadfile: {e:?}"));
        }
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
    /// running), fading the old video out and the idle clock in rather than
    /// cutting straight to it. A Stop while already idle and not loading is a
    /// no-op transition (just re-asserts the clock), so it doesn't blink the
    /// screen.
    ///
    /// "Already idle" is signalled by the idle clock being on screen, not by
    /// mpv's `idle-active`: with `keep-open=yes` (see `new()`) a clip that
    /// reached end-of-file on its own is not `idle-active` -- mpv stays paused
    /// on the last frame -- even though the eof watcher has already brought
    /// the clock back. Keying off `idle-active` there would conceal the
    /// visible clock to black and fade it straight back in: an ~800ms blink
    /// for a Stop that changes nothing. `fade_in_idle_clock` also cannot be
    /// used while the clock is already shown -- its `render_at` contract
    /// forbids running alongside `show`'s refresh thread -- so taking the
    /// no-op branch here is what keeps that path structurally out of reach.
    pub fn stop(&self) -> Result<()> {
        let clock_showing = self.idle.current().is_some();
        let was_loading = self.overlay.is_active();
        let already_idle = clock_showing && !was_loading;
        if !already_idle && !was_loading {
            if let Err(e) = self.overlay.conceal() {
                let _ = self.abort_loading_to_idle();
                return Err(e);
            }
        }
        if let Err(e) = self.mpv.command("stop", &[]) {
            let _ = self.abort_loading_to_idle();
            return Err(anyhow::anyhow!("stop failed: {e:?}"));
        }
        if already_idle {
            self.show_idle_screen(IdleScreen::Clock)
        } else {
            self.fade_in_idle_clock()
        }
    }

    /// Fade the idle clock in from black: drop the cover overlays (the screen
    /// behind is already black from `conceal` or the loading overlay), ramp
    /// the clock's own OSD alpha up, then install it as the current idle
    /// screen. The clock cannot be revealed by fading a cover rect away: mpv
    /// stacks overlays by recency, so a clock re-created after the rect would
    /// sit above it and pop in instead of fading.
    fn fade_in_idle_clock(&self) -> Result<()> {
        // The clock is already the current screen, so `show`'s refresh thread
        // is running and `render_at` must not be used alongside it (see its
        // contract); there is also nothing to fade. Drop any overlay and
        // re-assert the clock. This is the case a Stop after a clip reached
        // end-of-file, or a `conceal` failure before `hide_idle_screen`, lands
        // in -- keeping the alpha-fade path structurally out of reach rather
        // than relying on a timing assumption.
        if self.idle.current().is_some() {
            self.overlay.clear()?;
            return self.show_idle_screen(IdleScreen::Clock);
        }
        // Cancel/remove only the spinner; the opaque fade rect stays as the
        // black backdrop (a not-yet-cleared video frame must not flash
        // through). Draw the clock above it at zero opacity, ramp that alpha
        // up, and only then drop the rect -- by then the clock's own opaque
        // background covers the canvas, so removing the rect is invisible.
        let fade = || -> Result<()> {
            self.overlay.stop_spinner()?;
            self.idle.render_at(IdleScreen::Clock, 0)?;
            self.overlay
                .fade_in(|opacity| self.idle.render_at(IdleScreen::Clock, opacity))?;
            self.overlay.clear()?;
            self.show_idle_screen(IdleScreen::Clock)
        };
        if let Err(e) = fade() {
            // A failure partway through (spinner teardown, an OSD alpha draw,
            // or dropping the opaque rect) would otherwise leave the overlay
            // active with the black rect up and no watcher event coming to
            // clear it: the screen stays opaque black until the next command.
            // Roll back to a visible clock, best-effort.
            let _ = self.overlay.clear();
            let _ = self.show_idle_screen(IdleScreen::Clock);
            return Err(e);
        }
        Ok(())
    }

    /// A load ended without ever starting playback: cancel the spinner (if
    /// it's up) and fade back to the idle clock, so a failed Play ends on the
    /// idle screen with the console error report rather than an endless
    /// spinner. Used by the synchronous error paths in `play`/`stop`; clearing
    /// the overlay and showing the clock are both idempotent, so it is safe to
    /// call even when the overlay has already cleared itself.
    fn abort_loading_to_idle(&self) -> Result<()> {
        self.fade_in_idle_clock()
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

/// Spawn a background thread that logs mpv's *asynchronous* playback
/// errors -- the ones `Player::play`'s immediate `loadfile` call can't see,
/// because `loadfile` only queues the load; mpv resolves/opens the target
/// (including running `ytdl_hook`'s `yt-dlp` subprocess for a YouTube URL)
/// afterwards, off of that call stack. Without this, a `ytdl_hook`/`yt-dlp`
/// failure or timeout is a silent black screen with nothing in the log to
/// diagnose it from.
///
/// This blocks on mpv's event queue (`wait_event(-1.0)`) rather than
/// polling, so it costs nothing until mpv actually has something to report,
/// consistent with the daemon's power-efficiency design principle. It uses
/// a second client handle from `Mpv::create_client` (its own independent
/// event queue onto the same player core) so it never contends with the
/// `Player` methods' direct use of `mpv` from other threads.
fn spawn_error_logger(mpv: &Mpv, last_target: Arc<Mutex<String>>) -> Result<()> {
    let events = mpv
        .create_client(Some("castoff-error-logger"))
        .map_err(|e| anyhow::anyhow!("failed to create mpv event client: {e:?}"))?;
    std::thread::spawn(move || {
        loop {
            match events.wait_event(-1.0) {
                Some(Err(e)) => {
                    let url = last_target.lock().unwrap().clone();
                    // `describe_playback_error` gives the mpv error code a
                    // plain-language meaning (libmpv2's own `Display` is just
                    // `Raw(<int>)`); mpv's own log line -- printed because
                    // `terminal=yes`, see `new()` -- carries the concrete
                    // cause, e.g. an HTTP 403 from the media/CDN host.
                    error!(
                        url,
                        error = ?e,
                        reason = describe_playback_error(&e),
                        "mpv reported an async playback error -- playback did not start; \
                         see the mpv log line(s) above for the underlying cause (e.g. a \
                         ytdl_hook/yt-dlp resolution failure or an HTTP error from the \
                         media/CDN host)"
                    );
                }
                Some(Ok(Event::Shutdown)) => break,
                _ => {}
            }
        }
    });
    Ok(())
}

/// Spawn the background thread that tracks *playback lifecycle* on a second
/// mpv client handle: it takes the loading spinner down the moment mpv
/// reports that playback actually restarted (`PlaybackRestart` -- the first
/// frame is ready and rendering, not merely that `loadfile` was queued), and
/// hands the screen back to the idle clock if the load fails instead.
///
/// Like `spawn_error_logger`, this blocks on `wait_event(-1.0)` with no
/// polling, and uses its own client so it doesn't contend with the idle
/// clock's eof watcher (which owns the main handle's event queue) or with
/// the error logger. A failed load surfaces here as `Some(Err(..))` (mpv's
/// `END_FILE` with an error code); an `END_FILE` at EOF while the spinner is
/// still up is a load that never reached `PlaybackRestart` and also returns
/// to idle. The STOP/REDIRECT end reasons a superseding `loadfile` produces
/// are ignored, so a rapid re-Play keeps its own spinner.
fn spawn_lifecycle_watcher(
    mpv: &Mpv,
    idle: Arc<IdleScreenController>,
    overlay: Arc<PlaybackOverlay>,
) -> Result<()> {
    let events = mpv
        .create_client(Some("castoff-lifecycle"))
        .map_err(|e| anyhow::anyhow!("failed to create mpv lifecycle client: {e:?}"))?;
    // A fresh client doesn't necessarily have these enabled; ask explicitly
    // for the two this watcher depends on.
    for event in [
        libmpv2::events::mpv_event_id::PlaybackRestart,
        libmpv2::events::mpv_event_id::EndFile,
    ] {
        events
            .enable_event(event)
            .map_err(|e| anyhow::anyhow!("failed to enable mpv event: {e:?}"))?;
    }
    std::thread::spawn(move || loop {
        match events.wait_event(-1.0) {
            Some(Ok(event)) => {
                if !handle_lifecycle_event(event, &idle, &overlay) {
                    return;
                }
            }
            // `wait_event` surfaces an `END_FILE` with a nonzero error code as
            // `Err`; the concrete reason is already logged by
            // `spawn_error_logger`. A failed load must never leave the spinner
            // up forever.
            Some(Err(_)) => restore_idle_clock(&idle, &overlay),
            None => {}
        }
    });
    Ok(())
}

/// Put the idle clock back and tear the spinner down because the in-flight
/// load is not going to start. A no-op when nothing is showing.
fn restore_idle_clock(idle: &IdleScreenController, overlay: &PlaybackOverlay) {
    if !overlay.is_active() {
        return;
    }
    // See `Player::fade_in_idle_clock`: keep the opaque rect as the backdrop,
    // fade the clock in above it, then drop the rect.
    let _ = overlay.stop_spinner();
    let _ = idle.render_at(IdleScreen::Clock, 0);
    let _ = overlay.fade_in(|opacity| idle.render_at(IdleScreen::Clock, opacity));
    let _ = overlay.clear();
    let _ = idle.show(IdleScreen::Clock);
}

/// Dispatch one lifecycle event; returns `false` when the watcher should stop
/// (mpv shutdown). A file that reaches end-of-file without ever emitting
/// `PlaybackRestart` is a load that never started rendering and returns to the
/// idle clock; STOP/REDIRECT are deliberately ignored because those are what a
/// superseding `loadfile` produces for the load being replaced, and clearing on
/// them would kill the newer load's spinner.
fn handle_lifecycle_event(
    event: Event<'_>,
    idle: &IdleScreenController,
    overlay: &PlaybackOverlay,
) -> bool {
    match event {
        // First frame is ready: stop spinning and fade the black overlay away
        // to reveal playback.
        Event::PlaybackRestart => {
            let _ = overlay.reveal();
        }
        Event::EndFile(libmpv2::mpv_end_file_reason::Eof) => restore_idle_clock(idle, overlay),
        Event::Shutdown => return false,
        _ => {}
    }
    true
}

fn describe_playback_error(e: &libmpv2::Error) -> &'static str {
    use libmpv2::mpv_error;
    // Only the codes a failed network/media load actually produces are named
    // specially; anything else falls back to pointing at mpv's own log line.
    match e {
        libmpv2::Error::Raw(mpv_error::NothingToPlay) => {
            "nothing to play: mpv could not open any stream the URL resolved to \
             (the mpv log line above has the concrete cause, e.g. an HTTP 403)"
        }
        libmpv2::Error::Raw(mpv_error::LoadingFailed) => {
            "loading failed: mpv could not load/open this URL"
        }
        _ => "see the mpv log line above for the concrete cause",
    }
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
        let player = Player::from_mpv(Arc::new(mpv), Arc::new(Mutex::new(String::new())))
            .expect("create player");
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
    fn wait_until(mpv: &Mpv, pred: impl FnMut(&Mpv) -> bool, what: &str) {
        wait_until_timeout(mpv, Duration::from_secs(5), pred, what)
    }

    /// Poll `player`'s loading overlay for up to 30s until it is down; panics
    /// on timeout. The generous window covers a genuinely failing network
    /// load (`MPV_ERROR_NOTHING_TO_PLAY` after DNS/connect failure) in a
    /// sandbox with no network, which can take a while to give up.
    fn wait_until_loading_cleared(player: &Player, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if !player.loading_overlay_active() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    /// A minimal, valid 8kHz mono PCM WAV of `seconds` seconds of silence,
    /// built by hand so the tests need no media fixture on disk or encoder on
    /// `PATH`.
    fn silent_wav(seconds: u32) -> Vec<u8> {
        const SAMPLE_RATE: u32 = 8000;
        const CHANNELS: u16 = 1;
        const BITS: u16 = 16;
        let data_len = SAMPLE_RATE * seconds * CHANNELS as u32 * (BITS as u32 / 8);
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&CHANNELS.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * CHANNELS as u32 * (BITS as u32 / 8)).to_le_bytes());
        wav.extend_from_slice(&(CHANNELS * BITS / 8).to_le_bytes()); // block align
        wav.extend_from_slice(&BITS.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        wav
    }

    /// Serve one HTTP request over a loopback port, but hold the response
    /// until the test releases it: the handler reads the request, signals
    /// `request_seen`, then blocks on `release` before sending any response
    /// bytes. That makes "the load is in flight and not a single response byte
    /// has arrived yet" a deterministic state to assert on, rather than a
    /// race against a fixed sleep. Returns the `http://` URL to hand to
    /// `Play` plus the two channels.
    #[allow(clippy::type_complexity)]
    fn spawn_gated_http_source(
        body: Vec<u8>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test http server");
        let addr = listener.local_addr().expect("local addr");
        let (request_seen_tx, request_seen_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = request_seen_tx.send(());
            let _ = release_rx.recv();
            let header = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: audio/wav\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });
        (
            format!("http://{addr}/test.wav"),
            request_seen_rx,
            release_tx,
        )
    }

    /// Like `wait_until`, but with a caller-chosen timeout -- for cases (e.g.
    /// a real network `yt-dlp` resolution) where 5s can be too tight.
    fn wait_until_timeout(
        mpv: &Mpv,
        timeout: Duration,
        mut pred: impl FnMut(&Mpv) -> bool,
        what: &str,
    ) {
        let deadline = Instant::now() + timeout;
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
    /// each round-trip through a real `mpv.command("osd-overlay", ...)` call
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

    /// Run `action` while sampling `player.loading_overlay_active()` at high
    /// frequency, and report whether the loading/fade overlay ever came up.
    /// The overlay only exists *during* a blocking `stop()` (it is torn down
    /// before the call returns), so whether a Stop took the fade path is not
    /// observable afterwards; this samples the real state while it runs.
    fn overlay_came_up_during(player: &Player, action: impl FnOnce()) -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::TryRecvError;

        let saw = AtomicBool::new(false);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let saw = &saw;
            scope.spawn(move || loop {
                if player.loading_overlay_active() {
                    saw.store(true, Ordering::SeqCst);
                }
                match done_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => {
                        if player.loading_overlay_active() {
                            saw.store(true, Ordering::SeqCst);
                        }
                        return;
                    }
                    Err(TryRecvError::Empty) => {}
                }
                std::thread::sleep(Duration::from_millis(2));
            });
            action();
            let _ = done_tx.send(());
        });
        saw.load(Ordering::SeqCst)
    }

    /// Regression test: a Stop after a clip has already ended must not blink
    /// the screen. `keep-open=yes` leaves mpv paused at the last frame (so
    /// `idle-active` is still false) while the eof watcher has already put the
    /// idle clock back; the old `was_playing = !idle-active` check therefore
    /// concealed the visible clock to black and faded it straight back in. The
    /// fade/loading overlay is the only thing that blacks the screen on a
    /// Stop, so it must never become active here, and the clock must stay up.
    #[test]
    fn stop_after_end_of_file_does_not_blink_the_idle_clock() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };
        player.play(&msg).expect("play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property("eof-reached").unwrap_or(false),
            "clip to reach eof",
        );
        wait_until_idle_screen(
            &player,
            Some(IdleScreen::Clock),
            "clock to return after end-of-file",
        );
        // Sanity check on the test's own premise: with `keep-open=yes` mpv is
        // still not `idle-active` at this point, so the old
        // `was_playing = !idle-active` check did take the blink path.
        assert!(
            !player.mpv.get_property::<bool>("idle-active").unwrap_or(true),
            "sanity: keep-open leaves mpv non-idle at eof, which caused the blink"
        );

        assert!(
            !overlay_came_up_during(&player, || player.stop().expect("stop")),
            "a Stop after end-of-file must not conceal the clock to black"
        );
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the clock must stay up across a no-op Stop"
        );
    }

    /// The other direction of the no-op rule: a Stop while playback is
    /// genuinely on screen must still take the fade path (video out through
    /// black, clock in) and land on the idle clock.
    #[test]
    fn stop_while_playing_fades_out_to_the_idle_clock() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=30".to_string()),
            ..Default::default()
        };
        player.play(&msg).expect("play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.1,
            "playback to start",
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "the clock is hidden while the video plays"
        );

        assert!(
            overlay_came_up_during(&player, || player.stop().expect("stop")),
            "a Stop during playback must fade through the overlay"
        );
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the clock must return after the fade"
        );
    }

    /// Verifies real YouTube playback end-to-end through mpv's built-in
    /// `ytdl_hook` (see README's "How YouTube playback works"): no daemon
    /// code shells out to `yt-dlp` itself, mpv's bundled Lua script does,
    /// automatically, for any URL it doesn't recognize as directly playable.
    /// Requires network access and `yt-dlp` on `PATH` (already the case in
    /// `nix develop`'s dev shell -- see `devShells.default` in flake.nix),
    /// so this is `#[ignore]`d by default: the sandboxed `nix build`/
    /// `nix flake check` checkPhase has no network access, and `yt-dlp` is a
    /// runtime-only dependency (see flake.nix), not a build input. Run with
    /// `nix develop -c cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access and yt-dlp on PATH; run with `cargo test -- --ignored`"]
    fn real_youtube_url_resolves_and_plays_via_ytdl_hook() {
        let player = headless_player();
        let msg = PlayMessage {
            // "Me at the zoo", the first video ever uploaded to YouTube:
            // short (19s), extremely unlikely to ever be removed -- a stable
            // target for this test.
            url: Some("https://www.youtube.com/watch?v=jNQXAC9IVRw".to_string()),
            ..Default::default()
        };

        player
            .play(&msg)
            .expect("play with a youtube url must be accepted");

        // `duration` is only known once ytdl_hook has resolved a real,
        // direct media URL via `yt-dlp` and mpv has opened it -- a bare
        // subprocess spawn with no real resolution wouldn't produce this.
        wait_until_timeout(
            &player.mpv,
            Duration::from_secs(30),
            |mpv| mpv.get_property::<f64>("duration").unwrap_or(0.0) > 0.0,
            "duration to be known (ytdl_hook resolved a real stream)",
        );
        let duration: f64 = player.mpv.get_property("duration").unwrap();
        assert!(
            (15.0..25.0).contains(&duration),
            "expected ~19s duration for the known test video, got {duration}"
        );

        wait_until_timeout(
            &player.mpv,
            Duration::from_secs(15),
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.5,
            "playback time-pos to advance",
        );
    }

    /// Regression test for a race between concurrent `play()` calls (one per
    /// FCast connection, see `main.rs`'s per-connection `spawn_blocking`):
    /// without holding `last_target`'s lock across both the write and the
    /// `loadfile` submission, one thread's write and its `loadfile` call
    /// could straddle another thread's write/`loadfile` pair, so the URL
    /// mpv ends up actually loading and the URL recorded in `last_target`
    /// (what `spawn_error_logger` blames for a later async error) could
    /// disagree. Hammers `play()` from many threads at once, releasing them
    /// together via a `Barrier` to maximize interleaving, and asserts that
    /// whatever mpv actually loaded always matches what `last_target` says
    /// it loaded.
    #[test]
    fn concurrent_plays_keep_last_target_consistent_with_mpv() {
        let player = headless_player();
        const THREADS: usize = 8;
        const ROUNDS: usize = 30;

        for round in 0..ROUNDS {
            let barrier = std::sync::Barrier::new(THREADS);
            std::thread::scope(|scope| {
                for i in 0..THREADS {
                    let barrier = &barrier;
                    let player = &player;
                    scope.spawn(move || {
                        // The `duration` decimal encodes `(round, i)` uniquely
                        // (just to make each url distinguishable to mpv/us);
                        // it plays no role in the race being tested.
                        let uid = round * THREADS + i;
                        let msg = PlayMessage {
                            url: Some(format!(
                                "av://lavfi:testsrc=size=64x64:rate=10:duration={:.3}",
                                5.0 + uid as f64 * 0.001
                            )),
                            ..Default::default()
                        };
                        barrier.wait();
                        player.play(&msg).expect("play");
                    });
                }
            });

            // Let mpv's command queue settle so its `path` property reflects
            // the most recently submitted `loadfile`.
            wait_until(
                &player.mpv,
                {
                    let mut last_seen = String::new();
                    let mut stable_polls = 0;
                    move |mpv| {
                        let path: String = mpv.get_property("path").unwrap_or_default();
                        if path == last_seen {
                            stable_polls += 1;
                        } else {
                            stable_polls = 0;
                            last_seen = path;
                        }
                        stable_polls >= 3
                    }
                },
                "mpv path property to settle",
            );

            let mpv_path: String = player.mpv.get_property("path").unwrap_or_default();
            let recorded = player.last_target.lock().unwrap().clone();
            assert_eq!(
                mpv_path, recorded,
                "round {round}: last_target must always name whatever mpv actually loaded"
            );
        }
    }

    /// The spinner must be up for the *whole* wait of a slow load and gone as
    /// soon as playback actually starts rendering. The server is gated: it
    /// signals when it has received the request, then withholds every response
    /// byte until the test releases it, so there is a deterministic window in
    /// which mpv is genuinely still waiting on the network. Only the real mpv
    /// `PlaybackRestart` event may take the spinner down.
    #[test]
    fn loading_overlay_shows_during_slow_load_and_clears_when_playback_starts() {
        let player = headless_player();
        let (url, request_seen, release) = spawn_gated_http_source(silent_wav(5));
        assert!(
            !player.loading_overlay_active(),
            "no spinner before any Play"
        );

        let msg = PlayMessage {
            url: Some(url),
            ..Default::default()
        };
        player
            .play(&msg)
            .expect("play with a slow url must be accepted");

        assert!(
            player.loading_overlay_active(),
            "spinner must be up once the slow load is in flight"
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "the idle clock is hidden behind the loading overlay"
        );

        // Wait until mpv has actually opened the stream and its request has
        // reached the server, which is now withholding the response. The
        // spinner must still be up here: no response byte has been sent, so
        // mpv cannot have reached `PlaybackRestart` yet. This is the invariant
        // the old sleep-based test never actually asserted.
        request_seen
            .recv_timeout(Duration::from_secs(10))
            .expect("server to receive the load request");
        assert!(
            player.loading_overlay_active(),
            "spinner must stay up while the server withholds its first byte"
        );

        release.send(()).expect("release the server response");
        wait_until_loading_cleared(&player, "spinner to clear once playback restarts");
        assert!(
            !player.loading_overlay_active(),
            "spinner must stay cleared after playback starts"
        );
        // Playback really did start (not merely the overlay timing out): the
        // slow WAV is playing and its clock advances.
        wait_until_timeout(
            &player.mpv,
            Duration::from_secs(10),
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.0,
            "slow source to actually start playing",
        );
    }

    /// A superseding Play must not tear down the new load's spinner. mpv
    /// produces an `EndFile` for the load being replaced (STOP/REDIRECT) when
    /// a newer `loadfile` arrives; only an EOF-without-`PlaybackRestart` should
    /// return to idle. Both sources are gated, so while the second load is in
    /// flight nothing else can have legitimately cleared its overlay.
    #[test]
    fn rapid_replay_keeps_the_new_loads_spinner_up() {
        let player = headless_player();
        let (url_a, seen_a, _release_a) = spawn_gated_http_source(silent_wav(5));
        player
            .play(&PlayMessage {
                url: Some(url_a),
                ..Default::default()
            })
            .expect("first play");
        seen_a
            .recv_timeout(Duration::from_secs(10))
            .expect("server A to receive the first request");

        let (url_b, seen_b, release_b) = spawn_gated_http_source(silent_wav(5));
        player
            .play(&PlayMessage {
                url: Some(url_b),
                ..Default::default()
            })
            .expect("second play");
        seen_b
            .recv_timeout(Duration::from_secs(10))
            .expect("server B to receive the second request");

        // mpv has by now superseded load A, which emits an `EndFile` for A
        // (not EOF). B's spinner must survive it; if that event cleared the
        // overlay, B would stay uncovered over the opaque fade.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            player.loading_overlay_active(),
            "a superseding load must keep the new spinner up"
        );

        release_b.send(()).expect("release server B");
        wait_until_loading_cleared(&player, "second spinner to clear once playback restarts");
    }

    /// A load that fails must not leave the spinner up (the console error
    /// report is the real feedback for failure): the overlay clears and the
    /// idle clock comes back. Uses an unresolvable host so the load fails
    /// asynchronously, which is the path this daemon's own error logger covers.
    #[test]
    fn failed_load_clears_loading_overlay_and_returns_to_idle_clock() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("https://example.invalid/does-not-exist.mp4".to_string()),
            ..Default::default()
        };
        player
            .play(&msg)
            .expect("play is accepted; the load fails asynchronously");
        assert!(
            player.loading_overlay_active(),
            "spinner is up while the failing load is in flight"
        );

        wait_until_loading_cleared(&player, "spinner to clear after the load fails");
        wait_until_idle_screen(
            &player,
            Some(IdleScreen::Clock),
            "idle clock to return after the load fails",
        );
    }

    /// A load that ends at EOF without ever emitting `PlaybackRestart` must
    /// return to the idle clock instead of leaving the spinner up over opaque
    /// black. mpv can end an incomplete/corrupted/interrupted remote source at
    /// EOF with no error code (mpv `client.h`, `MPV_END_FILE_REASON_EOF`).
    ///
    /// This daemon runs mpv with `keep-open=yes`, under which a *normal* EOF
    /// emits no `END_FILE` at all (the idle clock's `eof-reached` property
    /// watcher handles it instead); confirmed empirically. An
    /// EOF-before-`PlaybackRestart` load end could not be produced end-to-end
    /// either -- a zero-length WAV and a truncated remote WAV both emitted
    /// `PlaybackRestart` (or `MPV_ERROR_LOADING_FAILED`) first. So this drives
    /// the lifecycle watcher's real event handler with the real `Event` value
    /// it is built to receive and asserts the observable state transition.
    #[test]
    fn end_of_file_without_playback_restart_returns_to_idle_clock() {
        let player = headless_player();
        // Put the loading overlay up exactly as a Play does ...
        player.overlay.conceal().expect("conceal");
        player.hide_idle_screen().expect("hide idle");
        player.overlay.spawn_spinner().expect("spinner");
        assert!(player.loading_overlay_active(), "spinner must be up");

        // ... then deliver the load-will-not-start event the watcher handles.
        let keep_going = handle_lifecycle_event(
            Event::EndFile(libmpv2::mpv_end_file_reason::Eof),
            &player.idle,
            &player.overlay,
        );

        assert!(keep_going, "a normal EOF must not stop the watcher");
        assert!(
            !player.loading_overlay_active(),
            "the spinner must be torn down rather than stuck over opaque black"
        );
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the idle clock must return"
        );
    }

    /// Regression test for the failed-setup path where the loading overlay has
    /// *already* cleared itself (e.g. `spawn_spinner`'s draw failed and its
    /// error path called `clear()`) after `hide_idle_screen()` removed the
    /// clock. `abort_loading_to_idle` must still put the clock back; otherwise
    /// `play()` returns Err with no overlay up and no clock, leaving an
    /// unadorned black screen until the next command.
    #[test]
    fn abort_restores_idle_clock_when_overlay_already_cleared() {
        let player = headless_player();
        assert_eq!(player.idle_screen(), Some(IdleScreen::Clock));

        player.overlay.conceal().expect("conceal");
        player.hide_idle_screen().expect("hide idle clock");
        // Exactly what `spawn_spinner`'s error path does when its draw fails.
        player.overlay.clear().expect("clear overlay");
        assert!(
            !player.loading_overlay_active(),
            "precondition: the overlay has already cleared itself"
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "precondition: clock hidden and overlay gone -- screen would be black"
        );

        player
            .abort_loading_to_idle()
            .expect("abort back to the idle clock");

        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the idle clock must be restored even when the overlay had already cleared"
        );
    }
}
