# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.
- Layout, build/run/test commands, protocol details, and scope (what's implemented vs. planned)
  are documented in `README.md` — read that first, it's the source of truth, not this file.
- `flake.nix` pins `nixpkgs` to `nixos-25.11` deliberately: `libmpv2` requires Rust's
  `edition2024`, which needs Cargo/rustc >= 1.85; `nixos-24.11`'s toolchain is too old and fails
  with "feature `edition2024` is required". It also has a `nixpkgs-unstable` input used for
  exactly one package, `yt-dlp` (the daemon's runtime `PATH` wrapper and the dev shell). Reason:
  `nixos-25.11`'s yt-dlp (2026.06.09) auto-selects YouTube's `android_vr` player client for some
  videos, whose signed stream URLs the CDN answers with HTTP 403 → mpv reports
  `MPV_ERROR_NOTHING_TO_PLAY`; `nixos-unstable`'s (2026.08.19) selects `visionos` and plays the
  same video. That is a yt-dlp extractor/client-selection issue, not a castoff or mpv one. Keep
  everything else on `nixos-25.11`; only `yt-dlp` comes from unstable.
- `nix build`/`nix flake check` fetch crate sources straight from `crates.io` (no vendored
  `Cargo.lock` hashes beyond what `cargoLock.lockFile` gives). That endpoint occasionally 403s a
  plain-`curl` fetch (no User-Agent) with no pattern tied to a specific crate; if a build fails
  with `curl: (22) ... 403` on a `crate-*.tar.gz.drv`, just retry the same `nix build` — it
  resumes from whatever already fetched successfully and has always succeeded within a few
  retries. This is a `crates.io`-side bot-mitigation quirk, not a broken lockfile.
- The unit-test binary (`--release`) has repeatedly been seen to die with a bare `SIGSEGV`; every
  occurrence investigated so far turned out to be a real, reproducible defect, not sandbox
  flakiness to retry past -- see `daemon/.cargo/config.toml` and `player.rs`'s `headless_mpv` doc
  comment for the four concurrency hazards found and fixed so far (a fontconfig race; real
  mpv/ffmpeg cores alive concurrently; a test-only background thread -- `spawn_metadata_lookup` --
  left unjoined and racing a later test's real mpv core; and background watcher threads that never
  observed mpv's `Event::Shutdown`, so a test's real mpv core and its threads leaked for the rest
  of the process instead of being torn down -- fixed by a test-only `Player::Drop` that sends
  mpv's `quit` command and joins those watcher threads). **None of these four fixes eliminates the
  crash on its own** -- measured (2026-09-19) at roughly 12-20% of runs on plain `cargo test
  --release` outside the sandbox, but a much higher ~40-57% inside the actual `nix build`
  sandboxed `checkPhase`, both before and after all four fixes; the sandbox-specific amplification
  is itself unexplained and is open follow-up work, not resolved. Do not treat a future SIGSEGV
  here as already-explained by this history -- root-cause it, and expect it to reproduce more
  reliably by running several `nix build --rebuild` attempts (or `nix develop -c cargo test
  --release` in a loop, faster to iterate) than by trusting a single run either way. Note also that
  this repo's CI (`.github/workflows/ci.yml`) retries its whole `nix flake check` step up to 5
  times on *any* failure (originally to ride out the `crates.io` 403 above), so an intermittent
  SIGSEGV at even a moderate per-run rate can pass CI by sheer retry odds while still failing a
  plain local `nix build`; a CI-green PR is not proof a flaky crash like this is absent.
- `nix build`/`nix flake check` see only *git-tracked* files (via the flake's `self` source
  filter) — a new source file left untracked compiles fine under a plain `cargo build` in
  `nix develop` but fails the flake build with a "file not found for module" error. `git add` new
  files (staging is enough, no need to commit) before trusting a flake build/check result.
- `libmpv2::Mpv::wait_event(-1.0)` (infinite timeout) can still return `None` (its "no event"
  sentinel) as a spurious wakeup with nothing actually queued — this is normal/documented mpv
  client-API behavior, not a signal that the core shut down. Any event-watcher loop built on
  `wait_event` must treat `None` as "loop and wait again," and only stop on an explicit
  `Event::Shutdown`; treating `None` as shutdown makes the watcher silently exit after its first
  spurious wakeup (see `daemon/src/idle_screen.rs`'s eof-watcher).
- mpv's OSD (`show-text`/`osd-overlay`) has no read-back property to confirm what's currently
  displayed, and `screenshot`/`screenshot-to-file` fail outright while mpv is idle (no file
  loaded) even with `force-window=yes` — confirmed empirically, there's no headless (no
  GPU/display) way to assert on rendered idle-screen pixels. Tests covering idle-screen-type
  behavior (`daemon/src/player.rs`'s idle-screen test) instead assert on real mpv command
  success/failure plus real state transitions (e.g. `eof-reached`), not pixels. For a one-off
  *visual* check there is a path: run the daemon/mpv under `Xvfb` with a real `vo`
  (`--gpu-context=x11egl`, `LIBGL_ALWAYS_SOFTWARE=1`) and capture the X root window
  (`import -window root`, or `x11grab` from `nixpkgs#ffmpeg-full` — the default `nixpkgs#ffmpeg`
  is built `--disable-libxcb` and has no x11grab). `screenshot-to-file` does *not* include
  `osd-overlay` overlays, and `vo=null` cannot screenshot at all, which is why the automated tests
  stay headless. That Xvfb capture is how the loading spinner and start/stop fades were verified
  end-to-end; it needs a display, so it stays a manual evidence step, not a test.
- The loading spinner and start/stop fade live in `daemon/src/overlay.rs`, drawing through the same
  `osd-overlay` ASS path as the idle clock (not a second rendering stack). It owns overlay ids
  9100 (fade rect) / 9101 (spinner); the idle clock keeps 9000/9001, so don't reuse those.
  IMPORTANT: mpv stacks `osd-overlay` layers by **recency, not by id** (empirically verified) --
  the most recently added/updated overlay is on top. That's fine for covering video (an overlay is
  always above video), but it means a cover rect can't reveal the idle clock: the re-created clock
  is stacked above the rect and pops in. `Player::fade_in_idle_clock` therefore ramps the clock's
  own `\1a` alpha up (`IdleScreenController::render_at`) over the still-opaque rect, then drops the
  rect once the clock's opaque background covers the canvas. Do NOT reintroduce a remove+re-add of
  the rect to force z-order: mpv can render between the remove and the add, flashing the video.
  The spinner is an ASS vector annular sector rotated in place via
  `\an7\pos` + `\org` + `\frz` (no font dependency; `\an5` would make the arc orbit the canvas --
  see `spinner_ass`'s comment before touching the anchor). Every animation is a bounded
  loop that stops on a generation-counter bump; the spinner thread is deadline-scheduled to
  redraw at ~30/s (measured 30.3/s; 12 degrees/frame, ~1s revolution) and only while a Play is in
  flight. Overlay draws hold the state
  mutex across their `osd-overlay` command and `clear()` holds
  it across its teardown, so a draw that passed the staleness check can never land after the
  overlays were removed (the stale-spinner-draw race). `Player::play`/`stop` block ~400ms per fade
  (20 opacity steps at 20ms), but a superseding command aborts the old animation at its next frame,
  so the concurrent-`play` test stays fast; `play`/`stop` overlay error paths fall back through
  `abort_loading_to_idle` -> `fade_in_idle_clock`, which rolls the overlay back to a visible clock
  if any of its own steps fail, so a partial setup can't leave the opaque fade covering the screen.
  `stop` decides "already idle" from the idle clock being on screen, not mpv's `idle-active`: with
  `keep-open=yes` a clip that reached EOF is not `idle-active` though the clock is already back, so
  keying off that would conceal the visible clock and blink it (regression test
  `stop_after_end_of_file_does_not_blink_the_idle_clock`); `fade_in_idle_clock` skips its alpha
  ramp in that case, since `render_at` must not run alongside `show`'s refresh thread.
  `CASTOFF_ANIMATION_SLOWDOWN` (read once in `PlaybackOverlay::new`, parsed by `parse_slowdown`)
  multiplies both step durations for manual inspection (measured 30.3/s -> 3.03/s at 10x);
  unset/unusable -> 1 (shipping), clamped at
  1000, and it never makes an animation always-on.
  `PlaybackOverlay::is_active()` stays true for the whole ~400ms `reveal` fade-out that follows a
  genuine `PlaybackRestart`, not just for the loading phase -- it's "are the overlay's decorative
  pixels on screen," not "is a load still unresolved." `Player::is_idle` (the play-queue gate, see
  below) needs the latter, so it checks a separate `is_restarted`/`mark_restarted` flag on the same
  `State` instead: true from `PlaybackRestart` (set at the top of `handle_lifecycle_event`'s arm,
  before `reveal` starts its fade) until the next `spawn_spinner` call resets it. Using
  `is_active()` there instead made a `Play` arriving during that cosmetic fade-out still interrupt
  already-genuine playback rather than queue behind it (regression tests
  `play_while_playing_enqueues_instead_of_interrupting` and siblings in `player.rs`).
- YouTube playback needs no daemon-side code: mpv's built-in `ytdl_hook` Lua script (same core in
  both CLI mpv and `libmpv2`) auto-detects non-direct-media URLs and shells out to `yt-dlp` on
  `PATH`, unconditionally, with no libmpv init tweaks required — see README's "How YouTube
  playback works". `yt-dlp` is a runtime-only dependency: the packaged `nix build` binary gets it
  from the `nixpkgs-unstable` input via `makeWrapper`/`wrapProgram` (not `buildInputs`, since it's
  invoked as a subprocess, not linked), and `devShells.default` separately lists that same
  `yt-dlp` in `packages` for the same
  reason -- `inputsFrom` only pulls a package's buildInputs/nativeBuildInputs, never its
  `postFixup` wrapping, so the dev shell needs its own copy or `cargo build`/`cargo run` there
  silently lack `yt-dlp` on `PATH`. Its one real (non-mocked) test in `daemon/src/player.rs`
  (`real_youtube_url_resolves_and_plays_via_ytdl_hook`) needs network, so it's `#[ignore]`d — the
  `nix build`/`nix flake check` sandbox has no network. Run it manually with
  `nix develop -c cargo test -- --ignored` (yt-dlp is already on `PATH` there).
- That `yt-dlp` difference is exactly why a test must not poll mpv's `path` property to prove "mpv
  was handed this URL": mpv clears `path` (it becomes `MPV_ERROR_PROPERTY_UNAVAILABLE`, and
  `playlist-count` drops to 0) the moment a load fails. The dev shell's `yt-dlp` makes mpv's
  `ytdl_hook` intercept the URL and delay the failure, so the poll wins there; the `nix flake
  check` sandbox has no `yt-dlp`, the failure is immediate, and the poll never sees the property.
  Hold the load open instead — a loopback server that waits for the test's go-ahead before
  answering (`serve_gated_failure` in `player.rs`) makes the observation deterministic in both
  environments.
- Async mpv playback errors (e.g. `ytdl_hook`/`yt-dlp` failing to resolve a URL) don't surface from
  `Player::play`'s `loadfile` call — that only queues the load; mpv resolves/opens it later, off
  that call stack. `player.rs`'s `spawn_async_event_watcher` catches these via a second `Mpv` client
  handle (`Mpv::create_client`) dedicated to blocking on `wait_event`, logging a tracked load's
  error at `error!` — event-driven, not a polling loop. Attribution is by mpv's own
  `playlist_entry_id`, not by submission order: `loadfile` creates a playlist entry whose id is
  read back from `playlist/0/id`, and mpv reports that id on
  `MPV_EVENT_END_FILE`/`MPV_EVENT_START_FILE`. The watcher therefore reads events through the
  raw `libmpv2-sys` `mpv_wait_event` (with a direct
  `libmpv2-sys` dependency), because libmpv2's safe `Event::EndFile` keeps only the reason and
  error code and drops the id. Only the newest submission is tracked (`Routing`), and an event for
  any other entry — a playlist mpv expanded the URL into, or a superseded `Play` — simply does not
  match and is ignored, so it can neither clear nor consume a newer `Play`'s routing probe. Only a
  probe that never loaded falls back to the browser; an error that matches no submission belongs to
  no tracked load, so it is logged at `debug` by entry id alone and is never attributed to the URL
  the daemon is now on (see README's "How YouTube playback works"). `FileLoaded` has no id of its
  own, so it is attributed through the `StartFile` that precedes it.
- libmpv disables its own log output by default, so a failed load used to be a silent black
  screen; `Player::new` sets `terminal=yes`/`msg-level=all=warn` so mpv's concrete error line
  (e.g. `[ffmpeg] https: HTTP error 403 Forbidden`, `[ytdl_hook] ... failed`) reaches the daemon's
  stderr/journal. libmpv2's `Error` `Display` is only `Raw(<int>)` (`-16` is
  `MPV_ERROR_NOTHING_TO_PLAY`), so `spawn_async_event_watcher` pairs it with
  `describe_playback_error`'s plain-language reason; keep new async-error paths going through that
  logging rather than adding a second mechanism.
- Web page display (`daemon/src/webpage.rs`) launches Chromium as a *second client of the same Cage
  session*, never as a screenshot loop and never on a second compositor: Cage renders views in map
  order (`cage/view.c` — `view_map` appends the new surface's scene node, `view_unmap` destroys
  it), so the engine sits on top of mpv's window, and taking it down reveals mpv's idle clock
  again — see README's "How webpage (dashboard) display works". `chromium` is therefore a second
  runtime dependency of the packaged binary (`makeWrapper` PATH in `flake.nix`, plus
  `devShells.default`), alongside `yt-dlp`. A `Play` is routed by FCast's `container` MIME field
  (`PlayMessage::explicit_target` in `daemon/src/fcast.rs`), so no protocol change was needed.
- Cage registers `wlr_screencopy_v1` (`cage.c`), so any Cage session can be screen-captured with
  `grim`. `daemon/tests/webpage_display.rs` uses that for real (non-mocked) pixel assertions: it
  starts Cage on wlroots' headless backend (`WLR_BACKENDS=headless` — no GPU, display or X server
  needed) with the real daemon binary as its client, serves a page over loopback, and checks what
  the compositor actually composites. It is `#[ignore]`d (needs `cage`, `chromium`, `grim`) and is
  also run by the package's Nix build (`postCheck`, after the default `checkPhase` has skipped it).
  Knobs: `CASTOFF_E2E_SKIP_MPV_PIXELS` (see two bullets down), `CASTOFF_E2E_BROWSER_FLAGS`
  (test-only Chromium flags, e.g. `--no-sandbox --disable-gpu`), `CASTOFF_BROWSER` (browser
  program; the unit tests point it at stub scripts).
- mpv's `vo=gpu` needs a buffer-sharing path that a compositor only has with a GL renderer. In a
  GL-less environment (the Nix build sandbox: no `/dev/dri`, so wlroots picks the pixman renderer)
  mpv cannot present *and* aborts the whole daemon with an assertion inside its own context probing
  (`vo_x11_init: Assertion '!vo->x11' failed`, still happens with `DISPLAY` unset). The end-to-end
  test therefore runs the daemon with `CASTOFF_MPV_VO=null` there and skips only mpv's pixel
  assertions — Chromium presents over shared memory, so the page pixels are still asserted. Keep
  the `vo` default (`gpu`) for the appliance.
- A `Play` whose sender did not set FCast's `container` MIME type is *decided by the daemon*, not
  by the client: try the media path (mpv) first and hand the URL to the browser engine if mpv
  reports the load failed before it loaded the file (`MPV_EVENT_END_FILE` with an error, which
  libmpv2 surfaces as `Some(Err(_))` from `wait_event`; a *superseded* load ends with reason STOP
  instead, verified against mpv 0.41, so the two are tellable apart in practice). That is what
  keeps YouTube on the video path without any host list or Content-Type probe -- YouTube watch
  URLs are `text/html`, which is exactly the trap -- see README's "How the daemon decides between
  media and a web page" for the measurements and reasoning. `container`
  (`PlayMessage::explicit_target`) remains an explicit override in both directions, and a load
  that started playing is never re-routed: a load is marked as media as soon as mpv reports
  `MPV_EVENT_FILE_LOADED`, so only a load that never loaded at all stays fallback-eligible.
- The daemon's own log goes to **stderr** (`main.rs` uses `tracing_subscriber::fmt()
  .with_writer(std::io::stderr)`), alongside mpv's, Cage's and Chromium's output. It used to go to
  stdout, which the Nix build sandbox does not forward to a client's redirected fd: the e2e test's
  console assertions silently saw only the other processes' stderr, and the daemon's own lines
  were lost there. `start_session_with` now asserts the captured console contains the daemon's
  startup line, so a harness that stops capturing fails loudly instead of weakening every console
  assertion.
- "Playback actually started rendering" is mpv's `PlaybackRestart` event, not the `loadfile` call
  returning (that only queues the load). A third `Mpv` client (`spawn_lifecycle_watcher`) consumes
  it and takes the spinner down; a fresh client must `enable_event` it (`libmpv2`'s
  `mpv_event_id::PlaybackRestart`/`EndFile`). `wait_event` returns `Some(Err(..))` for an `END_FILE`
  with a nonzero error code, and `Ok(Event::EndFile(reason))` otherwise: `EndFileReason::Eof` while
  the spinner is still up is treated as a load that never started and returns to the idle clock,
  while the STOP/REDIRECT a superseding `loadfile` produces are ignored so a rapid re-Play keeps
  its spinner (see `player.rs`'s `handle_lifecycle_event`). With the shipped `keep-open=yes`, a
  normal EOF emits no `END_FILE` at all (mpv pauses at `eof-reached` and the idle clock returns via
  that property watcher), so the EOF arm is defensive; it could not be produced end-to-end in
  tests, so `end_of_file_without_playback_restart_returns_to_idle_clock` drives
  `handle_lifecycle_event` directly with the real event. Each mpv event consumer (idle-screen eof
  watcher on the main handle, the routing/error watcher `spawn_async_event_watcher`, and the
  lifecycle watcher) has its own client/queue to avoid contention.
- Every FCast sender's persistent socket (`handle_connection` in `main.rs`) is split
  (`TcpStream::into_split`) into an owned read half driving the existing per-connection reply loop
  and an owned write half shared (`Arc<tokio::sync::Mutex<OwnedWriteHalf>>`) with that connection's
  push task, so the synchronous command-reply path and the unprompted `PlaybackUpdate` push path
  (`Player::subscribe_status`/`main.rs`'s `push_updates`) can never interleave bytes of two frames
  onto the same wire. `Player`'s broadcast is a `tokio::sync::watch::Sender<PlaybackUpdateMessage>`,
  not `broadcast`, on purpose: every subscriber only ever cares about the *current* status, so
  `watch`'s coalescing-to-latest-value semantics are exactly right and sidestep `broadcast`'s
  slow-subscriber lag/`RecvError::Lagged` entirely -- there is no backpressure concern to design
  around here. Each push task's periodic ~1s tick is gated by a `tokio::select!` arm's `if
  last_state == Playing` guard, so the interval future isn't even polled while idle/paused. That
  tick takes a *fresh* `Player::status()` snapshot (via `spawn_blocking`, like `send_status`),
  never the `watch` value: the watch only changes on a state change, so re-sending it would repeat
  the same `time`/`generationTime` every tick and no progress bar would ever advance.
  `snapshot_status` (in `player.rs`) is the single status shape all publishers use -- the six
  state-changing `Player` methods, the idle-screen `on_change` callback, `handle_lifecycle_event`'s
  `PlaybackRestart` arm, and `fall_back_to_browser`; it takes `&Mpv` + `&WebpageController` rather
  than `&Player` so those non-`Player` contexts can call it. `fall_back_to_browser` must publish
  after `webpage.show` succeeds: that is the only false->true `webpage.is_active()` flip that
  neither an idle-screen callback nor a `Player` method covers, and without it a client would stay
  stuck on the `Idle` the preceding `idle.show` published -- the ~1s tick only starts once a
  subscriber has seen `Playing`, so it would never self-correct. A web page that exits on its own
  *is* covered by the tick (last state was `Playing`) rather than by a publish: `webpage.rs` clears
  its active page from the reaping thread with no callback, so the tick arm itself folds the
  observed state back into `last_state`. That fold is load-bearing -- without it the tick would
  keep firing every second for the life of the connection after such a transition, the standing
  wake loop the power-efficiency principle forbids; `main.rs`'s
  `periodic_push_stops_when_playback_ends` covers the tick stopping.
- The synchronous `PlaybackUpdate` reply to a `Play` command (and any push fired by `play()`'s own
  end-of-method `publish_status()` call) can legitimately still report `state: Idle`: mpv's
  `idle-active` property does not necessarily flip to `false` synchronously within the
  `loadfile`/unpause calls `Player::play` makes -- like the async playback-error/`PlaybackRestart`
  timing documented above, mpv resolves this off that call stack. A real client (or a test
  asserting over the wire, see `main.rs`'s `periodic_push_only_fires_while_playing`) must not
  assume the very first `PlaybackUpdate` after `Play` already reports `Playing`; wait for one that
  does, or rely on the push path's later `PlaybackRestart`-triggered update instead.
- The play queue (`daemon/src/queue.rs`) is castoff's own private FCast extension: opcodes 14-19
  (`RequestQueue`/`QueueState`/`QueueJumpForward`/`QueueJumpBackward`/`ClearQueue`/
  `QueueJumpToIndex`, beyond FCast's reserved 0-13), documented in README's "Queueing (private
  extension)". `Player::play` (`is_idle`, see
  above) enqueues instead of interrupting whenever something is already playing -- including a
  displayed web page, so a `Play` that arrives while one is on screen also only enqueues, superseded
  media or not; superseding what's already showing now needs an explicit `QueueJumpForward` (see
  the unit test `a_second_webpage_play_replaces_the_first_engine` and the e2e test
  `casting_a_webpage_puts_that_page_on_screen_and_stop_returns_to_idle`, which drives the same jump
  over the wire). The queue is persisted as JSON (write-then-rename) to
  `queue::default_state_path()` and reloaded at `Player::build`; `nix/tv-box.nix` gives the
  `cage-tty1` unit a systemd `StateDirectory=castoff` for this. A jump command (not a plain `Play`)
  legitimately produces *two* `QueueState` frames on the wire, not one: `play_jumped_item` publishes
  the new position via the queue-changed watch channel (picked up by `push_updates`) *and*
  `dispatch`'s own handler separately replies with `send_queue_state` -- a test that reads exactly
  one `QueueState` frame per jump command risks consuming a leftover frame from the *previous* jump
  instead of the current one's (see `main.rs`'s `read_queue_state_until`, which loops until the
  frame it wants shows up, rather than trusting a 1:1 command/frame correspondence). `Player::play`,
  `play_jumped_item` and `auto_advance_queue` all commit the queue's new position and broadcast it
  *before* the load is confirmed, since a load failure can surface well after that call returns
  (see the async-error notes above); each carries a `QueueRollback` (the position and what to
  restore it to) through to wherever that load's outcome is actually resolved -- synchronously in
  the same function, or later in `spawn_async_event_watcher`/`fall_back_to_browser` -- so
  `revert_queue_position` can put the position back if it still points at the failed load and
  nothing else has moved it since (`player.rs`'s `queue_position_reverts_after_an_async_load_failure`).

- Chromium's process-singleton socket is created under the engine's **`TMPDIR`**
  (`<TMPDIR>/org.chromium.Chromium.<random>/SingletonSocket`), *not* under `--user-data-dir`:
  verified against the pinned Chromium -- a deep `TMPDIR` aborts with
  `FATAL ... Socket path too long` (exit 133) even with a short profile, while a short `TMPDIR`
  with a deep profile works. The captain's box had a generated nix-shell `TMPDIR` ~120 characters
  deep, so the engine died before painting and the page never appeared. `daemon/src/webpage.rs`
  therefore gives the engine a fixed short `TMPDIR` *and* profile root (`/tmp`,
  `CASTOFF_BROWSER_PROFILE_DIR` to move it), checks both fit Chromium's 107-byte socket limit and
  refuses with a plain-language console error otherwise, and `daemon/tests/webpage_display.rs`
  runs every session's daemon under a deliberately deep `TMPDIR` so the page cases cover this.
  Don't reintroduce `std::env::temp_dir()` for either path.

- **"One window where it's cheap" is the captain's stated direction for where castoff is going
  (2026-09-14) and is not implemented yet.** The cases that are cheap to unify -- dashboards,
  images and static pages -- should render into one daemon-owned window, so the daemon is a normal
  windowed app on a desktop with a window manager (not kiosk-fullscreen-only) and so castoff owns
  its on-screen displays, volume indicators and picture-in-picture consistently, whatever is on
  screen. His reasoning survives: "multiple windows isn't a viable product. That wouldn't match
  what a user would expect." It is explicitly *not* a blanket requirement that everything render
  into one window: video keeps the merged model -- the daemon playing through mpv plus a real
  Chromium client for web pages (`daemon/src/webpage.rs`) -- which on the appliance's own kiosk
  compositor already presents as one fullscreen image.
  The earlier, blanket wording was withdrawn rather than overlooked. The only realistic
  invisible-Chromium mechanism was CEF windowless rendering, and the 2026-09-14 spike measured two
  blockers to a browser-rendered video path: the pinned CEF build cannot decode H.264 or AAC for
  licensing reasons (no proprietary codecs), and its accelerated windowless path delivered zero
  frames on this machine's NVIDIA driver. A video-capable single window would therefore need a
  from-source CEF build with its own pinning and CI, which is not pursued; this note scopes no CEF
  work. The measurements and evidence are in the spike report
  `/ai/firstmate/data/castoff-single-window-architecture/report.md` (with the earlier
  investigation it followed at
  `/ai/firstmate/data/castoff-daemon-webpage-display/single-window-investigation.md`).

- The NixOS VM preview (`nix build .#tv-box-vm` / `nixos-rebuild build-vm --flake .#tv-box`) is
  a *separate* surface from the appliance, and all of its wiring lives in one place:
  `virtualisation.vmVariant` in `nix/tv-box.nix`. The flake's `tv-box-vm` package is
  `nixosConfigurations.tv-box.config.system.build.vm`, which *is* that vmVariant (NixOS's own
  `build-vm` hook, `nixos/modules/virtualisation/build-vm.nix`), so both documented commands build
  the same closure and neither touches the real box. Keep both properties: do not reintroduce a
  second `nixosConfiguration` for the VM, and do not put VM-only settings in the shared part of the
  module. Two VM-only facts that are easy to lose and were expensive to find:
  - **Plymouth blocks the kiosk entirely.** With the VM's default kernel command line (the one the
    qemu-vm module generates), `plymouth-quit.service` never finishes,
    `plymouth-quit-wait.service` ("Hold until boot process finishes up") holds `multi-user.target`,
    and `cage-tty1.service` is ordered `After=plymouth-quit.service` -- so Cage is *never started at
    all* and the screen is a cleared VT with a blinking cursor (exactly the reported symptom;
    reproduced on repeated boots). The same image reaches `graphical.target` in ~11s and starts
    Cage with `plymouth.enable=0`. It is plymouth's *console handover* that hangs, not something
    downstream: pointing `/dev/console` at the serial console instead of tty0 let
    `plymouth-quit` finish. The VM disables plymouth.
  - **Xwayland cannot start without a GPU, and takes Cage down with it.** Cage starts Xwayland
    lazily and sets `DISPLAY=:0` for its client; *two* separate pieces of mpv open that display on
    their own -- its automatic GPU-context probing (Vulkan, then X11, before Wayland) and its
    `hwdec=auto-safe` VDPAU probe, so the failure surfaces both at startup and on the first `Play`.
    Merely connecting wakes Xwayland, which aborts (`Refusing to try glamor on llvmpipe` -> `Fatal
    server error: Couldn't add screen`) and makes wlroots assert on the dead surface
    (`xwayland/xwm.c:592`), killing the session. The VM keeps mpv off X11 with two
    `daemon/src/player.rs` knobs -- `CASTOFF_MPV_GPU_CONTEXT=wayland` and `CASTOFF_MPV_HWDEC=no`
    (unset on the appliance, where auto-detection already reaches Wayland and the hwdec probes find
    real hardware). Do not "fix" this by unsetting `DISPLAY` globally or by disabling Xwayland for
    the appliance: `-vga virtio` alone does not fix it (Xwayland refuses glamor on llvmpipe even
    with a render node present), and `vo=wlshm` is not an alternative -- this mpv/wlroots pair kills
    the Wayland connection on it (`wl_viewport.set_destination sent with invalid values`).
  - The VM needs `-vga virtio` (not qemu's default `std`/bochs-drm, which has *no render node*),
    plus `services.cage.environment.WLR_RENDERER_ALLOW_SOFTWARE=1`: Mesa comes up as llvmpipe,
    which wlroots refuses unless told otherwise. Hardware-accelerated rendering is the one thing
    the VM genuinely cannot do; everything else (kiosk, idle clock, mpv video, Chromium pages) was
    verified to render in the VM by QEMU `screendump` on the built VM, in software.
  - The VM's serial console autologins root (a `serial-getty@ttyS0` override in the vmVariant).
    It must be `overrideStrategy = "asDropin"`: systemd ignores a unit *file* named after a
    template *instance*, so the plain `serviceConfig` form silently leaves the `login:` prompt.
    That console is the VM's promised fallback for a blank screen; see README's VM section for what
    the captain should see (QEMU `Ctrl+Alt+3`) and the host-side FCast port forward
    (`virtualisation.forwardPorts`, 46899, bound on all host interfaces by SLiRP).
  - **`virtualisation.cores` defaults to 1** in nixpkgs' qemu-vm module (`virtualisation.memorySize`
    already defaulted sanely). One core is not enough for this VM's all-software stack (Cage, mpv
    and Chromium all draw through llvmpipe, which itself wants several cores) -- measured on a
    16-core host, boot-to-FCast-ready plus a Chromium cast rendering went from 300+ seconds at 1
    core to ~33 seconds at 4. `nix/tv-box.nix` sets `cores = 4` and `memorySize = 4096` for this.
  - **Resizing the QEMU window does not, by itself, resize what Cage renders**, even though
    `-vga virtio`'s device already supports it end-to-end on the QEMU/kernel side: a window resize
    reaches the guest for free (confirmed with `udevadm monitor --subsystem-match=drm` while
    resizing: it fires a plain `change` uevent on the DRM card, no disconnect/reconnect), and the
    guest kernel's own connector state genuinely tracks it live -- the first line of
    `/sys/class/drm/card*-Virtual-1/modes` is always the most recently requested size, confirmed by
    resizing to several different sizes in a row and re-reading it each time. What doesn't move is
    wlroots: its generic DRM-backend hotplug handler (`scan_drm_connectors` in
    `backend/drm/drm.c`, verified against the pinned wlroots source) only re-probes a connector's
    modes across a connect/disconnect transition, never for a mode-only change while the connector
    stays connected -- and Cage's own output code (`output.c`) only ever calls
    `wlr_output_preferred_mode` once, when the output is first created (`handle_new_output`). No
    choice of virtio-gpu device or QEMU display backend changes this, since the gap is in the
    compositor, not the device -- confirmed by driving a raw RFB `SetDesktopSize` client message at
    the VNC display backend (bypassing any real window entirely) and getting QEMU's own
    "request forwarded" acknowledgement while the guest's rendered framebuffer (verified by size via
    QEMU `screendump`) stayed unchanged. Cage *does* accept an externally-driven mode change,
    though: it implements `wlr-output-management-v1` (`output.c`'s `handle_output_manager_apply`),
    which is exactly what `wlr-randr` speaks, and `wlr-randr --output <name> --custom-mode <W>x<H>`
    was confirmed (via `screendump`) to switch Cage's actual rendered resolution to an arbitrary
    exact size, not just one of the connector's pre-baked EDID modes. `nix/tv-box.nix` wires this
    up for real with a udev rule (`SUBSYSTEM=="drm", ACTION=="change"`) that re-reads the live sysfs
    mode and pushes it through `wlr-randr`, run via `systemd-run --no-block` (so the udev worker
    never blocks on it) as the `kiosk` user via `runuser` (only that user can reach Cage's Wayland
    socket at `/run/user/<uid>/wayland-0`). A resize event that lands before Cage itself is up is a
    harmless no-op (`wlr-randr` fails to connect, the script's `set -eu` + per-step `continue`
    guards just skip it) -- confirmed in the journal (`castoff-vm-follow-resize.service`) during a
    real boot. Entirely VM-only: `wlr-randr` and the udev rule are not part of the appliance's
    runtime closure, since the real box has no virtio-gpu resize event to react to.

- Image uploads (`daemon/src/images.rs`, `daemon/src/upload.rs`) and idle-screen wallpaper
  rotation (`daemon/src/idle_screen.rs`) reuse existing seams rather than adding new machinery --
  see README's "Image uploads (private extension)" for the feature; these are the sharp edges that
  were not obvious going in:
  - `--image-display-duration=inf` (`Player::new`, see its own doc comment for why mpv's 5s default
    is wrong for a queued/wallpaper image) has to be set in **both** `Player::new` and the test
    suite's separate `headless_mpv()` constructor -- they are two independent
    `Mpv::with_initializer` calls, not one shared init path, so a property added to only one is
    silently absent from every test. Caught by `queued_image_is_held_up_indefinitely_not_a_fixed_duration_slideshow`
    initially failing (the test's mpv reached EOF at ~5s and the eof watcher brought the idle clock
    back) until `headless_mpv()` got the same property.
  - The wallpaper rotation's own `loadfile` (a plain background-image swap, not a real `Play`)
    still fires a genuine mpv `PlaybackRestart` event, which `handle_lifecycle_event` reacts to
    unconditionally. This turns out to be harmless without any special-casing: `PlaybackOverlay::reveal`
    early-returns when `is_active()` is false (true only during a real `Play`'s spinner/fade), so a
    wallpaper's restart never runs the loading-overlay fade-out; `mark_restarted()` does still flip
    `PlaybackOverlay::is_restarted`, but `Player::is_idle` only inspects that flag when
    `idle.current()` is `None`, and it stays `Some(Clock)` throughout wallpaper rotation, so this
    never affects whether a real `Play` interrupts vs. queues. Any *new* consumer of
    `PlaybackRestart` should re-check this reasoning rather than assume it's still moot.
  - mpv's `osd-overlay` stacks by recency, not id (see the loading-spinner entry above) -- so once a
    wallpaper image is the video frame, the clock's own opaque background rect (used over the plain
    black backdrop) would otherwise sit on top of and hide it. `IdleScreen::render_over_wallpaper`
    clears that rect outright (`format="none"`, same as `IdleScreenController::hide`) instead of
    drawing it at zero opacity, and draws only the clock text on top.
  - The rotation timer's `loadfile` call takes `Player::operation`'s lock (now threaded into
    `IdleScreenController::new`) around itself and re-checks `IdleScreen` generation/current *after*
    acquiring it, not just before -- the same "operation lock serializes whole play/stop
    operations" discipline `player.rs` already documents, extended to this timer so a real
    `Play`/`Stop` racing the rotation can never be clobbered by a stale wallpaper swap.
- `nixosConfigurations.tv-box-rpi4` / `packages.aarch64-linux.tv-box-rpi4-image` build a flashable
  Raspberry Pi 4 SD image via [`nixos-raspberrypi`](https://github.com/nvmd/nixos-raspberrypi)
  (github:nvmd/nixos-raspberrypi), a separate flake input from this project's main `nixpkgs` pin --
  its hardware modules are validated against its own pinned nixpkgs, so it is deliberately not
  `nixpkgs.follows`-ed. Its `lib.nixosInstaller` (not the plainer `nixosSystem`/`nixosSystemFull`)
  is what makes the result both flashable installer media *and* a ready-to-boot system in one
  image -- it layers that flake's own `sd-image`/`raspberrypi-installer` modules on top, whose
  partition table auto-expands to fill the SD card on first boot, so no separate
  `nixos-anywhere`/`disko` install step is needed for this use case (nixos-raspberrypi also
  supports that combination separately, for installing onto other target disks -- not what this
  project uses). `nixosInstaller` (like its siblings) automatically injects `nixos-raspberrypi`
  itself into every module's `specialArgs`, so `nix/tv-box-rpi4.nix` can just reference it without
  `flake.nix` wiring that by hand. `config.system.build.sdImage`'s output is the compressed image
  file itself (`<name>.img.zst`), not a directory -- `nix build`'s `./result` symlink points
  straight at it, decompress with `zstd -d` before `dd`.
  `nix/tv-box.nix` (the shared kiosk config: Cage/`kiosk` user, the daemon service, audio,
  firewall, avahi, power trimming) is target-independent; `nix/tv-box-x86_64.nix` and
  `nix/tv-box-rpi4.nix` are the two per-target hardware/filesystem/bootloader layers on top of it
  (generic x86_64 placeholders for `tv-box`/`tv-box-vm`, real Pi 4 hardware modules for
  `tv-box-rpi4`), assembled per-target in `flake.nix`.
  This sandbox has no `aarch64-linux` builder and no `aarch64-linux` QEMU user-mode emulation
  configured (no `boot.binfmt.emulatedSystems`, nothing under `/proc/sys/fs/binfmt_misc`) -- a
  direct, non-dry-run `nix build` of any `aarch64-linux` output fails with a genuine `error:
  Cannot build ... Reason: platform mismatch, Required system: 'aarch64-linux', Current system:
  'x86_64-linux'`. `nix flake check --all-systems` and `nix build --dry-run` still fully evaluate
  every `aarch64-linux` output (including `nixosConfigurations.tv-box-rpi4`, which `nix flake
  check` evaluates even without `--all-systems`, since NixOS-configuration evaluation isn't
  system-gated the way `packages`/`checks` realization is) and resolve the whole build closure with
  no errors -- `nix flake check --all-systems` reports "all checks passed!" for `aarch64-linux`
  outputs on this basis alone, without ever realizing them (confirmed via `nix path-info` on the
  resulting store path: not valid, i.e. never built). That distinction matters for honestly
  reporting what was and wasn't actually verified here; a real Raspberry Pi 4 build/boot/playback
  test needs real hardware or a genuine `aarch64-linux` builder.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
