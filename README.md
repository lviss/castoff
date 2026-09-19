# castoff

An open-source, appliance-like Chromecast/Roku alternative for a boat: a TV-connected box that
boots straight into a fullscreen player, eventually paired with a native Android control app
(including receiving Android "Share" intents). This repo currently contains the first working
scaffold: the TV-box daemon and its NixOS packaging. Nothing else exists yet -- see
[Not yet implemented](#not-yet-implemented-follow-up-work) below.

## What works today

Right now the supported playback sources are a direct media URL, YouTube, a web page, and an image
uploaded from a phone. Send the daemon an FCast `Play` command with a URL, and it works out what to
do with it:

- a media URL -- a remote (`http(s)://`) or local (`file://`) file, or a `youtube.com`/`youtu.be`
  watch URL -- is loaded and played via mpv (see
  [How YouTube playback works](#how-youtube-playback-works));
- a web page URL is displayed fullscreen in a real browser engine (Chromium) running as a second
  client of the same Cage session (see
  [How webpage (dashboard) display works](#how-webpage-dashboard-display-works));
- an image (uploaded via the daemon's own HTTP endpoint, e.g. from the Android app's Share flow)
  is displayed fullscreen via mpv, held up indefinitely until the queue advances, and can also be
  tagged to rotate through as the idle-screen wallpaper (see
  [Image uploads (private extension)](#image-uploads-private-extension)).

The daemon decides between media and a web page itself -- it tries media first and hands the URL to
the browser if that fails -- so a client only needs to know the URL. A sender that wants to be
certain can still say so with FCast's `container` MIME type (`text/html` forces the browser, any
`image/*` value or any other value forces mpv): see
[How the daemon decides between media and a web page](#how-the-daemon-decides-between-media-and-a-web-page).
A `Play` that is still loading shows an on-screen spinner, and starts/stops are wrapped in a
short fade (see
[On-screen feedback](#on-screen-feedback-loading-indicator-and-startstop-fade)).

That's it: no Jellyfin, no Immich/local-folder slideshows. See
[Not yet implemented](#not-yet-implemented-follow-up-work) below for what's planned but not built.

## What's here

- **`flake.nix` / `nix/tv-box.nix`** -- a NixOS configuration (`nixosConfigurations.tv-box`, for
  generic `x86_64-linux`) that boots directly into [Cage](https://github.com/cage-kiosk/cage) --
  a wlroots-based Wayland compositor that shows exactly one fullscreen client and nothing else, no
  desktop environment or display manager -- running the castoff daemon as that one client. Cage
  stacks a second, newer client on top when there is one, which is how a cast web page takes the
  screen; see [How webpage (dashboard) display works](#how-webpage-dashboard-display-works).
  `nix/tv-box.nix` holds this target-independent kiosk config; `nix/tv-box-x86_64.nix` layers
  generic x86_64-linux placeholders on top of it for `nixosConfigurations.tv-box`, and
  `nix/tv-box-rpi4.nix` layers real Raspberry Pi 4 hardware modules on top of it for
  `nixosConfigurations.tv-box-rpi4` -- see
  [Raspberry Pi 4 image](#raspberry-pi-4-image).
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
| `Play` (1) | sender -> receiver | queues the `url` (media for mpv, or a web page for the browser engine -- the daemon decides which unless the sender's `container` MIME type says so explicitly, see [How the daemon decides between media and a web page](#how-the-daemon-decides-between-media-and-a-web-page)); if nothing is currently playing it starts immediately and becomes the queue's current item, otherwise it is only appended (see [Queueing (private extension)](#queueing-private-extension)). Inline `content` (e.g. a DASH manifest) is rejected -- not yet supported; replies with `PlaybackUpdate` |
| `Pause` (2) / `Resume` (3) | sender -> receiver | toggles mpv's `pause` property; replies with `PlaybackUpdate` |
| `Stop` (4) | sender -> receiver | stops the current item, taking a displayed web page down; mpv returns to idle; replies with `PlaybackUpdate`. Does not clear or move within the queue (see [Queueing (private extension)](#queueing-private-extension)) |
| `Seek` (5) | sender -> receiver | absolute seek; replies with `PlaybackUpdate` |
| `SetVolume` (8) | sender -> receiver | sets mpv volume (FCast's 0.0-1.0 scale, mapped to mpv's 0-100); replies with `VolumeUpdate` |
| `SetSpeed` (10) | sender -> receiver | sets mpv playback speed; replies with `PlaybackUpdate` |
| `Version` (11) | bidirectional | replies with the protocol version this daemon speaks (`2`) |
| `Ping` (12) | bidirectional | replies `Pong` |
| `RequestQueue` (14, private extension) | sender -> receiver | replies with `QueueState` (see [Queueing (private extension)](#queueing-private-extension)) |
| `QueueState` (15, private extension) | receiver -> sender | the full queue and the current position in it; sent both as `RequestQueue`'s reply and, unprompted, to every connected sender whenever the queue changes |
| `QueueJumpForward` (16, private extension) | sender -> receiver | moves to the next queue item, if any, and plays it; replies with `QueueState`. A no-op (still replies) at the last item |
| `QueueJumpBackward` (17, private extension) | sender -> receiver | moves to the previous queue item, if any, and plays it; replies with `QueueState`. A no-op (still replies) at the first item |
| `ClearQueue` (18, private extension) | sender -> receiver | empties the play queue, stopping playback first if its current item is actively playing; replies with `QueueState` |
| `QueueJumpToIndex` (19, private extension) | sender -> receiver | moves straight to an arbitrary queue index (`QueueJumpToIndexMessage`) and plays it; replies with `QueueState`. A no-op (still replies) for an out-of-range index |
| `SetImageWallpaper` (20, private extension) | sender -> receiver | tags or untags a previously-uploaded image (by the id the upload endpoint returned) for idle-screen wallpaper rotation; replies with `ImageWallpaperUpdate` (see [Image uploads (private extension)](#image-uploads-private-extension)) |
| `ImageWallpaperUpdate` (21, private extension) | receiver -> sender | confirms the wallpaper tag `SetImageWallpaper` just set |

Every connected sender keeps its FCast TCP connection open (`daemon/src/main.rs`'s
`handle_connection` reads one persistent socket per sender, not a reconnect-per-command model),
and the daemon uses that for more than command replies: `PlaybackUpdate` is sent both as an
immediate reply to the command that caused it (unchanged) *and* pushed, unprompted, to every
other connected sender the moment playback state changes for any reason -- a command on a
different connection, or an async transition like mpv actually starting to render
(`PlaybackRestart`) or a clip reaching end-of-file on its own. While the last known state is
`Playing`, each connection also gets a `PlaybackUpdate` on a further ~1s tick, recomputed from
mpv at that moment so its `time`/`generationTime` advance and a sender's progress bar can track
playback live without polling the daemon; that tick stops entirely while
idle or paused (see [Design principles](#design-principles)). `VolumeUpdate` is unaffected: still
only an immediate reply to `SetVolume`, on no timer. Wire format is unchanged -- the push path
sends the same `PlaybackUpdateMessage` shape (`generationTime`/`state`/`time`/`duration`/`speed`)
as the synchronous reply, just possibly more than once and without an incoming command.

### Queueing (private extension)

FCast v2 itself has no queue concept, and this daemon only ever implements a subset of the
protocol for a single-user personal project (not aiming for interop with third-party FCast
senders) -- so queueing is a straightforward private extension: six new opcodes beyond FCast's
reserved `0`-`13` range (`RequestQueue` 14, `QueueState` 15, `QueueJumpForward` 16,
`QueueJumpBackward` 17, `ClearQueue` 18, `QueueJumpToIndex` 19), using the same
length-prefixed-opcode-plus-JSON-body framing as every other message. See
[`daemon/src/fcast.rs`](daemon/src/fcast.rs) for the exact `QueueItemMessage`/`QueueStateMessage`/
`QueueJumpToIndexMessage` struct shapes (each field is documented there) and
[`daemon/src/queue.rs`](daemon/src/queue.rs) for the queue itself.

- **Queueing instead of interrupting.** A `Play` (opcode 1) no longer always interrupts whatever
  is playing. `Player::play` (`daemon/src/player.rs`) appends every `Play` to the queue; if
  nothing is currently playing (the idle clock is up, or a load is still in flight with nothing
  rendering yet -- see `Player::is_idle`'s doc comment for exactly which states count), it also
  starts immediately and becomes the queue's current position. Otherwise it just waits its turn.
  A displayed web page counts as "currently playing" here too, so a queue can freely mix media and
  web-page items -- that mix is expected, not a special case.
- **Auto-advance on completion.** When the current item reaches genuine end-of-file (not a `Stop`
  -- see below), the daemon automatically starts the next queued item instead of falling back to
  the idle clock, if one exists (`auto_advance_queue` in `daemon/src/player.rs`, wired through
  `IdleScreenController`'s eof watcher in `daemon/src/idle_screen.rs`). A web page has no
  end-of-file of its own, so an item behind one only starts on an explicit jump.
- **`Stop` halts, it does not clear the queue.** `Stop` (opcode 4) stops the current item the same
  as before; the queue's contents and position are untouched, so the same item (or the next one)
  is still there to jump back to or resume from with a later `Play`/jump.
- **Jumping.** `QueueJumpForward`/`QueueJumpBackward` move to the next/previous item in the queue
  and play it, on demand -- not only on auto-advance. "Backward" means the previous *queue* item,
  not rewinding the current item's playback position (that's `Seek`, unrelated). Both are a no-op
  (but still reply with `QueueState`) at either edge of the queue. `QueueJumpToIndex` moves
  straight to an arbitrary index in one call instead of stepping one item at a time -- e.g. a tap
  on an item in the Android app's queue list -- and is a no-op (still replies) for an out-of-range
  index. Jumping to the already-current index is in range, so it replays that item rather than
  being treated as a no-op, the same as jumping anywhere else.
- **Clearing.** `ClearQueue` empties the queue and forgets its position. If the queue's current
  item is actively playing, it is stopped first (mpv returns to idle, the same visible effect as
  `Stop`) since clearing leaves nothing in the queue left to be "current."
- **The daemon remembers the queue.** The queue (its items and current position) is persisted to a
  small JSON file and reloaded at startup, so it survives a restart -- see
  `queue::default_state_path`'s doc comment in `daemon/src/queue.rs` for exactly where that file
  lives (`CASTOFF_STATE_DIR`, then systemd's `STATE_DIRECTORY`, then XDG's state-home convention).
  A restart does not by itself resume playback: mpv always starts fresh and idle, and only a
  client command (a `Play`, or a jump) starts anything playing again.
- **YouTube title/length lookup.** Queuing a YouTube URL (recognized by host, see
  `metadata::is_youtube_url` in `daemon/src/metadata.rs`) kicks off a background `yt-dlp -j`
  lookup for its title and length; `QueueItemMessage.title`/`durationSecs` start absent and are
  filled in (via another unprompted `QueueState`) once that lookup resolves, or stay absent if it
  fails -- queuing itself never waits on it, and a failed lookup never fails the enqueue. This is
  a separate concern from playback: mpv's own `ytdl_hook` still resolves and plays the URL
  independently (see [How YouTube playback works](#how-youtube-playback-works)).
- **Seeing and being notified of the queue.** `RequestQueue` asks for the current queue on demand;
  `QueueState` is both that reply and, unprompted, pushed to every connected sender whenever the
  queue changes (an add, an auto-advance, or a jump) -- the same push-on-change model
  `PlaybackUpdate` already uses, and, like it, purely event-driven with no polling timer.
- **A failed item does not stick as "current."** Becoming the queue's current item happens as soon
  as a `Play`/jump/auto-advance is accepted, before the load is known to have succeeded. If that
  item's load then fails (including an async failure discovered only after the daemon has already
  moved on to showing the idle clock), the daemon reverts the queue's position to whatever it was
  before -- the failed item stays in the queue, just no longer marked current.

### Image uploads (private extension)

Images shared from the Android app (or any other sender) don't fit through the FCast TCP
connection at all: every FCast frame is capped at 32 KiB (see [Why FCast, and what's
implemented](#why-fcast-and-whats-implemented) above), nowhere near enough for a phone photo. So an
uploaded image travels over its own small local HTTP server instead of a new FCast frame format,
and only the resulting id/URL crosses FCast, in an ordinary `Play`:

- **Upload.** `POST /images` to the daemon on port `46900` (override with `CASTOFF_IMAGE_PORT`,
  same pattern as `CASTOFF_PORT`) with the raw image bytes as the body and an `image/*`
  `Content-Type` header -- one request per image; a multi-image share is one request per image, not
  a batch endpoint. See [`daemon/src/upload.rs`](daemon/src/upload.rs). A non-`image/*` content
  type or a body over 32 MiB is rejected (`400`/`413`); a successful upload replies `200` with:
  ```json
  {"id": "<stable id>", "url": "file://<path>", "container": "image/<type>"}
  ```
  `url`/`container` are already shaped for the sender to drop straight into a `Play` message (see
  below); `id` is what a later `SetImageWallpaper` tags.
- **Storage.** Each uploaded image is written under an `images/` subdirectory next to the queue's
  own persistence file, using the same state-directory resolution as
  `queue::default_state_path`/`Queue::save` (`CASTOFF_STATE_DIR`, then systemd's
  `STATE_DIRECTORY`, then XDG's state-home convention) -- see
  [`daemon/src/images.rs`](daemon/src/images.rs). Which images are tagged for wallpaper rotation is
  tracked in a small JSON manifest (`images.json`) alongside them, following the same
  write-then-rename/missing-file-loads-as-empty pattern `queue.rs` uses for the play queue.
- **Displaying an uploaded image as a queue item.** The sender takes the upload response's `url`
  and `container` and sends an ordinary `Play` -- no new opcode needed. An `image/*` `container` is
  not a web MIME type, so it already routes to mpv like any other explicit media container (see
  [How the daemon decides between media and a web page](#how-the-daemon-decides-between-media-and-a-web-page));
  mpv displays it natively as a still image. The daemon sets mpv's `image-display-duration` to
  infinite, so an image queue item sits up indefinitely -- exactly like a web page -- until the
  queue is explicitly advanced, rather than auto-advancing on its own after mpv's unconfigured
  5-second default. Multiple shared images become multiple queue entries, played through one at a
  time like any other queue item (see [Queueing (private extension)](#queueing-private-extension)).
- **Tagging an image for idle-screen wallpaper rotation.** `SetImageWallpaper` (opcode 20) tags or
  untags a previously-uploaded image (by its `id`) for wallpaper rotation, independent of the play
  queue; the daemon replies `ImageWallpaperUpdate` (opcode 21) confirming the tag. See
  [`daemon/src/fcast.rs`](daemon/src/fcast.rs) for the exact `SetImageWallpaperMessage`/
  `ImageWallpaperUpdateMessage` shapes.
- **Idle-screen wallpaper rotation.** While idle (no active queue playback) and at least one image
  is tagged, the idle clock's existing redraw timer also rotates the on-screen background through
  the tagged images in random order (never immediately repeating the previous pick when more than
  one is tagged), on a fixed one-minute cadence (`idle_screen::WALLPAPER_ROTATION_INTERVAL` -- one
  named constant, so a future configurable-interval follow-up only needs to change it in one place;
  not implemented now). The clock itself keeps drawing on top exactly as it does over the plain
  black idle background -- rotating in a wallpaper never removes or replaces the clock. mpv's own
  `idle-active` property goes false once a wallpaper image is loaded, but FCast clients still see
  `PlaybackUpdate.state = Idle`: this is the idle screen, not content anyone asked to play. See
  [`daemon/src/idle_screen.rs`](daemon/src/idle_screen.rs).

### How YouTube playback works

A `Play` message's `url` can be a `youtube.com`/`youtu.be` watch URL, not just a direct media
URL -- no new opcode or protocol change, and no bespoke YouTube API integration or Cast-protocol
emulation. This works because mpv (and therefore `libmpv2`, since it's the same core) ships a
built-in `ytdl_hook` Lua script that automatically shells out to
[`yt-dlp`](https://github.com/yt-dlp/yt-dlp) to resolve a direct, playable stream URL whenever it's
given a URL it doesn't recognize as directly playable media. This is unconditional for playback:
no daemon-side code detects YouTube URLs, spawns `yt-dlp`, or parses its output in order to play
one -- `Player::play` (`daemon/src/player.rs`) just hands `url` to mpv's `loadfile` exactly as it
already did for a direct remote mp4, and mpv/`ytdl_hook` do the rest, as verified by a real
(non-mocked) test against a real public YouTube URL
(`real_youtube_url_resolves_and_plays_via_ytdl_hook`, gated `#[ignore]` since it needs network
access and `yt-dlp` on `PATH` -- see that test's doc comment to run it). The daemon does run
`yt-dlp` itself for one unrelated purpose -- a background title/length lookup for the queue, see
[Queueing (private extension)](#queueing-private-extension) -- which is independent of and never
blocks this playback path.
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
submission is tracked by the `playlist_entry_id` mpv assigned it (read back from `playlist/0/id`),
and mpv reports that same id on its `EndFile`/`StartFile` events, so only the newest submission's
own error is attributed to its own URL. An event that matches no tracked submission -- a load a
newer `Play` superseded, or an extra entry mpv expanded a playlist URL into -- is not attributed
to a newer request at all: it is logged at `debug` by entry id alone, never as a playback failure
of the URL the daemon is currently on.

### How the daemon decides between media and a web page

A client only has to know a URL; the daemon decides what it is. The rules, in order
(`Player::play` in `daemon/src/player.rs`):

1. **The sender said so.** FCast's `container` MIME type is an explicit override:
   `text/html` or `application/xhtml+xml` (case-insensitive, MIME parameters ignored) means the
   browser engine; any other MIME type means mpv, including `image/*` -- an uploaded image (see
   [Image uploads (private extension)](#image-uploads-private-extension)) is just another explicit
   media container; mpv displays it natively as a still image, no separate code path. Nothing is
   guessed and nothing falls back -- a sender that classifies its URL keeps control either way (a
   `video/mp4` or `image/png` URL that fails is an error, not a page).
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
data-efficient and the correct behavior. The engine's profile and its `TMPDIR` both live under a
*fixed* short root (`/tmp`, tmpfs on the appliance; `CASTOFF_BROWSER_PROFILE_DIR` moves it), not
under whatever `TMPDIR` the daemon inherited: Chromium builds its process-singleton unix socket at
`<TMPDIR>/org.chromium.Chromium.<random>/SingletonSocket`, and a deep ambient `TMPDIR` made that
path exceed the kernel's limit, aborting the engine with `FATAL ... Socket path too long` before
the page appeared (seen on real hardware). Browsing state therefore never spins up the disk, and
the daemon refuses a root without room for that socket with a plain-language console error instead
of letting Chromium die cryptically; while a page is up, mpv's decode pipeline is stopped, so
nothing plays or decodes behind it.

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
./result/bin/castoff-daemon        # FCast on 0.0.0.0:46899 (override with CASTOFF_PORT),
                                    # image uploads on 0.0.0.0:46900 (override with CASTOFF_IMAGE_PORT)
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

or upload and display an image (see [Image uploads (private extension)](#image-uploads-private-extension)):

```sh
resp=$(curl -s -X POST --data-binary @photo.jpg -H 'Content-Type: image/jpeg' \
  http://127.0.0.1:46900/images)
echo "$resp"   # -> {"id":"...","url":"file://...","container":"image/jpeg"}

# Feed the response's url/container straight into an ordinary Play:
perl -e '
  my $body = shift;
  print pack("V", length($body) + 1), chr(1), $body;
' "$(echo "$resp" | jq -c '{url, container}')" | socat - TCP:127.0.0.1:46899 | xxd

# Tag that same id for idle-screen wallpaper rotation (opcode 20):
id=$(echo "$resp" | jq -r .id)
perl -e '
  my $body = shift;
  print pack("V", length($body) + 1), chr(20), $body;
' "{\"id\":\"$id\",\"wallpaper\":true}" | socat - TCP:127.0.0.1:46899 | xxd
# -> ImageWallpaperUpdate reply (opcode 21): {"generationTime":...,"id":"...","wallpaper":true}
```

### Whole-system checks

```sh
nix flake check   # builds and tests the daemon package (via `checks`), and evaluates
                   # the tv-box NixOS configuration (which includes the `tv-box-vm`
                   # package) and the dev shell
```

This builds and evaluates the `x86_64-linux` outputs. `nix flake check --all-systems` also
evaluates the `aarch64-linux` outputs (`tv-box-rpi4`, `tv-box-rpi4-image`) but does not build them
on a machine that can't execute `aarch64-linux` derivations -- see
[Raspberry Pi 4 image](#raspberry-pi-4-image).

The package's tests include the end-to-end webpage/routing test (see
[How webpage (dashboard) display works](#how-webpage-dashboard-display-works)): it starts a real
headless Cage session with the real Chromium engine and asserts on compositor pixels. The Nix
build sandbox has no GPU, where mpv has no way to present frames, and no network; that run
therefore skips mpv's own pixel assertions (`CASTOFF_E2E_SKIP_MPV_PIXELS=1`) and the two cases
that need a public URL (`example.com`, YouTube), saying so in its output, while still asserting
the locally served page's pixels.

### End-to-end webpage/routing test

The tests in `daemon/tests/webpage_display.rs` are `#[ignore]`d because they need a compositor and
a browser, which few environments have (`nix flake check` runs them anyway, via the package's Nix
`postCheck`). On a machine with a GPU context for mpv -- the dev shell lists `cage`, `chromium`
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

Two equivalent ways to boot the whole appliance (Cage + castoff-daemon, as the real box's kiosk
session) in a throwaway VM:

```sh
# Either:
nixos-rebuild build-vm --flake .#tv-box
./result/bin/run-*-vm

# Or, without nixos-rebuild (e.g. in a sandbox/CI with no nixos-rebuild):
nix build .#tv-box-vm
./result/bin/run-*-vm
```

Both build the *same* thing: `.#tv-box-vm` is the `tv-box` configuration's `system.build.vm`,
which is NixOS's `virtualisation.vmVariant` (see `nix/tv-box.nix`) with nixpkgs' qemu-vm module
folded in -- so a VM-only fix cannot accidentally land on one of the two commands and not the
other. None of the VM-only settings apply to the appliance on real hardware.

**What you should see.** A QEMU window, ~10-20 seconds of boot messages, then the appliance's
**idle screen: a large white clock centred on a black screen**. That is the kiosk session (Cage
running the daemon) up and drawing. It is not the desktop, and there is no menu, panel or
cursor: the appliance is one fullscreen client, exactly as on the TV.

**Resizing the QEMU window resizes the guest's picture, not just the window frame around it.**
Growing or shrinking the window changes the actual resolution Cage renders at -- drag a corner and
the clock (or whatever's on screen) redraws crisp at the new size, it doesn't just get stretched or
letterboxed. See "Why the resize needs a nudge" below for what makes this work and its one sharp
edge (a resize sent before Cage itself is up is a harmless no-op, not an error).

Cast something to it to check playback. The VM forwards FCast's port to the host, so the
daemon inside the VM is reachable at **`127.0.0.1:46899`** (SLiRP's host side binds
`0.0.0.0:46899`, i.e. every host interface, so another machine on the same LAN can also reach
it at the host's LAN address and port -- useful for trying the Android app; to keep it to the
host alone, set `host.address = "127.0.0.1"` on the `forwardPorts` entry in `nix/tv-box.nix`).
From the host:

```sh
# Serve something for the VM to play. The guest reaches the *host's* loopback as
# 10.0.2.2, so the URL to cast is http://10.0.2.2:<port>/... -- anything else
# (a file: URL, a LAN address, a public URL) works too, as long as the VM can
# reach it.
mkdir -p /tmp/castoff-vm-media && cd /tmp/castoff-vm-media
printf '<html><body style="background:#123;color:#0ff;font:48px sans-serif">dashboard</body></html>' > dashboard.html
nix run nixpkgs#python3 -- -m http.server 8000 &

# Send an FCast Play (opcode 1): 4-byte little-endian length, 1-byte opcode, JSON body.
perl -e '
  my $body = q({"url":"http://10.0.2.2:8000/dashboard.html","container":"text/html"});
  use IO::Socket::INET;
  my $s = IO::Socket::INET->new(PeerAddr => "127.0.0.1", PeerPort => 46899,
                                Proto => "tcp", Timeout => 5) or die "connect: $!";
  syswrite($s, pack("V", length($body) + 1) . chr(1) . $body);
  my $buf; sysread($s, $buf, 4096); print unpack("H*", $buf), "\n";
'
```

The page replaces the clock (Chromium maps over mpv), a media URL plays instead, and `Stop`
(opcode `4`, no body) brings the clock back. `printf '\x01\x00\x00\x00\x0c' | nc 127.0.0.1 46899`
is a `Ping`, if you just want to check the port is live.

**"I see a black screen but no clock."** Press **Ctrl+Alt+3** in the QEMU window (or use its
**View** menu to select the serial console): the VM autologins **root** there, so a blank screen
always comes with a shell rather than a dead rectangle. Then:

```sh
systemctl status cage-tty1          # the kiosk session
journalctl -b -u cage-tty1          # cage's and the daemon's own output
ss -ltnp | grep 46899               # the daemon's control listener
```

Things that legitimately stop the VM from coming up, and what they look like:

- **The host already uses port 46899.** QEMU then refuses to start at all, before any boot:
  `Could not set up host forwarding rule 'tcp::46899-:46899'`. Stop whatever holds the port (or
  change `host.port` in `nix/tv-box.nix`). The daemon running on the host itself is the usual
  cause.
- **No GPU, on purpose.** The VM has no hardware GPU, so everything -- the clock, video, web
  pages -- is drawn by Mesa's llvmpipe software renderer (`nix/tv-box.nix` gives the VM a virtio
  GPU so that renderer has something to draw through, and lets Cage and mpv use it).
  Hardware-accelerated video decode is genuinely unavailable here; playback is software-decoded
  and so is choppier and more CPU-hungry than on the real box. That is the one capability the VM
  cannot preview. On a slower host, expect the clock and playback to be visibly heavy.
- **A first boot creates the disk image.** That's a one-time few extra seconds on the very first
  run; every boot after that reuses the same `nixos.qcow2`.

If the kiosk session is up (`cage-tty1` active) but nothing is drawn, check the console output
for mpv's concrete error line -- the daemon turns on mpv's own logging precisely so a failed load
says why (see [How YouTube playback works](#how-youtube-playback-works)); the same reasoning
applies to a load that fails in the VM. A `journalctl -b -u cage-tty1 | grep -i assertion` that
shows `xwayland/xwm.c` means the VM's X11-avoiding settings above have been lost -- that assertion
is what a GPU-less Xwayland does to the kiosk session.

Three environment variables in `nix/tv-box.nix`'s `virtualisation.vmVariant` are worth knowing
about when debugging the VM's rendering, because all three are VM-only settings for a GPU-less
machine:

- `WLR_RENDERER_ALLOW_SOFTWARE=1` lets wlroots' DRM backend use Mesa's software GL renderer
  instead of refusing it and falling back to pixman (which advertises no dma-buf, leaving mpv
  with nothing to present into).
- `CASTOFF_MPV_GPU_CONTEXT=wayland` and `CASTOFF_MPV_HWDEC=no` (`daemon/src/player.rs`) keep mpv
  off X11. Left alone, mpv's context probing and its `hwdec=auto-safe` VDPAU probe both open the
  X display Cage advertises, which makes wlroots start its lazily-spawned Xwayland -- and
  Xwayland cannot bring up a screen without a GPU, so it aborts and takes the kiosk session down
  with it (`cage: xwayland/xwm.c:592: ... Assertion ... failed`), typically a few seconds after
  the boot or after the first `Play`. On the real box mpv's auto-detection reaches the same
  Wayland/EGL context first and its hwdec probes find real hardware, so neither setting is used
  there.

**Give it enough CPU.** `virtualisation.cores` defaults to 1 in nixpkgs' qemu-vm module, and 1
core is nowhere near enough for this VM's all-software stack (Cage, mpv and Chromium all draw
through Mesa's llvmpipe, and llvmpipe itself wants several cores). `nix/tv-box.nix` raises it to 4
(and memory to 4096 MiB) for exactly that reason -- measured on a 16-core host, that took boot
(fresh disk) to the point where the daemon answers FCast, plus casting a Chromium page and having
it render, from **over five minutes total down to about 33 seconds**. Lower it if the host can't
spare 4 cores; raise it if the host has room and casting still feels heavy.

**Why the resize needs a nudge.** QEMU's virtio-gpu already forwards a host window resize to the
guest for free -- no config needed beyond the `-vga virtio` from above -- but Cage's compositor
doesn't act on it by itself: wlroots only re-reads a connector's modes when it connects or
disconnects, never for a same-connector resize, and Cage only ever picks a mode once, when its
output first appears. `nix/tv-box.nix` closes that gap with a udev rule (`ACTION=="change"` on the
DRM device) that re-reads the guest kernel's live mode -- the first line of
`/sys/class/drm/card*-Virtual-1/modes`, which does track the resize -- and pushes it to Cage
through `wlr-randr --custom-mode` (Cage accepts external mode changes via
`wlr-output-management-v1`; this is what `wlr-randr` speaks). It runs as the `kiosk` user because
only that user can reach Cage's Wayland socket. One consequence: a resize that lands *before* the
kiosk session itself is up (early in boot) is a no-op -- `wlr-randr` can't reach a Wayland socket
that doesn't exist yet, the helper unit exits without changing anything, and the very next resize
after Cage starts works normally. This is entirely VM-only tooling (`wlr-randr` and the udev rule
are not part of the appliance's runtime closure); the real box has no virtio-gpu resize event to
react to in the first place.

### Raspberry Pi 4 image

`nixosConfigurations.tv-box-rpi4` (`nix/tv-box.nix` plus `nix/tv-box-rpi4.nix`) is the same
appliance config as `tv-box`, but layered onto real Raspberry Pi 4 hardware modules from
[nixos-raspberrypi](https://github.com/nvmd/nixos-raspberrypi) (kernel, firmware, `vc4-kms-v3d`
display, Bluetooth) instead of `tv-box`'s generic x86_64 placeholders.
`packages.aarch64-linux.tv-box-rpi4-image` builds it with `nixos-raspberrypi.lib.nixosInstaller`,
which is what makes the result a single image that's both flashable installation media *and* a
ready-to-use booted system -- the partition table auto-expands to fill the SD card on first boot,
so there's no separate `nixos-anywhere`/`disko` install step.

Build it (on an `aarch64-linux` machine, or an `x86_64-linux` machine with `aarch64-linux` cross
build support enabled -- see below; this repo's own sandbox has neither, see further down):

```sh
# --accept-flake-config trusts nixos-raspberrypi's binary cache (see its own README), which
# avoids rebuilding the Raspberry Pi kernel from source. --system aarch64-linux is required:
# the bare flake shorthand `.#tv-box-rpi4-image` resolves against the CALLING machine's own
# system first (i.e. `packages.<caller's system>.tv-box-rpi4-image`), so on any non-aarch64
# machine it fails with "does not provide attribute packages.x86_64-linux.tv-box-rpi4-image"
# without this flag -- confirmed against a real x86_64-linux laptop.
nix build --accept-flake-config --system aarch64-linux .#packages.aarch64-linux.tv-box-rpi4-image
```

**Building on an `x86_64-linux` machine** (the common case) additionally needs `aarch64-linux`
cross build support enabled on that machine, or the build fails at the same platform-mismatch
error this project's own sandbox hits (see further down) -- `--system aarch64-linux` alone only
selects *which* output to build, it doesn't grant the ability to build it. On NixOS, add this to
your system configuration and `sudo nixos-rebuild switch`:

```nix
boot.binfmt.emulatedSystems = [ "aarch64-linux" ];
```

This registers QEMU user-mode emulation so the build actually runs (slowly -- it's emulating
another CPU architecture) instead of hard-failing at the platform-mismatch check. On a non-NixOS
distro, the equivalent is installing `qemu-user-static` and registering it with `binfmt_misc`
(package name and exact steps vary by distro); a genuine `aarch64-linux` remote builder is an
alternative to either.

**If you see `warning: ignoring untrusted substituter ... you are not a trusted user`**: passing
`--accept-flake-config` only *offers* trust in nixos-raspberrypi's binary cache -- Nix itself still
refuses to use a substituter or its trusted public keys unless your user is in `trusted-users`.
Without that, the build still succeeds, just slower (it compiles the Raspberry Pi kernel from
source instead of fetching a prebuilt one). On NixOS, add yourself and rebuild:

```nix
nix.settings.trusted-users = [ "root" "your-username" ];
```

(non-NixOS: add the same to `trusted-users` in `/etc/nix/nix.conf` and restart the `nix-daemon`
service).

That produces `./result`, a compressed image (`nixos-image-rpi4-uboot.img.zst`). Flash it to an SD
card (**this overwrites the entire card** -- double-check `of=`):

```sh
zstd -d --stdout ./result | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync
# or, with raspberrypi-imager: choose "Use custom" and point it at ./result directly.
```

Put the card in a Raspberry Pi 4B, connect it to power, Ethernet (or configure Wi-Fi -- see
`nix/tv-box.nix`'s `networking.networkmanager`) and an HDMI display, and boot it. What to look for:

- **The appliance's idle screen**: a large white clock centred on a black screen, the same as the
  VM (see above) -- that's Cage running the daemon as its one fullscreen client.
- **The box is reachable on the LAN.** It advertises itself over mDNS/avahi (`castoff-rpi4.local`)
  and listens for FCast on port 46899; from another machine on the same network:
  ```sh
  ping castoff-rpi4.local
  nc -zv castoff-rpi4.local 46899
  ```
- **`castoff-daemon` is actually running the kiosk session.** Over SSH (if enabled) or a directly
  attached keyboard:
  ```sh
  systemctl status cage-tty1
  journalctl -b -u cage-tty1
  ```
- **A shared YouTube link plays.** Cast a `youtube.com`/`youtu.be` URL to the box (an FCast `Play`,
  same as the [manual protocol test](#manual-protocol-test) above, or a real FCast sender) and
  confirm it starts playing video, not just that the daemon accepted the command.

**What was and wasn't verified here.** This sandbox has neither an `aarch64-linux` builder nor
`aarch64-linux` QEMU user-mode emulation configured (no `boot.binfmt.emulatedSystems`, no
`binfmt_misc` entries) -- `nix flake check --all-systems` and `nix eval` on every new output
resolve cleanly, and `nix build --dry-run .#tv-box-rpi4-image` resolves the entire ~430-derivation
closure with no errors, but an actual build was never executed: a direct, non-dry-run `nix build`
attempt fails with a genuine `error: Cannot build ... Reason: platform mismatch, Required system:
'aarch64-linux', Current system: 'x86_64-linux'`, confirming this is an environment limitation, not
a configuration error. Hardware playback performance (YouTube decode, Cage/wlroots rendering) on
real Pi 4 silicon was explicitly out of scope for this change -- see
[Not yet implemented](#not-yet-implemented-follow-up-work) -- and was not and could not be tested
here; the steps above are exactly what to check on real hardware.

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
  event-driven (`tokio`, no polling loop); `VolumeUpdate` replies are sent only in response to a
  command, never on a timer. `PlaybackUpdate` is the one exception, and deliberately so: each
  connected sender's push task (`daemon/src/main.rs`) ticks on a ~1s timer *only* while the last
  known state is `Playing`, using `tokio::select!` so that arm isn't even polled while idle/paused
  -- the timer itself only exists (and only costs anything) for the duration of active playback,
  not as a standing wake loop; mpv is configured with `hwdec=auto-safe` so decode
  uses hardware acceleration when available; and `stop`/idle leaves mpv's *decode* pipeline
  dormant rather than rendering a video. Displaying a web page follows the same rule: the browser
  engine keeps the page live, so the daemon polls nothing and re-fetches nothing on a timer (a
  dashboard refreshes itself or not at all), mpv stops playback for the duration, and only the
  engine is drawing. Idle is no longer fully dark, though: whenever there's
  no active playback (at startup, after `Stop`, or after a clip reaches end-of-file with nothing
  queued next -- see [`daemon/src/idle_screen.rs`](daemon/src/idle_screen.rs)), the daemon shows
  an on-screen clock via mpv's own OSD instead of a black screen, redrawn on a ~1s
  `std::thread::sleep` timer rather than a busy loop or a second rendering stack. That same timer
  also rotates in an idle-screen wallpaper image when one is tagged (see
  [Image uploads (private extension)](#image-uploads-private-extension)), rather than a second
  timer of its own; web pages do not go through `IdleScreen` at all, because a real engine cannot be
  an mpv OSD overlay -- see [How webpage (dashboard) display works](#how-webpage-dashboard-display-works).
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
  password) and a slideshow pulled from Immich or a local folder. (YouTube, web pages, and images
  uploaded from a phone are implemented -- see
  [How YouTube playback works](#how-youtube-playback-works),
  [How webpage (dashboard) display works](#how-webpage-dashboard-display-works), and
  [Image uploads (private extension)](#image-uploads-private-extension).)
- The native Android control app, including handling Android `Share` intents and the upload
  client for [Image uploads (private extension)](#image-uploads-private-extension)'s HTTP
  endpoint -- the daemon-side upload contract exists, but nothing in this repo sends to it yet.
- x86_64 appliance disk-image generation for real hardware (e.g. via `nixos-generators`/`disko`);
  `tv-box`'s `fileSystems."/"` and bootloader target are still generic placeholders for
  `nix flake check`/VM use (see `nix/tv-box-x86_64.nix`). A flashable image *is* implemented for
  the Raspberry Pi 4 target -- see [Raspberry Pi 4 image](#raspberry-pi-4-image).
- Playback-quality/resolution options for the Raspberry Pi 4 target (e.g. capping YouTube
  quality) -- deliberately not built preemptively; a follow-up only if real Pi 4 hardware testing
  finds a need for it.
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
