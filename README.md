# castoff

An open-source, appliance-like Chromecast/Roku alternative for a boat: a TV-connected box that
boots straight into a fullscreen player, eventually paired with a native Android control app
(including receiving Android "Share" intents). This repo currently contains the first working
scaffold: the TV-box daemon and its NixOS packaging. Nothing else exists yet -- see
[Not yet implemented](#not-yet-implemented-follow-up-work) below.

## What works today

Right now the only supported playback source is a direct media URL: send the daemon an FCast
`Play` command with a remote (`http(s)://`) or local (`file://`) URL to a media file -- e.g. an
mp4 -- and it loads and plays that file via mpv. That's it: no YouTube, no Jellyfin, no images, no
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
nix develop   # cargo, rustc, rust-analyzer, clippy, with mpv already wired up for linking
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
  uses hardware acceleration when available; and `stop`/idle leaves mpv's decode pipeline
  dormant rather than rendering.
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

- Playback sources: YouTube (via `yt-dlp`, no Google login for public videos), Jellyfin
  (authenticated via Jellyfin's Quick Connect flow -- never a typed password), images from
  Immich or a local folder, and casting/displaying an arbitrary webpage (e.g. a Grafana
  dashboard).
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
