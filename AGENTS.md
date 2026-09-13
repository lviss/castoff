# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.
- Layout, build/run/test commands, protocol details, and scope (what's implemented vs. planned)
  are documented in `README.md` — read that first, it's the source of truth, not this file.
- `flake.nix` pins `nixpkgs` to `nixos-25.11` deliberately: `libmpv2` requires Rust's
  `edition2024`, which needs Cargo/rustc >= 1.85; `nixos-24.11`'s toolchain is too old and fails
  with "feature `edition2024` is required".
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
  success/failure plus real state transitions (e.g. `eof-reached`), not pixels.
- YouTube playback needs no daemon-side code: mpv's built-in `ytdl_hook` Lua script (same core in
  both CLI mpv and `libmpv2`) auto-detects non-direct-media URLs and shells out to `yt-dlp` on
  `PATH`, unconditionally, with no libmpv init tweaks required — see README's "How YouTube
  playback works". `yt-dlp` is a runtime-only dependency, wired into `flake.nix` via
  `makeWrapper`/`wrapProgram` rather than `buildInputs`, since it's invoked as a subprocess, not
  linked. Its one real (non-mocked) test in `daemon/src/player.rs`
  (`real_youtube_url_resolves_and_plays_via_ytdl_hook`) needs network + `yt-dlp` on `PATH`, so it's
  `#[ignore]`d — the `nix build`/`nix flake check` sandbox has no network. Run it manually with
  `nix shell nixpkgs#yt-dlp -c cargo test -- --ignored` (from `nix develop`, or with cargo/rustc
  otherwise on `PATH`).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
