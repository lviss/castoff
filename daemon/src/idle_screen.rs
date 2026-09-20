//! What to display on-screen when the daemon has no active playback (no
//! file loaded yet, or paused at end-of-file with nothing queued next) --
//! castoff's answer to a Chromecast's ambient/idle screen.
//!
//! `IdleScreen` is the seam future content types plug into: `Clock` is the
//! only variant implemented today, but a future static-wallpaper variant is
//! just another `render`/`refresh_interval` match arm here, not a rewrite of
//! `Player`'s idle/active-playback wiring or of `IdleScreenController` below.
//! Web pages are deliberately not such a variant -- a real browser engine
//! cannot be an mpv OSD overlay, so they are a second Cage client instead
//! (`webpage.rs`); see README's "How webpage (dashboard) display works".

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use libmpv2::events::{Event, PropertyData};
use libmpv2::{Format, Mpv};

/// `mpv_observe_property` id used for the end-of-file watch. Only one
/// property is ever observed on a given `Mpv`, so any id works.
const EOF_WATCH_ID: u64 = 1;

/// How often the idle-screen wallpaper rotates to a new tagged image (see
/// `images.rs`'s `ImageStore::pick_next_wallpaper` and the `on_wallpaper_tick`
/// callback below). A single named constant rather than a literal so a future
/// "configurable interval" follow-up only needs to change this one place.
pub(crate) const WALLPAPER_ROTATION_INTERVAL: Duration = Duration::from_secs(60);

/// `osd-overlay` id the currently-shown idle screen's background rect draws
/// into. Only one idle screen is ever shown at a time, so every variant
/// sharing these two ids is enough for `IdleScreenController::hide` to clear
/// them generically.
///
/// ASS/libass only honors one `\pos`/`\an` override per event, so the
/// background rect and the foreground content (e.g. the clock text) must be
/// two separate `osd-overlay` calls with distinct ids -- combining them into
/// one ASS event makes the second `\pos`/`\an` block (and everything
/// positioned by it) fail to render.
const OSD_OVERLAY_BG_ID: i64 = 9000;
const OSD_OVERLAY_FG_ID: i64 = 9001;

/// Virtual ASS canvas the clock is drawn on; mpv scales this to whatever the
/// real output resolution is, so positions/sizes below are resolution
/// independent.
const CANVAS_WIDTH: i64 = 1920;
const CANVAS_HEIGHT: i64 = 1080;
/// ~33% of the canvas height.
const CLOCK_FONT_SIZE: i64 = 356;

/// See `IdleScreenController`'s `on_eof` field.
pub(crate) type OnEof = Arc<dyn Fn(&IdleScreenController) -> bool + Send + Sync>;

/// See `IdleScreenController`'s `on_change` field.
pub(crate) type OnChange = Arc<dyn Fn(&IdleScreenController) + Send + Sync>;

/// See `IdleScreenController`'s `on_wallpaper_tick` field.
pub(crate) type OnWallpaperTick = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// Content shown while nothing is playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleScreen {
    /// A simple on-screen clock (local time, `HH:MM`), centered on a blank
    /// background.
    Clock,
}

impl IdleScreen {
    /// How often `render` needs to be called to keep the screen current, or
    /// `None` for content a single `render` call is enough for (e.g. a
    /// future static wallpaper).
    fn refresh_interval(self) -> Option<Duration> {
        match self {
            IdleScreen::Clock => Some(Duration::from_secs(1)),
        }
    }

    /// Draw one frame of this idle screen using mpv's own OSD facilities
    /// (an ASS overlay), rather than a second rendering stack or a second
    /// Cage-visible client. `opacity` is 0 (invisible) to 255 (fully opaque),
    /// applied to both the background and the content so the screen can be
    /// faded in without a separate cover layer.
    fn render(self, mpv: &Mpv, opacity: u8) -> Result<()> {
        match self {
            IdleScreen::Clock => {
                // ASS alpha is inverted from opacity: 00 is opaque, FF is
                // transparent.
                let alpha = 255 - opacity;
                // A black rectangle covering the whole canvas -- so the last
                // video frame doesn't linger behind the clock -- and the
                // time, centered (`\an5`) at a large font size, are sent as
                // two separate ASS events/overlays: see `OSD_OVERLAY_BG_ID`'s
                // doc comment for why they can't share one event.
                let bg = format!(
                    "{{\\an7\\pos(0,0)\\1c&H000000&\\1a&H{alpha:02X}&\\bord0\\shad0\\p1}}\
                     m 0 0 l {w} 0 l {w} {h} l 0 {h}{{\\p0}}",
                    w = CANVAS_WIDTH,
                    h = CANVAS_HEIGHT,
                );
                draw_ass(mpv, OSD_OVERLAY_BG_ID, &bg)
                    .map_err(|e| anyhow::anyhow!("osd-overlay (background) failed: {e:?}"))?;
                draw_ass(mpv, OSD_OVERLAY_FG_ID, &clock_fg_ass(alpha))
                    .map_err(|e| anyhow::anyhow!("osd-overlay (foreground) failed: {e:?}"))
            }
        }
    }

    /// Draw just the clock text, fully opaque, and clear the background rect
    /// outright (`format="none"`, same as `IdleScreenController::hide`) so
    /// whatever mpv is currently showing as the video frame -- an idle-screen
    /// wallpaper image, see `WALLPAPER_ROTATION_INTERVAL` -- shows through
    /// instead of being covered by the clock's own opaque backdrop. The clock
    /// itself is drawn exactly as `render` draws it otherwise; only the
    /// backdrop differs.
    fn render_over_wallpaper(self, mpv: &Mpv) -> Result<()> {
        match self {
            IdleScreen::Clock => {
                mpv.command("osd-overlay", &[&OSD_OVERLAY_BG_ID.to_string(), "none", ""])
                    .map_err(|e| anyhow::anyhow!("osd-overlay (clear background) failed: {e:?}"))?;
                draw_ass(mpv, OSD_OVERLAY_FG_ID, &clock_fg_ass(0))
                    .map_err(|e| anyhow::anyhow!("osd-overlay (foreground) failed: {e:?}"))
            }
        }
    }
}

/// The clock text's ASS event, centered on the canvas at `CLOCK_FONT_SIZE`,
/// shared by `IdleScreen::render` and `render_over_wallpaper` so the two
/// backdrops (opaque black vs. a wallpaper image) draw an identical clock.
fn clock_fg_ass(alpha: u8) -> String {
    let text = local_time_hh_mm();
    let cx = CANVAS_WIDTH / 2;
    let cy = CANVAS_HEIGHT / 2;
    format!(
        "{{\\an5\\pos({cx},{cy})\\1c&HFFFFFF&\\1a&H{alpha:02X}\\fs{fs}\\bord0\\shad0}}{text}",
        fs = CLOCK_FONT_SIZE,
    )
}

fn draw_ass(mpv: &Mpv, id: i64, ass: &str) -> std::result::Result<(), libmpv2::Error> {
    mpv.command(
        "osd-overlay",
        &[
            &id.to_string(),
            "ass-events",
            ass,
            &CANVAS_WIDTH.to_string(),
            &CANVAS_HEIGHT.to_string(),
        ],
    )
}

fn local_time_hh_mm() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is a plain-old-data struct `localtime_r` fully populates.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[derive(Default)]
struct State {
    current: Option<IdleScreen>,
    /// Bumped on every show/hide so a redraw thread from a superseded
    /// `show` notices it's stale and exits instead of piling up.
    generation: u64,
    /// Whether mpv's current video frame is an idle-screen wallpaper image
    /// (loaded via `loadfile`) rather than the plain black backdrop. mpv's
    /// own `idle-active` property goes false once a wallpaper image is
    /// loaded, so `Player`'s status snapshot needs this to still report
    /// `Idle` to FCast clients (see `IdleScreenController::is_wallpaper_active`).
    wallpaper_active: bool,
    /// When the wallpaper was last rotated, so the refresh thread knows when
    /// `WALLPAPER_ROTATION_INTERVAL` has elapsed. `None` means "never, rotate
    /// on the next opportunity" -- reset by every fresh `show`, so idling
    /// picks a first wallpaper promptly rather than waiting a full interval.
    last_wallpaper_change: Option<Instant>,
}

/// Owns idle-screen state for one `Player`: which screen (if any) is
/// currently shown, the low-frequency redraw timer that keeps it current,
/// and the mpv event watcher that notices natural end-of-file.
pub(crate) struct IdleScreenController {
    mpv: Arc<Mpv>,
    state: Arc<Mutex<State>>,
    /// Serializes whole play/stop operations (see `Player::operation`),
    /// shared here so the wallpaper rotation's own `loadfile` call can never
    /// race a real `Play`/`Stop`'s `loadfile` -- both always take this lock
    /// before touching what mpv has loaded.
    operation: Arc<Mutex<()>>,
    /// Notified on every real `show`/`hide` transition (never from the
    /// per-second refresh thread's own re-renders, which don't change
    /// `current`) so `Player` can push an unprompted `PlaybackUpdate` for
    /// idle/active transitions that happen off of any of its own methods --
    /// e.g. the eof watcher bringing the clock back after a natural
    /// end-of-file. Takes `&Self` (rather than capturing it) for the same
    /// reason `on_eof` does: built and threaded into `new` before this
    /// controller exists (see `Player::build`). A plain callback, not a
    /// `PlaybackUpdateMessage` sender directly, so this module doesn't need
    /// to know about FCast message types.
    on_change: Option<OnChange>,
    /// Called by the eof watcher when mpv reaches end-of-file on its own,
    /// *before* it would otherwise show the idle screen -- lets `Player` try
    /// to advance the play queue instead (see `player.rs`'s queue
    /// auto-advance). Takes `&Self` (rather than capturing it) because this
    /// closure is built and threaded in before this controller exists (see
    /// `Player::build`). Returns whether it handled the end-of-file (started
    /// something else playing): when `false` or unset, the eof watcher falls
    /// back to showing the idle screen as before.
    on_eof: Option<OnEof>,
    /// Picks the next wallpaper image to rotate to (see
    /// `images.rs`'s `ImageStore::pick_next_wallpaper`), called by the
    /// existing Clock refresh thread on `WALLPAPER_ROTATION_INTERVAL`.
    /// `None` (the callback itself, or its `Some(())` return) means "nothing
    /// tagged," in which case the refresh thread leaves today's plain black
    /// backdrop alone -- unset entirely in the same test constructors that
    /// pass no `on_eof`.
    on_wallpaper_tick: Option<OnWallpaperTick>,
}

impl IdleScreenController {
    pub(crate) fn new(
        mpv: Arc<Mpv>,
        operation: Arc<Mutex<()>>,
        on_change: Option<OnChange>,
        on_eof: Option<OnEof>,
        on_wallpaper_tick: Option<OnWallpaperTick>,
    ) -> Self {
        Self {
            mpv,
            state: Arc::new(Mutex::new(State::default())),
            operation,
            on_change,
            on_eof,
            on_wallpaper_tick,
        }
    }

    /// Whether mpv's current video frame is an idle-screen wallpaper image
    /// rather than the plain black backdrop -- see `State::wallpaper_active`.
    /// `Player::status`/`snapshot_status` use this to keep reporting `Idle`
    /// to FCast clients even though mpv's own `idle-active` goes false once a
    /// wallpaper image is loaded.
    pub(crate) fn is_wallpaper_active(&self) -> bool {
        self.state.lock().unwrap().wallpaper_active
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn current(&self) -> Option<IdleScreen> {
        self.state.lock().unwrap().current
    }

    /// Render `screen` at a fraction of full opacity without changing which
    /// screen is current or starting the refresh timer. `Player` uses this to
    /// fade the idle clock in on a Stop or a failed load: the clock is itself
    /// an OSD overlay, so `mpv`'s recency-based stacking means a cover rect
    /// cannot reveal it -- fading its own alpha in is the crossfade. The
    /// caller must not use this while `show`'s refresh thread is running.
    pub(crate) fn render_at(&self, screen: IdleScreen, opacity: u8) -> Result<()> {
        screen.render(&self.mpv, opacity)
    }

    pub(crate) fn show(&self, screen: IdleScreen) -> Result<()> {
        screen.render(&self.mpv, 255)?;

        let generation = {
            let mut state = self.state.lock().unwrap();
            state.current = Some(screen);
            state.generation += 1;
            // A fresh idle period starts on the plain backdrop and is free
            // to pick a wallpaper right away rather than waiting a full
            // `WALLPAPER_ROTATION_INTERVAL` (see `State::last_wallpaper_change`).
            state.wallpaper_active = false;
            state.last_wallpaper_change = None;
            state.generation
        };

        if let Some(interval) = screen.refresh_interval() {
            let mpv = Arc::clone(&self.mpv);
            let state = Arc::clone(&self.state);
            let operation = Arc::clone(&self.operation);
            let on_wallpaper_tick = self.on_wallpaper_tick.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(interval);
                let still_current = {
                    let state = state.lock().unwrap();
                    state.generation == generation && state.current == Some(screen)
                };
                if !still_current {
                    return;
                }
                maybe_rotate_wallpaper(
                    &mpv,
                    &state,
                    &operation,
                    generation,
                    screen,
                    on_wallpaper_tick.as_ref(),
                );
                // Re-check after `maybe_rotate_wallpaper`, which briefly
                // drops and re-takes the state lock around the `operation`
                // lock/`loadfile`: a real `Play`/`Stop` could have taken over
                // in that window, and this redraw must not land on top of it.
                let (still_current, on_wallpaper) = {
                    let state = state.lock().unwrap();
                    (
                        state.generation == generation && state.current == Some(screen),
                        state.wallpaper_active,
                    )
                };
                if !still_current {
                    return;
                }
                let _ = if on_wallpaper {
                    screen.render_over_wallpaper(&mpv)
                } else {
                    screen.render(&mpv, 255)
                };
            });
        }
        if let Some(cb) = &self.on_change {
            cb(self);
        }
        Ok(())
    }

    pub(crate) fn hide(&self) -> Result<()> {
        let hid = {
            let mut state = self.state.lock().unwrap();
            if state.current.take().is_none() {
                false
            } else {
                state.generation += 1;
                state.wallpaper_active = false;
                true
            }
        };
        if !hid {
            return Ok(());
        }
        // format="none" removes the overlay outright, rather than replacing
        // it with empty content.
        for id in [OSD_OVERLAY_BG_ID, OSD_OVERLAY_FG_ID] {
            self.mpv
                .command("osd-overlay", &[&id.to_string(), "none", ""])
                .map_err(|e| anyhow::anyhow!("osd-overlay (clear) failed: {e:?}"))?;
        }
        if let Some(cb) = &self.on_change {
            cb(self);
        }
        Ok(())
    }

    /// Spawn the background thread that watches for mpv reaching
    /// end-of-file on its own -- nothing queued next, since this daemon
    /// only ever loads a single file at a time -- and brings the idle
    /// screen back without needing an incoming FCast command to trigger it.
    /// Blocks on mpv's own event queue rather than polling, so it costs
    /// nothing while playback is ongoing. Returns the thread's `JoinHandle`
    /// so a test-only teardown can join it (see `player.rs`'s `Player`
    /// `Drop` impl); production never joins it; the thread runs for the
    /// daemon's lifetime and exits on `Event::Shutdown` at process exit.
    pub(crate) fn spawn_eof_watcher(self: &Arc<Self>) -> Option<std::thread::JoinHandle<()>> {
        let this = Arc::clone(self);
        if this
            .mpv
            .observe_property("eof-reached", Format::Flag, EOF_WATCH_ID)
            .is_err()
        {
            return None;
        }
        Some(std::thread::spawn(move || loop {
            // A negative timeout blocks until the next real event, but mpv's
            // own wakeup mechanism can also return `None` (its "no event"
            // sentinel) as a spurious wakeup with nothing queued -- looping
            // back and waiting again is the documented way to handle that,
            // not treating it as shutdown.
            match this.mpv.wait_event(-1.0) {
                Some(Ok(Event::PropertyChange {
                    change: PropertyData::Flag(true),
                    reply_userdata: EOF_WATCH_ID,
                    ..
                })) => {
                    let handled = this.on_eof.as_ref().is_some_and(|cb| cb(&this));
                    if !handled {
                        let _ = this.show(IdleScreen::Clock);
                    }
                }
                Some(Ok(Event::Shutdown)) => return,
                _ => {}
            }
        }))
    }
}

/// Called from the Clock refresh thread on every ~1s tick: rotates to a new
/// wallpaper image once `WALLPAPER_ROTATION_INTERVAL` has elapsed since the
/// last one (or immediately, the first time this idle period ticks -- see
/// `State::last_wallpaper_change`), leaving mpv and `State` untouched
/// otherwise. The caller re-reads `State` itself afterward to decide what to
/// draw, rather than trusting a return value here, since this can also leave
/// `wallpaper_active` at whatever it already was.
///
/// Takes the `operation` lock (see `IdleScreenController::operation`) around
/// its own `loadfile` and re-checks `generation`/`current` under that lock
/// (not just the caller's earlier, lock-free check) so a real `Play`/`Stop`
/// that started while this thread was waiting for the lock always wins --
/// this call must never clobber genuine playback that started in the
/// meantime.
fn maybe_rotate_wallpaper(
    mpv: &Mpv,
    state: &Mutex<State>,
    operation: &Mutex<()>,
    generation: u64,
    screen: IdleScreen,
    on_wallpaper_tick: Option<&OnWallpaperTick>,
) {
    let Some(tick) = on_wallpaper_tick else {
        return;
    };
    let due = {
        let state = state.lock().unwrap();
        state
            .last_wallpaper_change
            .is_none_or(|t| t.elapsed() >= WALLPAPER_ROTATION_INTERVAL)
    };
    if !due {
        return;
    }
    let Some(path) = tick() else {
        return;
    };
    let Some(path) = path.to_str() else {
        return;
    };
    let _operation = operation.lock().unwrap();
    let still_current = {
        let state = state.lock().unwrap();
        state.generation == generation && state.current == Some(screen)
    };
    if !still_current || mpv.command("loadfile", &[path, "replace"]).is_err() {
        return;
    }
    let mut state = state.lock().unwrap();
    state.wallpaper_active = true;
    state.last_wallpaper_change = Some(Instant::now());
}
