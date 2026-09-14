//! Displays web pages with a real browser engine, alongside -- not instead
//! of -- the mpv playback path.
//!
//! # Why a second *client*, not a second display stack
//!
//! The appliance's compositor is Cage, "a Wayland kiosk [that] runs a single,
//! maximized application" (`nix/tv-box.nix`). Today that one application is
//! this daemon, rendering through embedded mpv. mpv cannot render HTML, so
//! web pages need a real engine (Chromium); the question is where it runs.
//!
//! *Embedding* the engine inside the daemon (WebKitGTK/WPE, CEF, Servo) was
//! rejected: there is no maintained Rust embedding for any of them that fits
//! this box, and every one of them would still need a GL context and a
//! composited surface of its own -- i.e. a second rendering pipeline *inside*
//! the daemon, plus a way to hand the screen between two in-process
//! renderers.
//!
//! A *second compositor* (running Chromium nested in e.g. cage/weston, with
//! mpv hidden while the browser is up) was rejected as exactly the "second
//! display stack" this project refuses to bolt on: another compositor to
//! configure, update and keep alive on battery power, to do work Cage
//! already does.
//!
//! What is left is the mechanism every other Wayland client uses: Chromium
//! runs as a second fullscreen *toplevel in the same Cage session*. Cage
//! stacks views the way the compositor spec says to -- `view_map` appends
//! the new surface's scene node (`cage/view.c`), so the newest-mapped client
//! is on top, and `view_unmap` destroys it, revealing the view underneath.
//! So:
//!
//! * casting a page spawns Chromium over mpv's (idle) window;
//! * stopping it, superseding it, or Chromium exiting on its own removes
//!   that view, and mpv's window -- which keeps showing the idle screen
//!   (`idle_screen.rs`) throughout -- is visible again.
//!
//! No compositor switching, no second display stack, and the exact same
//! engine then serves later work (authenticated Jellyfin/Netflix/Grafana
//! pages), which a screenshot pipeline never could. This is also why the
//! daemon never re-fetches or re-renders the page on a timer: the engine
//! keeps the page live, and any refresh cadence is the page's own
//! (e.g. Grafana's auto-refresh) -- see README's design principles.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{error, info};

/// Time the browser is given to exit on SIGTERM before it is killed
/// outright, and then the time SIGKILL is given to take effect. Bounded so
/// `Stop` (or a superseding `Play`) can never hang on a wedged browser.
const TERM_GRACE: Duration = Duration::from_secs(3);
const KILL_GRACE: Duration = Duration::from_secs(2);

/// URL schemes a `Play` may point the browser at. `http(s)` covers
/// dashboards and web apps; `file` covers a locally served/offline page.
/// Anything else is refused before a process is started, which also keeps an
/// arbitrary string from ever reaching Chromium's argv.
const ALLOWED_SCHEMES: [&str; 3] = ["http://", "https://", "file://"];

/// Root the browser engine's directories are created under, unless
/// `CASTOFF_BROWSER_PROFILE_DIR` overrides it. Deliberately a *fixed* short
/// absolute path rather than `std::env::temp_dir()`: `temp_dir()` honours
/// `TMPDIR`, and a deep ambient `TMPDIR` is what broke the engine on real
/// hardware (see `Browser::temp_dir` and `SINGLETON_PATH_RESERVE`).
const BROWSER_ROOT: &str = "/tmp";

/// What Chromium appends to its temp directory for its process singleton:
/// `/org.chromium.Chromium.<6 random chars>/SingletonSocket` (plus the
/// separator). Reserved up front rather than measured per launch, because the
/// random component only exists once Chromium is running -- and this has to
/// be checked before it runs.
const SINGLETON_PATH_RESERVE: usize = 64;

/// The longest singleton socket path Linux accepts: `sun_path` is 108 bytes
/// *including* the terminating NUL.
const MAX_SOCKET_PATH: usize = 107;

/// Distinguishes browser directories when more than one `WebpageController`
/// exists in one process (e.g. the test suite, which builds a `Player` per
/// test).
static PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// How to launch the browser engine.
pub(crate) struct Browser {
    program: String,
    /// Chromium's on-disk state: `<root>/castoff-browser-<pid>-<n>`, one per
    /// `WebpageController` and shared by every engine that controller starts
    /// (a persistent profile is what later authenticated pages need -- see
    /// `WebpageController::show`).
    ///
    /// Under a *fixed* short root, never `std::env::temp_dir()`: `/tmp` is
    /// short and is tmpfs on the appliance, so the profile stays non-durable
    /// (no disk writes) by design, which is what a dashboard needs, and its
    /// length does not depend on the caller's `TMPDIR`.
    /// `CASTOFF_BROWSER_PROFILE_DIR` can move it, and `Browser::spawn`
    /// refuses a root without room to sit before Chromium's socket limit
    /// rather than letting the engine die cryptically.
    profile_dir: PathBuf,
    /// The engine's `TMPDIR`: `<root>/castoff-browser-tmp-<pid>-<n>`.
    ///
    /// Chromium's process singleton is a unix socket it creates at
    /// `<TMPDIR>/org.chromium.Chromium.<random>/SingletonSocket` -- under
    /// *TMPDIR*, not under `--user-data-dir`. Verified against the pinned
    /// Chromium: a deep `TMPDIR` aborts with `FATAL ... Socket path too
    /// long` even with a short profile, while a short `TMPDIR` with a deep
    /// profile works. Inheriting the daemon's `TMPDIR` therefore killed the
    /// engine on the captain's box, whose generated nix-shell `TMPDIR` is
    /// ~120 characters; the engine gets this fixed short one instead, and
    /// `Browser::spawn` creates it before launching.
    temp_dir: PathBuf,
}

impl Browser {
    pub(crate) fn from_env() -> Self {
        // `chromium` is put on `PATH` by the packaged daemon's wrapper (see
        // flake.nix) the same way `yt-dlp` is; `CASTOFF_BROWSER` exists so a
        // host can point at another Chromium build (or a wrapper around it)
        // without rebuilding, and is used by the test suite.
        let program = std::env::var("CASTOFF_BROWSER").unwrap_or_else(|_| "chromium".to_string());
        let root = std::env::var("CASTOFF_BROWSER_PROFILE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(BROWSER_ROOT));
        let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        Self {
            program,
            profile_dir: root.join(format!("castoff-browser-{pid}-{seq}")),
            temp_dir: root.join(format!("castoff-browser-tmp-{pid}-{seq}")),
        }
    }

    /// Refuse to launch Chromium when either directory it will use leaves no
    /// room for the process-singleton socket Chromium puts under its
    /// `TMPDIR`.
    ///
    /// Chromium's own way of reporting this is a `FATAL` from deep inside
    /// `process_singleton_posix.cc` ("Socket path too long") followed by an
    /// immediate exit, which on a TV box looks like "nothing happened". This
    /// says it up front, with the path and the numbers, and the caller turns
    /// it into an error the daemon's console and the sender both see.
    fn check_paths_fit(&self) -> Result<()> {
        for (what, path) in [("temp", &self.temp_dir), ("profile", &self.profile_dir)] {
            let rendered = path.display();
            let longest = rendered.to_string().len() + SINGLETON_PATH_RESERVE;
            if longest > MAX_SOCKET_PATH {
                error!(
                    directory = %rendered,
                    which = what,
                    path_len_with_singleton = longest,
                    limit = MAX_SOCKET_PATH,
                    "refusing to start the browser engine: this path leaves no room for \
                     Chromium's process-singleton unix socket, which would abort the engine with \
                     `Socket path too long` before it painted anything; the browser root must be \
                     a short path"
                );
                anyhow::bail!(
                    "browser {what} path is too long for Chromium's process-singleton socket: \
                     {rendered} (+{SINGLETON_PATH_RESERVE} bytes of Chromium socket path = \
                     {longest}, limit {MAX_SOCKET_PATH}); set CASTOFF_BROWSER_PROFILE_DIR to a \
                     shorter root"
                );
            }
        }
        Ok(())
    }

    /// Chromium argv for `url` (program excluded). Pure so tests can assert
    /// the exact flags the appliance launches with.
    fn args(&self, url: &str) -> Vec<String> {
        vec![
            // The appliance session is pure Wayland (Cage); Chromium's
            // default ozone platform is X11, so this is explicit rather than
            // inferred from the environment.
            "--ozone-platform=wayland".to_string(),
            // No browser chrome (tabs, omnibox) and no window affordances:
            // this is a dashboard on a TV, not a browsable browser. Cage
            // maximizes every primary toplevel anyway; `--kiosk` also
            // requests fullscreen explicitly. The URL travels inside the
            // `--app=` argument so it can never be parsed as a flag.
            "--kiosk".to_string(),
            format!("--app={url}"),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            // Dashboards author colors in sRGB; don't let a missing display
            // profile shift them. Also makes rendered pixels deterministic
            // (the end-to-end test asserts on them).
            "--force-color-profile=srgb".to_string(),
            format!("--user-data-dir={}", self.profile_dir.display()),
        ]
    }

    /// Remove Chromium's process-singleton files from the profile directory.
    ///
    /// Every browser in this controller shares one `--user-data-dir`, and
    /// Chromium admits only one browser per profile via these files. A
    /// browser taken down by signal can leave them behind; Chromium can
    /// detect a lock whose pid is dead, but removing them after the previous
    /// engine has been reaped makes the restart deterministic instead of
    /// relying on that detection.
    fn clear_singleton_artifacts(&self) {
        for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let path = self.profile_dir.join(name);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => error!(
                    path = %path.display(),
                    error = %e,
                    "failed to clear a stale browser profile singleton file; the next \
                     engine start will fall back to Chromium's own stale-lock detection"
                ),
            }
        }
    }

    /// Start Chromium on `url`. Returns as soon as the process is spawned;
    /// loading and painting the page happen inside the engine afterwards.
    fn spawn(&self, url: &str) -> Result<Child> {
        self.check_paths_fit()?;
        // Chromium needs its `TMPDIR` to exist before it starts (it creates
        // its process-singleton socket under it), and it is one shared
        // directory per controller, like the profile.
        std::fs::create_dir_all(&self.temp_dir).with_context(|| {
            format!(
                "failed to create the browser engine's temp dir {:?}",
                self.temp_dir
            )
        })?;
        let mut command = Command::new(&self.program);
        command
            .args(self.args(url))
            // The engine's own temp dir, so Chromium's process-singleton
            // socket path cannot inherit a deep ambient `TMPDIR` -- see the
            // field's doc comment.
            .env("TMPDIR", &self.temp_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Chromium's own diagnostics -- a page that failed to load, a
            // crashed renderer -- go to the daemon's stderr/journal, the
            // same way mpv's do (`terminal=yes` in player.rs), instead of
            // being captured and discarded here.
            .stderr(Stdio::inherit());
        // SAFETY: `setsid` is a thin, async-signal-safe syscall wrapper and
        // nothing else runs between `fork` and `exec` here, so the child
        // cannot inherit a locked allocator or other inconsistent state.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command.spawn().with_context(|| {
            format!(
                "failed to start browser engine {:?} for {url}",
                self.program
            )
        })
    }
}

/// The page currently on screen, plus everything needed to take it down.
struct ActivePage {
    pid: i32,
    /// Signals (once) that the browser process has been reaped.
    exited: Receiver<()>,
}

#[derive(Default)]
struct State {
    current: Option<ActivePage>,
    /// Bumped whenever `current` is replaced or cleared. A reaper thread
    /// whose generation no longer matches was superseded deliberately and
    /// must not touch state or report an unexpected exit.
    generation: u64,
}

/// Owns the browser engine that renders web pages: starting it over mpv's
/// window, and taking it down again (including its process group's children)
/// on `Stop`, on a superseding `Play`, or when it exits by itself.
pub(crate) struct WebpageController {
    browser: Browser,
    state: Arc<Mutex<State>>,
}

impl WebpageController {
    pub(crate) fn from_env() -> Self {
        Self::with_browser(Browser::from_env())
    }

    #[cfg(test)]
    pub(crate) fn with_program(program: impl Into<String>) -> Self {
        let mut browser = Browser::from_env();
        browser.program = program.into();
        Self::with_browser(browser)
    }

    fn with_browser(browser: Browser) -> Self {
        Self {
            browser,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Whether a web page is currently displayed (the engine is running on
    /// top of mpv's window).
    pub(crate) fn is_active(&self) -> bool {
        self.state.lock().unwrap().current.is_some()
    }

    /// Display `url`, replacing whatever page (if any) is on screen.
    ///
    /// The incumbent engine is taken down and *reaped* before the
    /// replacement is started. Every browser here shares one
    /// `--user-data-dir` (deliberately: a persistent profile is what later
    /// authenticated pages need), and Chromium enforces a single browser per
    /// profile, so starting the replacement first would make it abort on the
    /// incumbent's lock -- or hand its URL to the incumbent and exit --
    /// leaving the sender's page undisplayed. The brief idle gap between the
    /// two engines is the accepted cost of that ordering.
    pub(crate) fn show(&self, url: &str) -> Result<()> {
        validate_url(url)?;

        // Take the incumbent out of `current` and wait for its process to be
        // gone before the replacement can touch the shared profile.
        let previous = {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            state.current.take()
        };
        if let Some(previous) = previous {
            self.terminate(previous);
        }
        self.browser.clear_singleton_artifacts();

        let mut child = self.browser.spawn(url)?;
        let pid = child.id() as i32;
        let (exited_tx, exited_rx) = mpsc::channel();

        let generation = {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            state.current = Some(ActivePage {
                pid,
                exited: exited_rx,
            });
            state.generation
        };

        info!(url, pid, "browser engine started for web page");

        // `wait` needs ownership of the `Child`, so reaping happens on
        // its own thread (which blocks on the process, not on a timer);
        // `terminate` only ever needs the pid.
        let state = Arc::clone(&self.state);
        let url = url.to_string();
        std::thread::spawn(move || {
            let status = child.wait();
            let superseded = {
                let mut state = state.lock().unwrap();
                if state.generation == generation {
                    state.current = None;
                    false
                } else {
                    true
                }
            };
            let _ = exited_tx.send(());
            if superseded {
                return;
            }
            // The page was on screen and nothing asked it to go away:
            // Cage has already revealed mpv's idle screen again, so this
            // is an error worth in the log, not a silent state change.
            match status {
                Ok(status) => error!(
                    url,
                    ?status,
                    "browser engine exited on its own; the screen is back to the \
                     daemon's idle screen"
                ),
                Err(e) => error!(url, error = %e, "failed to wait for the browser engine"),
            }
        });

        Ok(())
    }

    /// Stop displaying the page, waiting (bounded) for the engine to be gone
    /// so that by the time this returns the screen is the daemon's again.
    pub(crate) fn hide(&self) {
        let active = {
            let mut state = self.state.lock().unwrap();
            // Deliberate teardown: the reaper for this pid must stay quiet.
            state.generation += 1;
            state.current.take()
        };
        if let Some(active) = active {
            self.terminate(active);
        }
    }

    /// Terminate a browser process group and block until its reaper thread
    /// confirms it has exited, escalating to SIGKILL if it doesn't.
    fn terminate(&self, active: ActivePage) {
        // Negative pid targets the whole process group: Chromium's zygote
        // and renderer children live in the session `Browser::spawn`
        // created, so none of them survive as orphans painting off-screen.
        // SAFETY: `kill` is a syscall wrapper; a stale pid can at worst
        // fail with ESRCH.
        unsafe { libc::kill(-active.pid, libc::SIGTERM) };
        if active.exited.recv_timeout(TERM_GRACE) == Err(RecvTimeoutError::Timeout) {
            unsafe { libc::kill(-active.pid, libc::SIGKILL) };
            // Disconnected means the reaper already finished; either way the
            // process group is gone or has been killed.
            let _ = active.exited.recv_timeout(KILL_GRACE);
        }
    }
}

fn validate_url(url: &str) -> Result<()> {
    if ALLOWED_SCHEMES.iter().any(|scheme| url.starts_with(scheme)) {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to display {url:?}: a web page must be an http://, https:// or file:// URL"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_args_render_the_url_as_a_fullscreen_app() {
        let browser = Browser {
            program: "chromium".to_string(),
            profile_dir: PathBuf::from("/tmp/castoff-browser-test"),
            temp_dir: PathBuf::from("/tmp/castoff-browser-tmp-test"),
        };
        let args = browser.args("http://127.0.0.1:8000/dashboard");

        assert!(args.iter().any(|a| a == "--ozone-platform=wayland"));
        assert!(args.iter().any(|a| a == "--kiosk"));
        assert!(args
            .iter()
            .any(|a| a == "--app=http://127.0.0.1:8000/dashboard"));
        assert!(args
            .iter()
            .any(|a| a == "--user-data-dir=/tmp/castoff-browser-test"));
        // The URL must never be its own argv element, where Chromium could
        // read it as a flag.
        assert!(!args.iter().any(|a| a == "http://127.0.0.1:8000/dashboard"));
    }

    /// Regression test for the failure seen on real hardware: a deep ambient
    /// `TMPDIR` (there: a ~120-character nix-shell path) pushed Chromium's
    /// process-singleton socket past the kernel's unix-socket limit and the
    /// engine aborted with `FATAL ... Socket path too long` before painting
    /// anything. Both directories the daemon hands Chromium must therefore
    /// come from the fixed short root, not from whatever `TMPDIR` says.
    #[test]
    fn browser_directories_are_short_and_tmpdir_independent() {
        let browser = Browser::from_env();
        for dir in [&browser.profile_dir, &browser.temp_dir] {
            assert!(
                dir.starts_with(BROWSER_ROOT),
                "{dir:?} must live under the fixed short root {BROWSER_ROOT:?}"
            );
            let longest = dir.display().to_string().len() + SINGLETON_PATH_RESERVE;
            assert!(
                longest <= MAX_SOCKET_PATH,
                "{dir:?} leaves only {} of {MAX_SOCKET_PATH} bytes for Chromium's singleton \
                 socket",
                MAX_SOCKET_PATH - longest,
            );
        }
        browser
            .check_paths_fit()
            .expect("the default directories must fit");
    }

    /// And when one of them *cannot* fit -- a caller-supplied root that is too
    /// deep -- the daemon must say so before Chromium does, rather than
    /// letting the engine die with an internal message: no process is started,
    /// and the error names the path and the limit. Both the deep-`TMPDIR`
    /// shape (the real failure) and a deep profile are covered.
    #[test]
    fn a_path_without_room_for_the_singleton_socket_is_refused_legibly() {
        let too_deep = format!("/tmp/{}", "nested/".repeat(40));
        let deep_temp = Browser {
            program: "chromium".to_string(),
            profile_dir: PathBuf::from("/tmp/castoff-browser-test"),
            temp_dir: PathBuf::from(&too_deep),
        };
        let deep_profile = Browser {
            program: "chromium".to_string(),
            profile_dir: PathBuf::from(&too_deep),
            temp_dir: PathBuf::from("/tmp/castoff-browser-tmp-test"),
        };

        for browser in [&deep_temp, &deep_profile] {
            let err = browser
                .check_paths_fit()
                .expect_err("a directory this deep must be refused");
            let message = err.to_string();
            assert!(
                message.contains(&too_deep) && message.contains(&MAX_SOCKET_PATH.to_string()),
                "the refusal must name the path and the limit: {message}"
            );
            // `spawn` is what reaches Chromium, so it must refuse before that.
            let err = browser
                .spawn("http://127.0.0.1:8000/dashboard")
                .expect_err("spawn must refuse instead of launching Chromium");
            assert!(
                err.to_string()
                    .contains("too long for Chromium's process-singleton socket"),
                "unexpected spawn error: {err}"
            );
        }
    }

    #[test]
    fn only_http_https_and_file_urls_are_allowed() {
        for url in [
            "http://grafana.lan:3000/d/boat",
            "https://example.invalid/dashboard",
            "file:///srv/dashboard.html",
        ] {
            validate_url(url).unwrap_or_else(|e| panic!("{url} should be allowed: {e}"));
        }
        for url in [
            "",
            "ftp://example.invalid/x",
            "--kiosk",
            "javascript:alert(1)",
        ] {
            assert!(validate_url(url).is_err(), "{url:?} must be refused");
        }
    }
}
