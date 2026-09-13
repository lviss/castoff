//! What to display on-screen when the daemon has no active playback (no
//! file loaded yet, or paused at end-of-file with nothing queued next) --
//! castoff's answer to a Chromecast's ambient/idle screen.
//!
//! `IdleScreen` is the seam future content types plug into: `Clock` is the
//! only variant implemented today, but a future static-wallpaper or
//! cast-a-webpage variant is just another `render`/`refresh_interval` match
//! arm here, not a rewrite of `Player`'s idle/active-playback wiring or of
//! `IdleScreenController` below.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use libmpv2::events::{Event, PropertyData};
use libmpv2::{Format, Mpv};

/// `mpv_observe_property` id used for the end-of-file watch. Only one
/// property is ever observed on a given `Mpv`, so any id works.
const EOF_WATCH_ID: u64 = 1;

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
    /// Cage-visible client.
    fn render(self, mpv: &Mpv) -> Result<()> {
        match self {
            IdleScreen::Clock => {
                let text = local_time_hh_mm();
                let cx = CANVAS_WIDTH / 2;
                let cy = CANVAS_HEIGHT / 2;
                // An opaque black rectangle covering the whole canvas -- so
                // the last video frame doesn't linger behind the clock --
                // and the time, centered (`\an5`) at a large font size, are
                // sent as two separate ASS events/overlays: see
                // `OSD_OVERLAY_BG_ID`'s doc comment for why they can't share
                // one event.
                let bg = format!(
                    "{{\\an7\\pos(0,0)\\1c&H000000&\\1a&H00&\\bord0\\shad0\\p1}}\
                     m 0 0 l {w} 0 l {w} {h} l 0 {h}{{\\p0}}",
                    w = CANVAS_WIDTH,
                    h = CANVAS_HEIGHT,
                );
                let fg = format!(
                    "{{\\an5\\pos({cx},{cy})\\1c&HFFFFFF&\\1a&H00&\\fs{fs}\\bord0\\shad0}}{text}",
                    fs = CLOCK_FONT_SIZE,
                );
                mpv.command(
                    "osd-overlay",
                    &[
                        &OSD_OVERLAY_BG_ID.to_string(),
                        "ass-events",
                        &bg,
                        &CANVAS_WIDTH.to_string(),
                        &CANVAS_HEIGHT.to_string(),
                    ],
                )
                .map_err(|e| anyhow::anyhow!("osd-overlay (background) failed: {e:?}"))?;
                mpv.command(
                    "osd-overlay",
                    &[
                        &OSD_OVERLAY_FG_ID.to_string(),
                        "ass-events",
                        &fg,
                        &CANVAS_WIDTH.to_string(),
                        &CANVAS_HEIGHT.to_string(),
                    ],
                )
                .map_err(|e| anyhow::anyhow!("osd-overlay (foreground) failed: {e:?}"))
            }
        }
    }
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
}

/// Owns idle-screen state for one `Player`: which screen (if any) is
/// currently shown, the low-frequency redraw timer that keeps it current,
/// and the mpv event watcher that notices natural end-of-file.
pub(crate) struct IdleScreenController {
    mpv: Arc<Mpv>,
    state: Arc<Mutex<State>>,
}

impl IdleScreenController {
    pub(crate) fn new(mpv: Arc<Mpv>) -> Self {
        Self {
            mpv,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn current(&self) -> Option<IdleScreen> {
        self.state.lock().unwrap().current
    }

    pub(crate) fn show(&self, screen: IdleScreen) -> Result<()> {
        screen.render(&self.mpv)?;

        let generation = {
            let mut state = self.state.lock().unwrap();
            state.current = Some(screen);
            state.generation += 1;
            state.generation
        };

        if let Some(interval) = screen.refresh_interval() {
            let mpv = Arc::clone(&self.mpv);
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || loop {
                std::thread::sleep(interval);
                let still_current = {
                    let state = state.lock().unwrap();
                    state.generation == generation && state.current == Some(screen)
                };
                if !still_current {
                    return;
                }
                let _ = screen.render(&mpv);
            });
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
        Ok(())
    }

    /// Spawn the background thread that watches for mpv reaching
    /// end-of-file on its own -- nothing queued next, since this daemon
    /// only ever loads a single file at a time -- and brings the idle
    /// screen back without needing an incoming FCast command to trigger it.
    /// Blocks on mpv's own event queue rather than polling, so it costs
    /// nothing while playback is ongoing.
    pub(crate) fn spawn_eof_watcher(self: &Arc<Self>) {
        let this = Arc::clone(self);
        if this
            .mpv
            .observe_property("eof-reached", Format::Flag, EOF_WATCH_ID)
            .is_err()
        {
            return;
        }
        std::thread::spawn(move || loop {
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
                    let _ = this.show(IdleScreen::Clock);
                }
                Some(Ok(Event::Shutdown)) => return,
                _ => {}
            }
        });
    }
}
