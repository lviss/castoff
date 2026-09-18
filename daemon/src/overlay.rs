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
//!
//! The observable sequence, confirmed on a real mpv/X capture:
//!
//! * A Play first fades the old content (idle clock or previous video) to
//!   black (`conceal`, ~400ms), then shows the rotating spinner on the black.
//!   When mpv reports playback has started, the spinner is removed and the
//!   black fades away (`reveal`, ~400ms) to the new video. So the fade covers
//!   both boundaries on a Play -- idle/old content to black, and black to new
//!   video -- with the spinner sitting between them; it never masks either
//!   fade because it is drawn only after `conceal` finishes and is cleared
//!   before `reveal`'s first frame.
//! * A Stop fades the video to black (`conceal`, ~400ms), then fades the idle
//!   clock in over that same black (`Player::fade_in_idle_clock`, ~400ms). The
//!   clock is itself an OSD overlay, and mpv stacks overlays by recency rather
//!   than by id, so a cover rect cannot be faded away to reveal a clock that
//!   was re-created on top of it -- instead the clock's own `\1a` alpha is
//!   ramped up while the opaque rect stays behind it as a black backdrop (so
//!   a not-yet-cleared video frame can't flash through), then the rect is
//!   dropped once the clock's own opaque background covers the canvas.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use libmpv2::Mpv;

/// Virtual ASS canvas both overlays are drawn on; mpv scales this to the real
/// output resolution, so every coordinate below is resolution independent.
/// Matches the idle clock's canvas (`idle_screen.rs`) so the two layers line
/// up, but is declared locally to keep this module self-contained.
const CANVAS_WIDTH: i64 = 1920;
const CANVAS_HEIGHT: i64 = 1080;

/// `osd-overlay` ids. Distinct from the idle clock's 9000/9001. Note that
/// mpv does **not** order overlays by id: whichever overlay was most recently
/// added/updated is drawn on top. That is fine for covering *video* (an
/// overlay is always above video), but it means a cover rect cannot be used
/// to reveal the idle clock, which is itself an overlay: the clock would be
/// re-added above the rect. `Player` fades the clock in via its own alpha
/// instead (`fade_in` + `IdleScreenController::render_at`).
const OSD_OVERLAY_FADE_ID: i64 = 9100;
const OSD_OVERLAY_SPINNER_ID: i64 = 9101;

/// Fade-through-black: 20 frames at 20ms each, so one direction is ~400ms --
/// long enough to read as a fade on a TV rather than a cut, with fine enough
/// steps that the opacity ramp is smooth instead of a few visible jumps. Still
/// a bounded transition that stops redrawing once settled.
const FADE_STEPS: u32 = 20;
const FADE_STEP: Duration = Duration::from_millis(20);

/// Spinner cadence: 12 degrees every 33ms = ~30 redraws/s and one revolution
/// per ~1s, smooth enough to read as continuous rotation while staying a
/// bounded, load-only animation (it draws nothing once the load ends).
const SPINNER_STEP: Duration = Duration::from_millis(33);
const SPINNER_DEGREES_PER_STEP: f64 = 12.0;

/// Environment variable that scales *both* animations for manual inspection or
/// testing, e.g. `CASTOFF_ANIMATION_SLOWDOWN=10` makes the whole sequence ten
/// times slower. Read once at startup. Deliberately an environment variable
/// rather than a control-protocol opcode: it is an operator/testing
/// affordance, not a user setting, so it stays out of the FCast surface.
/// Unset, empty, malformed, non-finite, or below `1` all fall back to `1.0`
/// (exactly the shipping behavior); values are clamped to
/// `MAX_ANIMATION_SLOWDOWN` so a typo can't produce an absurd sleep. See
/// `parse_slowdown`.
pub(crate) const ANIMATION_SLOWDOWN_ENV: &str = "CASTOFF_ANIMATION_SLOWDOWN";
const MAX_ANIMATION_SLOWDOWN: f64 = 1000.0;

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
    /// True once mpv has confirmed the in-flight load genuinely started
    /// rendering (`PlaybackRestart`), even while `active` stays true for the
    /// rest of `reveal`'s ~400ms cosmetic fade-out. Distinct from `active` on
    /// purpose -- see `PlaybackOverlay::is_restarted`.
    restarted: bool,
}

/// Owns the loading/fade overlays for one `Player`.
pub(crate) struct PlaybackOverlay {
    mpv: Arc<Mpv>,
    state: Arc<Mutex<State>>,
    /// Multiplier on every animation duration, read once from the environment
    /// at startup (see `parse_slowdown`). `1.0` is the shipping behavior.
    slowdown: f64,
}

impl PlaybackOverlay {
    pub(crate) fn new(mpv: Arc<Mpv>) -> Self {
        Self {
            mpv,
            state: Arc::new(Mutex::new(State::default())),
            slowdown: animation_slowdown_from_env(),
        }
    }

    /// Whether the loading overlay is currently up (either mid-fade or
    /// spinning). Used by `Player` to decide whether a failed load needs to
    /// return the screen to the idle clock, and by tests.
    pub(crate) fn is_active(&self) -> bool {
        self.state.lock().unwrap().active
    }

    /// Whether the in-flight load has genuinely started rendering
    /// (`PlaybackRestart`) -- unlike `is_active`, this flips true the instant
    /// that event is confirmed, *not* after `reveal`'s ~400ms fade-out also
    /// finishes. `Player::is_idle` uses this (not `is_active`) to decide
    /// whether a `Play` should supersede an in-flight load or only queue
    /// behind a load that has already succeeded -- otherwise a `Play`
    /// arriving during the cosmetic fade-out would still wrongly interrupt
    /// already-genuine playback instead of queueing behind it.
    pub(crate) fn is_restarted(&self) -> bool {
        self.state.lock().unwrap().restarted
    }

    /// Record that the in-flight load has genuinely started rendering. Called
    /// by `handle_lifecycle_event`'s `PlaybackRestart` arm, before `reveal`'s
    /// fade-out begins.
    pub(crate) fn mark_restarted(&self) {
        self.state.lock().unwrap().restarted = true;
    }

    /// Start an animation sequence: mark the overlay active and invalidate
    /// any animation already running, returning this sequence's generation.
    fn begin(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        state.active = true;
        state.generation
    }

    /// Draw one overlay frame, but only while `generation` is still current,
    /// holding the state lock across the draw. `clear_if_current` holds that
    /// same lock across its OSD teardown, so a draw that passed the staleness
    /// check can never land after the overlays have been removed. Returns
    /// `false` when the sequence has been superseded.
    fn draw_if_current(
        &self,
        generation: u64,
        opacity: u8,
        spinner_deg: Option<f64>,
    ) -> Result<bool> {
        let state = self.state.lock().unwrap();
        if state.generation != generation {
            return Ok(false);
        }
        draw_overlay(&self.mpv, opacity, spinner_deg)?;
        Ok(true)
    }

    /// Fade whatever is on screen to opaque black. Used at the start of a
    /// Play (so the old content or idle clock disappears behind the spinner)
    /// and at the start of a Stop (so the old video disappears before the
    /// idle clock takes its place). Blocking but bounded (~400ms).
    pub(crate) fn conceal(&self) -> Result<()> {
        let generation = self.begin();
        let start = self.state.lock().unwrap().opacity;
        for step in 1..=FADE_STEPS {
            let opacity = lerp_u8(start, 255, step, FADE_STEPS);
            match self.draw_if_current(generation, opacity, None) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) => {
                    let _ = self.clear();
                    return Err(e);
                }
            }
            std::thread::sleep(scale_duration(FADE_STEP, self.slowdown));
        }
        let mut state = self.state.lock().unwrap();
        if state.generation == generation {
            state.opacity = 255;
        }
        Ok(())
    }

    /// Draw the spinner over the (now black) screen and keep it rotating
    /// until the sequence is superseded, `finish_loading` runs, or `clear`
    /// is called. Returns as soon as the first frame is drawn; the redraw
    /// loop runs on its own thread and idles otherwise.
    pub(crate) fn spawn_spinner(&self) -> Result<()> {
        let generation = self.begin();
        // A fresh load has not restarted playback yet; clear any stale
        // `true` left by a previous, already-finished load (see
        // `is_restarted`'s doc comment) so a brand-new spinner is correctly
        // superseded by a rapid re-Play instead of only queueing behind it.
        self.state.lock().unwrap().restarted = false;
        match self.draw_if_current(generation, 255, Some(0.0)) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => {
                let _ = self.clear();
                return Err(e);
            }
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.generation == generation {
                state.opacity = 255;
            }
        }

        let mpv = Arc::clone(&self.mpv);
        let state = Arc::clone(&self.state);
        let step = scale_duration(SPINNER_STEP, self.slowdown);
        std::thread::spawn(move || {
            let mut angle = 0.0;
            // Schedule against an absolute deadline rather than sleeping a
            // fixed `step` each turn: `thread::sleep` overshoots by scheduler
            // granularity, so a naive loop runs measurably slower than 30
            // redraws/s. Charging one `step` per tick keeps the *average*
            // rate at the intended ~30/s (and exactly `1/slowdown` of that
            // for a slowed run) even when individual sleeps are late.
            let mut next = Instant::now() + step;
            loop {
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
                next += step;
                // Hold the lock across the (single) spinner draw so it can
                // never land after `clear_if_current` has torn the overlays
                // down; only the spinner layer is re-issued here -- the
                // opaque fade rect it sits on is unchanged, so there is no
                // reason to redraw it every frame.
                let state = state.lock().unwrap();
                if state.generation != generation {
                    return;
                }
                angle = (angle + SPINNER_DEGREES_PER_STEP) % 360.0;
                let _ = draw_spinner(&mpv, angle);
            }
        });
        Ok(())
    }

    /// Playback has genuinely started rendering (`PlaybackRestart`): stop
    /// the spinner, fade the black away, and remove the overlays entirely.
    /// No-op when nothing is showing. A load that fails or never starts is
    /// handed back to the idle clock instead, through `restore_idle_clock` /
    /// `Player::fade_in_idle_clock`, which fades the clock's own alpha in
    /// rather than revealing video.
    pub(crate) fn reveal(&self) -> Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        let generation = self.begin();
        let start = self.state.lock().unwrap().opacity;
        for step in 1..=FADE_STEPS {
            let opacity = lerp_u8(start, 0, step, FADE_STEPS);
            match self.draw_if_current(generation, opacity, None) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) => {
                    let _ = self.clear();
                    return Err(e);
                }
            }
            std::thread::sleep(scale_duration(FADE_STEP, self.slowdown));
        }
        self.clear_if_current(Some(generation))
    }

    /// Fade `render` in from transparent to opaque over one fade duration,
    /// calling it once per opacity step. Used to bring the idle clock back on
    /// a Stop or a failed load: the clock is itself an OSD overlay, so it
    /// cannot be revealed by removing a cover rect that was stacked above it
    /// (`mpv` stacks overlays by recency); fading its own alpha in is the
    /// crossfade. Bounded like every other animation here: a fixed number of
    /// steps, then it returns and stops drawing.
    pub(crate) fn fade_in(&self, mut render: impl FnMut(u8) -> Result<()>) -> Result<()> {
        for step in 1..=FADE_STEPS {
            render(lerp_u8(0, 255, step, FADE_STEPS))?;
            std::thread::sleep(scale_duration(FADE_STEP, self.slowdown));
        }
        Ok(())
    }

    /// Stop the spinner (cancelling its redraw thread) and remove just the
    /// spinner layer, leaving the opaque fade rect in place. Used while
    /// fading the idle clock in: the rect is the black backdrop that keeps a
    /// not-yet-cleared video frame from flashing through, while the spinner
    /// must not keep turning underneath the clock.
    pub(crate) fn stop_spinner(&self) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
        }
        clear_spinner(&self.mpv)
    }

    /// Remove both overlays immediately (no fade) and mark the overlay
    /// inactive. Used when a synchronous error means there is nothing worth
    /// fading.
    pub(crate) fn clear(&self) -> Result<()> {
        self.clear_if_current(None)
    }

    /// Teardown shared by `clear` and `reveal`: when `expected` is `Some`,
    /// only act if that generation is still current, so a fading `reveal`
    /// can't tear down a newer command's overlay. Holds the state lock across
    /// `clear_overlay` so a concurrent draw (which holds the same lock) can
    /// never land after the OSD overlays have been removed.
    fn clear_if_current(&self, expected: Option<u64>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(generation) = expected {
            if state.generation != generation {
                return Ok(());
            }
        }
        state.generation += 1;
        state.active = false;
        state.opacity = 0;
        clear_overlay(&self.mpv)
    }
}

/// Draw one frame of the overlay: the black fade rect at `opacity`, plus the
/// spinner at `spinner_deg` (or no spinner when `None`, clearing that layer).
fn draw_overlay(mpv: &Mpv, opacity: u8, spinner_deg: Option<f64>) -> Result<()> {
    draw_fade_rect(mpv, opacity)?;
    match spinner_deg {
        Some(deg) => draw_spinner(mpv, deg),
        None => clear_spinner(mpv),
    }
}

/// Draw the full-canvas black rectangle at `opacity` (0 = transparent, 255 =
/// opaque) into the fade overlay id. Drawn as an ASS vector rect, faded with
/// `\1a` (inverted alpha).
fn draw_fade_rect(mpv: &Mpv, opacity: u8) -> Result<()> {
    // ASS alpha is inverted from opacity: 00 is opaque, FF is transparent.
    let alpha = 255 - opacity;
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
    Ok(())
}

/// Draw the rotating spinner into the spinner overlay id. One `osd-overlay`
/// command per frame, on top of the already-opaque fade rect.
fn draw_spinner(mpv: &Mpv, deg: f64) -> Result<()> {
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
    Ok(())
}

/// Remove the spinner layer only; `format=none` removes the overlay outright
/// rather than replacing it with empty content.
fn clear_spinner(mpv: &Mpv) -> Result<()> {
    mpv.command(
        "osd-overlay",
        &[&OSD_OVERLAY_SPINNER_ID.to_string(), "none", ""],
    )
    .map_err(|e| anyhow::anyhow!("osd-overlay (spinner clear) failed: {e:?}"))?;
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
/// The drawing's coordinates are centered on the origin, and `\an7` (top-left)
/// makes libass map that origin directly to `\pos(cx,cy)`, so the arc's own
/// center sits on the canvas center. `\org(cx,cy)` then pins the `\frz`
/// rotation pivot to that same screen point, spinning the arc in place. Do NOT
/// switch this to `\an5` (center): for a vector drawing whose ink is not
/// symmetric about its coordinate origin -- this 270-degree arc is missing a
/// quadrant -- libass does not place the coordinate origin at `\pos`, so the
/// arc and its rotation pivot are both displaced and `\frz` orbits the arc
/// around the canvas instead of spinning it. Verified against a libass render
/// (see the commit that fixed the orbit).
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
        "{{\\an7\\pos({cx:.0},{cy:.0})\\org({cx:.0},{cy:.0})\\frz{angle_deg:.0}\
         \\1c&HFFFFFF&\\1a&H00&\\bord0\\shad0\\p1}}{path}{{\\p0}}"
    )
}

fn lerp_u8(from: u8, to: u8, step: u32, steps: u32) -> u8 {
    let from = from as i32;
    let to = to as i32;
    (from + (to - from) * step as i32 / steps as i32) as u8
}

/// Scale a base animation duration by the runtime slowdown multiplier. Only
/// ever called with a `slowdown` already normalized to `>= 1` and `<= MAX`
/// by `parse_slowdown`, so the result can't be zero, negative, or absurd.
fn scale_duration(base: Duration, slowdown: f64) -> Duration {
    Duration::from_secs_f64(base.as_secs_f64() * slowdown)
}

fn animation_slowdown_from_env() -> f64 {
    parse_slowdown(std::env::var(ANIMATION_SLOWDOWN_ENV).ok().as_deref())
}

/// Parse `CASTOFF_ANIMATION_SLOWDOWN`. Anything unusable -- unset, empty,
/// unparseable, non-finite, or below `1` (which would *speed up* the
/// animation, not slow it) -- yields the shipping default of `1.0`; a valid
/// multiplier above 1 is clamped to `MAX_ANIMATION_SLOWDOWN`.
fn parse_slowdown(raw: Option<&str>) -> f64 {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 1.0)
        .map(|value| value.min(MAX_ANIMATION_SLOWDOWN))
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::{parse_slowdown, MAX_ANIMATION_SLOWDOWN};

    /// The shipping experience must be untouched: every unusable value (and
    /// in particular *unset*) means exactly 1x, with no panic.
    #[test]
    fn slowdown_defaults_to_one_for_unset_empty_or_unusable_values() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("abc"),
            Some("0"),
            Some("-10"),
            Some("NaN"),
            Some("inf"),
            Some("-inf"),
            Some("0.5"),
        ] {
            assert_eq!(parse_slowdown(raw), 1.0, "raw={raw:?} must fall back to 1x");
        }
    }

    #[test]
    fn slowdown_accepts_multipliers_at_or_above_one_and_clamps_absurd_ones() {
        assert_eq!(parse_slowdown(Some("1")), 1.0);
        assert_eq!(parse_slowdown(Some("10")), 10.0);
        assert_eq!(parse_slowdown(Some(" 2.5 ")), 2.5);
        assert_eq!(parse_slowdown(Some("99999")), MAX_ANIMATION_SLOWDOWN);
    }
}
