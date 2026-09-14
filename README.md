# castoff

An open-source, appliance-like Chromecast/Roku alternative for a boat: a TV-connected box that
boots straight into a fullscreen player, eventually paired with a native Android control app
(including receiving Android "Share" intents). This repo currently contains the first working
scaffold: the TV-box daemon and its NixOS packaging. Nothing else exists yet -- see
[Not yet implemented](#not-yet-implemented-follow-up-work) below.

## What works today

Right now the supported playback sources are a direct media URL, YouTube, and a web page. Send
the daemon an FCast `Play` command with a URL, and it works out what to do with it:

- a media URL -- a remote (`http(s)://`) or local (`file://`) file, or a `youtube.com`/`youtu.be`
  watch URL -- is loaded and played via mpv (see
  [How YouTube playback works](#how-youtube-playback-works));
- a web page URL is displayed fullscreen in a real browser engine (Chromium) running as a second
  client of the same Cage session (see
  [How webpage (dashboard) display works](#how-webpage-dashboard-display-works)).

The daemon decides between those two itself -- it tries media first and hands the URL to the
browser if that fails -- so a client only needs to know the URL. A sender that wants to be certain
can still say so with FCast's `container` MIME type (`text/html` forces the browser, any other
value forces mpv): see
[How the daemon decides between media and a web page](#how-the-daemon-decides-between-media-and-a-web-page).
A `Play` that is still loading shows an on-screen spinner, and starts/stops are wrapped in a
short fade (see
[On-screen feedback](#on-screen-feedback-loading-indicator-and-startstop-fade)).

That's it: no Jellyfin, no images. See [Not yet implemented](#not-yet-implemented-follow-up-work)
below for what's planned but not built.

## What's here

- **`flake.nix` / `nix/tv-box.nix`** -- a NixOS configuration (`nixosConfigurations.tv-box`, for
  generic `x86_64-linux`) that boots directly into [Cage](https://github.com/cage-kiosk/cage) --
  a wlroots-based Wayland compositor that shows exactly one fullscreen client and nothing else, no
  desktop environment or display manager -- running the castoff daemon as that one client. Cage
  stacks a second, newer client on top when there is one, which is how a cast web page takes the
  screen; see [How webpage (dashboard) display works](#how-webpage-dashboard-display-works).
- **`daemon/`** -- a Rust daemon (`castoff-daemon`) that embeds mpv via
  [libmpv2](https://crates.io/crates/libmpv2), spawns Chromium (packaged alongside it, see
  [Building and running](#building-and-running)) for web pages, and exposes a local control API
  modeled on the [FCast](https://fcast.org/) protocol.

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
| `Play` (1) | sender -> receiver | plays the `url` as media in mpv, or displays it as a web page in the browser engine; the daemon decides which unless the sender's `container` MIME type says so explicitly (see [How the daemon decides between media and a web page](#how-the-daemon-decides-between-media-and-a-web-page)). Inline `content` (e.g. a DASH manifest) is rejected -- not yet supported; replies with `PlaybackUpdate` |
| `Pause` (2) / `Resume` (3) | sender -> receiver | toggles mpv's `pause` property; replies with `PlaybackUpdate` |
| `Stop` (4) | sender -> receiver | stops playback, taking a displayed web page down; mpv returns to idle; replies with `PlaybackUpdate` |
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
async-error listener (`daemon/src/player.rs`) logs the URL of the failed load together with a
plain-language reason (libmpv2's own error display is only `Raw(<int>)`). Each `loadfile`
submission is tracked in submission order, so an error is attributed to its own load: a stale
event from a superseded Play names its own URL, never a newer request's. Only an error with no
submitted load outstanding at all (mpv's own idle state) falls back to logging the most recently
submitted URL, where attribution stays best-effort.

### How the daemon decides between media and a web page

A client only has to know a URL; the daemon decides what it is. The rules, in order
(`Player::play` in `daemon/src/player.rs`):

1. **The sender said so.** FCast's `container` MIME type is an explicit override:
   `text/html` or `application/xhtml+xml` (case-insensitive, MIME parameters ignored) means the
   browser engine; any other MIME type means mpv. Nothing is guessed and nothing falls
   back -- a sender that classifies its URL keeps control either way (a `video/mp4` URL that
   fails is an error, not a page).
2. **Otherwise the daemon tries media first, and falls back to the web page.** The URL goes to
   mpv, which is what makes YouTube, every other yt-dlp-supported source, and plain media work
   with no client help. If mpv reports the load failed *before it loaded the file* (an HTTP
   error, an unsupported format -- which is what an ordinary HTML page is to it), the same URL is
   handed to the browser engine instead, and the console says so at `warn`. Once mpv reports the
   file loaded, that URL is media: a failure later -- even before the first frame -- is a playback
   error, never re-routed.

What that costs the viewer, measured on the development box (`daemon/tests/webpage_display.rs`
prints these numbers on every run):

| Cast | Time to content |
| --- | --- |
| A local web page | ~2.0-2.4s for the media attempt to fail, ~3.0-3.4s until the page's pixels are on screen |
| An ordinary public web page (`example.com`) | the same media-first path, the same cost |
| A local media file over HTTP | ~1s to playback |
| A YouTube watch URL | ~3s to playback (`yt-dlp` resolution), unaffected by the fallback |

If the browser engine cannot be started either (no browser installed, an unsupported URL scheme),
the daemon logs an error naming both failures and the screen goes back to the idle clock -- never
a silent black screen and never an endless spinner. `CASTOFF_BROWSER` chooses the browser program
(the packaged daemon puts `chromium` on `PATH`).

Why media-first, rather than probing the response's Content-Type or listing media extensions:
YouTube watch URLs *are* `text/html`, so a probe misroutes every one of them unless a yt-dlp host
list is maintained alongside it; an extension list misses streams that have no extension; and both
need an HTTP client inside the daemon with its own failure modes (redirects, servers without
`HEAD`, bot-blocking). Asking the real player "can this be played?" answers the actual question on
exactly the code path that will play it, and costs nothing at all for media. The price is the
media attempt's failure time before a page appears, which is the trade for a routing decision no
client has to make.

### How webpage (dashboard) display works

A `Play` that the daemon routes to the browser -- either because the sender set `container` to a
web MIME type, or because the media attempt failed (see
[the routing rules above](#how-the-daemon-decides-between-media-and-a-web-page)) -- puts the page
on the TV. The `url` may be `http://`, `https://` or `file://`; anything else is refused before a
process is started. (For reference, `container` is FCast's own MIME-type field --
[docs.fcast.org/protocol/v2](https://docs.fcast.org/protocol/v2) documents it as "The MIME type
(video/mp4)" -- so this fits the existing message shape exactly: no new opcode, no new field and
no protocol version bump.)

The page is rendered by a **real browser engine** -- Chromium, a runtime dependency of the daemon
-- running fullscreen as a **second client of the same Cage session**. The alternatives were
considered and rejected on purpose:

- **Embedding an engine in the daemon.** mpv cannot render HTML, and there is no maintained Rust
  embedding for WebKit/WPE/CEF/Servo that fits this box; every one of them would still need its
  own GL context and composed surface *inside* the daemon, plus an in-process handoff between two
  renderers. Running the engine as an ordinary Wayland client does the same work through the
  compositor, with the engine's full feature set -- which is what later authenticated
  Jellyfin/Netflix/Grafana pages will need.
- **Screenshotting the page into mpv.** No refresh cadence can make a screenshot a live page, and
  it would have to re-fetch and re-render everything on a timer.
- **Nesting a second compositor under Cage.** That is a second display stack to configure, update
  and keep alive on battery power, doing work Cage already does.

The handoff rides on the compositor's own view ordering: Cage stacks views in the order they are
mapped (`cage/view.c` appends each new surface's scene node; `view_unmap` destroys it), so the
browser simply maps over mpv's window, and taking it down reveals whatever mpv was showing. The
daemon keeps mpv on its idle screen (`daemon/src/idle_screen.rs`) the whole time a page is
displayed -- playback stopped, decode pipeline dormant -- so the screen Cage reveals again is
already the clock. No compositor switching, no window juggling of ours, no second display stack.

The daemon supervises the engine (`daemon/src/webpage.rs`):

- `Stop`, a new media `Play`, or a new page `Play` terminates the engine's whole process group
  (SIGTERM, escalating to SIGKILL) and waits for it to be gone, so nothing keeps painting or
  decoding off-screen. Because every page shares one persistent browser profile -- what later
  authenticated pages need -- a replacement engine is spawned only after the previous one has
  actually exited, so swapping pages has a brief idle gap by design.
- If the engine exits by itself (a crash), the daemon notices without any incoming command, logs
  it at error level, and the screen is back to the idle clock that was behind it all along.
- A `Play` that cannot be started is reported to the sender as a `PlaybackError`. A bad `url` (an
  unsupported scheme, or none at all) is refused before anything changes, so the current page
  stays up; a missing browser is only discovered at spawn time, after the incumbent has been
  taken down, so that failure leaves the idle clock on screen rather than a stale page pretending
  to be the new one.
- `PlaybackUpdate` reports `Playing` while a page is displayed: mpv itself is idle (it is only
  carrying the clock behind the page), but the content the sender asked for is live. A page has no
  mpv timeline, so `time`/`duration` are absent. `Pause`/`Resume`/`Seek`/`SetSpeed` do not apply to
  a page -- interactive control is out of scope for v1.

The daemon never re-fetches or re-renders the page on a timer: the engine keeps it live, and any
refresh cadence belongs to the page itself (Grafana's auto-refresh, say), which is both the
data-efficient and the correct behavior. The engine's profile lives under the system temp
directory (tmpfs on the appliance), so browsing state never spins up the disk; while a page is up,
mpv's decode pipeline is stopped, so nothing plays or decodes behind it.

Not supported: pages behind a login (there is no credential flow yet -- see
[Not yet implemented](#not-yet-implemented-follow-up-work)), DRM-protected video, and any
interaction -- no clicking, typing, scrolling or remote control. Chromium is launched as a kiosk
(`--kiosk --app=<url>`): no tabs, no omnibox, no window controls. As everywhere else in this
daemon, an async engine failure is never silent -- Chromium's own diagnostics go to the daemon's
stderr/journal, the same way mpv's do.

### On-screen feedback: loading indicator and start/stop fade

A `Play` that has been accepted but has not started rendering yet does not leave the screen
unchanged: the daemon fades whatever is on screen to black and shows a rotating spinner over it,
so a slow network or a cold `yt-dlp` resolution reads as "working on it" instead of "nothing
happened". Both are drawn through mpv's own OSD (`osd-overlay` ASS events) -- the same surface as
the idle clock, in `daemon/src/overlay.rs`, not a compositor or a second window. The spinner goes
up *before* `loadfile` is submitted and comes down the moment mpv reports `PlaybackRestart` (the
first frame is actually rendering), not merely when the load was queued. It redraws at ~30
frames/s (12 degrees per frame, one revolution per ~1s) and stops completely as soon as the load
ends, errors, or is superseded. If the load fails, the spinner is torn down and the idle clock
returns; the failure itself is still reported on the console by the error logger described above,
never hidden behind the spinner.

**What the fade covers.** A transition is two ~400ms fades through black (20 opacity steps at
20ms each), so each direction is long enough to read on a TV as a fade rather than a cut:

- **Play.** (1) The old content -- the idle clock, or a previous video -- fades to black. (2) The
  spinner appears on the black and rotates until mpv reports the first frame is rendering. (3) The
  spinner is removed and the black fades away to the new video.
- **Stop.** (1) The video fades to black. (2) The idle clock fades in over that black while the
  black backdrop stays up (so a not-yet-cleared video frame can't flash through). (3) The backdrop
  is dropped once the clock's own opaque background covers the canvas. A `Stop` that arrives after
  a clip already ended on its own is a no-op instead: the eof watcher has already restored the
  clock, so there is nothing to fade and the screen does not blink.

So a fade runs on *both* boundaries of a Play (old content to black, black to new video) and
*both* boundaries of a Stop (video to black, black to idle clock). The spinner does not mask
either fade: it is drawn only after the fade to black finishes and is cleared before the fade
away begins.

**Testing affordance: slowing the animations.** Set
`CASTOFF_ANIMATION_SLOWDOWN=<factor>` (e.g. `10`) in the daemon's environment to scale *both*
the fade's per-step duration and the spinner's step interval by that factor, so the sequence can
be watched frame by frame. It is read once at startup, applies to both animations (so a slowed
run shows the whole sequence in proportion, rather than a slow fade next to a full-speed
spinner), and changes only the duration of an already-bounded animation: the spinner still stops
redrawing the instant a load ends, errors, or is superseded. Unset, empty, malformed,
non-finite, or below `1` all mean `1` (the shipping behavior), and large values are clamped, so a
typo can't stretch a transition indefinitely. This is an operator/testing affordance, not a user
setting, so it is deliberately an environment variable rather than a control-protocol option.

Per-frame cost: a spinner frame is a single `osd-overlay` command, measured at ~11µs headless
(`vo=null`), i.e. ~0.03% of one core at 30 redraws/s just to submit the frame -- trivial next to
video decode, and it stops entirely once the load ends. The opaque fade rect underneath is not
re-issued while the spinner turns, since it does not change.

A fade through black was chosen over a crossfade between the new and old content because it is
one mechanism that covers every combination -- idle clock to video and video to video, in both
directions -- whereas a true crossfade would require compositing two decode/render pipelines at
once, i.e. the second rendering stack and extra power draw the design principles rule out. Both
the spinner and the fades are bounded animations that stop redrawing once settled; see
[Design principles](#design-principles).

Measured in the `.md`-documented Xvfb capture harness: the spinner redraw loop runs at 30.3
redraws/s (one `osd-overlay` command every 33ms, 12 degrees each, so one revolution per second),
and setting `CASTOFF_ANIMATION_SLOWDOWN=10` drops it to 3.03/s for a ~10s revolution -- exactly
one tenth, applied to both animations. `mpv`'s own present rate under the software renderer used
for the capture (a few frames/s) coalesces those redraws, so a real GPU presents every step;
what is measured here is the daemon's redraw cadence, which is what the animation controls.

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

The packaged binary is wrapped so the two runtime helpers it shells out to are on its `PATH`
regardless of the caller's environment: `yt-dlp` (for YouTube URLs) and `chromium` (for web
pages). Web pages additionally need a Wayland session, because the engine is launched with
`--ozone-platform=wayland` -- running the daemon as Cage's client, as the appliance does, provides
that; a bare desktop session where `WAYLAND_DISPLAY` is set does as well.

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

or a web page (opcode `1`). No `container` is needed -- the daemon tries media first and falls
back to the browser engine, see
[How the daemon decides between media and a web page](#how-the-daemon-decides-between-media-and-a-web-page)
-- but a sender that already knows can say so explicitly:

```sh
perl -e '
  my $body = q({"url":"http://127.0.0.1:8000/dashboard.html"});
  print pack("V", length($body) + 1), chr(1), $body;
' | socat - TCP:127.0.0.1:46899 | xxd
# -> PlaybackUpdate reply: {"state":1,...}  (Playing while the page is displayed)

# ... or, with the explicit MIME type that skips the media attempt entirely:
perl -e '
  my $body = q({"container":"text/html","url":"http://127.0.0.1:8000/dashboard.html"});
  print pack("V", length($body) + 1), chr(1), $body;
' | socat - TCP:127.0.0.1:46899 | xxd
```

### Whole-system checks

```sh
nix flake check   # builds and tests the daemon package (via `checks`), and evaluates
                   # the tv-box and tv-box-vm NixOS configurations and the dev shell
```

The package's tests include the end-to-end webpage/routing test (see
[How webpage (dashboard) display works](#how-webpage-dashboard-display-works)): it starts a real
headless Cage session with the real Chromium engine and asserts on compositor pixels. The Nix
build sandbox has no GPU, where mpv has no way to present frames, and no network; that run
therefore skips mpv's own pixel assertions (`CASTOFF_E2E_SKIP_MPV_PIXELS=1`) and the two cases
that need a public URL (`example.com`, YouTube), saying so in its output, while still asserting
the locally served page's pixels.

### End-to-end webpage/routing test

The tests in `daemon/tests/webpage_display.rs` are `#[ignore]`d because they need a compositor and
a browser, which few environments have (`nix flake check` runs them anyway, via the Nix
`checkPhase`). On a machine with a GPU context for mpv -- the dev shell lists `cage`, `chromium`
and `grim` for exactly this -- run the full-strength version manually:

```sh
nix develop -c cargo test -p castoff-daemon --test webpage_display -- --ignored --nocapture
```

They serve real pages and a real video file over loopback, cast them with real FCast `Play`
frames, and capture what the compositor actually composites (`wlr-screencopy`, via `grim`) to
assert: the daemon's own routing (an unclassified page is fetched by the browser after the media
attempt fails; an unclassified media file stays with mpv; a YouTube URL plays as video rather
than being mistaken for a page; `example.com` displays in the browser), the page/media/Stop
lifecycle, and that a URL nothing can render is reported on the console. They print the fallback
and start-up timings they measure. The two cases that need the public internet check for it and
report that they are being skipped when it is absent. On a host without a working GPU context for
mpv, add `CASTOFF_E2E_SKIP_MPV_PIXELS=1` (the page's pixels are still asserted) and, if
Chromium's own sandbox is unavailable (a container without user namespaces),
`CASTOFF_E2E_BROWSER_FLAGS="--no-sandbox --disable-gpu"`.

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
nix develop   # cargo, rustc, rust-analyzer, clippy, yt-dlp, chromium, cage, grim,
              # with mpv already wired up for linking
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
  dormant rather than rendering a video. Displaying a web page follows the same rule: the browser
  engine keeps the page live, so the daemon polls nothing and re-fetches nothing on a timer (a
  dashboard refreshes itself or not at all), mpv stops playback for the duration, and only the
  engine is drawing. Idle is no longer fully dark, though: whenever there's
  no active playback (at startup, after `Stop`, or after a clip reaches end-of-file with nothing
  queued next -- see [`daemon/src/idle_screen.rs`](daemon/src/idle_screen.rs)), the daemon shows
  an on-screen clock via mpv's own OSD instead of a black screen, redrawn on a ~1s
  `std::thread::sleep` timer rather than a busy loop or a second rendering stack. `IdleScreen` is
  a small seam (`Clock` is the only variant today) meant to grow a static-wallpaper variant later
  without restructuring; web pages do not go through it, because a real engine cannot be an mpv
  OSD overlay -- see [How webpage (dashboard) display works](#how-webpage-dashboard-display-works).
  The loading spinner and the start/stop
  fade live in `daemon/src/overlay.rs` and follow the same rule: the spinner redraws at ~30
  frames/s only while a `Play` is genuinely in flight, stops the moment playback starts or the
  load fails, and the fade is a fixed, bounded set of ~20 frames that then stops redrawing -- no
  always-on animation and no per-frame redraw once settled.
- **Data efficiency.** Avoid needless re-fetching over the network. This isn't exercised by the
  scaffold (there's no Immich integration yet), but it constrains that future work: when the
  Immich slideshow integration is built, it must cache each displayed image locally and only
  re-fetch an image if it isn't already cached or the source has changed -- never re-download an
  image on every pass through an album. Web pages already follow it: the daemon never reloads
  them on a cadence, the engine's own HTTP cache (in tmpfs on the appliance) does the caching, and
  a page is only ever loaded once per `Play`.
- **Selectable stream quality (long-term, not required for v1).** A future goal is letting the
  user pick a lower streaming quality/bitrate to save data and power. Not implemented now, but
  the `Play` message (`daemon/src/fcast.rs`) already has room to grow: a `quality`/`bitrate`
  field can be added there later as an additional optional field without breaking the existing
  `url`/`time`/`volume`/`speed` fields or requiring a protocol version bump.

## Not yet implemented (follow-up work)

Out of scope for this scaffold, deliberately:

- Playback sources: Jellyfin (authenticated via Jellyfin's Quick Connect flow -- never a typed
  password) and images from Immich or a local folder. (YouTube and web pages are implemented -- see
  [How YouTube playback works](#how-youtube-playback-works) and
  [How webpage (dashboard) display works](#how-webpage-dashboard-display-works).)
- The native Android control app, including handling Android `Share` intents.
- Appliance disk-image generation (e.g. via `nixos-generators`/`disko`) for a flashable image;
  today's `tv-box` configuration needs a real `fileSystems."/"` and bootloader target to install
  to actual hardware (the flake ships placeholder values for `nix flake check`/VM use).
- Any authentication/credential flow, which is also what a dashboard behind a login (e.g. a
  private Grafana) needs; today's webpage display is for pages reachable without credentials.
- Interaction with a displayed page (clicking, typing, scrolling, remote control) and
  DRM-protected video in it.
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
