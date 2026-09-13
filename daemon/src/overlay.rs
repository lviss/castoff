//! The "something is happening" layer drawn on top of whatever mpv is
//! showing: a loading spinner for a Play request whose load hasn't started
//! rendering yet, and a short fade-through-black around starts and stops.
//!
//! Both are drawn through mpv's own OSD (`osd-overlay` ASS events), exactly
//! like the idle clock in `idle_screen.rs` -- the same rendering surface, not
//! a compositor, a second window, or a second drawing stack. They use their
//! own overlay ids so they compose with (rather than disturb) the idle
//! clock's overlays.
//!
//! Power efficiency (see README's design principles): every animation here is
//! a *bounded* loop of `draw` calls with a `sleep` between them. The fade is a
//! fixed handful of frames and then stops; the spinner redraws only while a
//! Play request is genuinely in flight and is torn down the moment playback
//! restarts, a load fails, or a newer command supersedes it. Nothing here
//! redraws on a timer once settled.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use libmpv2::Mpv;

/// Virtual ASS canvas both overlays are drawn on; mpv scales this to the real
/// output resolution, so every coordinate below is resolution independent.
/// Matches the idle clock's canvas (`idle_screen.rs`) so the two layers line
/// up, but is declared locally to keep this module self-contained.
const CANVAS_WIDTH: i64 = 1920;
const CANVAS_HEIGHT: i64 = 1080;

/// `osd-overlay` ids. Distinct from the idle clock's 9000/9001; mpv draws
/// higher ids above lower ones, so the fade rect (9100) covers the idle
/// clock and video, and the spinner (9101) sits on top of the fade rect.
const OSD_OVERLAY_FADE_ID: i64 = 9100;
const OSD_OVERLAY_SPINNER_ID: i64 = 9101;

/// Fade-through-black: a fixed number of frames at a fixed cadence, so one
/// direction is ~150ms -- a transition, not a presentation.
const FADE_STEPS: u32 = 5;
const FADE_STEP: Duration = Duration::from_millis(30);

/// Spinner cadence: 30 degrees every 100ms = one revolution per 1.2s, and a
/// redraw rate (10/s) that is trivial next to video decode but still reads as
/// motion.
const SPINNER_STEP: Duration = Duration::from_millis(100);
const SPINNER_DEGREES_PER_STEP: f64 = 30.0;

#[derive(Default)]
struct State {
    /// Bumped by every command so a spinner thread (or an in-flight fade)
    /// from a superseded command notices it is stale and stops promptly
    /// instead of piling up or drawing over newer state.
    generation: u64,
    /// True while the loading overlay (opaque black + spinner) is up.
    active: bool,
    /// Current fade-rect opacity: 0 = fully transparent, 255 = opaque black.
    opacity: u8,
}

/// Owns the loading/fade overlays for one `Player`.
pub(crate) struct PlaybackOverlay {
    mpv: Arc<Mpv>,
    state: Arc<Mutex<State>>,
}

impl PlaybackOverlay {
    pub(crate) fn new(mpv: Arc<Mpv>) -> Self {
        Self {
            mpv,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Whether the loading overlay is currently up (either mid-fade or
    /// spinning). Used by `Player` to decide whether a failed load needs to
    /// return the screen to the idle clock, and by tests.
    pub(crate) fn is_active(&self) -> bool {
        self.state.lock().unwrap().active
    }

    /// Start an animation sequence: mark the overlay active and invalidate
    /// any animation already running, returning this sequence's generation.
    fn begin(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        state.active = true;
        state.generation
    }

    fn stale(&self, generation: u64) -> bool {
        self.state.lock().unwrap().generation != generation
    }

    /// Fade whatever is on screen to opaque black. Used at the start of a
    /// Play (so the old content or idle clock disappears behind the spinner)
    /// and at the start of a Stop (so the old video disappears before the
    /// idle clock takes its place). Blocking but bounded (~150ms).
    pub(crate) fn conceal(&self) -> Result<()> {
        let generation = self.begin();
        let start = self.state.lock().unwrap().opacity;
        for step in 1..=FADE_STEPS {
            if self.stale(generation) {
                return Ok(());
            }
            let opacity = lerp_u8(start, 255, step, FADE_STEPS);
            draw_overlay(&self.mpv, opacity, None)?;
            std::thread::sleep(FADE_STEP);
        }
        self.state.lock().unwrap().opacity = 255;
        Ok(())
    }

    /// Draw the spinner over the (now black) screen and keep it rotating
    /// until the sequence is superseded, `finish_loading` runs, or `clear`
    /// is called. Returns as soon as the first frame is drawn; the redraw
    /// loop runs on its own thread and idles otherwise.
    pub(crate) fn spawn_spinner(&self) -> Result<()> {
        let generation = self.begin();
        self.state.lock().unwrap().opacity = 255;
        draw_overlay(&self.mpv, 255, Some(0.0))?;

        let mpv = Arc::clone(&self.mpv);
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || {
            let mut angle = 0.0;
            loop {
                std::thread::sleep(SPINNER_STEP);
                if state.lock().unwrap().generation != generation {
                    return;
                }
                angle = (angle + SPINNER_DEGREES_PER_STEP) % 360.0;
                let _ = draw_overlay(&mpv, 255, Some(angle));
            }
        });
        Ok(())
    }

    /// Playback has genuinely started rendering, or a failed load is being
    /// handed back to the idle clock: stop the spinner, fade the black away,
    /// and remove the overlays entirely. No-op when nothing is showing.
    pub(crate) fn reveal(&self) -> Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        let generation = self.begin();
        let start = self.state.lock().unwrap().opacity;
        for step in 1..=FADE_STEPS {
            if self.stale(generation) {
                return Ok(());
            }
            let opacity = lerp_u8(start, 0, step, FADE_STEPS);
            draw_overlay(&self.mpv, opacity, None)?;
            std::thread::sleep(FADE_STEP);
        }
        self.clear()
    }

    /// Remove both overlays immediately (no fade) and mark the overlay
    /// inactive. Used when a synchronous error means there is nothing worth
    /// fading.
    pub(crate) fn clear(&self) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            state.active = false;
            state.opacity = 0;
        }
        clear_overlay(&self.mpv)
    }
}

/// Draw one frame of the overlay: the black fade rect at `opacity`, plus the
/// spinner at `spinner_deg` (or no spinner when `None`, clearing that layer).
fn draw_overlay(mpv: &Mpv, opacity: u8, spinner_deg: Option<f64>) -> Result<()> {
    // ASS alpha is inverted from opacity: 00 is opaque, FF is transparent.
    let alpha = 255 - opacity;
    // A full-canvas black rectangle, faded by `\1a`. Drawn in the same two
    // separate `osd-overlay` calls as the idle clock (`idle_screen.rs`): one
    // layer for the background, one for the foreground, because libass only
    // honors one `\pos`/`\an` override per event.
    let fade = format!(
        "{{\\an7\\pos(0,0)\\1c&H000000&\\1a&H{alpha:02X}&\\bord0\\shad0\\p1}}\
         m 0 0 l {w} 0 l {w} {h} l 0 {h}{{\\p0}}",
        w = CANVAS_WIDTH,
        h = CANVAS_HEIGHT,
    );
    mpv.command(
        "osd-overlay",
        &[
            &OSD_OVERLAY_FADE_ID.to_string(),
            "ass-events",
            &fade,
            &CANVAS_WIDTH.to_string(),
            &CANVAS_HEIGHT.to_string(),
        ],
    )
    .map_err(|e| anyhow::anyhow!("osd-overlay (fade) failed: {e:?}"))?;

    match spinner_deg {
        Some(deg) => {
            let spinner = spinner_ass(deg);
            mpv.command(
                "osd-overlay",
                &[
                    &OSD_OVERLAY_SPINNER_ID.to_string(),
                    "ass-events",
                    &spinner,
                    &CANVAS_WIDTH.to_string(),
                    &CANVAS_HEIGHT.to_string(),
                ],
            )
            .map_err(|e| anyhow::anyhow!("osd-overlay (spinner) failed: {e:?}"))?;
        }
        None => {
            // Clear the spinner layer only; `format=none` removes the
            // overlay outright rather than replacing it with empty content.
            mpv.command(
                "osd-overlay",
                &[&OSD_OVERLAY_SPINNER_ID.to_string(), "none", ""],
            )
            .map_err(|e| anyhow::anyhow!("osd-overlay (spinner clear) failed: {e:?}"))?;
        }
    }
    Ok(())
}

fn clear_overlay(mpv: &Mpv) -> Result<()> {
    for id in [OSD_OVERLAY_FADE_ID, OSD_OVERLAY_SPINNER_ID] {
        mpv.command("osd-overlay", &[&id.to_string(), "none", ""])
            .map_err(|e| anyhow::anyhow!("osd-overlay (clear) failed: {e:?}"))?;
    }
    Ok(())
}

/// A 270-degree annulus sector ("arc") centered on the canvas, rotated by
/// `angle_deg`. Built as an ASS vector drawing so it needs no particular font
/// to be present on the appliance, unlike a spinner glyph.
///
/// The drawing's coordinates are centered on the origin; `\org` pins rotation
/// to the same point `\pos` places the origin, so `\frz` spins the arc in
/// place without the wobble that centering the arc's (rotating) bounding box
/// would cause.
fn spinner_ass(angle_deg: f64) -> String {
    let cx = CANVAS_WIDTH as f64 / 2.0;
    let cy = CANVAS_HEIGHT as f64 / 2.0;
    let outer_radius = 110.0;
    let inner_radius = 82.0;
    let sweep_degrees = 270.0;
    let arc_steps = 30;

    let mut points: Vec<(f64, f64)> = Vec::with_capacity((arc_steps + 1) * 2);
    for step in 0..=arc_steps {
        let angle = (sweep_degrees * step as f64 / arc_steps as f64).to_radians();
        points.push((outer_radius * angle.cos(), outer_radius * angle.sin()));
    }
    for step in (0..=arc_steps).rev() {
        let angle = (sweep_degrees * step as f64 / arc_steps as f64).to_radians();
        points.push((inner_radius * angle.cos(), inner_radius * angle.sin()));
    }

    let mut path = String::new();
    for (index, (x, y)) in points.iter().enumerate() {
        if index == 0 {
            path.push_str(&format!("m {x:.1} {y:.1}"));
        } else {
            path.push_str(&format!(" l {x:.1} {y:.1}"));
        }
    }

    format!(
        "{{\\an5\\pos({cx:.0},{cy:.0})\\org({cx:.0},{cy:.0})\\frz{angle_deg:.0}\
         \\1c&HFFFFFF&\\1a&H00&\\bord0\\shad0\\p1}}{path}{{\\p0}}"
    )
}

fn lerp_u8(from: u8, to: u8, step: u32, steps: u32) -> u8 {
    let from = from as i32;
    let to = to as i32;
    (from + (to - from) * step as i32 / steps as i32) as u8
}
