# castoff

An open-source, appliance-like Chromecast/Roku alternative for a boat: a TV-connected box that
boots straight into a fullscreen player, eventually paired with a native Android control app
(including receiving Android "Share" intents). This repo currently contains the first working
scaffold: the TV-box daemon and its NixOS packaging. Nothing else exists yet -- see
[Not yet implemented](#not-yet-implemented-follow-up-work) below.

## What works today

Right now the supported playback sources are a direct media URL and YouTube: send the daemon an
FCast `Play` command with a remote (`http(s)://`) or local (`file://`) URL to a media file -- e.g.
an mp4 -- or a `youtube.com`/`youtu.be` watch URL, and it loads and plays it via mpv (see
[How YouTube playback works](#how-youtube-playback-works)). That's it: no Jellyfin, no images, no
casting a webpage. See [Not yet implemented](#not-yet-implemented-follow-up-work) below for what's
planned but not built.

## What's here

- **`flake.nix` / `nix/tv-box.nix`** -- a NixOS configuration (`nixosConfigurations.tv-box`, for
  generic `x86_64-linux`) that boots directly into [Cage](https://github.com/cage-kiosk/cage) --
  a wlroots-based Wayland compositor that shows exactly one fullscreen client and nothing else, no
  desktop environment or display manager -- running the castoff daemon as that one client.
- **`daemon/`** -- a Rust daemon (`castoff-daemon`) that embeds mpv via
  [libmpv2](https://crates.io/crates/libmpv2) and exposes a local control API modeled on the
  [FCast](https://fcast.org/) protocol.

### Why Rust

Rust was chosen over Go (the other candidate) mainly for the mpv binding: `libmpv2` links
directly against `libmpv.so` with no runtime beyond libc, which packages cleanly and
predictably with `pkgs.rustPlatform.buildRustPackage` + `pkgs.mpv-unwrapped` -- Nix just needs
`mpv-unwrapped` in `buildInputs` and the linker finds `-lmpv` on its own, no FFI/cgo wrapper
layer or extra libc shims. Rust also gives an async runtime (`tokio`) with no GC pauses and a
small static-ish footprint, which suits a box that is meant to idle for long stretches on battery
power.

### Why FCast, and what's implemented

Rather than invent a bespoke local control protocol, the daemon speaks a subset of
[FCast](https://fcast.org/) (protocol v2, see [docs.fcast.org/protocol/v2](https://docs.fcast.org/protocol/v2)
and the reference implementation at [github.com/futo-org/fcast](https://github.com/futo-org/fcast)):
an open, non-Google-controlled casting protocol built for exactly this problem (see
[TeamNewPipe/NewPipe#11403](https://github.com/TeamNewPipe/NewPipe/issues/11403) for the
motivating context).

Wire format: a TCP connection on port `46899`, each message framed as a 4-byte little-endian
length prefix + a 1-byte opcode + an optional UTF-8 JSON body (`length` = 1 + body size, max
32 KiB). See [`daemon/src/fcast.rs`](daemon/src/fcast.rs) for the exact structs.

Implemented opcodes (all of FCast v2's playback-control surface):

| Opcode | Direction | Daemon behavior |
| --- | --- | --- |
| `Play` (1) | sender -> receiver | `loadfile` the given `url` into mpv (inline `content`, e.g. a DASH manifest, is rejected -- not yet supported); replies with `PlaybackUpdate` |
| `Pause` (2) / `Resume` (3) | sender -> receiver | toggles mpv's `pause` property; replies with `PlaybackUpdate` |
| `Stop` (4) | sender -> receiver | stops playback, mpv returns to idle; replies with `PlaybackUpdate` |
| `Seek` (5) | sender -> receiver | absolute seek; replies with `PlaybackUpdate` |
| `SetVolume` (8) | sender -> receiver | sets mpv volume (FCast's 0.0-1.0 scale, mapped to mpv's 0-100); replies with `VolumeUpdate` |
| `SetSpeed` (10) | sender -> receiver | sets mpv playback speed; replies with `PlaybackUpdate` |
| `Version` (11) | bidirectional | replies with the protocol version this daemon speaks (`2`) |
| `Ping` (12) | bidirectional | replies `Pong` |

`PlaybackUpdate`/`VolumeUpdate` are sent as an immediate reply to a command, not on a polling
timer -- see [Design principles](#design-principles).

### How YouTube playback works

A `Play` message's `url` can be a `youtube.com`/`youtu.be` watch URL, not just a direct media
URL -- no new opcode or protocol change, and no bespoke YouTube API integration or Cast-protocol
emulation. This works because mpv (and therefore `libmpv2`, since it's the same core) ships a
built-in `ytdl_hook` Lua script that automatically shells out to
[`yt-dlp`](https://github.com/yt-dlp/yt-dlp) to resolve a direct, playable stream URL whenever it's
given a URL it doesn't recognize as directly playable media. This is unconditional: no
daemon-side code detects YouTube URLs, spawns `yt-dlp`, or parses its output -- `Player::play`
(`daemon/src/player.rs`) just hands `url` to mpv's `loadfile` exactly as it already did for a
direct remote mp4, and mpv/`ytdl_hook` do the rest, as verified by a real (non-mocked) test against
a real public YouTube URL (`real_youtube_url_resolves_and_plays_via_ytdl_hook`, gated `#[ignore]`
since it needs network access and `yt-dlp` on `PATH` -- see that test's doc comment to run it).
No Google login is needed for public videos, matching `yt-dlp`'s own no-auth-required default for
public content. `yt-dlp` is declared as a runtime dependency of the `castoff-daemon` Nix package
(`flake.nix`): the built binary is wrapped (`makeWrapper`) to prepend `yt-dlp`'s Nix store path to
`PATH`, so this works regardless of the caller's environment (e.g. the `cage` kiosk session, which
execs the binary directly with no shell) -- not merely assumed present on some machine's `PATH`.
`devShells.default` also lists `yt-dlp` directly (see [Dev shell](#dev-shell)): `inputsFrom` alone
doesn't carry over that wrapping, so a plain `cargo build`/`cargo run` in `nix develop` would
otherwise silently lack `yt-dlp` on `PATH`.

`yt-dlp` is deliberately taken from the flake's `nixpkgs-unstable` input rather than the
`nixos-25.11` pin used for everything else: the stable pin's yt-dlp (2026.06.09) auto-selects
YouTube's `android_vr` player client for some videos and the CDN then answers the resolved stream
URL with HTTP 403 (mpv reports "nothing to play"), while unstable's (2026.08.19) selects the
working `visionos` client. Everything else -- mpv/`libmpv2` and the Rust toolchain -- stays on
`nixos-25.11`.

If playback still fails (private or removed video, an extractor regression, a CDN rejection), the
daemon does not fail silently: it turns on mpv's own `terminal` logging (`msg-level=all=warn`), so
mpv's concrete error line (e.g. `[ffmpeg] https: HTTP error 403 Forbidden` or
`[ytdl_hook] youtube-dl failed: ...`) is written to the daemon's stderr/journal, and the
async-error listener (`daemon/src/player.rs`) logs the failing URL together with a
plain-language reason (libmpv2's own error display is only `Raw(<int>)`).

## Building and running

### Build the daemon on its own

```sh
nix build .#castoff-daemon
./result/bin/castoff-daemon        # listens on 0.0.0.0:46899 (override with CASTOFF_PORT)
```

This works on any machine with Nix (no NixOS, no TV hardware needed) since it only needs
`libmpv.so` at link/run time. mpv opens its own window via whatever video output it can find
(Wayland/X11 if a display is available; it starts fine headless too, though playback obviously
needs a display to show anything).

### Manual protocol test

With the daemon running, send raw FCast frames, e.g. a `Ping` (opcode `12`, no body -> length 1):

```sh
printf '\x01\x00\x00\x00\x0c' | socat - TCP:127.0.0.1:46899 | xxd
# -> 01 00 00 00 0d   (length=1, opcode=13 = Pong)
```

or a `SetVolume`:

```sh
perl -e '
  my $body = q({"volume":0.5});
  print pack("V", length($body) + 1), chr(8), $body;
' | socat - TCP:127.0.0.1:46899 | xxd
# -> VolumeUpdate reply: {"generationTime":...,"volume":0.5}
```

### Whole-system checks

```sh
nix flake check   # builds and tests the daemon package (via `checks`), and evaluates
                   # the tv-box and tv-box-vm NixOS configurations and the dev shell
```

### NixOS VM, no TV hardware required

Two equivalent ways to boot the whole appliance (Cage + castoff-daemon as PID-1-adjacent kiosk
session) in a throwaway VM:

```sh
# Either:
nixos-rebuild build-vm --flake .#tv-box
./result/bin/run-*-vm

# Or, without nixos-rebuild (e.g. in a sandbox/CI with no nixos-rebuild):
nix build .#tv-box-vm
./result/bin/run-*-vm
```

### Dev shell

```sh
nix develop   # cargo, rustc, rust-analyzer, clippy, yt-dlp, with mpv already wired up for linking
cd daemon && cargo build && cargo clippy
```

## Design principles

These are project-wide, captain-stated non-functional requirements. Most don't fully apply to
this scaffold yet, but they should carry forward into every later task on this codebase:

- **Power efficiency.** This device runs on boat power (a battery bank, not shore mains), so the
  daemon must avoid busy-polling/wake loops, idle CPU spin, or keeping the display/decode
  pipeline active when nothing is playing. Concretely so far: the FCast TCP server is
  event-driven (`tokio`, no polling loop); `PlaybackUpdate`/`VolumeUpdate` replies are sent only
  in response to a command, never on a timer; mpv is configured with `hwdec=auto-safe` so decode
  uses hardware acceleration when available; and `stop`/idle leaves mpv's *decode* pipeline
  dormant rather than rendering a video. Idle is no longer fully dark, though: whenever there's
  no active playback (at startup, after `Stop`, or after a clip reaches end-of-file with nothing
  queued next -- see [`daemon/src/idle_screen.rs`](daemon/src/idle_screen.rs)), the daemon shows
  an on-screen clock via mpv's own OSD instead of a black screen, redrawn on a ~1s
  `std::thread::sleep` timer rather than a busy loop or a second rendering stack. `IdleScreen` is
  a small seam (`Clock` is the only variant today) meant to grow a static-wallpaper or
  cast-a-webpage variant later without restructuring.
- **Data efficiency.** Avoid needless re-fetching over the network. This isn't exercised by the
  scaffold (there's no Immich integration yet), but it constrains that future work: when the
  Immich slideshow integration is built, it must cache each displayed image locally and only
  re-fetch an image if it isn't already cached or the source has changed -- never re-download an
  image on every pass through an album.
- **Selectable stream quality (long-term, not required for v1).** A future goal is letting the
  user pick a lower streaming quality/bitrate to save data and power. Not implemented now, but
  the `Play` message (`daemon/src/fcast.rs`) already has room to grow: a `quality`/`bitrate`
  field can be added there later as an additional optional field without breaking the existing
  `url`/`time`/`volume`/`speed` fields or requiring a protocol version bump.

## Not yet implemented (follow-up work)

Out of scope for this scaffold, deliberately:

- Playback sources: Jellyfin (authenticated via Jellyfin's Quick Connect flow -- never a typed
  password), images from Immich or a local folder, and casting/displaying an arbitrary webpage
  (e.g. a Grafana dashboard). (YouTube is implemented -- see
  [How YouTube playback works](#how-youtube-playback-works).)
- The native Android control app, including handling Android `Share` intents.
- Appliance disk-image generation (e.g. via `nixos-generators`/`disko`) for a flashable image;
  today's `tv-box` configuration needs a real `fileSystems."/"` and bootloader target to install
  to actual hardware (the flake ships placeholder values for `nix flake check`/VM use).
- Any authentication/credential flow.
- Selectable stream quality (see [Design principles](#design-principles) above).

The daemon and its FCast-based control API are meant to be generic enough that all of the above
can be layered on later without a rewrite.

## Credits / prior art

- [matthewbauer/nixos-kiosk](https://github.com/matthewbauer/nixos-kiosk) and
  [matthewbauer/nixiosk](https://github.com/matthewbauer/nixiosk) -- the NixOS + Cage kiosk
  pattern this repo's `nix/tv-box.nix` is adapted from (dedicated kiosk user, avahi
  advertisement, trimmed-down desktop services), updated to use nixpkgs' upstream
  `services.cage` module.
- [FCast](https://fcast.org/) / [futo-org/fcast](https://github.com/futo-org/fcast) -- the local
  control protocol this daemon implements a subset of.
