//! End-to-end test: does casting a web page actually put *that page's* pixels
//! on the screen?
//!
//! This is deliberately not a mock: it starts a real headless
//! [Cage](https://github.com/cage-kiosk/cage) session (the same compositor
//! the appliance runs, see `nix/tv-box.nix`) with the real `castoff-daemon`
//! binary as its client, casts pages and media with real FCast frames over
//! TCP, lets the *real* Chromium engine the daemon spawns render what it
//! routes to the browser, and then asks the compositor for its composited
//! output over `wlr-screencopy` (`grim`). The assertions are on those pixels --
//! the cast page's colour fills the screen, and it is gone again once a second
//! page supersedes it, once media supersedes it, or a `Stop` returns the
//! screen to the daemon's idle clock -- plus the `PlaybackUpdate` replies and
//! the daemon's own console, which is where a URL that cannot be rendered is
//! reported.
//!
//! The cases cover the daemon's own routing decision (a `Play` with no
//! `container`, see README's routing rules): a local page falls back from the
//! media attempt to the browser, a direct media file stays with mpv, a YouTube
//! URL plays as video rather than being mistaken for a web page, an ordinary
//! web page displays in the browser, and a URL that renders neither way says so
//! on the console. The cases that need the public internet check
//! `network_available()` and report that they are being skipped when it is
//! absent -- the Nix build sandbox has no network, so `nix flake check` covers
//! the local cases only; `nix develop` runs all of them.
//!
//! Cage is run on wlroots' *headless* backend, so no GPU, display or X server
//! is needed; the session, the engine, and the screenshot are all real.
//!
//! Requires `cage`, `chromium` and `grim` on `PATH` (the dev shell provides
//! them: `flake.nix`'s `devShells.default`), which is why it is `#[ignore]`d
//! by default -- `cargo test` in a plain sandbox has none of them. Run it
//! with:
//!
//! ```sh
//! nix develop -c cargo test -p castoff-daemon --test webpage_display -- --ignored --nocapture
//! ```
//!
//! Set `CASTOFF_E2E_BROWSER_FLAGS` to run the engine behind a wrapper adding
//! environment-specific flags (the Nix build's check phase uses this, since
//! its sandbox has no user namespaces for Chromium's own sandbox and no GPU
//! device).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The colour the test page paints, and the colour mpv's `lavfi` source
/// paints, chosen to be unlike anything else on screen (and unlike each
/// other).
const PAGE_COLOR: [u8; 3] = [255, 0, 255];
/// The colour of the *second* page cast over the first: superseding a page
/// must put this one's pixels on screen, not leave the first up or fall back
/// to the idle screen.
const SECOND_PAGE_COLOR: [u8; 3] = [255, 165, 0];
const MEDIA_COLOR: [u8; 3] = [0, 255, 255];
/// BT.601 limited-range encoding of pure green: the colour of the local
/// video file the media-routing test serves over HTTP. (Pure green is
/// `R = 1.164*(145-16) + 1.596*(34-128) ~= 0`, `G ~= 255`, `B ~= 0`.)
const VIDEO_YUV: (u8, u8, u8) = (145, 54, 34);
const VIDEO_COLOR: [u8; 3] = [0, 255, 0];

/// FCast opcodes/bodies this test speaks (see `daemon/src/fcast.rs`).
const OPCODE_PLAY: u8 = 1;
const OPCODE_RESUME: u8 = 3;
const OPCODE_STOP: u8 = 4;
const OPCODE_PLAYBACK_UPDATE: u8 = 6;
const STATE_IDLE: u64 = 0;
const STATE_PLAYING: u64 = 1;

/// Fraction of the screen a colour must cover to count as "on screen": the
/// page fills the whole output, but one column of the composited frame can
/// belong to a neighbouring surface, so this is not 100%.
const ON_SCREEN_FRACTION: f64 = 0.9;

/// A running appliance-shaped session, torn down on drop.
struct Session {
    /// Cage, whose primary client is the daemon under test. Cage does not
    /// `setsid` its client, and this test starts Cage in its own session, so
    /// the daemon stays in Cage's process group.
    cage: Child,
    runtime_dir: PathBuf,
    daemon_port: u16,
    /// Everything the daemon (and mpv, and the browser engine) printed: the
    /// console is where a URL that cannot be rendered is reported, so tests
    /// assert on it -- see `Session::console`.
    console_log: PathBuf,
    /// Whether this session's compositor can present mpv's frames at all --
    /// see `mpv_can_present`.
    mpv_pixels_expected: bool,
}

impl Session {
    fn wayland_display(&self) -> String {
        for entry in std::fs::read_dir(&self.runtime_dir).expect("read XDG_RUNTIME_DIR") {
            let name = entry.expect("dir entry").file_name();
            let name = name.to_str().expect("Wayland socket name is UTF-8");
            if name.starts_with("wayland-") && !name.ends_with(".lock") {
                return name.to_string();
            }
        }
        panic!(
            "Cage did not create a Wayland socket in {:?}",
            self.runtime_dir
        );
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.daemon_port))
            .expect("connect to the daemon's FCast port");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("set read timeout");
        stream
    }

    /// One composited frame, via `wlr-screencopy` (grim), decoded to RGB.
    fn capture(&self, name: &str) -> Frame {
        let path = self.runtime_dir.join(format!("capture-{name}.png"));
        let status = Command::new("grim")
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("WAYLAND_DISPLAY", self.wayland_display())
            .arg(&path)
            .status()
            .expect("run grim (is it on PATH?)");
        assert!(
            status.success(),
            "grim failed to capture the composited output"
        );
        decode_png_rgb(&path)
    }

    /// Everything the daemon has printed so far (its `tracing` log plus
    /// mpv's and the browser engine's own stderr).
    fn console(&self) -> String {
        std::fs::read_to_string(&self.console_log).unwrap_or_default()
    }

    /// Poll the console until it contains `needle`, then return the whole log;
    /// panics on timeout with the log's tail, so a failure shows what was
    /// actually reported.
    fn wait_for_console(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let log = self.console();
            if log.contains(needle) {
                return log;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for the console to report {needle:?}; console tail:\n{}",
                    console_tail(&log)
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The console's last lines, for a test's own evidence output.
    fn print_console_tail(&self) {
        eprintln!(
            "--- daemon console (tail) ---\n{}\n---",
            console_tail(&self.console())
        );
    }
}

fn console_tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(25)..].join("\n")
}

impl Drop for Session {
    fn drop(&mut self) {
        // Killing Cage's process group takes down Cage *and* the daemon (its
        // primary client, which stays in the same group); the browser is a
        // client of Cage and exits when the compositor's socket goes away.
        //
        // SAFETY: `kill` is a syscall wrapper; a stale pgid can at worst
        // fail with ESRCH.
        unsafe { libc::kill(-(self.cage.id() as i32), libc::SIGKILL) };
        let _ = self.cage.wait();
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Sessions are serialized: they all render through the same GPU/compositor
/// stack, and several mpv/Cage/Chromium sessions at once on one host make the
/// screenshots meaningless (mpv's client EGL path, in particular, needs
/// `/dev/dri` access that a second session can lose). Every test that starts a
/// session holds this for its whole body, so `cargo test` is correct whether
/// or not the harness runs tests in parallel.
fn exclusive_session() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start Cage (headless wlroots backend) with the daemon under test as its
/// one primary client, on a private `XDG_RUNTIME_DIR` and a free TCP port,
/// with the daemon's console captured to a file (see `Session::console`).
fn start_session() -> Session {
    start_session_with(&[])
}

/// [`start_session`] with extra environment for the daemon (e.g. a browser
/// program that cannot be started, to exercise the "neither way" path).
fn start_session_with(extra_env: &[(&str, &str)]) -> Session {
    let mpv_pixels_expected = mpv_can_present();
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_nanos()
    );
    let runtime_dir = std::env::temp_dir().join(format!("castoff-e2e-runtime-{unique}"));
    let home = std::env::temp_dir().join(format!("castoff-e2e-home-{unique}"));
    std::fs::create_dir_all(&runtime_dir).expect("create XDG_RUNTIME_DIR");
    std::fs::create_dir_all(&home).expect("create HOME");
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
        .expect("chmod XDG_RUNTIME_DIR");
    let console_log = runtime_dir.join("daemon-console.log");

    let daemon_port = free_port();
    let mut command = Command::new("cage");
    command
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_castoff-daemon"))
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("HOME", &home)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env("CASTOFF_PORT", daemon_port.to_string())
        // The captain's box had a generated nix-shell `TMPDIR` ~120 characters
        // deep. Chromium used to build its profile under `TMPDIR`, where the
        // process-singleton socket no longer fit the kernel's unix-socket
        // limit, so the engine aborted with `FATAL ... Socket path too long`
        // before painting anything (the page simply never appeared). Every
        // session in this file now runs under exactly that shape, so the page
        // cases below are the regression test.
        .env("TMPDIR", deep_tmpdir(&runtime_dir));
    if let Ok(flags) = std::env::var("CASTOFF_E2E_BROWSER_FLAGS") {
        command.env("CASTOFF_BROWSER", browser_wrapper(&runtime_dir, &flags));
    }
    if !mpv_pixels_expected {
        // Where mpv cannot present (see `mpv_can_present`), keep it off the
        // video-output probing path that aborts the daemon there.
        command.env("CASTOFF_MPV_VO", "null");
    }
    // Per-test environment wins over the defaults above, so a test can point
    // the daemon at a browser program that cannot be started, for instance.
    for (key, value) in extra_env {
        command.env(key, value);
    }
    // The daemon's, mpv's and Chromium's own logs go to one file: the console
    // is part of what the feature must get right (a URL that cannot be
    // rendered has to be reported there), and it is the diagnostic trail when
    // an assertion below fails. The daemon logs through `tracing`, which
    // writes to stderr.
    let console = std::fs::File::create(&console_log).expect("create console log");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(console.try_clone().expect("clone console log")))
        .stderr(Stdio::from(console));
    // Its own process group, so `Drop` can take the whole session down.
    //
    // SAFETY: `setsid` is an async-signal-safe syscall wrapper and nothing
    // else runs between `fork` and `exec` here.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let cage = command.spawn().expect("start cage (is it on PATH?)");

    let session = Session {
        cage,
        runtime_dir,
        daemon_port,
        console_log,
        mpv_pixels_expected,
    };
    wait_for_port(session.daemon_port, Duration::from_secs(20));
    // Harness sanity check: the console this test asserts on must really be
    // the daemon's console. (It was not, once: the daemon logged to stdout,
    // which some environments do not forward to a client's redirected fd.)
    session.wait_for_console("FCast control server listening", Duration::from_secs(20));
    session
}

/// A deliberately deep temp directory, mirroring the generated nix-shell
/// `TMPDIR` (a ~120-character path) that made Chromium's process-singleton
/// socket too long on the captain's box. Lives under the session's runtime
/// dir, so it is cleaned up with the session.
fn deep_tmpdir(root: &Path) -> PathBuf {
    let mut dir = root.to_path_buf();
    while dir.display().to_string().len() < 120 {
        dir.push("nix-shell-1310600-2273476824");
    }
    std::fs::create_dir_all(&dir).expect("create deep TMPDIR");
    dir
}

/// Whether mpv can be expected to present frames into this session's
/// compositor.
///
/// mpv's `vo=gpu` presents over `linux-dmabuf` (or EGL), which a compositor
/// needs a GL renderer to support. Where no DRM device exists -- the Nix
/// build sandbox, whose Cage falls back to wlroots' *pixman* renderer -- mpv
/// has no way to paint at all, however real the rest of the session is. The
/// *browser* is unaffected: Chromium presents over shared memory, so the
/// page-pixel assertions below still run there. Set
/// `CASTOFF_E2E_SKIP_MPV_PIXELS=1` (as the package's Nix `postCheck` does)
/// to keep the media step to the state/page assertions instead of failing on
/// a pixel that this environment can never paint. In that case the daemon
/// also runs with `CASTOFF_MPV_VO=null` (see `start_session`), because mpv's
/// `gpu` output aborts the process inside its own context probing when no
/// context can be created at all.
fn mpv_can_present() -> bool {
    let skip = matches!(
        std::env::var("CASTOFF_E2E_SKIP_MPV_PIXELS").as_deref(),
        Ok("1") | Ok("true")
    );
    if skip {
        eprintln!(
            "NOTE: CASTOFF_E2E_SKIP_MPV_PIXELS is set: skipping mpv's pixel assertions \
             (this session has no GL renderer for mpv to present into); the browser \
             engine's pixels are still asserted through the real compositor."
        );
    }
    !skip
}

/// A wrapper script running Chromium with `CASTOFF_E2E_BROWSER_FLAGS` added,
/// for environments where the engine needs extra test-only flags (see the
/// module docs). The engine behind it is the real Chromium.
fn browser_wrapper(dir: &Path, extra_flags: &str) -> PathBuf {
    let wrapper = dir.join("chromium-e2e-wrapper");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec chromium {extra_flags} \"$@\"\n"),
    )
    .expect("write browser wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("chmod browser wrapper");
    wrapper
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn wait_for_port(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the daemon never accepted a connection on port {port}");
}

/// A one-page HTTP server on loopback, plus a counter of how many requests it
/// has answered (used to show the *engine* fetches the page, and that nothing
/// re-fetches it behind the scenes).
/// A one-request-at-a-time HTTP server on loopback, plus a counter of how
/// many requests it answered. `content_type` is served verbatim, so tests can
/// exercise both ".mp4"-style senders and servers that mislabel a video as
/// `application/octet-stream`.
fn serve_bytes(content_type: &str, body: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind page server");
    let port = listener.local_addr().expect("local addr").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let content_type = content_type.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                 Content-Length: {len}\r\nConnection: close\r\n\r\n",
                len = body.len(),
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (port, requests)
}

/// [`serve_bytes`] for an HTML page: what a dashboard-looking sender serves.
fn serve_page(html: String) -> (u16, Arc<AtomicUsize>) {
    serve_bytes("text/html; charset=utf-8", html.into_bytes())
}

/// A real, playable video file with no encoder involved: YUV4MPEG2 (`y4m`) is
/// a plain-text header plus raw BT.601 frames, which mpv/ffmpeg's yuv4mpeg
/// demuxer recognizes by magic bytes (no file extension or video MIME type
/// needed -- see `serve_bytes`'s callers). 16:9, so mpv scales it to fill the
/// screen rather than pillarboxing it.
fn y4m_video(y: u8, cb: u8, cr: u8, frames: usize) -> Vec<u8> {
    const WIDTH: usize = 160;
    const HEIGHT: usize = 90;
    let mut video = format!("YUV4MPEG2 W{WIDTH} H{HEIGHT} F25:1 Ip A1:1 C420mpeg2\n").into_bytes();
    for _ in 0..frames {
        video.extend_from_slice(b"FRAME\n");
        video.extend(std::iter::repeat_n(y, WIDTH * HEIGHT));
        video.extend(std::iter::repeat_n(cb, (WIDTH / 2) * (HEIGHT / 2)));
        video.extend(std::iter::repeat_n(cr, (WIDTH / 2) * (HEIGHT / 2)));
    }
    video
}

/// Whether this environment can reach the public internet. The Nix build
/// sandbox cannot, so the tests that need a public URL say so and return
/// instead of failing a sandboxed `nix flake check` run; the same tests are
/// run for real in the dev shell, where the network is available.
fn network_available() -> bool {
    for host in ["example.com:443", "one.one.one.one:443"] {
        let Ok(mut addrs) = host.to_socket_addrs() else {
            continue;
        };
        if let Some(addr) = addrs.next() {
            if TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok() {
                return true;
            }
        }
    }
    false
}

/// A page that paints `color` over its whole viewport and nothing else: the
/// assertion below is then about *this* page's content, not about some
/// incidental text/colours.
fn solid_color_page(color: [u8; 3]) -> String {
    let hex = format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>castoff e2e</title>\
         <style>html,body{{margin:0;padding:0;width:100%;height:100%;background:{hex}}}</style>\
         </head><body></body></html>"
    )
}

/// Send one FCast frame and read the daemon's reply frame back.
fn fcast(
    stream: &mut TcpStream,
    opcode: u8,
    body: Option<&serde_json::Value>,
) -> serde_json::Value {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.extend_from_slice(&(1 + body.len() as u32).to_le_bytes());
    frame.push(opcode);
    frame.extend_from_slice(body.as_bytes());
    stream.write_all(&frame).expect("write FCast frame");
    stream.flush().expect("flush FCast frame");

    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .expect("read reply length prefix");
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).expect("read reply payload");
    assert_eq!(
        payload[0], OPCODE_PLAYBACK_UPDATE,
        "expected a PlaybackUpdate reply, got opcode {}",
        payload[0]
    );
    serde_json::from_slice(&payload[1..]).expect("parse PlaybackUpdate body")
}

/// One decoded composited frame (RGB, 3 bytes per pixel).
struct Frame {
    pixels: Vec<u8>,
}

impl Frame {
    fn color_fraction(&self, expected: [u8; 3], tolerance: u8) -> f64 {
        let mut matching = 0usize;
        let mut total = 0usize;
        for pixel in self.pixels.chunks_exact(3) {
            total += 1;
            if pixel
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= tolerance)
            {
                matching += 1;
            }
        }
        matching as f64 / total.max(1) as f64
    }

    /// The most common colour in the frame and its share of the screen, for
    /// failure messages.
    fn dominant_color(&self) -> ([u8; 3], f64) {
        let mut counts: std::collections::HashMap<[u8; 3], usize> =
            std::collections::HashMap::new();
        for pixel in self.pixels.chunks_exact(3) {
            *counts.entry([pixel[0], pixel[1], pixel[2]]).or_default() += 1;
        }
        let total = self.pixels.len() / 3;
        match counts.into_iter().max_by_key(|(_, count)| *count) {
            Some((color, count)) => (color, count as f64 / total.max(1) as f64),
            None => ([0, 0, 0], 0.0),
        }
    }
}

fn decode_png_rgb(path: &Path) -> Frame {
    let file = std::fs::File::open(path).expect("open screenshot");
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().expect("read PNG info");
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).expect("decode PNG frame");
    buffer.truncate(info.buffer_size());

    let pixels = match info.color_type {
        png::ColorType::Rgb => buffer,
        png::ColorType::Rgba => buffer
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect(),
        other => panic!("unexpected screenshot colour type {other:?}"),
    };
    Frame { pixels }
}

/// Poll the compositor's output until `expected` fills it, or fail.
fn wait_for_color(session: &Session, expected: [u8; 3], what: &str, timeout: Duration) -> Frame {
    let deadline = Instant::now() + timeout;
    let mut best = frame_of(session, what);
    loop {
        let fraction = best.color_fraction(expected, 8);
        if fraction >= ON_SCREEN_FRACTION {
            return best;
        }
        if Instant::now() >= deadline {
            let (dominant, share) = best.dominant_color();
            panic!(
                "timed out waiting for {what}: best match was {:.1}% of pixels for {expected:?}; \
                 the screen is mostly {:02x}{:02x}{:02x} ({:.1}%); console tail:\n{}",
                fraction * 100.0,
                dominant[0],
                dominant[1],
                dominant[2],
                share * 100.0,
                console_tail(&session.console()),
            );
        }
        std::thread::sleep(Duration::from_millis(500));
        best = frame_of(session, what);
    }
}

/// Poll the compositor's output until `color` is gone, and return that
/// frame. Needed because mpv processes `stop` asynchronously: the daemon's
/// reply arrives once the command is queued, the repaint follows on mpv's
/// own thread.
fn wait_for_color_gone(session: &Session, color: [u8; 3], what: &str, timeout: Duration) -> Frame {
    let deadline = Instant::now() + timeout;
    loop {
        let frame = frame_of(session, what);
        let fraction = frame.color_fraction(color, 8);
        if fraction < 0.05 {
            return frame;
        }
        if Instant::now() >= deadline {
            let (dominant, share) = frame.dominant_color();
            panic!(
                "timed out waiting for {what}: {:.1}% of the screen still shows {color:?}; \
                 the screen is mostly {:02x}{:02x}{:02x} ({:.1}%); console tail:\n{}",
                fraction * 100.0,
                dominant[0],
                dominant[1],
                dominant[2],
                share * 100.0,
                console_tail(&session.console()),
            );
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn frame_of(session: &Session, what: &str) -> Frame {
    session.capture(&what.replace(' ', "-"))
}

/// Cast a page at the daemon, supersede it with a second page, then with
/// media, then `Stop` -- the whole lifecycle the task promises, on real
/// pixels.
#[test]
#[ignore = "needs cage, chromium and grim on PATH; starts a headless compositor session"]
fn casting_a_webpage_puts_that_page_on_screen_and_stop_returns_to_idle() {
    let _serial = exclusive_session();
    let (page_port, page_requests) = serve_page(solid_color_page(PAGE_COLOR));
    let (second_page_port, second_page_requests) = serve_page(solid_color_page(SECOND_PAGE_COLOR));
    let session = start_session();
    let mut control = session.connect();

    // Cast the page exactly as a sender would: `container` says text/html
    // (see `fcast::PlayMessage::explicit_target`), the daemon owns the rest.
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "container": "text/html",
            "url": format!("http://127.0.0.1:{page_port}/"),
        })),
    );
    assert_eq!(
        reply["state"], STATE_PLAYING,
        "a cast page must be reported as playing, not idle: {reply}"
    );

    // The browser engine -- not the daemon -- is what fetches the page.
    wait_for_color(
        &session,
        PAGE_COLOR,
        "the cast page's pixels on screen",
        Duration::from_secs(40),
    );
    assert!(
        page_requests.load(Ordering::SeqCst) >= 1,
        "the browser engine must have fetched the page from the local server"
    );

    // Supersede the page with a *second page*. Both engines share one
    // Chromium profile, so the replacement must only start once the first is
    // gone: a replacement started while the first still holds the profile's
    // ProcessSingleton lock aborts (or defers to the first and exits), and the
    // sender's second page never appears. This asserts on real pixels: only
    // the second page's colour may fill the screen.
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "container": "text/html",
            "url": format!("http://127.0.0.1:{second_page_port}/"),
        })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");
    let after_second = wait_for_color(
        &session,
        SECOND_PAGE_COLOR,
        "the second cast page's pixels on screen",
        Duration::from_secs(40),
    );
    assert!(
        after_second.color_fraction(PAGE_COLOR, 8) < 0.05,
        "the first page must be off screen once the second page is displayed"
    );
    assert!(
        second_page_requests.load(Ordering::SeqCst) >= 1,
        "the browser engine must have fetched the second page from the local server"
    );

    // Media supersedes the page: the engine goes away and mpv takes the
    // screen back (a real synthetic video, decoded and painted by mpv).
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "url": "av://lavfi:color=c=cyan:size=640x360:rate=10:duration=60",
        })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");
    let after_media = if session.mpv_pixels_expected {
        let frame = wait_for_color(
            &session,
            MEDIA_COLOR,
            "media playback to take the screen back",
            Duration::from_secs(20),
        );
        assert!(
            frame.color_fraction(SECOND_PAGE_COLOR, 8) < 0.05,
            "the superseded page must be off screen"
        );
        frame
    } else {
        // No GL for mpv to present into (see `mpv_can_present`), but the
        // page it superseded must still be gone: the engine was terminated
        // and Cage dropped its view.
        wait_for_color_gone(
            &session,
            SECOND_PAGE_COLOR,
            "the superseded page to leave the screen",
            Duration::from_secs(20),
        )
    };
    let _ = after_media;

    // Stop returns the screen to the daemon's idle behaviour: the page's
    // pixels are gone for good.
    let reply = fcast(&mut control, OPCODE_STOP, None);
    assert_eq!(reply["state"], STATE_IDLE, "reply after Stop: {reply}");
    let after_stop = if session.mpv_pixels_expected {
        wait_for_color_gone(
            &session,
            MEDIA_COLOR,
            "media playback to stop on a Stop",
            Duration::from_secs(15),
        )
    } else {
        frame_of(&session, "after stop")
    };
    assert!(
        after_stop.color_fraction(PAGE_COLOR, 8) < 0.05
            && after_stop.color_fraction(SECOND_PAGE_COLOR, 8) < 0.05,
        "Stop must take the page off screen"
    );

    // And nothing re-fetches the page on a timer behind the sender's back:
    // with the engine stopped, the request count is stable.
    let settled = page_requests.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        page_requests.load(Ordering::SeqCst),
        settled,
        "the daemon must not re-fetch the page without a reason"
    );
}

/// Send a `Resume` (which replies with a `PlaybackUpdate`) until the reply
/// satisfies `pred`, polling every 250ms; returns that reply. `Resume` is
/// used because *any* command replies with the daemon's current playback
/// state, which is how a sender observes whether media is really playing.
fn wait_for_status(
    control: &mut TcpStream,
    pred: impl Fn(&serde_json::Value) -> bool,
    what: &str,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let reply = fcast(control, OPCODE_RESUME, None);
        if pred(&reply) {
            return reply;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}; last reply: {reply}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Poll the compositor's output until `pred` holds for a frame; returns that
/// frame, or fails with the frame's dominant colour.
fn wait_for_frame(
    session: &Session,
    what: &str,
    timeout: Duration,
    pred: impl Fn(&Frame) -> bool,
) -> Frame {
    let deadline = Instant::now() + timeout;
    loop {
        let frame = frame_of(session, what);
        if pred(&frame) {
            return frame;
        }
        if Instant::now() >= deadline {
            let (dominant, share) = frame.dominant_color();
            panic!(
                "timed out waiting for {what}; the screen is mostly \
                 {:02x}{:02x}{:02x} ({:.1}%)",
                dominant[0],
                dominant[1],
                dominant[2],
                share * 100.0
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The daemon's own routing decision, over the real wire: a sender that knows
/// only a URL casts a *web page*. The daemon tries mpv first, and when that
/// attempt fails it puts the page's pixels on screen in the browser engine --
/// reporting the failed attempt and the fallback on the console, with the
/// timing the failed attempt costs.
#[test]
#[ignore = "needs cage, chromium and grim on PATH; starts a headless compositor session"]
fn unclassified_page_reaches_the_browser_after_a_failed_media_attempt() {
    let _serial = exclusive_session();
    let (page_port, page_requests) = serve_page(solid_color_page(PAGE_COLOR));
    let session = start_session();
    let mut control = session.connect();

    // No `container`: exactly what a client that only knows a URL sends.
    let played_at = Instant::now();
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "url": format!("http://127.0.0.1:{page_port}/"),
        })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");

    // The media attempt is what fails first ...
    session.wait_for_console("URL is not playable as media", Duration::from_secs(30));
    let media_attempt = played_at.elapsed();
    // ... and the page really reaches the screen, through the real engine and
    // the real compositor.
    wait_for_color(
        &session,
        PAGE_COLOR,
        "the cast page's pixels on screen",
        Duration::from_secs(40),
    );
    eprintln!(
        "fallback timing: media attempt failed after {:.2}s, page pixels on screen after {:.2}s",
        media_attempt.as_secs_f64(),
        played_at.elapsed().as_secs_f64()
    );
    assert!(
        page_requests.load(Ordering::SeqCst) >= 1,
        "the browser engine (not the daemon) must have fetched the page"
    );
    let console = session.console();
    assert!(
        console.contains("displaying it as a web page instead"),
        "the console must say the URL was handed to the browser; console tail:\n{}",
        console_tail(&console)
    );
    assert!(
        !console.contains("mpv reported an async playback error"),
        "a fallback that succeeded must not be reported as a playback error; console tail:\n{}",
        console_tail(&console)
    );
    session.print_console_tail();
}

/// Regression for the daemon's routing when mpv expands a submitted URL into a
/// playlist: an unrouted `.m3u` Play is media (mpv plays its entries), and a
/// *later* unrouted page Play must still fall back to the browser even though
/// mpv reports `StartFile`/`FileLoaded`/`EndFile` for the playlist's own
/// entries. Before the fix, one of those extra per-entry events was attributed
/// to the page Play's probe, so the page was never handed to the engine and
/// stayed on a black screen.
#[test]
#[ignore = "needs cage, chromium and grim on PATH; starts a headless compositor session"]
fn unclassified_playlist_then_page_displays_the_page() {
    let _serial = exclusive_session();
    // The playlist's media: a real video, served at the URL the `.m3u` points
    // at.
    let (media_port, media_requests) = serve_bytes(
        "application/octet-stream",
        y4m_video(VIDEO_YUV.0, VIDEO_YUV.1, VIDEO_YUV.2, 300),
    );
    // mpv detects the playlist from the URL's `.m3u` extension; the body is a
    // single local media URL. Serving a playlist rather than a bare media URL
    // is what makes mpv emit the extra per-entry events this is about.
    let playlist = format!("http://127.0.0.1:{media_port}/clip.y4m\n");
    let (list_port, list_requests) = serve_bytes("audio/x-mpegurl", playlist.into_bytes());
    let (page_port, page_requests) = serve_page(solid_color_page(PAGE_COLOR));
    let session = start_session();
    let mut control = session.connect();

    // Unrouted playlist URL: the daemon tries it as media first, and mpv
    // expands and plays it.
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "url": format!("http://127.0.0.1:{list_port}/list.m3u"),
        })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");
    // Wait until the playlist's media is actually playing, so it is in flight
    // when the page Play supersedes it.
    let playing = wait_for_status(
        &mut control,
        |reply| reply["time"].as_f64().unwrap_or(0.0) > 0.2,
        "the playlist's media to start playing",
        Duration::from_secs(20),
    );
    eprintln!(
        "playlist: playing at t={:.2}s",
        playing["time"].as_f64().unwrap_or_default()
    );

    // Supersede it with an unrouted page. mpv stops the playlist's entry first
    // (an `EndFile` for an entry the daemon never submitted), then fails to
    // load the page; the daemon must still fall back.
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "url": format!("http://127.0.0.1:{page_port}/"),
        })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");

    wait_for_color(
        &session,
        PAGE_COLOR,
        "the page cast over the playlist to be on screen",
        Duration::from_secs(40),
    );
    assert!(
        list_requests.load(Ordering::SeqCst) >= 1,
        "mpv must have fetched the playlist"
    );
    assert!(
        media_requests.load(Ordering::SeqCst) >= 1,
        "mpv must have fetched the playlist's media"
    );
    assert!(
        page_requests.load(Ordering::SeqCst) >= 1,
        "the browser engine must have fetched the page"
    );
    assert!(
        session.console().contains("displaying it as a web page instead"),
        "the console must say the page was handed to the browser; console tail:\n{}",
        console_tail(&session.console())
    );
    session.print_console_tail();
}

/// A direct media file URL the sender did not classify stays on the media
/// path: the daemon's first attempt *is* the answer, so mpv keeps playing and
/// the browser engine never starts. The file is a real video (`y4m`) served
/// over real HTTP with no video MIME type -- mpv/ffmpeg sniff it by content.
#[test]
#[ignore = "needs cage, chromium and grim on PATH; starts a headless compositor session"]
fn unclassified_direct_media_url_plays_as_media() {
    let _serial = exclusive_session();
    let (media_port, media_requests) = serve_bytes(
        "application/octet-stream",
        y4m_video(VIDEO_YUV.0, VIDEO_YUV.1, VIDEO_YUV.2, 300),
    );
    let session = start_session();
    let mut control = session.connect();

    let played_at = Instant::now();
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "url": format!("http://127.0.0.1:{media_port}/clip.y4m"),
        })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");

    // mpv really has a timeline: the sender sees playback advance, which is
    // the evidence that the media path won (a page would have no `time`).
    let playing = wait_for_status(
        &mut control,
        |reply| reply["time"].as_f64().unwrap_or(0.0) > 0.2,
        "mpv playback to advance",
        Duration::from_secs(20),
    );
    eprintln!(
        "media start: playing at t={:.2}s after {:.2}s",
        playing["time"].as_f64().unwrap_or_default(),
        played_at.elapsed().as_secs_f64()
    );
    if session.mpv_pixels_expected {
        wait_for_color(
            &session,
            VIDEO_COLOR,
            "the video file's pixels on screen",
            Duration::from_secs(20),
        );
    }
    assert!(
        media_requests.load(Ordering::SeqCst) >= 1,
        "the media file must have been fetched"
    );
    let console = session.console();
    assert!(
        !console.contains("URL is not playable as media"),
        "playable media must never be re-routed to the browser; console tail:\n{}",
        console_tail(&console)
    );
}

/// The trap the captain called out: a YouTube watch URL *is* `text/html` as
/// far as HTTP is concerned, so a Content-Type probe would misroute it. With
/// the daemon's media-first decision it plays as video, `yt-dlp` resolving a
/// real stream (a plausible duration) and mpv advancing through it. Needs the
/// network and `yt-dlp`, so it reports and returns where neither is available.
#[test]
#[ignore = "needs network, yt-dlp, cage, chromium and grim"]
fn unclassified_youtube_url_plays_as_video() {
    if !network_available() {
        eprintln!(
            "NOTE: no network in this environment; skipping the YouTube routing case \
             (run this test in `nix develop`, where the public internet is reachable)."
        );
        return;
    }
    let _serial = exclusive_session();
    let session = start_session();
    let mut control = session.connect();

    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({
            "url": "https://www.youtube.com/watch?v=jNQXAC9IVRw",
        })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");

    // A real resolution by `yt-dlp` produces a real duration: "Me at the zoo"
    // is ~19s. (The immediate reply can precede it, hence the polling.)
    let resolved = wait_for_status(
        &mut control,
        |reply| reply["duration"].as_f64().is_some(),
        "yt-dlp to resolve the YouTube video into a real stream",
        Duration::from_secs(60),
    );
    let duration = resolved["duration"].as_f64().unwrap_or_default();
    assert!(
        (15.0..25.0).contains(&duration),
        "expected ~19s for the known test video, got {duration}"
    );
    let playing = wait_for_status(
        &mut control,
        |reply| reply["time"].as_f64().unwrap_or(0.0) > 0.5,
        "the YouTube video to be playing",
        Duration::from_secs(30),
    );
    eprintln!(
        "youtube: duration {:.1}s, playing at t={:.1}s",
        duration,
        playing["time"].as_f64().unwrap_or_default()
    );

    let console = session.console();
    assert!(
        !console.contains("URL is not playable as media"),
        "YouTube must stay on the media path; console tail:\n{}",
        console_tail(&console)
    );
    if session.mpv_pixels_expected {
        // Something other than the idle clock is on screen.
        let frame = session.capture("youtube");
        assert!(
            frame.color_fraction([0, 0, 0], 8) < 0.95,
            "the idle clock is still on screen instead of the video: {:?}",
            frame.dominant_color()
        );
    }
    session.print_console_tail();
}

/// An ordinary web page (`example.com`; `google.com` behaves the same way)
/// displays in the browser: the media attempt fails, the browser renders the
/// real page, and no mpv timeline exists while it is up.
#[test]
#[ignore = "needs network, cage, chromium and grim"]
fn unclassified_ordinary_web_page_displays_in_the_browser() {
    if !network_available() {
        eprintln!(
            "NOTE: no network in this environment; skipping the ordinary-web-page case \
             (run this test in `nix develop`, where the public internet is reachable)."
        );
        return;
    }
    let _serial = exclusive_session();
    let session = start_session();
    let mut control = session.connect();

    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({ "url": "https://example.com/" })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");
    session.wait_for_console("URL is not playable as media", Duration::from_secs(30));

    // A rendered document, not the idle clock: mostly white with some dark
    // text. (Asserting "not the clock" alone would pass on a black screen.)
    let frame = wait_for_frame(
        &session,
        "a rendered web page",
        Duration::from_secs(40),
        |frame| frame.color_fraction([255, 255, 255], 32) > 0.5,
    );
    eprintln!(
        "example.com: white covers {:.1}% of the screen",
        frame.color_fraction([255, 255, 255], 32) * 100.0
    );

    // A web page has no mpv timeline to report.
    let status = fcast(&mut control, OPCODE_RESUME, None);
    assert!(
        status["time"].is_null() && status["duration"].is_null(),
        "a displayed page must not report an mpv timeline: {status}"
    );
    session.print_console_tail();
}

/// Nothing serves the URL: the media attempt fails, the browser engine takes
/// over (and shows its own error page), and both steps are reported on the
/// console. A separate test from the no-browser case below so only one
/// compositor session is ever up at a time (see `exclusive_session`).
#[test]
#[ignore = "needs cage, chromium and grim on PATH; starts a headless compositor session"]
fn a_url_nothing_can_render_falls_back_to_the_browser() {
    let _serial = exclusive_session();
    let session = start_session();
    let mut control = session.connect();
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({ "url": "http://127.0.0.1:1/nothing" })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");
    let console = session.wait_for_console(
        "displaying it as a web page instead",
        Duration::from_secs(30),
    );
    assert!(
        console.contains("URL is not playable as media"),
        "the failed media attempt must be named; console tail:\n{}",
        console_tail(&console)
    );
    session.print_console_tail();
}

/// The other side of the previous case: when the browser engine cannot be
/// started either, that is reported too, with the screen back on the idle
/// clock rather than left black or spinning.
#[test]
#[ignore = "needs cage, chromium and grim on PATH; starts a headless compositor session"]
fn a_url_nothing_can_render_and_no_browser_is_reported_on_the_console() {
    let _serial = exclusive_session();
    let session = start_session_with(&[("CASTOFF_BROWSER", "/nonexistent/castoff-e2e-browser")]);
    let mut control = session.connect();
    let reply = fcast(
        &mut control,
        OPCODE_PLAY,
        Some(&serde_json::json!({ "url": "http://127.0.0.1:1/nothing" })),
    );
    assert_eq!(reply["state"], STATE_PLAYING, "reply: {reply}");
    session.wait_for_console(
        "could not be displayed as a web page",
        Duration::from_secs(30),
    );
    let frame = wait_for_frame(
        &session,
        "the screen to return to the idle clock",
        Duration::from_secs(15),
        |frame| frame.color_fraction([0, 0, 0], 40) > 0.5,
    );
    eprintln!(
        "neither-route: screen back to the idle clock, mostly {:?} (white clock text \
         included in the tolerance)",
        frame.dominant_color()
    );
    session.print_console_tail();
}
