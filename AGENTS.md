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
  so the concurrent-`play` test stays fast; every error path in `play`/`stop` calls
  `abort_loading_to_idle` so a partial setup can't leave the opaque fade covering the screen.
  `CASTOFF_ANIMATION_SLOWDOWN` (read once in `PlaybackOverlay::new`, parsed by `parse_slowdown`)
  multiplies both step durations for manual inspection (measured 30.3/s -> 3.03/s at 10x);
  unset/unusable -> 1 (shipping), clamped at
  1000, and it never makes an animation always-on.
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
- Async mpv playback errors (e.g. `ytdl_hook`/`yt-dlp` failing to resolve a URL) don't surface from
  `Player::play`'s `loadfile` call — that only queues the load; mpv resolves/opens it later, off
  that call stack. `player.rs`'s `spawn_error_logger` catches these via a second `Mpv` client
  handle (`Mpv::create_client`) dedicated to blocking on `wait_event(-1.0)`, logging any `Err` at
  `error!` with the most recently submitted url (attribution is best-effort — see README's "How
  YouTube playback works") — event-driven, not a polling loop.
- libmpv disables its own log output by default, so a failed load used to be a silent black
  screen; `Player::new` sets `terminal=yes`/`msg-level=all=warn` so mpv's concrete error line
  (e.g. `[ffmpeg] https: HTTP error 403 Forbidden`, `[ytdl_hook] ... failed`) reaches the daemon's
  stderr/journal. libmpv2's `Error` `Display` is only `Raw(<int>)` (`-16` is
  `MPV_ERROR_NOTHING_TO_PLAY`), so `spawn_error_logger` pairs it with
  `describe_playback_error`'s plain-language reason; keep new async-error paths going through that
  logging rather than adding a second mechanism.
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
  watcher on the main handle, error logger, lifecycle watcher) has its own client/queue to avoid
  contention.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
