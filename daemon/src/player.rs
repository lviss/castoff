//! Thin wrapper around libmpv2 that maps FCast-shaped requests onto mpv
//! commands/properties, and reads back mpv state as an FCast PlaybackUpdate.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use libmpv2::events::{mpv_event_id, Event};
use libmpv2::Mpv;
use tokio::sync::watch;
use tracing::{debug, error, warn};

use crate::fcast::{
    PlayMessage, PlayTarget, PlaybackState, PlaybackUpdateMessage, QueueStateMessage,
};
use crate::idle_screen::{IdleScreen, IdleScreenController};
use crate::overlay::PlaybackOverlay;
use crate::queue::Queue;
use crate::webpage::WebpageController;

pub struct Player {
    mpv: Arc<Mpv>,
    idle: Arc<IdleScreenController>,
    /// The browser engine that renders web pages (`webpage`): a second
    /// fullscreen client of the same Cage session, shown over mpv's window.
    webpage: Arc<WebpageController>,
    /// Loading spinner and start/stop fade drawn on top of whatever mpv is
    /// showing (see `overlay.rs`).
    overlay: Arc<PlaybackOverlay>,
    /// Serializes whole `play`/`stop` operations. The media path and the
    /// webpage path touch each other's state (a webpage `Play` stops mpv and
    /// raises the idle clock; a media `Play` takes the browser down and hides
    /// the clock), so without this a webpage `Play` on one FCast connection
    /// can interleave with a media `Play` on another and leave the clock
    /// painted over playing video. The asynchronous fallback in
    /// `spawn_async_event_watcher` takes it too.
    operation: Arc<Mutex<()>>,
    /// The in-flight `loadfile` submission and the daemon's own
    /// media-vs-web-page decision for it (`Play`s whose sender did not
    /// classify the URL); see `Routing`.
    routing: Arc<Mutex<Routing>>,
    /// The play queue: items queued after (or as) the currently playing one
    /// (`queue.rs`). `play` appends to it and either plays the new item
    /// immediately (nothing else is playing) or leaves it queued;
    /// `queue_jump_forward`/`queue_jump_backward` and the eof-triggered
    /// auto-advance (`auto_advance_queue`, wired through
    /// `IdleScreenController`'s `on_eof`) move the current position within
    /// it.
    queue: Arc<Mutex<Queue>>,
    /// Where `queue` is persisted (see `queue::default_state_path`), or
    /// `None` if no writable state directory could be resolved -- the queue
    /// then stays in-memory only for this run (logged once at startup by
    /// `Player::new`).
    state_path: Option<PathBuf>,
    /// Broadcasts the latest `PlaybackUpdate`-shaped snapshot whenever any
    /// method below changes playback state, or an async transition does (a
    /// `PlaybackRestart`, an idle-screen show/hide, or the browser fallback
    /// taking the screen -- see `IdleScreenController`'s `on_change`
    /// callback, `fall_back_to_browser` and `spawn_lifecycle_watcher`).
    /// `main.rs` gives every connected FCast sender its own subscription
    /// (`subscribe_status`) so it can push updates onto that sender's
    /// already-open socket without polling. `watch` (not `broadcast`)
    /// deliberately: subscribers only ever care about the *current* status,
    /// so coalescing rapid updates into the latest value is correct here and
    /// sidesteps `broadcast`'s slow-subscriber lag/`RecvError::Lagged`
    /// entirely.
    status_tx: watch::Sender<PlaybackUpdateMessage>,
    /// Broadcasts the latest queue snapshot whenever it changes (an item is
    /// added, playback auto-advances past one, or a client jumps
    /// forward/backward) -- the same push-on-change model `status_tx` uses
    /// for `PlaybackUpdate` (see `main.rs`'s `push_updates`).
    queue_tx: watch::Sender<QueueStateMessage>,
}

/// The handles play/queue operations need, grouped so the free functions
/// below -- used both by `Player`'s own methods and the queue auto-advance
/// callback, which runs (via `IdleScreenController`'s `on_eof`) before a
/// `Player` exists to borrow `&self` from -- don't need a long parameter
/// list.
struct Handles<'a> {
    mpv: &'a Mpv,
    webpage: &'a WebpageController,
    overlay: &'a PlaybackOverlay,
    idle: &'a IdleScreenController,
    routing: &'a Mutex<Routing>,
}

/// The play queue state `spawn_async_event_watcher`'s background thread needs
/// for the run's lifetime, bundled into one parameter (owned, since the
/// thread's closure must move them in) rather than three -- the same reason
/// `Handles` exists. `QueueContext` is the borrowed shape built from these for
/// a single resolution (`fall_back_to_browser`).
struct QueueHandles {
    queue: Arc<Mutex<Queue>>,
    state_path: Option<PathBuf>,
    queue_tx: watch::Sender<QueueStateMessage>,
}

/// One `loadfile` submitted to mpv, tracked by the playlist entry mpv created
/// for it so `spawn_async_event_watcher` can attribute mpv's
/// `FileLoaded`/`EndFile` events to the submission they belong to instead of
/// to whichever entry mpv happens to report next.
struct Load {
    /// The URL handed to mpv.
    url: String,
    /// The sender did not classify the URL, so this load is the daemon's own
    /// media-vs-web-page probe and may fall back to the browser if it never
    /// loads.
    probing: bool,
    /// mpv has reported `FileLoaded` for this load. Once true the URL is
    /// media: a later failure is a playback error, never a fallback.
    loaded: bool,
    /// mpv's `playlist_entry_id` for the playlist entry `loadfile` created,
    /// read back from `playlist/0/id` (see `Routing::submit`). `None` when
    /// that read failed, in which case no mpv event matches this load and it
    /// is never re-routed.
    entry_id: Option<i64>,
    /// If this load was submitted as the play queue's new current item, where
    /// to revert `Queue`'s position if it ends up never playing (see
    /// `revert_queue_position`). `None` for a load `execute_play` was not
    /// asked to track against the queue.
    queue_rollback: Option<QueueRollback>,
}

/// The queue position to restore if a load submitted as the queue's current
/// item fails before ever playing -- an unreachable URL's mpv error surfaces
/// off `Player::play`'s call stack (see `spawn_async_event_watcher`), well
/// after the position was already committed and broadcast, so undoing it
/// needs to travel with the load's own tracking rather than live on the call
/// stack that submitted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueRollback {
    /// The queue position this load was submitted as.
    index: usize,
    /// The position to restore if the load never plays.
    previous: Option<usize>,
}

/// The queue handles plus the specific rollback for the load being resolved
/// right now -- bundles what `fall_back_to_browser` needs about the queue
/// into one parameter, borrowed from a `QueueHandles` for the duration of one
/// resolution.
struct QueueContext<'a> {
    queue: &'a Mutex<Queue>,
    state_path: Option<&'a Path>,
    queue_tx: &'a watch::Sender<QueueStateMessage>,
    rollback: Option<QueueRollback>,
}

/// What the watcher should do with an mpv error that ended the submitted
/// load.
#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    /// The load was a probe that never loaded: hand its URL to the browser
    /// engine instead.
    FallBack(String, Option<QueueRollback>),
    /// The load had already loaded, or the sender classified it as media: a
    /// genuine playback error, never re-routed.
    PlaybackError(String, Option<QueueRollback>),
}

/// The daemon's media-vs-web-page decision in flight: the newest `loadfile`
/// submission that mpv has not finished with.
///
/// Attribution is by mpv's own `playlist_entry_id`, never by submission
/// order. `loadfile` creates a playlist entry, whose id is read back from
/// `playlist/0/id` (see `Routing::submit`), and mpv reports that same id on
/// `MPV_EVENT_END_FILE` (and `MPV_EVENT_START_FILE`). A submission's events
/// therefore resolve only that submission: an `EndFile` for any other entry
/// -- a playlist mpv expanded into entries of its own, or a superseded load
/// -- does not match and is ignored, so it can neither clear nor consume a
/// newer attempt's probe. `FileLoaded` carries no id of its own, so it is
/// attributed through the `MPV_EVENT_START_FILE` that precedes it
/// (`last_started`).
///
/// Only the newest submission is tracked; an older one is dropped when a new
/// `loadfile` is submitted, and its later events no longer match anything.
/// Correctness does not depend on the watcher draining an event before the
/// next `Play` arrives, because no decision is ever made from position.
/// `Stop`, and a direct webpage `Play`, cancel the in-flight decision
/// (`cancel`): it is no longer fallback-eligible, but its own events still
/// resolve it.
#[derive(Default)]
struct Routing {
    current: Option<Load>,
    /// `playlist_entry_id` of the most recent `MPV_EVENT_START_FILE`, so the
    /// `MPV_EVENT_FILE_LOADED` that follows it can be attributed without an
    /// id of its own.
    last_started: Option<i64>,
}

impl Routing {
    /// Record a `loadfile` that has just been submitted to mpv, together with
    /// the `playlist_entry_id` mpv assigned it. Called under the `operation`
    /// lock, before the watcher can drain any event for it.
    fn submit(
        &mut self,
        url: &str,
        probing: bool,
        entry_id: Option<i64>,
        queue_rollback: Option<QueueRollback>,
    ) {
        self.current = Some(Load {
            url: url.to_string(),
            probing,
            loaded: false,
            entry_id,
            queue_rollback,
        });
    }

    /// mpv started a file; remember its id so the `FileLoaded` that follows
    /// can be attributed to it.
    fn start_file(&mut self, entry_id: i64) {
        self.last_started = Some(entry_id);
    }

    /// mpv opened a file; if it is the submitted load, it is media from here
    /// on.
    fn file_loaded(&mut self) {
        if let Some(load) = self.current.as_mut() {
            if load.entry_id == self.last_started {
                load.loaded = true;
            }
        }
    }

    /// The submitted load ended cleanly (end-of-file, a playlist redirect, or
    /// a later `loadfile`/`stop` superseding it): it is over and never a
    /// fallback.
    fn resolve_end(&mut self, entry_id: i64) {
        if self
            .current
            .as_ref()
            .is_some_and(|load| load.entry_id == Some(entry_id))
        {
            self.current = None;
        }
    }

    /// The submitted load ended with an error. Returns what the watcher should
    /// do, or `None` when the error belongs to no submitted load (a playlist
    /// entry mpv expanded, a superseded submission, or mpv's own idle state).
    fn resolve_error(&mut self, entry_id: i64) -> Option<Resolution> {
        if self.current.as_ref()?.entry_id != Some(entry_id) {
            return None;
        }
        let load = self.current.take()?;
        if load.probing && !load.loaded {
            Some(Resolution::FallBack(load.url, load.queue_rollback))
        } else {
            Some(Resolution::PlaybackError(load.url, load.queue_rollback))
        }
    }

    /// Cancel the in-flight decision: it is no longer fallback-eligible, but
    /// it is not dropped -- its own mpv events still resolve it (as a playback
    /// error, since `probing` is now false), and until then a newer
    /// submission simply replaces it.
    fn cancel(&mut self) {
        if let Some(load) = self.current.as_mut() {
            load.probing = false;
        }
    }
}

impl Player {
    /// Create the mpv core. No window is opened and no decoding happens until
    /// the first `play()` call: mpv's `idle` mode holds an empty, otherwise
    /// dormant window that Cage can still fullscreen, without spinning up a
    /// decode pipeline for nothing on boat power. Shows the idle screen
    /// (see `idle_screen`) immediately, since there is no playback yet.
    pub fn new() -> Result<Self> {
        // `CASTOFF_MPV_VO` is a troubleshooting/test knob: on a host where mpv
        // cannot create a GPU context at all, its `gpu` video output aborts
        // the whole daemon inside mpv's context probing (an mpv assertion,
        // not a castoff one). Pointing this at `null` keeps the daemon alive
        // there; the appliance's default stays hardware-accelerated `gpu`.
        let vo = std::env::var("CASTOFF_MPV_VO").unwrap_or_else(|_| "gpu".to_string());
        // `CASTOFF_MPV_GPU_CONTEXT` is the counterpart knob, for a machine
        // that *has* a display but no GPU (the NixOS VM): mpv's automatic
        // context probing tries its Vulkan and then X11 contexts first, and
        // merely connecting to the X display cage advertises is enough to make
        // wlroots start its lazily-spawned Xwayland. Xwayland cannot bring up
        // a screen without a GPU, so it aborts -- and takes the whole kiosk
        // session down with it (wlroots asserts while tearing down the dead
        // Xwayland surface). Pinning the context to `wayland` keeps mpv on the
        // Wayland/EGL path the appliance already uses on real hardware, where
        // mpv's own auto-detection reaches it first. Unset or empty (the
        // shipping default) leaves that auto-detection alone.
        let gpu_context = std::env::var("CASTOFF_MPV_GPU_CONTEXT").unwrap_or_default();
        // `CASTOFF_MPV_HWDEC` (default `auto-safe`) exists for the same
        // GPU-less machine, one layer down: `auto-safe` probes VDPAU, whose
        // backend lookup opens an X display, so *playing* anything wakes the
        // same doomed Xwayland even with the context pinned above. The VM has
        // no hardware decoder to find anyway.
        let hwdec = std::env::var("CASTOFF_MPV_HWDEC").unwrap_or_else(|_| "auto-safe".to_string());
        let mpv = Mpv::with_initializer(|init| {
            init.set_property("vo", vo.as_str())?;
            if !gpu_context.is_empty() {
                init.set_property("gpu-context", gpu_context.as_str())?;
            }
            init.set_property("fullscreen", "yes")?;
            init.set_property("force-window", "yes")?;
            init.set_property("idle", "yes")?;
            init.set_property("keep-open", "yes")?;
            // Prefer hardware decode when available: much lower CPU/power draw
            // than software decode for the long, mostly-static playback runs
            // this box is built for.
            init.set_property("hwdec", hwdec.as_str())?;
            init.set_property("input-default-bindings", "no")?;
            init.set_property("input-vo-keyboard", "no")?;
            init.set_property("osc", "no")?;
            // Print mpv's own warning/error log lines to the daemon's stderr
            // (journald/console on the appliance). libmpv defaults to
            // `terminal=no`, so without this a failed asynchronous load --
            // e.g. `[ffmpeg] https: HTTP error 403 Forbidden` from a YouTube
            // stream URL yt-dlp resolved, or a `ytdl_hook`/`yt-dlp`
            // resolution failure -- was only visible to mpv's internal log
            // and never reached the console, leaving a silent black screen.
            // `all=warn` keeps this to actionable lines rather than
            // info-level chatter on every load.
            init.set_property("terminal", "yes")?;
            init.set_property("msg-level", "all=warn")?;
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("failed to initialize mpv: {e:?}"))?;
        let mpv = Arc::new(mpv);
        let player = Self::from_mpv(mpv)?;
        player.show_idle_screen(IdleScreen::Clock)?;
        Ok(player)
    }

    fn from_mpv(mpv: Arc<Mpv>) -> Result<Self> {
        let state_path = crate::queue::default_state_path();
        if state_path.is_none() {
            warn!(
                "could not resolve a queue state directory (checked CASTOFF_STATE_DIR, \
                 STATE_DIRECTORY, XDG_STATE_HOME, HOME); the play queue will not survive a \
                 restart this run"
            );
        }
        Self::build(mpv, Arc::new(WebpageController::from_env()), state_path)
    }

    /// `from_mpv` with a caller-supplied browser program, so tests can drive
    /// the daemon's real process orchestration (spawn, supersede, terminate,
    /// spontaneous exit) without a compositor or a real engine -- the engine
    /// itself is covered end-to-end by `daemon/tests/webpage_display.rs`. No
    /// queue persistence (`state_path: None`): most tests don't care about it
    /// and every test in this one process would otherwise share (and
    /// clobber) the same default state file. `with_browser_and_state` is for
    /// the tests that do care.
    #[cfg(test)]
    fn with_browser(mpv: Arc<Mpv>, program: &str) -> Result<Self> {
        Self::build(
            mpv,
            Arc::new(WebpageController::with_program(program)),
            None,
        )
    }

    /// `with_browser`, but with queue persistence pointed at `state_path` --
    /// for tests exercising the queue surviving a simulated restart.
    #[cfg(test)]
    fn with_browser_and_state(mpv: Arc<Mpv>, program: &str, state_path: PathBuf) -> Result<Self> {
        Self::build(
            mpv,
            Arc::new(WebpageController::with_program(program)),
            Some(state_path),
        )
    }

    fn build(
        mpv: Arc<Mpv>,
        webpage: Arc<WebpageController>,
        state_path: Option<PathBuf>,
    ) -> Result<Self> {
        let (status_tx, _status_rx) = watch::channel(snapshot_status(&mpv, &webpage));
        let queue = Arc::new(Mutex::new(Queue::load(state_path.as_deref())));
        let (queue_tx, _queue_rx) = watch::channel(queue.lock().unwrap().to_state_message());
        // Fires whenever `IdleScreenController` shows/hides a screen (idle <->
        // active transitions the `Player` methods below don't themselves
        // cover, e.g. the eof-watcher bringing the clock back on a natural
        // end-of-file with nothing queued next, or a Stop/`play_webpage`
        // re-asserting it). Kept as a generic callback so `idle_screen.rs`
        // doesn't need to know about FCast message types.
        let on_change: Arc<dyn Fn() + Send + Sync> = {
            let status_tx = status_tx.clone();
            let mpv = Arc::clone(&mpv);
            let webpage = Arc::clone(&webpage);
            Arc::new(move || {
                let _ = status_tx.send_replace(snapshot_status(&mpv, &webpage));
            })
        };
        let overlay = Arc::new(PlaybackOverlay::new(Arc::clone(&mpv)));
        let operation = Arc::new(Mutex::new(()));
        let routing = Arc::new(Mutex::new(Routing::default()));
        // Tries to advance the play queue instead of letting the eof watcher
        // show the idle screen; built here (not as a `Player` method) because
        // it has to be threaded into `IdleScreenController::new` below,
        // before a `Player` exists to borrow `&self` from. Takes `idle` as a
        // parameter at call time (from the eof watcher, which already holds
        // it) rather than capturing it, for the same reason -- see
        // `auto_advance_queue`.
        let on_eof: crate::idle_screen::OnEof = {
            let mpv = Arc::clone(&mpv);
            let webpage = Arc::clone(&webpage);
            let overlay = Arc::clone(&overlay);
            let routing = Arc::clone(&routing);
            let operation = Arc::clone(&operation);
            let queue = Arc::clone(&queue);
            let status_tx = status_tx.clone();
            let queue_tx = queue_tx.clone();
            let state_path = state_path.clone();
            Arc::new(move |idle: &IdleScreenController| {
                let handles = Handles {
                    mpv: &mpv,
                    webpage: &webpage,
                    overlay: &overlay,
                    idle,
                    routing: &routing,
                };
                auto_advance_queue(
                    &handles,
                    &operation,
                    &queue,
                    state_path.as_deref(),
                    &status_tx,
                    &queue_tx,
                )
            })
        };
        let idle = Arc::new(IdleScreenController::new(
            Arc::clone(&mpv),
            Some(on_change),
            Some(on_eof),
        ));
        idle.spawn_eof_watcher();
        spawn_async_event_watcher(
            &mpv,
            Arc::clone(&idle),
            Arc::clone(&webpage),
            Arc::clone(&routing),
            Arc::clone(&operation),
            status_tx.clone(),
            QueueHandles {
                queue: Arc::clone(&queue),
                state_path: state_path.clone(),
                queue_tx: queue_tx.clone(),
            },
        )?;
        spawn_lifecycle_watcher(
            Arc::clone(&mpv),
            Arc::clone(&idle),
            Arc::clone(&overlay),
            Arc::clone(&webpage),
            status_tx.clone(),
        )?;
        Ok(Self {
            mpv,
            idle,
            overlay,
            webpage,
            operation,
            routing,
            queue,
            state_path,
            status_tx,
            queue_tx,
        })
    }

    /// Subscribe to this player's playback-state-changed broadcast. Every
    /// connected FCast sender (`main.rs`) gets its own receiver so it can push
    /// unprompted `PlaybackUpdate` frames onto its own already-open socket.
    pub fn subscribe_status(&self) -> watch::Receiver<PlaybackUpdateMessage> {
        self.status_tx.subscribe()
    }

    /// Recompute and broadcast the current status to every subscriber. Called
    /// after every state-changing command (`play`/`pause`/`resume`/`stop`/
    /// `seek`/`set_speed`) succeeds; async transitions (`PlaybackRestart`, an
    /// idle-screen show/hide, the browser fallback) publish through their own
    /// paths instead (see `build`'s `on_change`, `fall_back_to_browser` and
    /// `handle_lifecycle_event`).
    fn publish_status(&self) {
        let _ = self.status_tx.send_replace(self.status());
    }

    /// Subscribe to this player's queue-changed broadcast. Every connected
    /// FCast sender (`main.rs`) gets its own receiver so it can push
    /// unprompted `QueueState` frames onto its own already-open socket.
    pub fn subscribe_queue(&self) -> watch::Receiver<QueueStateMessage> {
        self.queue_tx.subscribe()
    }

    /// The current queue as the wire shape sent to FCast senders.
    pub fn queue_state(&self) -> QueueStateMessage {
        self.queue.lock().unwrap().to_state_message()
    }

    /// Recompute and broadcast the current queue to every subscriber. Called
    /// after every queue mutation (`play` enqueueing/starting an item, a
    /// jump, or the eof-triggered auto-advance).
    fn publish_queue_state(&self) {
        let _ = self.queue_tx.send_replace(self.queue_state());
    }

    /// Persist the queue to `self.state_path` (a no-op if that couldn't be
    /// resolved at startup -- see `queue::default_state_path`).
    fn persist_queue(&self) {
        self.queue.lock().unwrap().save(self.state_path.as_deref());
    }

    /// The handles the free play/queue functions below need, borrowed from
    /// this player's own fields (see `Handles`).
    fn handles(&self) -> Handles<'_> {
        Handles {
            mpv: &self.mpv,
            webpage: &self.webpage,
            overlay: &self.overlay,
            idle: &self.idle,
            routing: &self.routing,
        }
    }

    /// Whether nothing is genuinely playing right now, in the sense that
    /// matters for queueing: `play` uses this to decide whether a `Play`
    /// should start immediately (and become the queue's current item) or
    /// only be enqueued behind whatever is already playing.
    ///
    /// A displayed web page always counts as playing. Otherwise, a load
    /// still *loading* (the spinner is up, nothing has reached
    /// `PlaybackRestart` yet -- see `overlay.rs`'s `is_restarted`) counts as
    /// idle, same as the idle clock itself: nothing has committed to playing
    /// anything yet, so a fresh `Play` here supersedes the in-flight load
    /// exactly as it always has (mpv's own `loadfile ... replace`, and the
    /// rapid-re-Play spinner/routing behavior `player.rs`'s tests cover)
    /// rather than queueing behind it. Deliberately checks `is_restarted`,
    /// not `overlay.is_active()`: `is_active()` also covers `reveal`'s
    /// ~400ms cosmetic fade-out *after* `PlaybackRestart` already confirmed
    /// genuine playback, and a `Play` arriving during that fade must queue,
    /// not interrupt.
    fn is_idle(&self) -> bool {
        !self.webpage.is_active()
            && (self.idle.current().is_some()
                || (self.overlay.is_active() && !self.overlay.is_restarted()))
    }

    /// Move to the next/previous queue item (see `queue::Queue::jump_forward`/
    /// `jump_backward`) and, if one exists, play it -- what the Android
    /// task's Next/Previous buttons call (`Opcode::QueueJumpForward`/
    /// `QueueJumpBackward` in `main.rs`). A no-op, not an error, when already
    /// at either edge of the queue.
    pub fn queue_jump_forward(&self) -> Result<()> {
        let _operation = self.operation.lock().unwrap();
        let mut queue = self.queue.lock().unwrap();
        let previous = queue.position();
        let item = queue.jump_forward().cloned();
        drop(queue);
        self.play_jumped_item(item, previous)
    }

    /// See `queue_jump_forward`.
    pub fn queue_jump_backward(&self) -> Result<()> {
        let _operation = self.operation.lock().unwrap();
        let mut queue = self.queue.lock().unwrap();
        let previous = queue.position();
        let item = queue.jump_backward().cloned();
        drop(queue);
        self.play_jumped_item(item, previous)
    }

    /// Shared tail of `queue_jump_forward`/`queue_jump_backward`: publish the
    /// queue's new position and, if a jump actually moved somewhere, play it.
    /// Called with `self.operation` already held. `previous` is the position
    /// the jump moved away from, so a load that fails without ever playing
    /// can be rolled back to it (see `revert_queue_position`) instead of
    /// leaving every client (and the persisted queue) reporting a failed
    /// item as the current one.
    fn play_jumped_item(&self, item: Option<PlayMessage>, previous: Option<usize>) -> Result<()> {
        let Some(item) = item else {
            return Ok(());
        };
        let index = self.queue.lock().unwrap().position();
        self.persist_queue();
        self.publish_queue_state();
        let rollback = index.map(|index| QueueRollback { index, previous });
        let result = execute_play(&self.handles(), &item, rollback);
        if result.is_err() {
            revert_queue_position(
                &self.queue,
                self.state_path.as_deref(),
                &self.queue_tx,
                rollback,
            );
        }
        if result.is_ok() {
            self.publish_status();
        }
        result
    }

    /// Show `screen` (currently only `IdleScreen::Clock`) until the next
    /// `hide_idle_screen`/`show_idle_screen` call.
    pub fn show_idle_screen(&self, screen: IdleScreen) -> Result<()> {
        self.idle.show(screen)
    }

    /// Clear whatever idle screen is currently shown, if any. Not called by
    /// the daemon itself outside of tests any more: `play_media` (a free
    /// function since the play queue was added) hides the idle screen via
    /// its own `Handles` directly. Kept as a `Player` method for tests that
    /// drive idle-screen state without a full `Play`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn hide_idle_screen(&self) -> Result<()> {
        self.idle.hide()
    }

    /// The idle screen currently shown, or `None` while actively playing.
    /// Not read anywhere in the daemon itself today; exists so tests can
    /// observe idle-screen state without inferring it from mpv properties.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn idle_screen(&self) -> Option<IdleScreen> {
        self.idle.current()
    }

    /// Whether a web page is currently displayed by the browser engine.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn webpage_active(&self) -> bool {
        self.webpage.is_active()
    }

    /// Whether the loading spinner is currently up. Not read by the daemon
    /// itself; exists so tests can observe loading state directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn loading_overlay_active(&self) -> bool {
        self.overlay.is_active()
    }

    /// Queue `msg` and, if nothing is currently playing or loading, play it
    /// immediately (it then becomes the queue's current item); otherwise it
    /// is only appended, to be reached by auto-advance or an explicit jump
    /// later (`queue_jump_forward`/`queue_jump_backward`) -- see README's
    /// "Queueing (private extension)". `msg` is validated up front either
    /// way, so an invalid `Play` (e.g. inline `content` with no `url`) is
    /// rejected synchronously rather than silently queued to fail later.
    pub fn play(&self, msg: &PlayMessage) -> Result<()> {
        // Hold the operation lock for the whole cross-resource handoff (see
        // the field's doc comment). The guard is released on every return
        // path, including errors.
        let _operation = self.operation.lock().unwrap();
        validate_play_message(msg)?;

        let should_play_now = self.is_idle();
        let mut rollback = None;
        {
            let mut queue = self.queue.lock().unwrap();
            let index = queue.push(msg.clone());
            if should_play_now {
                rollback = Some(QueueRollback {
                    index,
                    previous: queue.position(),
                });
                queue.set_position(Some(index));
            }
        }
        self.persist_queue();
        self.publish_queue_state();

        let result = if should_play_now {
            execute_play(&self.handles(), msg, rollback)
        } else {
            Ok(())
        };
        if result.is_err() {
            revert_queue_position(
                &self.queue,
                self.state_path.as_deref(),
                &self.queue_tx,
                rollback,
            );
        }
        // Publish on every path (media or web page, played now or only
        // queued). A media load may still report `Idle` at this instant --
        // mpv resolves `idle-active` off this call stack (see README's FCast
        // notes) -- but the async `PlaybackRestart`/idle-screen paths publish
        // the later `Playing`; a web-page `Play` is `Playing` here already.
        if result.is_ok() {
            self.publish_status();
        }
        result
    }

    pub fn pause(&self) -> Result<()> {
        self.mpv
            .set_property("pause", true)
            .map_err(|e| anyhow::anyhow!("pause failed: {e:?}"))?;
        self.publish_status();
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        self.mpv
            .set_property("pause", false)
            .map_err(|e| anyhow::anyhow!("resume failed: {e:?}"))?;
        self.publish_status();
        Ok(())
    }

    /// Stop playback and return to mpv's idle state (no decode pipeline
    /// running), fading the old video out and the idle clock in rather than
    /// cutting straight to it, and taking a displayed web page down too
    /// (waiting for the engine to be gone) so the screen really is the
    /// daemon's again when this returns. A Stop while already idle and not
    /// loading is a no-op transition (just re-asserts the clock), so it
    /// doesn't blink the screen.
    ///
    /// "Already idle" is signalled by the idle clock being on screen, not by
    /// mpv's `idle-active`: with `keep-open=yes` (see `new()`) a clip that
    /// reached end-of-file on its own is not `idle-active` -- mpv stays paused
    /// on the last frame -- even though the eof watcher has already brought
    /// the clock back. Keying off `idle-active` there would conceal the
    /// visible clock to black and fade it straight back in: an ~800ms blink
    /// for a Stop that changes nothing. `fade_in_idle_clock` also cannot be
    /// used while the clock is already shown -- its `render_at` contract
    /// forbids running alongside `show`'s refresh thread -- so taking the
    /// no-op branch here is what keeps that path structurally out of reach.
    pub fn stop(&self) -> Result<()> {
        let _operation = self.operation.lock().unwrap();
        // A `Play` still being decided is over too: without this, an error
        // arriving for it right after a `Stop` would open the browser on a
        // cast the sender already stopped. The entries are cancelled, not
        // dropped -- see `Routing::cancel`.
        self.routing.lock().unwrap().cancel();
        self.webpage.hide();
        let clock_showing = self.idle.current().is_some();
        let was_loading = self.overlay.is_active();
        let already_idle = clock_showing && !was_loading;
        if !already_idle && !was_loading {
            if let Err(e) = self.overlay.conceal() {
                let _ = abort_loading_to_idle(&self.handles());
                return Err(e);
            }
        }
        if let Err(e) = self.mpv.command("stop", &[]) {
            let _ = abort_loading_to_idle(&self.handles());
            return Err(anyhow::anyhow!("stop failed: {e:?}"));
        }
        let result = if already_idle {
            self.show_idle_screen(IdleScreen::Clock)
        } else {
            fade_in_idle_clock(&self.handles())
        };
        // Both branches already publish via `IdleScreenController`'s
        // `on_change` callback (`show_idle_screen`/`fade_in_idle_clock` end by
        // showing the clock), but publish explicitly too so `stop` doesn't
        // rely on that indirection to notify subscribers.
        if result.is_ok() {
            self.publish_status();
        }
        result
    }

    pub fn seek(&self, time: f64) -> Result<()> {
        self.mpv
            .command("seek", &[&time.to_string(), "absolute"])
            .map_err(|e| anyhow::anyhow!("seek failed: {e:?}"))?;
        self.publish_status();
        Ok(())
    }

    pub fn set_volume(&self, volume: f64) -> Result<()> {
        self.mpv
            .set_property("volume", to_mpv_volume(volume))
            .map_err(|e| anyhow::anyhow!("set_volume failed: {e:?}"))
    }

    pub fn set_speed(&self, speed: f64) -> Result<()> {
        self.mpv
            .set_property("speed", speed)
            .map_err(|e| anyhow::anyhow!("set_speed failed: {e:?}"))?;
        self.publish_status();
        Ok(())
    }

    /// Snapshot current mpv state as an FCast PlaybackUpdate.
    pub fn status(&self) -> PlaybackUpdateMessage {
        snapshot_status(&self.mpv, &self.webpage)
    }

    /// Current volume on FCast's 0.0-1.0 scale.
    pub fn volume(&self) -> f64 {
        let v: f64 = self.mpv.get_property("volume").unwrap_or(0.0);
        v / 100.0
    }
}

/// `msg`'s validity as a `Play`, independent of whether it ends up playing
/// immediately or only queued (see `Player::play`): a webpage target needs a
/// `url`, and the media path (explicit or the daemon's own probe) needs a
/// `url` or rejects inline `content`-only messages outright (not yet
/// supported -- see README's roadmap).
fn validate_play_message(msg: &PlayMessage) -> Result<()> {
    if msg.explicit_target() == Some(PlayTarget::Webpage) {
        if msg.url.is_none() {
            anyhow::bail!(
                "Play message targets a web page (container {:?}) but has no `url`; \
                 pass the page's http(s):// or file:// URL",
                msg.container.as_deref().unwrap_or("")
            );
        }
        return Ok(());
    }
    match (msg.url.as_deref(), msg.content.as_deref()) {
        (Some(_), _) => Ok(()),
        (None, Some(_)) => anyhow::bail!(
            "Play message carries inline `content` (e.g. a DASH manifest) with no `url`; \
             inline manifest playback is not yet supported"
        ),
        (None, None) => anyhow::bail!("Play message has neither `url` nor `content`"),
    }
}

/// Dispatch a (already-validated) `Play` to the media or web-page path,
/// exactly as `Player::play` did inline before the play queue existed. Used
/// both by `Player::play`'s "play now" branch and by the queue paths that
/// replay an already-queued item (`Player::play_jumped_item`,
/// `auto_advance_queue`) -- a free function (not a `Player` method) because
/// the latter runs before a `Player` exists (see `Handles`).
fn execute_play(
    h: &Handles,
    msg: &PlayMessage,
    queue_rollback: Option<QueueRollback>,
) -> Result<()> {
    match msg.explicit_target() {
        // The sender said what this is: honour it, with no fallback.
        Some(PlayTarget::Webpage) => {
            let url = msg
                .url
                .as_deref()
                .expect("validate_play_message guarantees a webpage Play has a url");
            // A direct page takes the screen immediately; any media attempt
            // still being decided is cancelled (its own mpv event still
            // resolves its entry, so it cannot resolve a later load).
            h.routing.lock().unwrap().cancel();
            play_webpage(h, url)
        }
        // The sender classified this as media: honour it, with no fallback,
        // but still track the submission so its own events are attributed to
        // it and can never resolve an older probe.
        Some(PlayTarget::Media) => play_media(h, msg, false, queue_rollback),
        // The sender did not say (the common case: an app that only knows a
        // URL). The daemon decides for itself: try mpv first -- which is
        // what makes YouTube, every other yt-dlp-supported source and plain
        // media work with no client help -- and let
        // `spawn_async_event_watcher` hand the URL to the browser if that
        // attempt fails before the file loads. See README's routing rules.
        None => play_media(h, msg, true, queue_rollback),
    }
}

/// The media path: hand `msg`'s URL to mpv. `probing` is true when the
/// sender did not classify the URL, i.e. this load is the daemon's own
/// media-vs-web-page probe (see `Routing`). `queue_rollback`, if this load is
/// the play queue's new current item, travels with the submission so a
/// later async failure (see `spawn_async_event_watcher`) can undo the
/// position it already committed.
fn play_media(
    h: &Handles,
    msg: &PlayMessage,
    probing: bool,
    queue_rollback: Option<QueueRollback>,
) -> Result<()> {
    let target = match (msg.url.as_deref(), msg.content.as_deref()) {
        (Some(url), _) => url,
        (None, Some(_)) => anyhow::bail!(
            "Play message carries inline `content` (e.g. a DASH manifest) with no `url`; \
             inline manifest playback is not yet supported"
        ),
        (None, None) => anyhow::bail!("Play message has neither `url` nor `content`"),
    };
    // A page the browser engine is showing must not stay on top of the
    // media: take it down before mpv takes the screen.
    h.webpage.hide();
    // Fade the old content (previous video or the idle clock) out to black,
    // then put the spinner up over it, *before* submitting the load: the
    // spinner must be visible for the whole wait, so it can't be raced by an
    // instant `PlaybackRestart` from a fast load. It stays up until
    // `spawn_lifecycle_watcher` sees playback genuinely restart (or the load
    // fail), and redraws only until then. Any failure here rolls the overlay
    // back to the idle clock, so a partial setup can't leave an opaque
    // overlay with no spinner thread and no watcher event coming to clear it.
    let begin_loading = || -> Result<()> {
        h.overlay.conceal()?;
        h.idle.hide()?;
        h.overlay.spawn_spinner()
    };
    if let Err(e) = begin_loading() {
        let _ = abort_loading_to_idle(h);
        return Err(e);
    }
    if let Err(e) = h.mpv.command("loadfile", &[target, "replace"]) {
        // Nothing will load, so no async error/restart is coming to take the
        // spinner down; do it here and fall back to the idle clock.
        let _ = abort_loading_to_idle(h);
        return Err(anyhow::anyhow!("loadfile failed for url {target:?}: {e:?}"));
    }
    // `loadfile` synchronously creates a playlist entry for the URL; read
    // back its id so the background event watcher can attribute this load's
    // `FileLoaded`/`EndFile` events to that exact entry instead of to
    // whichever entry mpv reports next. (A playlist URL may already have
    // been expanded by the time this runs; the id read then belongs to the
    // first expanded entry, which is still this cast's media, and mpv's
    // events for the remaining entries no longer match this submission.)
    // Every caller holds the operation lock across this and the `loadfile`
    // call, so the watcher cannot drain an event for it before the entry
    // exists.
    let entry_id = match h.mpv.get_property::<i64>("playlist/0/id") {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(
                error = ?e,
                url = target,
                "could not read mpv's playlist entry id for the submitted load; this URL \
                 will not be eligible for the web-page fallback"
            );
            None
        }
    };
    h.routing
        .lock()
        .unwrap()
        .submit(target, probing, entry_id, queue_rollback);
    // `keep-open=yes` (see `new()`) leaves `pause` set to `true` once a
    // previous file hits EOF, and mpv does not reset that property on the
    // next `loadfile`. Without this, a second Play call loads the new file
    // but stays paused on its first frame forever: silent, endless black
    // screen with no error, since `time-pos` never advances past 0.
    if let Err(e) = h.mpv.set_property("pause", false) {
        // The load is queued but nothing guarantees a `PlaybackRestart`
        // (playback is still paused), so don't leave the spinner up over the
        // opaque fade: clear it and fall back to the idle clock.
        let _ = abort_loading_to_idle(h);
        return Err(anyhow::anyhow!("failed to unpause after loadfile: {e:?}"));
    }
    if let Some(time) = msg.time {
        let _ = h.mpv.set_property("start", time);
    }
    if let Some(volume) = msg.volume {
        let _ = h.mpv.set_property("volume", to_mpv_volume(volume));
    }
    if let Some(speed) = msg.speed {
        let _ = h.mpv.set_property("speed", speed);
    }
    Ok(())
}

/// Hand the screen to the browser engine (`webpage`) while keeping the
/// daemon's own idle machinery running behind it: mpv stops (decode pipeline
/// dormant, boat power) and shows the idle clock, which is what Cage reveals
/// again when the page is stopped -- or when the engine exits by itself.
fn play_webpage(h: &Handles, url: &str) -> Result<()> {
    // Bring the engine up before touching mpv, so a failure to start one (no
    // browser installed) is reported to the sender and leaves mpv's
    // playback (if any) untouched.
    h.webpage.show(url)?;
    h.mpv
        .command("stop", &[])
        .map_err(|e| anyhow::anyhow!("stop before displaying a web page failed: {e:?}"))?;
    // A media `Play` this page supersedes may have left the loading overlay
    // (and its redraw thread) up. Take it down before drawing the clock that
    // sits behind the page: otherwise the spinner would keep redrawing over
    // the page -- and over the idle clock after the engine exits -- at its
    // animation cadence, forever. `clear` is a no-op when nothing is up (see
    // `overlay.rs`).
    h.overlay.clear()?;
    h.idle.show(IdleScreen::Clock)
}

/// Fade the idle clock in from black: drop the cover overlays (the screen
/// behind is already black from `conceal` or the loading overlay), ramp the
/// clock's own OSD alpha up, then install it as the current idle screen. The
/// clock cannot be revealed by fading a cover rect away: mpv stacks overlays
/// by recency, so a clock re-created after the rect would sit above it and
/// pop in instead of fading.
fn fade_in_idle_clock(h: &Handles) -> Result<()> {
    // The clock is already the current screen, so `show`'s refresh thread is
    // running and `render_at` must not be used alongside it (see its
    // contract); there is also nothing to fade. Drop any overlay and
    // re-assert the clock. This is the case a Stop after a clip reached
    // end-of-file, or a `conceal` failure before `hide`, lands in -- keeping
    // the alpha-fade path structurally out of reach rather than relying on a
    // timing assumption.
    if h.idle.current().is_some() {
        h.overlay.clear()?;
        return h.idle.show(IdleScreen::Clock);
    }
    // Cancel/remove only the spinner; the opaque fade rect stays as the
    // black backdrop (a not-yet-cleared video frame must not flash through).
    // Draw the clock above it at zero opacity, ramp that alpha up, and only
    // then drop the rect -- by then the clock's own opaque background covers
    // the canvas, so removing the rect is invisible.
    let fade = || -> Result<()> {
        h.overlay.stop_spinner()?;
        h.idle.render_at(IdleScreen::Clock, 0)?;
        h.overlay
            .fade_in(|opacity| h.idle.render_at(IdleScreen::Clock, opacity))?;
        h.overlay.clear()?;
        h.idle.show(IdleScreen::Clock)
    };
    if let Err(e) = fade() {
        // A failure partway through (spinner teardown, an OSD alpha draw, or
        // dropping the opaque rect) would otherwise leave the overlay active
        // with the black rect up and no watcher event coming to clear it:
        // the screen stays opaque black until the next command. Roll back to
        // a visible clock, best-effort.
        let _ = h.overlay.clear();
        let _ = h.idle.show(IdleScreen::Clock);
        return Err(e);
    }
    Ok(())
}

/// A load ended without ever starting playback: cancel the spinner (if it's
/// up) and fade back to the idle clock, so a failed Play ends on the idle
/// screen with the console error report rather than an endless spinner.
/// Used by the synchronous error paths in `play_media`/`Player::stop`;
/// clearing the overlay and showing the clock are both idempotent, so it is
/// safe to call even when the overlay has already cleared itself.
fn abort_loading_to_idle(h: &Handles) -> Result<()> {
    fade_in_idle_clock(h)
}

/// Called by the idle-screen eof watcher (`idle_screen.rs`'s `on_eof`) when
/// mpv reaches end-of-file on its own, before it would otherwise show the
/// idle clock: tries to advance to the next queued item instead. Returns
/// whether it found (and successfully submitted a load for) one, so the eof
/// watcher knows whether to fall back to showing the idle clock itself.
///
/// Takes the same field handles `Player`'s own methods use (`Handles`)
/// rather than `&Player`, because this is built and threaded into
/// `IdleScreenController::new` in `Player::build`, before a `Player` exists
/// to borrow from.
fn auto_advance_queue(
    h: &Handles,
    operation: &Mutex<()>,
    queue: &Mutex<Queue>,
    state_path: Option<&Path>,
    status_tx: &watch::Sender<PlaybackUpdateMessage>,
    queue_tx: &watch::Sender<QueueStateMessage>,
) -> bool {
    let _operation = operation.lock().unwrap();
    let previous = { queue.lock().unwrap().position() };
    let next = { queue.lock().unwrap().jump_forward().cloned() };
    let Some(item) = next else {
        return false;
    };
    let index = { queue.lock().unwrap().position() };
    {
        let q = queue.lock().unwrap();
        q.save(state_path);
        let _ = queue_tx.send_replace(q.to_state_message());
    }
    let rollback = index.map(|index| QueueRollback { index, previous });
    if let Err(e) = execute_play(h, &item, rollback) {
        error!(
            error = %e,
            url = item.url.as_deref().unwrap_or(""),
            "queue auto-advance failed to start the next item; returning to the idle clock"
        );
        revert_queue_position(queue, state_path, queue_tx, rollback);
        return false;
    }
    let _ = status_tx.send_replace(snapshot_status(h.mpv, h.webpage));
    true
}

/// Revert the queue's current position back to `rollback.previous` if it
/// still points at the position the failed load was submitted as -- i.e.
/// nothing else (a jump, or a newer `Play`) has moved the queue on since.
/// Shared by every path that resolves a load which became the queue's
/// current item without ever playing: `Player::play`/`play_jumped_item`'s
/// own synchronous failures, `auto_advance_queue`'s, and the asynchronous
/// ones `spawn_async_event_watcher`/`fall_back_to_browser` resolve well
/// after the position was already committed and broadcast.
fn revert_queue_position(
    queue: &Mutex<Queue>,
    state_path: Option<&Path>,
    queue_tx: &watch::Sender<QueueStateMessage>,
    rollback: Option<QueueRollback>,
) {
    let Some(rollback) = rollback else { return };
    let mut queue = queue.lock().unwrap();
    if queue.position() != Some(rollback.index) {
        return;
    }
    queue.set_position(rollback.previous);
    queue.save(state_path);
    let _ = queue_tx.send_replace(queue.to_state_message());
}

fn to_mpv_volume(fcast_volume: f64) -> f64 {
    (fcast_volume.clamp(0.0, 1.0)) * 100.0
}

/// Snapshot `mpv`'s current state (plus whether the browser engine is
/// showing a page) as an FCast `PlaybackUpdate`. Takes the handles rather
/// than `&Player` so the contexts that only hold those handles -- the
/// idle-screen `on_change` callback, the async-event watcher's browser
/// fallback, and the lifecycle watcher thread -- can publish the exact same
/// shape as `Player::status`.
fn snapshot_status(mpv: &Mpv, webpage: &WebpageController) -> PlaybackUpdateMessage {
    let paused: bool = mpv.get_property("pause").unwrap_or(false);
    let idle: bool = mpv.get_property("idle-active").unwrap_or(true);
    let time: Option<f64> = mpv.get_property("time-pos").ok();
    let duration: Option<f64> = mpv.get_property("duration").ok();
    let speed: Option<f64> = mpv.get_property("speed").ok();
    let webpage_active = webpage.is_active();

    let state = if webpage_active {
        // A displayed web page is live content the sender asked for, not
        // the idle screen; mpv is only carrying the clock *behind* the
        // engine's window (see `play_webpage`), so mpv's own state must
        // not make this look like nothing is playing.
        PlaybackState::Playing
    } else if idle {
        PlaybackState::Idle
    } else if paused {
        PlaybackState::Paused
    } else {
        PlaybackState::Playing
    };

    PlaybackUpdateMessage {
        generation_time: now_millis(),
        state,
        // A web page has no mpv timeline to report.
        time: if webpage_active { None } else { time },
        duration: if webpage_active { None } else { duration },
        speed,
    }
}

/// Spawn a background thread that handles mpv's *asynchronous* load
/// outcomes -- the ones `Player::play`'s immediate `loadfile` call can't see,
/// because `loadfile` only queues the load; mpv resolves/opens the target
/// (including running `ytdl_hook`'s `yt-dlp` subprocess for a YouTube URL)
/// afterwards, off of that call stack.
///
/// What an error means is decided against `Routing`'s in-flight submission,
/// attributed by mpv's `playlist_entry_id` (read from the raw
/// `MPV_EVENT_END_FILE`; libmpv2's safe `Event::EndFile` drops the id):
///
/// - The error belongs to the submitted load, that load was a probe (a `Play`
///   whose sender did not say what the URL is, see `Routing`), and it never
///   loaded: it answers the daemon's own question -- this URL is not media --
///   so the URL is handed to the browser engine instead, logged at `warn`
///   because it is a normal outcome for a web page.
/// - Otherwise (an explicit media `Play`, or a load that had already loaded)
///   it is a genuine playback error, logged at `error`, with
///   `describe_playback_error`'s plain-language reason next to mpv's own log
///   line (printed because `terminal=yes`, see `new()`), so the console
///   always says what went wrong instead of leaving a silent black screen.
/// - An error for an entry the daemon did not submit -- a load a newer `Play`
///   superseded, or an extra entry mpv expanded a playlist URL into -- belongs
///   to no tracked submission, so it cannot be attributed to the URL the
///   daemon is now on and is logged at `debug` by entry id alone.
///
/// This blocks on mpv's event queue (`mpv_wait_event(-1.0)`) rather than
/// polling, so it costs nothing until mpv actually has something to report,
/// consistent with the daemon's power-efficiency design principle. It uses
/// a second client handle from `Mpv::create_client` (its own independent
/// event queue onto the same player core, and therefore an independent
/// handle commands can still be issued from) so it never contends with the
/// `Player` methods' direct use of `mpv` from other threads.
///
/// The raw `mpv_wait_event` (rather than libmpv2's safe `wait_event`) is what
/// makes the id available at all: the safe enum keeps only the end reason and
/// error code, and drops `playlist_entry_id`.
fn spawn_async_event_watcher(
    mpv: &Mpv,
    idle: Arc<IdleScreenController>,
    webpage: Arc<WebpageController>,
    routing: Arc<Mutex<Routing>>,
    operation: Arc<Mutex<()>>,
    status_tx: watch::Sender<PlaybackUpdateMessage>,
    queue: QueueHandles,
) -> Result<()> {
    let events = mpv
        .create_client(Some("castoff-event-watcher"))
        .map_err(|e| anyhow::anyhow!("failed to create mpv event client: {e:?}"))?;
    // A fresh client doesn't necessarily have these enabled; ask explicitly
    // for the ones this watcher depends on.
    for event in [
        mpv_event_id::StartFile,
        mpv_event_id::EndFile,
        mpv_event_id::FileLoaded,
    ] {
        events
            .enable_event(event)
            .map_err(|e| anyhow::anyhow!("failed to enable mpv event: {e:?}"))?;
    }
    std::thread::spawn(move || loop {
        // SAFETY: `events` owns a live mpv handle. The returned pointer is
        // valid until the next `mpv_wait_event` call on that handle, which
        // cannot happen until this iteration has finished using it (one
        // thread, one call per iteration).
        let raw = unsafe { libmpv2_sys::mpv_wait_event(events.ctx.as_ptr(), -1.0) };
        if raw.is_null() {
            continue;
        }
        let event = unsafe { &*raw };
        match event.event_id {
            libmpv2_sys::mpv_event_id_MPV_EVENT_SHUTDOWN => break,
            // A file is starting: remember its playlist entry id so the
            // `FileLoaded` that follows (which carries no id of its own) can
            // be attributed to it.
            libmpv2_sys::mpv_event_id_MPV_EVENT_START_FILE => {
                let start = unsafe { &*(event.data as *const libmpv2_sys::mpv_event_start_file) };
                routing.lock().unwrap().start_file(start.playlist_entry_id);
            }
            // mpv opened a file: if it is the submitted load, it is media
            // from here on. This is what makes "once mpv reports the file
            // loaded, a failure later -- even before the first frame -- is a
            // playback error, never re-routed" (README) true. The operation
            // lock keeps this from being attributed to a load `play_media`
            // has submitted to mpv but not yet recorded in `Routing`.
            libmpv2_sys::mpv_event_id_MPV_EVENT_FILE_LOADED => {
                let _operation = operation.lock().unwrap();
                routing.lock().unwrap().file_loaded();
            }
            libmpv2_sys::mpv_event_id_MPV_EVENT_END_FILE => {
                // SAFETY: for `MPV_EVENT_END_FILE`, `data` points to an
                // `mpv_event_end_file` (see mpv's client.h).
                let end = unsafe { &*(event.data as *const libmpv2_sys::mpv_event_end_file) };
                if end.error != 0 {
                    // Hold the operation lock for the whole resolution: a
                    // fallback cannot then interleave with a newer
                    // `play`/`stop` (see `Player::operation`).
                    let _operation = operation.lock().unwrap();
                    let error = libmpv2::Error::Raw(end.error);
                    match routing.lock().unwrap().resolve_error(end.playlist_entry_id) {
                        Some(Resolution::FallBack(url, rollback)) => fall_back_to_browser(
                            &url,
                            &error,
                            &events,
                            &idle,
                            &webpage,
                            &status_tx,
                            QueueContext {
                                queue: &queue.queue,
                                state_path: queue.state_path.as_deref(),
                                queue_tx: &queue.queue_tx,
                                rollback,
                            },
                        ),
                        Some(Resolution::PlaybackError(url, rollback)) => {
                            error!(
                                url,
                                error = ?error,
                                reason = describe_playback_error(&error),
                                "mpv reported an async playback error -- playback did not start; \
                                 see the mpv log line(s) above for the underlying cause (e.g. a \
                                 ytdl_hook/yt-dlp resolution failure or an HTTP error from the \
                                 media/CDN host)"
                            );
                            revert_queue_position(
                                &queue.queue,
                                queue.state_path.as_deref(),
                                &queue.queue_tx,
                                rollback,
                            );
                        }
                        // An error for an entry the daemon did not submit
                        // (a load a newer Play superseded, or an extra entry
                        // mpv expanded a playlist URL into): it belongs to no
                        // tracked submission, so it is not a playback error
                        // for the URL the daemon is now on and must not be
                        // logged as one.
                        None => debug!(
                            playlist_entry_id = end.playlist_entry_id,
                            error = ?error,
                            reason = describe_playback_error(&error),
                            "mpv reported a load error for an entry the daemon is no longer \
                             tracking (a superseded load or a playlist entry it expanded); see \
                             the mpv log line(s) above for the underlying cause"
                        ),
                    }
                } else {
                    // The clean end (end-of-file, a playlist redirect, or a
                    // later `loadfile`/`stop`) resolves the submitted load.
                    // Same lock as `FileLoaded`: it must not be attributed to
                    // a submission `play_media` has not recorded yet.
                    let _operation = operation.lock().unwrap();
                    routing.lock().unwrap().resolve_end(end.playlist_entry_id);
                }
            }
            _ => {}
        }
    });
    Ok(())
}

/// The daemon's own routing decision came back "not media": display `url` in
/// the browser engine instead, keeping the console honest about both the
/// failed attempt and the outcome.
///
/// Called from `spawn_async_event_watcher` with the `operation` lock already
/// held (see `Player::operation`), so the browser swap cannot interleave with
/// a newer `play`/`stop`.
fn fall_back_to_browser(
    url: &str,
    mpv_error: &libmpv2::Error,
    mpv: &Mpv,
    idle: &IdleScreenController,
    webpage: &WebpageController,
    status_tx: &watch::Sender<PlaybackUpdateMessage>,
    queue: QueueContext,
) {
    warn!(
        url,
        error = ?mpv_error,
        reason = describe_playback_error(mpv_error),
        "URL is not playable as media; displaying it as a web page instead"
    );
    // Whatever mpv was doing is over; put the idle clock back so it is behind
    // the page, and stays there if the engine cannot be started either.
    let _ = mpv.command("stop", &[]);
    let _ = idle.show(IdleScreen::Clock);
    if let Err(e) = webpage.show(url) {
        error!(
            url,
            error = %e,
            media_error = ?mpv_error,
            "URL could not be played as media and could not be displayed as a web page \
             either; the screen is back to the daemon's idle clock"
        );
        revert_queue_position(
            queue.queue,
            queue.state_path,
            queue.queue_tx,
            queue.rollback,
        );
    }
    // This path never goes through a `Player` method: a successful `show`
    // flips `webpage.is_active()` from false to true, which is the
    // difference between `Idle` and `Playing` in `snapshot_status`. Without
    // this publish a subscriber would stay at the `Idle` status the
    // `idle.show` just above sent, and -- since the push task only starts
    // its ~1s tick once it has seen `Playing` -- would never be corrected
    // until some unrelated later state change.
    let _ = status_tx.send_replace(snapshot_status(mpv, webpage));
}

/// Spawn the background thread that tracks *playback lifecycle* on a second
/// mpv client handle: it takes the loading spinner down the moment mpv
/// reports that playback actually restarted (`PlaybackRestart` -- the first
/// frame is ready and rendering, not merely that `loadfile` was queued), and
/// hands the screen back to the idle clock if the load fails instead.
///
/// Like `spawn_async_event_watcher`, this blocks on `wait_event(-1.0)` with no
/// polling, and uses its own client so it doesn't contend with the idle
/// clock's eof watcher (which owns the main handle's event queue) or with
/// the async event watcher. A failed load surfaces here as `Some(Err(..))`
/// (mpv's `END_FILE` with an error code); an `END_FILE` at EOF while the
/// spinner is still up is a load that never reached `PlaybackRestart` and
/// also returns to idle. The STOP/REDIRECT end reasons a superseding
/// `loadfile` produces are ignored, so a rapid re-Play keeps its own spinner.
fn spawn_lifecycle_watcher(
    mpv: Arc<Mpv>,
    idle: Arc<IdleScreenController>,
    overlay: Arc<PlaybackOverlay>,
    webpage: Arc<WebpageController>,
    status_tx: watch::Sender<PlaybackUpdateMessage>,
) -> Result<()> {
    let events = mpv
        .create_client(Some("castoff-lifecycle"))
        .map_err(|e| anyhow::anyhow!("failed to create mpv lifecycle client: {e:?}"))?;
    // A fresh client doesn't necessarily have these enabled; ask explicitly
    // for the two this watcher depends on.
    for event in [
        libmpv2::events::mpv_event_id::PlaybackRestart,
        libmpv2::events::mpv_event_id::EndFile,
    ] {
        events
            .enable_event(event)
            .map_err(|e| anyhow::anyhow!("failed to enable mpv event: {e:?}"))?;
    }
    std::thread::spawn(move || loop {
        match events.wait_event(-1.0) {
            Some(Ok(event)) => {
                if !handle_lifecycle_event(event, &idle, &overlay, &mpv, &webpage, &status_tx) {
                    return;
                }
            }
            // `wait_event` surfaces an `END_FILE` with a nonzero error code as
            // `Err`; the concrete reason is already logged by
            // `spawn_async_event_watcher`. A failed load must never leave the
            // spinner up forever.
            Some(Err(_)) => restore_idle_clock(&idle, &overlay),
            None => {}
        }
    });
    Ok(())
}

/// Put the idle clock back and tear the spinner down because the in-flight
/// load is not going to start. A no-op when nothing is showing.
fn restore_idle_clock(idle: &IdleScreenController, overlay: &PlaybackOverlay) {
    if !overlay.is_active() {
        return;
    }
    // See `Player::fade_in_idle_clock`: keep the opaque rect as the backdrop,
    // fade the clock in above it, then drop the rect.
    let _ = overlay.stop_spinner();
    let _ = idle.render_at(IdleScreen::Clock, 0);
    let _ = overlay.fade_in(|opacity| idle.render_at(IdleScreen::Clock, opacity));
    let _ = overlay.clear();
    let _ = idle.show(IdleScreen::Clock);
}

/// Dispatch one lifecycle event; returns `false` when the watcher should stop
/// (mpv shutdown). A file that reaches end-of-file without ever emitting
/// `PlaybackRestart` is a load that never started rendering and returns to the
/// idle clock; STOP/REDIRECT are deliberately ignored because those are what a
/// superseding `loadfile` produces for the load being replaced, and clearing on
/// them would kill the newer load's spinner.
fn handle_lifecycle_event(
    event: Event<'_>,
    idle: &IdleScreenController,
    overlay: &PlaybackOverlay,
    mpv: &Mpv,
    webpage: &WebpageController,
    status_tx: &watch::Sender<PlaybackUpdateMessage>,
) -> bool {
    match event {
        // First frame is ready: stop spinning and fade the black overlay away
        // to reveal playback. This is the one state transition into Playing
        // that doesn't go through `IdleScreenController` (it neither shows
        // nor hides an idle screen), so it must publish explicitly --
        // everywhere else, a `Player` method's own `publish_status` call or
        // the idle-screen `on_change` callback already covers it.
        Event::PlaybackRestart => {
            overlay.mark_restarted();
            let _ = overlay.reveal();
            let _ = status_tx.send_replace(snapshot_status(mpv, webpage));
        }
        Event::EndFile(libmpv2::mpv_end_file_reason::Eof) => restore_idle_clock(idle, overlay),
        Event::Shutdown => return false,
        _ => {}
    }
    true
}

fn describe_playback_error(e: &libmpv2::Error) -> &'static str {
    use libmpv2::mpv_error;
    // Only the codes a failed network/media load actually produces are named
    // specially; anything else falls back to pointing at mpv's own log line.
    match e {
        libmpv2::Error::Raw(mpv_error::NothingToPlay) => {
            "nothing to play: mpv could not open any stream the URL resolved to \
             (the mpv log line above has the concrete cause, e.g. an HTTP 403)"
        }
        libmpv2::Error::Raw(mpv_error::LoadingFailed) => {
            "loading failed: mpv could not load/open this URL"
        }
        _ => "see the mpv log line above for the concrete cause",
    }
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A headless mpv core (`vo=null`/`ao=null`, no window or audio device),
/// sufficient to drive real `Player` behavior in a sandbox with no display
/// or sound hardware. Mirrors `Player::new`'s `keep-open` setting (needed for
/// the double-play regression test). `pub(crate)`, not private to this
/// module's own `tests` submodule, so `main.rs`'s tests can build a real
/// headless `Player` too rather than needing a second mock.
///
/// Two concurrency hazards showed up as this suite grew to two dozen-plus
/// tests that each spin up one of these real cores: (1) libass's default
/// `auto` OSD font provider (used by the idle clock/spinner overlays,
/// `overlay.rs`) queries fontconfig, whose on-demand cache build is not safe
/// against many threads racing its first initialization at once -- in a
/// sandbox with no writable font cache directory (`nix build`'s checkPhase:
/// `HOME=/homeless-shelter`, no `/var/cache/fontconfig`) that reliably
/// crashed the whole test binary with a bare SIGSEGV inside fontconfig; (2)
/// even past that, many real mpv cores genuinely *alive* at once (not just
/// being created at once) still crashed the binary with an unrelated bare
/// SIGSEGV inside libmpv/ffmpeg, reproduced under both `nix build`'s sandbox
/// and plain `cargo test --release` outside it. `osd-font-provider=none`
/// below removes the fontconfig dependency entirely (tests never assert on
/// rendered glyphs -- no display to capture pixels from either, per
/// `AGENTS.md` -- only on overlay command success and state transitions, so
/// embedded/built-in fonts are sufficient); `.cargo/config.toml`'s
/// `RUST_TEST_THREADS=1` addresses the second hazard by never letting two of
/// these real cores be alive concurrently in the first place, which was
/// confirmed to make the whole suite pass reliably. Production
/// (`Player::new`) keeps the default `auto` font provider so the appliance
/// still renders with a real matched system font.
#[cfg(test)]
pub(crate) fn headless_mpv() -> Arc<Mpv> {
    Arc::new(
        Mpv::with_initializer(|init| {
            init.set_property("vo", "null")?;
            init.set_property("ao", "null")?;
            init.set_property("idle", "yes")?;
            init.set_property("keep-open", "yes")?;
            init.set_property("osd-font-provider", "none")?;
            Ok(())
        })
        .expect("failed to initialize headless mpv for test"),
    )
}

/// A `Player` over [`headless_mpv`], with the real (Chromium) browser engine
/// configured but never started by these tests, and its initial idle screen
/// showing (needed by the idle-screen test). `pub(crate)` for `main.rs`'s
/// push-path tests (see [`headless_mpv`]).
#[cfg(test)]
pub(crate) fn headless_player() -> Player {
    // No browser program that could exist: a test that accidentally trips
    // the unrouted-Play fallback fails to start an engine (and the watcher
    // logs it) instead of launching a real Chromium.
    let player = Player::with_browser(headless_mpv(), "/nonexistent/castoff-test-browser")
        .expect("create player");
    player
        .show_idle_screen(IdleScreen::Clock)
        .expect("show initial idle screen");
    player
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::*;

    /// [`headless_player`] with a caller-supplied browser engine program
    /// (see `stub_browser`) instead of Chromium.
    fn headless_player_with_browser(program: &Path) -> Player {
        let player = Player::with_browser(
            headless_mpv(),
            program.to_str().expect("stub browser path must be UTF-8"),
        )
        .expect("create player");
        player
            .show_idle_screen(IdleScreen::Clock)
            .expect("show initial idle screen");
        player
    }

    /// A fresh scratch directory: tests share one process, so stub files
    /// must not be shared between them.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("castoff-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// Writes an executable stub standing in for the browser engine: it
    /// appends `"<pid> <argv...>"` to `stub.log` and then runs `body` (e.g.
    /// `sleep 300` to be terminated later, or `exit 0` to vanish). Lets these
    /// tests drive the daemon's real process orchestration -- spawn,
    /// supersede, terminate, spontaneous exit -- without a compositor; the
    /// real engine is covered end-to-end by `daemon/tests/webpage_display.rs`.
    fn stub_browser(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$$ $@\" >> \"{log}\"\n{body}\n",
                log = dir.join("stub.log").display(),
            ),
        )
        .expect("write stub browser");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make stub browser executable");
        script
    }

    /// Block until the stub browser has recorded its invocation(s), then
    /// return the log file's contents.
    fn wait_for_stub_log(dir: &Path) -> String {
        let log = dir.join("stub.log");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(contents) = std::fs::read_to_string(&log) {
                if !contents.trim().is_empty() {
                    return contents;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the stub browser engine was never started");
    }

    /// Poll the stub log until it contains `needle`; panics on timeout.
    fn wait_for_log_contains(dir: &Path, needle: &str) {
        let log = dir.join("stub.log");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let seen = std::fs::read_to_string(&log)
                .map(|contents| contents.contains(needle))
                .unwrap_or(false);
            if seen {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the stub browser log never contained {needle:?}");
    }

    /// Like [`stub_browser`], but the stub records the SIGTERM it receives and
    /// lingers `term_delay_secs` before exiting, so a test can hold one
    /// operation inside its terminate-and-wait window.
    fn stub_browser_lingering_on_term(dir: &Path, name: &str, term_delay_secs: u64) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join(name);
        let log = dir.join("stub.log");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 trap 'echo \"term $$\" >> \"{log}\"; sleep {delay}; exit 0' TERM\n\
                 echo \"$$ $@\" >> \"{log}\"\n\
                 while true; do sleep 1; done\n",
                log = log.display(),
                delay = term_delay_secs,
            ),
        )
        .expect("write stub browser");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make stub browser executable");
        script
    }

    /// The pid the stub browser reported, parsed from `wait_for_stub_log`'s
    /// first invocation line.
    fn stub_pid(invocation: &str) -> u32 {
        invocation
            .split_whitespace()
            .next()
            .expect("stub log has a pid")
            .parse()
            .expect("stub pid is a number")
    }

    fn process_is_alive(pid: u32) -> bool {
        Path::new("/proc").join(pid.to_string()).exists()
    }

    fn wait_until_process_gone(pid: u32, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !process_is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what} (pid {pid}) to exit");
    }

    fn webpage_play(url: &str) -> PlayMessage {
        PlayMessage {
            container: Some("text/html".to_string()),
            url: Some(url.to_string()),
            ..Default::default()
        }
    }

    /// A `Play` the way a client that only knows a URL sends it: no
    /// `container`, so the daemon decides the route itself.
    fn unclassified_play(url: &str) -> PlayMessage {
        PlayMessage {
            url: Some(url.to_string()),
            ..Default::default()
        }
    }

    /// A URL nothing can serve: mpv fails on it immediately (connection
    /// refused), which is what makes it a fast, deterministic stand-in for
    /// "not media" in these tests.
    const UNREACHABLE_URL: &str = "http://127.0.0.1:1/not-media";

    /// Poll `player`'s idle screen for up to 5s until it equals `expected`;
    /// panics on timeout. Needed because the eof-watch thread that brings
    /// the idle screen back after a natural end-of-file runs asynchronously.
    fn wait_until_idle_screen(player: &Player, expected: Option<IdleScreen>, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if player.idle_screen() == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    /// Poll the queue's `current_index` for up to 30s until it equals
    /// `expected`; panics on timeout. The same generous window as
    /// `wait_until_loading_cleared`, since the queue position only settles
    /// once the async load failure it depends on has resolved.
    fn wait_until_queue_position(player: &Player, expected: Option<usize>, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if player.queue_state().current_index == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    /// Poll `mpv` for up to 5s until `pred` is true; panics on timeout so a
    /// stuck test fails fast instead of hanging.
    fn wait_until(mpv: &Mpv, pred: impl FnMut(&Mpv) -> bool, what: &str) {
        wait_until_timeout(mpv, Duration::from_secs(5), pred, what)
    }

    /// Poll `player`'s loading overlay for up to 30s until it is down; panics
    /// on timeout. The generous window covers a genuinely failing network
    /// load (`MPV_ERROR_NOTHING_TO_PLAY` after DNS/connect failure) in a
    /// sandbox with no network, which can take a while to give up.
    fn wait_until_loading_cleared(player: &Player, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if !player.loading_overlay_active() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    /// A minimal, valid 8kHz mono PCM WAV of `seconds` seconds of silence,
    /// built by hand so the tests need no media fixture on disk or encoder on
    /// `PATH`.
    fn silent_wav(seconds: u32) -> Vec<u8> {
        const SAMPLE_RATE: u32 = 8000;
        const CHANNELS: u16 = 1;
        const BITS: u16 = 16;
        let data_len = SAMPLE_RATE * seconds * CHANNELS as u32 * (BITS as u32 / 8);
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&CHANNELS.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * CHANNELS as u32 * (BITS as u32 / 8)).to_le_bytes());
        wav.extend_from_slice(&(CHANNELS * BITS / 8).to_le_bytes()); // block align
        wav.extend_from_slice(&BITS.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        wav
    }

    /// Serve one HTTP request over a loopback port, but hold the response
    /// until the test releases it: the handler reads the request, signals
    /// `request_seen`, then blocks on `release` before sending any response
    /// bytes. That makes "the load is in flight and not a single response byte
    /// has arrived yet" a deterministic state to assert on, rather than a
    /// race against a fixed sleep. Returns the `http://` URL to hand to
    /// `Play` plus the two channels.
    #[allow(clippy::type_complexity)]
    fn spawn_gated_http_source(
        body: Vec<u8>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test http server");
        let addr = listener.local_addr().expect("local addr");
        let (request_seen_tx, request_seen_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = request_seen_tx.send(());
            let _ = release_rx.recv();
            let header = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: audio/wav\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });
        (
            format!("http://{addr}/test.wav"),
            request_seen_rx,
            release_tx,
        )
    }

    /// Like `wait_until`, but with a caller-chosen timeout -- for cases (e.g.
    /// a real network `yt-dlp` resolution) where 5s can be too tight.
    fn wait_until_timeout(
        mpv: &Mpv,
        timeout: Duration,
        mut pred: impl FnMut(&Mpv) -> bool,
        what: &str,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred(mpv) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    /// A real, playable YUV4MPEG2 (`y4m`) media stream: a plain-text header
    /// plus raw BT.601 frames, which mpv/ffmpeg's yuv4mpeg demuxer recognizes
    /// by magic bytes (no file extension or video MIME type needed).
    fn y4m_stream(frames: usize) -> Vec<u8> {
        const WIDTH: usize = 160;
        const HEIGHT: usize = 90;
        let mut video =
            format!("YUV4MPEG2 W{WIDTH} H{HEIGHT} F25:1 Ip A1:1 C420mpeg2\n").into_bytes();
        for _ in 0..frames {
            video.extend_from_slice(b"FRAME\n");
            video.extend(std::iter::repeat_n(145u8, WIDTH * HEIGHT));
            video.extend(std::iter::repeat_n(54u8, (WIDTH / 2) * (HEIGHT / 2)));
            video.extend(std::iter::repeat_n(34u8, (WIDTH / 2) * (HEIGHT / 2)));
        }
        video
    }

    /// A loopback HTTP server that serves `body` with a `Content-Length`, like
    /// a plain media file host. Returns the port it is listening on.
    fn serve_media(body: Vec<u8>) -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind media server");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        port
    }

    /// A one-way latch: a test trips it once and every connection
    /// `serve_gated_failure`'s server is holding -- or accepts afterwards --
    /// is answered. A `Condvar` rather than a channel because a release has
    /// to cover however many connections mpv opens: with `yt-dlp` on `PATH`
    /// mpv's `ytdl_hook` may probe the URL before the media connection, and a
    /// single-shot wakeup would leave the later one hanging forever.
    struct Gate {
        released: Mutex<bool>,
        cv: std::sync::Condvar,
    }

    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                released: Mutex::new(false),
                cv: std::sync::Condvar::new(),
            })
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.cv.notify_all();
        }

        fn wait(&self) {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.cv.wait(released).unwrap();
            }
        }
    }

    /// A loopback HTTP server that accepts every request, tells the test each
    /// one arrived, then blocks until the test releases the gate before
    /// answering `500 Internal Server Error`.
    ///
    /// Holding the request open is what makes "mpv was handed the URL"
    /// observable without a race: while the server waits, mpv's load is
    /// genuinely in flight and `path` still names the URL, no matter how
    /// slowly the test gets around to reading it. A fast failure is exactly
    /// what can't be asserted on -- mpv clears `path` the moment the load
    /// ends, so a poll only wins if something (e.g. `yt-dlp` being on `PATH`,
    /// which is *not* true in the Nix build sandbox) happens to delay the
    /// failure. Answering `500` then fails the load with an error, which is
    /// what the daemon's media-vs-web-page decision keys on.
    ///
    /// Returns the port, a receiver that yields whenever a request arrives,
    /// and the gate that lets the held requests be answered.
    fn serve_gated_failure() -> (u16, std::sync::mpsc::Receiver<()>, Arc<Gate>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gated server");
        let port = listener.local_addr().expect("local addr").port();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let gate = Gate::new();
        let server_gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let _ = accepted_tx.send(());
                server_gate.wait();
                let _ = stream.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                );
            }
        });
        (port, accepted_rx, gate)
    }

    #[test]
    fn play_rejects_inline_content_without_url() {
        let player = headless_player();
        let msg = PlayMessage {
            content: Some("<MPD>...</MPD>".to_string()),
            ..Default::default()
        };

        let err = player.play(&msg).expect_err("content-only play must fail");
        assert!(
            err.to_string().contains("inline `content`"),
            "unexpected error: {err}"
        );

        // The rejected message must never have reached mpv as a playback target:
        // the core stays idle rather than treating the manifest text as a path.
        let idle: bool = player.mpv.get_property("idle-active").unwrap_or(false);
        assert!(idle, "mpv should remain idle after a rejected play() call");
    }

    #[test]
    fn play_with_url_hands_the_url_to_mpv_and_leaves_idle_state() {
        let player = headless_player();
        // The loopback server holds its answer until released, so the load
        // stays genuinely in flight (and mpv's `path` stays set) long enough
        // to observe; a fast failure could clear the property first.
        let (port, accepted, gate) = serve_gated_failure();
        let url = format!("http://127.0.0.1:{port}/does-not-exist.mp4");
        let msg = PlayMessage {
            // An explicit media MIME type keeps this on the media path -- an
            // *unclassified* Play would instead be routed by the daemon, see
            // `unclassified_play_tries_media_before_falling_back_to_the_browser`.
            container: Some("video/mp4".to_string()),
            url: Some(url.clone()),
            ..Default::default()
        };

        // A real (if unreachable) URL is accepted and queued for playback,
        // unlike the `content`-only case above; mpv is the one that gets it.
        player.play(&msg).expect("play with a url must be accepted");
        accepted
            .recv_timeout(Duration::from_secs(5))
            .expect("mpv to reach the loopback server");
        assert_eq!(
            player
                .mpv
                .get_property::<String>("path")
                .unwrap_or_default(),
            url,
            "mpv must hold the URL as its in-flight media target"
        );
        gate.release();
        // ... and it stays on the media path: an explicit media container
        // never falls back to the browser, however the load ends.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!player.webpage_active());
    }

    /// The status watch must hold the current state even while no sender is
    /// connected (the daemon is the only writer between connections). Tokio's
    /// `watch::Sender::send` leaves the watched value unchanged when there are
    /// zero receivers, so a sender that connects mid-playback would otherwise
    /// be seeded with a stale `Idle` and never corrected during steady play.
    #[test]
    fn status_watch_holds_current_state_with_no_subscribers() {
        let player = headless_player();
        // Zero receivers: `build` drops its own initial receiver, and no
        // other subscriber exists. Seed a distinct value so the assertion
        // below can tell a real update from the initial snapshot.
        player.status_tx.send_replace(PlaybackUpdateMessage {
            generation_time: 1,
            state: PlaybackState::Paused,
            time: None,
            duration: None,
            speed: None,
        });

        player.publish_status();

        let rx = player.subscribe_status();
        assert_eq!(
            rx.borrow().state,
            player.status().state,
            "publish_status must replace the watched value with no subscribers"
        );
        assert_eq!(
            rx.borrow().state,
            PlaybackState::Idle,
            "a fresh idle player must publish Idle, not the seeded value"
        );
    }

    /// Regression test for a bug where a second `Play` after the first clip
    /// finished loaded correctly but never advanced past its first frame
    /// (silent black screen, no error): `keep-open=yes` leaves `pause=true`
    /// once a clip hits EOF, and mpv does not reset that property on the next
    /// `loadfile`, so the freshly loaded second clip inherited the stale
    /// pause. Uses a synthetic, network-free `lavfi` test source (no real
    /// video file needed) so this runs headless in CI.
    #[test]
    fn second_play_after_first_reaches_eof_is_not_left_paused() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };

        player.play(&msg).expect("first play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property("eof-reached").unwrap_or(false),
            "first clip to reach eof",
        );
        // Sanity check on the test's own premise: `keep-open` should indeed
        // leave mpv paused at eof, otherwise this test is not exercising the
        // bug it claims to.
        assert!(
            player.mpv.get_property::<bool>("pause").unwrap_or(false),
            "sanity: keep-open=yes should pause mpv once eof is reached"
        );

        player.play(&msg).expect("second play");

        let paused: bool = player.mpv.get_property("pause").unwrap_or(true);
        assert!(
            !paused,
            "second play() must not leave mpv paused on the reloaded clip's first frame"
        );
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "second clip's playback time to advance past its first frame",
        );
    }

    /// Exercises the idle screen through all three triggers the spec calls
    /// for -- startup, natural end-of-file with nothing queued, and Stop --
    /// plus the one place it must disappear (a real Play). `show`/`hide`
    /// each round-trip through a real `mpv.command("osd-overlay", ...)` call
    /// (propagating any mpv error via `?`), and the eof case is driven by
    /// mpv's own `eof-reached` property flipping on a real synthetic clip
    /// rather than the test calling `show_idle_screen` itself, so this is
    /// checking that the real wiring fires at the right moments, not just
    /// that some function was called. (A headless `vo=null`/`ao=null` mpv
    /// core never produces a paintable frame -- confirmed empirically:
    /// `screenshot-to-file` errors out even while idle with `force-window`
    /// -- so there is no way to also assert on rendered pixels here.)
    #[test]
    fn idle_screen_appears_when_idle_and_disappears_once_playback_starts() {
        let player = headless_player();

        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "clock must be showing at startup, before any Play"
        );

        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };
        player.play(&msg).expect("first play");
        assert_eq!(
            player.idle_screen(),
            None,
            "idle screen must be hidden once playback actually starts"
        );

        // Let the clip run to completion. `keep-open=yes` pauses mpv at eof
        // instead of unloading it, and nothing else queues a next file, so
        // the eof-watch thread should bring the clock back on its own.
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property("eof-reached").unwrap_or(false),
            "clip to reach eof",
        );
        wait_until_idle_screen(
            &player,
            Some(IdleScreen::Clock),
            "idle screen to return after end-of-file with nothing queued next",
        );

        player.play(&msg).expect("second play");
        assert_eq!(
            player.idle_screen(),
            None,
            "idle screen must be hidden again once replay starts"
        );

        player.stop().expect("stop");
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "clock must return after an explicit Stop"
        );
    }

    /// Run `action` while sampling `player.loading_overlay_active()` at high
    /// frequency, and report whether the loading/fade overlay ever came up.
    /// The overlay only exists *during* a blocking `stop()` (it is torn down
    /// before the call returns), so whether a Stop took the fade path is not
    /// observable afterwards; this samples the real state while it runs.
    fn overlay_came_up_during(player: &Player, action: impl FnOnce()) -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::TryRecvError;

        let saw = AtomicBool::new(false);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let saw = &saw;
            scope.spawn(move || loop {
                if player.loading_overlay_active() {
                    saw.store(true, Ordering::SeqCst);
                }
                match done_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => {
                        if player.loading_overlay_active() {
                            saw.store(true, Ordering::SeqCst);
                        }
                        return;
                    }
                    Err(TryRecvError::Empty) => {}
                }
                std::thread::sleep(Duration::from_millis(2));
            });
            action();
            let _ = done_tx.send(());
        });
        saw.load(Ordering::SeqCst)
    }

    /// Regression test: a Stop after a clip has already ended must not blink
    /// the screen. `keep-open=yes` leaves mpv paused at the last frame (so
    /// `idle-active` is still false) while the eof watcher has already put the
    /// idle clock back; the old `was_playing = !idle-active` check therefore
    /// concealed the visible clock to black and faded it straight back in. The
    /// fade/loading overlay is the only thing that blacks the screen on a
    /// Stop, so it must never become active here, and the clock must stay up.
    #[test]
    fn stop_after_end_of_file_does_not_blink_the_idle_clock() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };
        player.play(&msg).expect("play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property("eof-reached").unwrap_or(false),
            "clip to reach eof",
        );
        wait_until_idle_screen(
            &player,
            Some(IdleScreen::Clock),
            "clock to return after end-of-file",
        );
        // Sanity check on the test's own premise: with `keep-open=yes` mpv is
        // still not `idle-active` at this point, so the old
        // `was_playing = !idle-active` check did take the blink path.
        assert!(
            !player
                .mpv
                .get_property::<bool>("idle-active")
                .unwrap_or(true),
            "sanity: keep-open leaves mpv non-idle at eof, which caused the blink"
        );

        assert!(
            !overlay_came_up_during(&player, || player.stop().expect("stop")),
            "a Stop after end-of-file must not conceal the clock to black"
        );
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the clock must stay up across a no-op Stop"
        );
    }

    /// The other direction of the no-op rule: a Stop while playback is
    /// genuinely on screen must still take the fade path (video out through
    /// black, clock in) and land on the idle clock.
    #[test]
    fn stop_while_playing_fades_out_to_the_idle_clock() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=30".to_string()),
            ..Default::default()
        };
        player.play(&msg).expect("play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.1,
            "playback to start",
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "the clock is hidden while the video plays"
        );

        assert!(
            overlay_came_up_during(&player, || player.stop().expect("stop")),
            "a Stop during playback must fade through the overlay"
        );
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the clock must return after the fade"
        );
    }

    /// Verifies real YouTube playback end-to-end through mpv's built-in
    /// `ytdl_hook` (see README's "How YouTube playback works"): no daemon
    /// code shells out to `yt-dlp` itself, mpv's bundled Lua script does,
    /// automatically, for any URL it doesn't recognize as directly playable.
    /// Requires network access and `yt-dlp` on `PATH` (already the case in
    /// `nix develop`'s dev shell -- see `devShells.default` in flake.nix),
    /// so this is `#[ignore]`d by default: the sandboxed `nix build`/
    /// `nix flake check` checkPhase has no network access, and `yt-dlp` is a
    /// runtime-only dependency (see flake.nix), not a build input. Run with
    /// `nix develop -c cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access and yt-dlp on PATH; run with `cargo test -- --ignored`"]
    fn real_youtube_url_resolves_and_plays_via_ytdl_hook() {
        let player = headless_player();
        let msg = PlayMessage {
            // "Me at the zoo", the first video ever uploaded to YouTube:
            // short (19s), extremely unlikely to ever be removed -- a stable
            // target for this test.
            url: Some("https://www.youtube.com/watch?v=jNQXAC9IVRw".to_string()),
            ..Default::default()
        };

        player
            .play(&msg)
            .expect("play with a youtube url must be accepted");

        // `duration` is only known once ytdl_hook has resolved a real,
        // direct media URL via `yt-dlp` and mpv has opened it -- a bare
        // subprocess spawn with no real resolution wouldn't produce this.
        wait_until_timeout(
            &player.mpv,
            Duration::from_secs(30),
            |mpv| mpv.get_property::<f64>("duration").unwrap_or(0.0) > 0.0,
            "duration to be known (ytdl_hook resolved a real stream)",
        );
        let duration: f64 = player.mpv.get_property("duration").unwrap();
        assert!(
            (15.0..25.0).contains(&duration),
            "expected ~19s duration for the known test video, got {duration}"
        );

        wait_until_timeout(
            &player.mpv,
            Duration::from_secs(15),
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.5,
            "playback time-pos to advance",
        );
    }

    /// Regression test for ordering between concurrent `play()` calls (one
    /// per FCast connection, see `main.rs`'s per-connection
    /// `spawn_blocking`): the operation lock must serialize whole plays, so
    /// whatever mpv ends up on is exactly the load `Routing` recorded last,
    /// never an older submission's. Hammers `play()` from many threads at
    /// once, releasing them together via a `Barrier` to maximize
    /// interleaving, and asserts after every round that mpv's actual `path`
    /// and playlist entry still match `Routing`'s newest submission.
    #[test]
    fn concurrent_plays_leave_mpv_on_the_last_submitted_load() {
        let player = headless_player();
        const THREADS: usize = 8;
        const ROUNDS: usize = 30;

        for round in 0..ROUNDS {
            let barrier = std::sync::Barrier::new(THREADS);
            std::thread::scope(|scope| {
                for i in 0..THREADS {
                    let barrier = &barrier;
                    let player = &player;
                    scope.spawn(move || {
                        // The `duration` decimal encodes `(round, i)` uniquely
                        // (just to make each url distinguishable to mpv/us);
                        // it plays no role in the race being tested.
                        let uid = round * THREADS + i;
                        let msg = PlayMessage {
                            url: Some(format!(
                                "av://lavfi:testsrc=size=64x64:rate=10:duration={:.3}",
                                600.0 + uid as f64 * 0.001
                            )),
                            ..Default::default()
                        };
                        barrier.wait();
                        player.play(&msg).expect("play");
                    });
                }
            });

            // Let mpv's command queue settle so its `path` property reflects
            // the most recently submitted `loadfile`.
            wait_until(
                &player.mpv,
                {
                    let mut last_seen = String::new();
                    let mut stable_polls = 0;
                    move |mpv| {
                        let path: String = mpv.get_property("path").unwrap_or_default();
                        if path == last_seen {
                            stable_polls += 1;
                        } else {
                            stable_polls = 0;
                            last_seen = path;
                        }
                        stable_polls >= 3
                    }
                },
                "mpv path property to settle",
            );

            let mpv_path: String = player.mpv.get_property("path").unwrap_or_default();
            let mpv_entry: i64 = player.mpv.get_property("playlist/0/id").unwrap_or_default();
            let (url, entry_id) = {
                let routing = player.routing.lock().unwrap();
                let load = routing
                    .current
                    .as_ref()
                    .expect("the last submitted load must still be tracked");
                (load.url.clone(), load.entry_id)
            };
            assert_eq!(
                mpv_path, url,
                "round {round}: mpv must be on the URL of the last submitted load"
            );
            assert_eq!(
                Some(mpv_entry),
                entry_id,
                "round {round}: mpv's playlist entry must be the one Routing recorded"
            );
        }
    }

    /// A webpage `Play` (see `fcast::PlayMessage::explicit_target`) must
    /// start the browser engine on the page's URL, keep mpv on the idle clock
    /// *behind* it (so a clean hand-off needs no work when the page goes
    /// away), report Playing -- not the Idle mpv itself is in -- to the
    /// sender, and a `Stop` must terminate the engine's whole process group
    /// again.
    #[test]
    fn webpage_play_starts_the_browser_engine_and_stop_terminates_it() {
        let dir = scratch_dir("webpage-play");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        player
            .play(&webpage_play("http://127.0.0.1:9/dashboard"))
            .expect("a webpage play must be accepted");

        assert!(
            player.webpage_active(),
            "the browser engine must be running"
        );
        let invocation = wait_for_stub_log(&dir);
        let pid = stub_pid(&invocation);
        assert!(
            invocation.contains("--app=http://127.0.0.1:9/dashboard"),
            "the engine must be handed the page's URL: {invocation}"
        );
        assert!(
            invocation.contains("--kiosk"),
            "the engine must run as a fullscreen kiosk client: {invocation}"
        );
        assert!(process_is_alive(pid), "the engine process must be running");

        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the idle clock must stay up behind the page, ready for the hand-off"
        );
        assert_eq!(
            player.status().state,
            PlaybackState::Playing,
            "a displayed page is Playing from the sender's point of view"
        );

        player.stop().expect("stop must terminate the engine");
        assert!(!player.webpage_active());
        wait_until_process_gone(pid, "the browser engine");
        assert_eq!(player.idle_screen(), Some(IdleScreen::Clock));
        assert_eq!(player.status().state, PlaybackState::Idle);
    }

    /// Casting media while a page is displayed must take the screen back:
    /// the engine is terminated and mpv loads the media URL, so Cage's newest
    /// view is mpv's again.
    #[test]
    fn media_play_supersedes_a_displayed_webpage() {
        let dir = scratch_dir("webpage-superseded-by-media");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        player
            .play(&webpage_play("http://127.0.0.1:9/dashboard"))
            .expect("webpage play");
        let pid = stub_pid(&wait_for_stub_log(&dir));
        assert!(player.webpage_active());

        // A displayed page counts as "currently playing" for queueing (see
        // `Player::is_idle`), so this second `Play` only enqueues; jumping
        // forward is what actually takes the screen back, the way the
        // Android task's Next button would.
        let media_url = "av://lavfi:testsrc=size=64x64:rate=10:duration=1";
        player
            .play(&PlayMessage {
                url: Some(media_url.to_string()),
                ..Default::default()
            })
            .expect("enqueue media play");
        assert!(
            player.webpage_active(),
            "an enqueued Play must not itself take the screen"
        );
        player.queue_jump_forward().expect("jump to the media item");

        assert!(
            !player.webpage_active(),
            "media must take the screen back from the page"
        );
        wait_until_process_gone(pid, "the superseded browser engine");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<String>("path").unwrap_or_default() == media_url,
            "mpv to load the media URL after the page was taken down",
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "media playback must hide the idle clock"
        );
    }

    /// Casting a second page must replace the first one's engine rather than
    /// stacking browsers.
    #[test]
    fn a_second_webpage_play_replaces_the_first_engine() {
        let dir = scratch_dir("webpage-superseded-by-page");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        player
            .play(&webpage_play("http://127.0.0.1:9/first"))
            .expect("first webpage play");
        let first_pid = stub_pid(&wait_for_stub_log(&dir));

        // The first page is still displayed, so this only enqueues (see
        // `Player::is_idle`); jumping forward is what actually swaps the
        // engine, the way the Android task's Next button would.
        player
            .play(&webpage_play("http://127.0.0.1:9/second"))
            .expect("enqueue second webpage play");
        player
            .queue_jump_forward()
            .expect("jump to the second webpage");

        assert!(player.webpage_active());
        wait_until_process_gone(first_pid, "the first browser engine");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::fs::read_to_string(dir.join("stub.log"))
                .unwrap_or_default()
                .contains("--app=http://127.0.0.1:9/second")
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the second engine was never started");
    }

    /// Regression test for the shared-profile replacement race: every engine
    /// uses one `--user-data-dir`, and Chromium admits one browser per profile
    /// (ProcessSingleton). A replacement started while the incumbent still
    /// holds the profile's lock aborts or defers to the incumbent -- which the
    /// daemon then terminates -- so the sender's page never appears. The
    /// replacement must therefore only be started once the incumbent has
    /// actually exited. Observed directly: while the first engine is lingering
    /// in its SIGTERM handler, the second must not yet have been spawned.
    #[test]
    fn replacement_engine_starts_only_after_the_incumbent_is_gone() {
        let dir = scratch_dir("webpage-replacement-ordering");
        let browser = stub_browser_lingering_on_term(&dir, "browser-slow-term", 2);
        let player = headless_player_with_browser(&browser);

        player
            .play(&webpage_play("http://127.0.0.1:9/first"))
            .expect("first webpage play");
        let first_pid = stub_pid(&wait_for_stub_log(&dir));

        // The first page is still displayed, so this only enqueues (see
        // `Player::is_idle`) -- it returns immediately, without waiting on
        // the first engine at all. The blocking wait-for-exit this test is
        // about only happens once something actually asks to take the
        // screen: the jump below.
        player
            .play(&webpage_play("http://127.0.0.1:9/second"))
            .expect("enqueue second webpage play");

        let second_started_while_first_lingered = std::thread::scope(|scope| {
            let jump = scope.spawn(|| player.queue_jump_forward());
            // The first engine logs its SIGTERM only once `show` has asked it
            // to go away and is waiting for it; sample shortly after, while it
            // still lingers (its handler sleeps 2s), for the second engine's
            // start line.
            wait_for_log_contains(&dir, "term ");
            std::thread::sleep(Duration::from_millis(500));
            let started = std::fs::read_to_string(dir.join("stub.log"))
                .unwrap_or_default()
                .contains("--app=http://127.0.0.1:9/second");
            jump.join()
                .expect("jump thread")
                .expect("jump to the second webpage");
            started
        });

        assert!(
            !second_started_while_first_lingered,
            "the replacement engine must not be spawned while the incumbent still holds \
             the shared browser profile"
        );
        wait_until_process_gone(first_pid, "the first browser engine");
        assert!(
            player.webpage_active(),
            "the second page's engine must be running once the first is gone"
        );
        player.stop().expect("stop after replacement");
    }

    /// A webpage `Play` that cannot be rendered must be refused before any
    /// engine is started: no `url`, or a URL that is not http(s)/file.
    #[test]
    fn unrenderable_webpage_plays_are_refused_without_starting_the_engine() {
        let dir = scratch_dir("webpage-refused");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        let err = player
            .play(&PlayMessage {
                container: Some("text/html".to_string()),
                content: Some("<html></html>".to_string()),
                ..Default::default()
            })
            .expect_err("a webpage play with no url must fail");
        assert!(
            err.to_string().contains("web page"),
            "unexpected error: {err}"
        );

        let err = player
            .play(&webpage_play("ftp://example.invalid/dashboard"))
            .expect_err("a non-http(s)/file url must fail");
        assert!(
            err.to_string().contains("http://"),
            "unexpected error: {err}"
        );

        assert!(!player.webpage_active());
        assert!(
            !dir.join("stub.log").exists(),
            "the engine must not be started for a refused Play"
        );
    }

    /// Regression test for `Player::play`/`Player::stop` interleaving across
    /// FCast connections: a media `Play` must not slip its `loadfile` under a
    /// concurrent `Stop` that is still taking a browser down, or the Stop's
    /// idle clock ends up painted over the playing media. Holds a `Stop`
    /// inside its terminate-and-wait window (the stub lingers on SIGTERM),
    /// then issues a media `Play` from the main thread: with the whole
    /// operation serialized the media play is the last word and the clock is
    /// hidden; without it the media play loads while the Stop waits and the
    /// Stop then raises the clock over the video.
    #[test]
    fn media_play_does_not_interleave_with_an_in_flight_stop() {
        let dir = scratch_dir("webpage-stop-serialized");
        let browser = stub_browser_lingering_on_term(&dir, "browser-slow-term", 2);
        let player = headless_player_with_browser(&browser);

        player
            .play(&webpage_play("http://127.0.0.1:9/dashboard"))
            .expect("webpage play");
        let _pid = stub_pid(&wait_for_stub_log(&dir));

        let media = "av://lavfi:testsrc=size=64x64:rate=10:duration=30";
        std::thread::scope(|scope| {
            let stop = scope.spawn(|| player.stop().expect("stop"));
            // The stub logs its SIGTERM only once `Stop` is inside
            // `terminate`, i.e. holding the operation lock and waiting on the
            // engine. Issue the media play exactly then.
            wait_for_log_contains(&dir, "term ");
            player
                .play(&PlayMessage {
                    url: Some(media.to_string()),
                    ..Default::default()
                })
                .expect("media play");
            stop.join().expect("stop thread");
        });

        assert_eq!(
            player.idle_screen(),
            None,
            "the Stop's idle clock must not be left painted over the media that \
             superseded it"
        );
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<String>("path").unwrap_or_default() == media,
            "mpv to load the media URL issued concurrently with the Stop",
        );
        assert!(
            !player.webpage_active(),
            "the browser engine must be gone once the Stop and the media play have settled"
        );
    }

    /// If the engine exits by itself (a crash), the daemon must notice
    /// without any incoming command and be idle again -- the clock Cage
    /// reveals was already on screen behind the page.
    #[test]
    fn browser_engine_exiting_on_its_own_returns_the_screen_to_idle() {
        let dir = scratch_dir("webpage-exit");
        let browser = stub_browser(&dir, "browser-exits", "exit 0");
        let player = headless_player_with_browser(&browser);

        player
            .play(&webpage_play("http://127.0.0.1:9/dashboard"))
            .expect("webpage play");

        let deadline = Instant::now() + Duration::from_secs(5);
        while player.webpage_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !player.webpage_active(),
            "the daemon must notice the engine exited on its own"
        );
        assert_eq!(player.status().state, PlaybackState::Idle);
        assert_eq!(player.idle_screen(), Some(IdleScreen::Clock));
    }

    /// Regression test for the stale-`FileLoaded` race: mpv's `FileLoaded` for
    /// an older load must resolve *that* load, never mark a newer `Play` as
    /// loaded. The old design attributed `FileLoaded` by position, so a page
    /// `Play` whose loadfile was submitted while the previous load's
    /// `FileLoaded` was still queued lost its own probe and was never handed
    /// to the browser -- a black screen for a URL the daemon is required to
    /// display as a page.
    #[test]
    fn stale_file_loaded_resolves_its_own_load_not_a_newer_probe() {
        const MEDIA: &str = "http://127.0.0.1:1/media";
        const PAGE: &str = "http://127.0.0.1:1/page";
        let mut routing = Routing::default();

        // A (unrouted media, entry 1) is submitted, then B (unrouted page,
        // entry 2) supersedes it before the watcher has drained A's
        // `FileLoaded`.
        routing.submit(MEDIA, true, Some(1), None);
        routing.submit(PAGE, true, Some(2), None);
        routing.start_file(1);
        routing.file_loaded();

        // A's stale `FileLoaded` belongs to entry 1, not to B, so B keeps its
        // probe and still reaches the browser.
        assert_eq!(
            routing.resolve_error(2),
            Some(Resolution::FallBack(PAGE.to_string(), None)),
            "a stale FileLoaded must not mark the newer Play's probe as loaded"
        );
    }

    /// An error for an entry the daemon did not submit -- a playlist mpv
    /// expanded, or a superseded load -- resolves nothing: only the entry the
    /// submitted `loadfile` created can drive the routing decision.
    #[test]
    fn error_for_a_foreign_entry_never_resolves_the_submitted_load() {
        const PAGE: &str = "http://127.0.0.1:1/page";
        let mut routing = Routing::default();

        routing.submit(PAGE, true, Some(3), None);
        assert_eq!(routing.resolve_error(2), None);

        assert_eq!(
            routing.resolve_error(3),
            Some(Resolution::FallBack(PAGE.to_string(), None))
        );
    }

    /// A superseded probe's own error resolves no tracked submission, so it
    /// can neither consume nor be blamed on the newer probe that replaced it:
    /// the newer `Play` still falls back.
    #[test]
    fn superseded_load_error_does_not_resolve_the_newer_probe() {
        const FIRST: &str = "http://127.0.0.1:1/media";
        const SECOND: &str = "http://127.0.0.1:1/page";
        let mut routing = Routing::default();

        // Probe A (entry 1) is superseded by a newer probe B (entry 2)
        // before A's error is drained.
        routing.submit(FIRST, true, Some(1), None);
        routing.submit(SECOND, true, Some(2), None);

        // A's error matches no tracked submission (only B is tracked), so it
        // is not a playback error for B.
        assert_eq!(routing.resolve_error(1), None);

        // B keeps its own probe and still reaches the browser.
        assert_eq!(
            routing.resolve_error(2),
            Some(Resolution::FallBack(SECOND.to_string(), None)),
            "a superseded load's error must not consume or blame the newer probe"
        );
    }

    /// Once mpv reports the file loaded, a later error is a playback error, not
    /// a fallback -- even if the error arrives before the first frame.
    #[test]
    fn loaded_load_that_fails_later_is_a_playback_error() {
        const URL: &str = "http://127.0.0.1:1/media";
        let mut routing = Routing::default();

        routing.submit(URL, true, Some(1), None);
        routing.start_file(1);
        routing.file_loaded();
        assert_eq!(
            routing.resolve_error(1),
            Some(Resolution::PlaybackError(URL.to_string(), None))
        );
    }

    /// An explicit media `Play` is never re-routed, however it fails; and
    /// cancelling an in-flight probe (a `Stop` or a direct page `Play`) makes
    /// it non-fallback-eligible while keeping its entry for its own mpv event
    /// to resolve.
    #[test]
    fn explicit_media_and_cancelled_probes_never_fall_back() {
        const URL: &str = "http://127.0.0.1:1/media";
        let mut routing = Routing::default();

        routing.submit(URL, false, Some(1), None);
        assert_eq!(
            routing.resolve_error(1),
            Some(Resolution::PlaybackError(URL.to_string(), None))
        );

        routing.submit(URL, true, Some(2), None);
        routing.cancel();
        assert_eq!(
            routing.resolve_error(2),
            Some(Resolution::PlaybackError(URL.to_string(), None)),
            "a cancelled probe must not fall back, but its own event still resolves its entry"
        );
    }

    /// Regression test for the `Stop`/direct-page cancellation race: a
    /// non-error `EndFile` for the cancelled load's own entry must resolve
    /// *that* load, so a page `Play` submitted right after a `Stop` keeps its
    /// own fallback.
    #[test]
    fn stale_end_after_cancel_resolves_its_own_load_not_a_newer_probe() {
        const FIRST: &str = "http://127.0.0.1:1/media";
        const SECOND: &str = "http://127.0.0.1:1/page";
        let mut routing = Routing::default();

        routing.submit(FIRST, true, Some(1), None);
        routing.cancel();
        routing.submit(SECOND, true, Some(2), None);

        // mpv's non-error EndFile for FIRST (entry 1) arrives after SECOND
        // (entry 2) was queued.
        routing.resolve_end(1);
        assert_eq!(
            routing.resolve_error(2),
            Some(Resolution::FallBack(SECOND.to_string(), None)),
            "a stale EndFile after cancel must not consume the newer Play's probe"
        );
    }

    /// The symmetric case: a stale `FileLoaded` for a cancelled load must mark
    /// that load, not a newer `Play`, so the newer probe still falls back.
    #[test]
    fn stale_file_loaded_after_cancel_resolves_its_own_load() {
        const FIRST: &str = "http://127.0.0.1:1/media";
        const SECOND: &str = "http://127.0.0.1:1/page";
        let mut routing = Routing::default();

        routing.submit(FIRST, true, Some(1), None);
        routing.cancel();
        routing.submit(SECOND, true, Some(2), None);

        // mpv's FileLoaded for FIRST (entry 1) arrives after SECOND (entry 2)
        // was queued.
        routing.start_file(1);
        routing.file_loaded();
        assert_eq!(
            routing.resolve_error(2),
            Some(Resolution::FallBack(SECOND.to_string(), None)),
            "a stale FileLoaded after cancel must not mark the newer load as loaded"
        );
    }

    /// Regression test for the playlist misattribution that position-based
    /// attribution could not prevent: mpv expands a submitted `.m3u` into
    /// playlist entries of its own and reports `StartFile`/`FileLoaded`/
    /// `EndFile` for them, none of which belongs to a `loadfile` the daemon
    /// submitted. Those events must not resolve a newer page `Play`'s probe --
    /// otherwise the page the daemon is required to display is left on a
    /// black screen. The event order mirrors real mpv, and the behaviour is
    /// covered end to end by
    /// `daemon/tests/webpage_display.rs::unclassified_playlist_then_page_displays_the_page`.
    #[test]
    fn playlist_entry_events_do_not_swallow_the_next_probes_fallback() {
        const LIST: &str = "http://127.0.0.1:1/list.m3u";
        const PAGE: &str = "http://127.0.0.1:1/page";
        let mut routing = Routing::default();

        // A: the submitted playlist (entry 1). mpv redirects it to its first
        // expanded entry, id 2.
        routing.submit(LIST, true, Some(1), None);
        routing.start_file(1);
        routing.resolve_end(1);

        // B: an unrouted page (entry 3), submitted while the playlist's
        // expanded entry 2 is still playing.
        routing.submit(PAGE, true, Some(3), None);

        // mpv stops entry 2 when B's `loadfile` replaces it, then a late
        // `StartFile`/`FileLoaded` for entry 2 still arrives. None of it may
        // touch B's probe.
        routing.resolve_end(2);
        routing.start_file(2);
        routing.file_loaded();

        // B's own load fails, so it must still reach the browser.
        assert_eq!(
            routing.resolve_error(3),
            Some(Resolution::FallBack(PAGE.to_string(), None)),
            "a playlist entry's events must not swallow the newer Play's probe"
        );
    }

    /// Behavioural form of the cancellation race, against the real mpv core
    /// and the real event watcher: a `Stop` that leaves a stale `EndFile`
    /// queued, immediately followed by an unrouted page `Play`, must still
    /// fall back to the browser. The test holds the `operation` lock across
    /// the cancellation and the new submission, so the watcher cannot drain
    /// the stale event before the new probe is queued -- deterministic, with
    /// no sleeps.
    #[test]
    fn stop_then_unrouted_page_play_still_falls_back_to_the_browser() {
        let dir = scratch_dir("stop-then-page-fallback");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        // A long clip, so it is still in flight when it is cancelled.
        player
            .play(&unclassified_play(
                "av://lavfi:testsrc=size=64x64:rate=10:duration=30",
            ))
            .expect("first play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "the first clip to start playing",
        );

        // The `Stop` path: cancel the in-flight probe, then stop mpv (which
        // queues the stale `EndFile`). Holding the operation lock keeps the
        // watcher from draining that event until the new page Play is queued.
        let operation = player.operation.lock().unwrap();
        player.routing.lock().unwrap().cancel();
        player
            .mpv
            .command("stop", &[])
            .expect("stop mpv while holding the operation lock");
        play_media(
            &player.handles(),
            &unclassified_play(UNREACHABLE_URL),
            true,
            None,
        )
        .expect("submit the page Play");
        drop(operation);

        let invocation = wait_for_stub_log(&dir);
        assert!(
            invocation.contains(&format!("--app={UNREACHABLE_URL}")),
            "the page must still fall back to the browser: {invocation}"
        );
    }

    /// The daemon's own routing: a `Play` with no `container` goes to mpv
    /// first (the media path -- what makes YouTube and every other
    /// yt-dlp-supported source work with no client help), and when that
    /// attempt fails before the file loads, the same URL is handed to the
    /// browser engine instead.
    #[test]
    fn unclassified_play_tries_media_before_falling_back_to_the_browser() {
        let dir = scratch_dir("unclassified-fallback");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);
        let (port, accepted, gate) = serve_gated_failure();
        let url = format!("http://127.0.0.1:{port}/not-media");

        player
            .play(&unclassified_play(&url))
            .expect("an unclassified play must be accepted");

        // Media first: mpv is the one that was handed the URL. The loopback
        // server holds its answer until the test releases it, so the load
        // stays genuinely in flight -- and `path` stays set -- for as long as
        // it takes to observe it, instead of a fast failure clearing the
        // property between polls.
        accepted
            .recv_timeout(Duration::from_secs(5))
            .expect("mpv to reach the loopback server");
        assert_eq!(
            player
                .mpv
                .get_property::<String>("path")
                .unwrap_or_default(),
            url,
            "mpv must hold the URL as its in-flight media target"
        );
        gate.release();

        // Then the asynchronous failure turns into a browser engine run.
        let invocation = wait_for_stub_log(&dir);
        assert!(
            invocation.contains(&format!("--app={url}")),
            "the engine must be handed the same URL: {invocation}"
        );
        assert!(player.webpage_active());
        assert_eq!(player.status().state, PlaybackState::Playing);
        // The clock stays behind the page, so taking it down is clean.
        assert_eq!(player.idle_screen(), Some(IdleScreen::Clock));
    }

    /// Regression test for a `Play` the daemon routed to the media path that
    /// started playing and then failed: the load is marked as media as soon as
    /// mpv reports the file loaded, so a later failure is never re-routed to
    /// the browser.
    ///
    /// A real y4m stream is served over loopback HTTP and an unclassified
    /// `Play` starts it. Once mpv is actually playing (which is only possible
    /// after `FileLoaded`), a deterministic later failure is injected by
    /// handing mpv a URL it cannot open directly (bypassing `Player::play`, so
    /// no new `Play` supersedes the entry), and the browser engine must stay
    /// unused. (A real mid-playback network drop is not usable here: mpv
    /// treats a truncated or reset connection as end-of-stream, not an error
    /// -- verified against mpv 0.41 with an HTTP response cut off mid-body and
    /// with an RST mid-body, both of which exit 0 -- so it cannot exercise
    /// this boundary.)
    #[test]
    fn unclassified_play_that_started_playing_is_never_re_routed() {
        let dir = scratch_dir("unclassified-started-then-failed");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);
        let port = serve_media(y4m_stream(300));

        player
            .play(&unclassified_play(&format!(
                "http://127.0.0.1:{port}/clip.y4m"
            )))
            .expect("an unclassified play must be accepted");

        // The media path got somewhere with the URL: it is actually playing,
        // which is only possible after mpv reported the file loaded.
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "the media to start playing",
        );

        // A later asynchronous failure must be a playback error, not a
        // fallback.
        player
            .mpv
            .command("loadfile", &["http://127.0.0.1:1/not-media", "replace"])
            .expect("hand mpv a URL it cannot open");

        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !player.webpage_active(),
            "a URL that already started playing must never be re-routed to the browser"
        );
        assert!(
            !dir.join("stub.log").exists(),
            "the browser engine must never have been started for a URL that played"
        );
    }

    /// The other half of the daemon's decision: when the URL *is* playable
    /// media, the browser engine is never started. This is the YouTube-shaped
    /// case -- a source mpv/yt-dlp can play must not be re-routed.
    #[test]
    fn unclassified_play_of_playable_media_never_starts_the_browser() {
        let dir = scratch_dir("unclassified-media");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        player
            .play(&unclassified_play(
                "av://lavfi:testsrc=size=64x64:rate=10:duration=30",
            ))
            .expect("an unclassified play must be accepted");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "the media to start playing",
        );

        // Give a fallback every chance to happen before calling it absent.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !player.webpage_active(),
            "playable media must stay on the media path"
        );
        assert!(
            !dir.join("stub.log").exists(),
            "the browser engine must never have been started"
        );
    }

    /// An explicit `container` is still an override, in both directions: a
    /// media MIME type that fails is *not* silently re-routed to the browser.
    #[test]
    fn explicit_media_container_is_not_reinterpreted_as_a_web_page() {
        let dir = scratch_dir("explicit-media-no-fallback");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        player
            .play(&PlayMessage {
                container: Some("video/mp4".to_string()),
                url: Some(UNREACHABLE_URL.to_string()),
                ..Default::default()
            })
            .expect("an explicit media play must be accepted");

        // Wait for mpv's attempt to be over, then check nothing took over.
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<bool>("idle-active").unwrap_or(false),
            "mpv to give up on the URL",
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !player.webpage_active(),
            "an explicit media container must not fall back to the browser"
        );
        assert!(
            !dir.join("stub.log").exists(),
            "the browser engine must never have been started"
        );
    }

    /// `Stop` ends an in-flight routing decision: a failed media attempt that
    /// resolves afterwards must not open the browser on a cast the sender has
    /// already stopped.
    #[test]
    fn stop_ends_an_in_flight_routing_decision() {
        let dir = scratch_dir("stop-during-probe");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        player
            .play(&unclassified_play(UNREACHABLE_URL))
            .expect("an unclassified play must be accepted");
        player.stop().expect("stop");

        // Give the (already queued or still to come) failure time to arrive.
        std::thread::sleep(Duration::from_millis(750));
        assert!(
            !player.webpage_active(),
            "a Stop must cancel the daemon's own routing decision"
        );
        assert!(
            !dir.join("stub.log").exists(),
            "the browser engine must never have been started"
        );
        assert_eq!(player.idle_screen(), Some(IdleScreen::Clock));
    }

    /// An explicit web MIME type skips the media attempt entirely.
    #[test]
    fn explicit_webpage_container_skips_the_media_path() {
        let dir = scratch_dir("explicit-webpage");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        player
            .play(&webpage_play("http://127.0.0.1:9/dashboard"))
            .expect("a webpage play must be accepted");

        let invocation = wait_for_stub_log(&dir);
        assert!(invocation.contains("--app=http://127.0.0.1:9/dashboard"));
        // mpv was never asked to play the URL.
        assert_ne!(
            player
                .mpv
                .get_property::<String>("path")
                .unwrap_or_default(),
            "http://127.0.0.1:9/dashboard"
        );
    }
    /// The spinner must be up for the *whole* wait of a slow load and gone as
    /// soon as playback actually starts rendering. The server is gated: it
    /// signals when it has received the request, then withholds every response
    /// byte until the test releases it, so there is a deterministic window in
    /// which mpv is genuinely still waiting on the network. Only the real mpv
    /// `PlaybackRestart` event may take the spinner down.
    #[test]
    fn loading_overlay_shows_during_slow_load_and_clears_when_playback_starts() {
        let player = headless_player();
        let (url, request_seen, release) = spawn_gated_http_source(silent_wav(5));
        assert!(
            !player.loading_overlay_active(),
            "no spinner before any Play"
        );

        let msg = PlayMessage {
            url: Some(url),
            ..Default::default()
        };
        player
            .play(&msg)
            .expect("play with a slow url must be accepted");

        assert!(
            player.loading_overlay_active(),
            "spinner must be up once the slow load is in flight"
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "the idle clock is hidden behind the loading overlay"
        );

        // Wait until mpv has actually opened the stream and its request has
        // reached the server, which is now withholding the response. The
        // spinner must still be up here: no response byte has been sent, so
        // mpv cannot have reached `PlaybackRestart` yet. This is the invariant
        // the old sleep-based test never actually asserted.
        request_seen
            .recv_timeout(Duration::from_secs(10))
            .expect("server to receive the load request");
        assert!(
            player.loading_overlay_active(),
            "spinner must stay up while the server withholds its first byte"
        );

        release.send(()).expect("release the server response");
        wait_until_loading_cleared(&player, "spinner to clear once playback restarts");
        assert!(
            !player.loading_overlay_active(),
            "spinner must stay cleared after playback starts"
        );
        // Playback really did start (not merely the overlay timing out): the
        // slow WAV is playing and its clock advances.
        wait_until_timeout(
            &player.mpv,
            Duration::from_secs(10),
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.0,
            "slow source to actually start playing",
        );
    }

    /// A superseding Play must not tear down the new load's spinner. mpv
    /// produces an `EndFile` for the load being replaced (STOP/REDIRECT) when
    /// a newer `loadfile` arrives; only an EOF-without-`PlaybackRestart` should
    /// return to idle. Both sources are gated, so while the second load is in
    /// flight nothing else can have legitimately cleared its overlay.
    #[test]
    fn rapid_replay_keeps_the_new_loads_spinner_up() {
        let player = headless_player();
        let (url_a, seen_a, _release_a) = spawn_gated_http_source(silent_wav(5));
        player
            .play(&PlayMessage {
                url: Some(url_a),
                ..Default::default()
            })
            .expect("first play");
        seen_a
            .recv_timeout(Duration::from_secs(10))
            .expect("server A to receive the first request");

        let (url_b, seen_b, release_b) = spawn_gated_http_source(silent_wav(5));
        player
            .play(&PlayMessage {
                url: Some(url_b),
                ..Default::default()
            })
            .expect("second play");
        seen_b
            .recv_timeout(Duration::from_secs(10))
            .expect("server B to receive the second request");

        // mpv has by now superseded load A, which emits an `EndFile` for A
        // (not EOF). B's spinner must survive it; if that event cleared the
        // overlay, B would stay uncovered over the opaque fade.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            player.loading_overlay_active(),
            "a superseding load must keep the new spinner up"
        );

        release_b.send(()).expect("release server B");
        wait_until_loading_cleared(&player, "second spinner to clear once playback restarts");
    }

    /// A load that fails must not leave the spinner up (the console error
    /// report is the real feedback for failure): the overlay clears and the
    /// idle clock comes back. Uses an unresolvable host so the load fails
    /// asynchronously, which is the path this daemon's own error logger covers.
    #[test]
    fn failed_load_clears_loading_overlay_and_returns_to_idle_clock() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("https://example.invalid/does-not-exist.mp4".to_string()),
            ..Default::default()
        };
        player
            .play(&msg)
            .expect("play is accepted; the load fails asynchronously");
        assert!(
            player.loading_overlay_active(),
            "spinner is up while the failing load is in flight"
        );

        wait_until_loading_cleared(&player, "spinner to clear after the load fails");
        wait_until_idle_screen(
            &player,
            Some(IdleScreen::Clock),
            "idle clock to return after the load fails",
        );
    }

    /// Regression test: an unreachable URL becomes the queue's current item
    /// synchronously (`Player::play` commits the position before the load
    /// resolves), but the failure itself only surfaces asynchronously, well
    /// after that position was already broadcast and persisted. The queue
    /// must not go on reporting a failed load as the current item once the
    /// daemon settles back on the idle clock.
    #[test]
    fn queue_position_reverts_after_an_async_load_failure() {
        let player = headless_player();
        let msg = PlayMessage {
            url: Some("https://example.invalid/does-not-exist.mp4".to_string()),
            ..Default::default()
        };
        player
            .play(&msg)
            .expect("play is accepted; the load fails asynchronously");
        assert_eq!(
            player.queue_state().current_index,
            Some(0),
            "the failing item is provisionally current while its load is in flight"
        );

        wait_until_loading_cleared(&player, "spinner to clear after the load fails");
        wait_until_idle_screen(
            &player,
            Some(IdleScreen::Clock),
            "idle clock to return after the load fails",
        );
        wait_until_queue_position(
            &player,
            None,
            "queue position to revert once the failed load resolves",
        );
        assert_eq!(
            player.queue_state().items.len(),
            1,
            "the failed item stays queued, just no longer marked current"
        );
    }

    /// A load that ends at EOF without ever emitting `PlaybackRestart` must
    /// return to the idle clock instead of leaving the spinner up over opaque
    /// black. mpv can end an incomplete/corrupted/interrupted remote source at
    /// EOF with no error code (mpv `client.h`, `MPV_END_FILE_REASON_EOF`).
    ///
    /// This daemon runs mpv with `keep-open=yes`, under which a *normal* EOF
    /// emits no `END_FILE` at all (the idle clock's `eof-reached` property
    /// watcher handles it instead); confirmed empirically. An
    /// EOF-before-`PlaybackRestart` load end could not be produced end-to-end
    /// either -- a zero-length WAV and a truncated remote WAV both emitted
    /// `PlaybackRestart` (or `MPV_ERROR_LOADING_FAILED`) first. So this drives
    /// the lifecycle watcher's real event handler with the real `Event` value
    /// it is built to receive and asserts the observable state transition.
    #[test]
    fn end_of_file_without_playback_restart_returns_to_idle_clock() {
        let player = headless_player();
        // Put the loading overlay up exactly as a Play does ...
        player.overlay.conceal().expect("conceal");
        player.hide_idle_screen().expect("hide idle");
        player.overlay.spawn_spinner().expect("spinner");
        assert!(player.loading_overlay_active(), "spinner must be up");

        // ... then deliver the load-will-not-start event the watcher handles.
        let keep_going = handle_lifecycle_event(
            Event::EndFile(libmpv2::mpv_end_file_reason::Eof),
            &player.idle,
            &player.overlay,
            &player.mpv,
            &player.webpage,
            &player.status_tx,
        );

        assert!(keep_going, "a normal EOF must not stop the watcher");
        assert!(
            !player.loading_overlay_active(),
            "the spinner must be torn down rather than stuck over opaque black"
        );
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the idle clock must return"
        );
    }

    /// A page `Play` that supersedes an in-flight media load must take the
    /// loading overlay (and its spinner thread) down. The page owns the
    /// screen, and a spinner redrawing over it -- and over the idle clock
    /// after the engine exits -- at the animation cadence is pure waste.
    #[test]
    fn webpage_play_clears_a_leftover_loading_overlay() {
        let dir = scratch_dir("webpage-clears-overlay");
        let browser = stub_browser(&dir, "browser-waiting", "sleep 300");
        let player = headless_player_with_browser(&browser);

        // Exactly what an in-flight media `Play` leaves behind while mpv is
        // still opening the URL.
        player.overlay.conceal().expect("conceal");
        player.hide_idle_screen().expect("hide idle");
        player.overlay.spawn_spinner().expect("spinner");
        assert!(player.loading_overlay_active(), "precondition: spinner up");

        player
            .play(&webpage_play("http://127.0.0.1:9/dashboard"))
            .expect("webpage play");

        assert!(
            !player.loading_overlay_active(),
            "the page must take the loading overlay down"
        );
        assert!(player.webpage_active(), "the engine must still be up");
        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the clock sits behind the page"
        );
        wait_for_stub_log(&dir);
    }

    /// Regression test for the failed-setup path where the loading overlay has
    /// *already* cleared itself (e.g. `spawn_spinner`'s draw failed and its
    /// error path called `clear()`) after `hide_idle_screen()` removed the
    /// clock. `abort_loading_to_idle` must still put the clock back; otherwise
    /// `play()` returns Err with no overlay up and no clock, leaving an
    /// unadorned black screen until the next command.
    #[test]
    fn abort_restores_idle_clock_when_overlay_already_cleared() {
        let player = headless_player();
        assert_eq!(player.idle_screen(), Some(IdleScreen::Clock));

        player.overlay.conceal().expect("conceal");
        player.hide_idle_screen().expect("hide idle clock");
        // Exactly what `spawn_spinner`'s error path does when its draw fails.
        player.overlay.clear().expect("clear overlay");
        assert!(
            !player.loading_overlay_active(),
            "precondition: the overlay has already cleared itself"
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "precondition: clock hidden and overlay gone -- screen would be black"
        );

        abort_loading_to_idle(&player.handles()).expect("abort back to the idle clock");

        assert_eq!(
            player.idle_screen(),
            Some(IdleScreen::Clock),
            "the idle clock must be restored even when the overlay had already cleared"
        );
    }

    /// The core of the queueing feature: a `Play` that arrives while
    /// something is genuinely playing (past `PlaybackRestart`, not merely
    /// loading) must not interrupt it -- it only joins the queue, to be
    /// reached by auto-advance or an explicit jump later.
    #[test]
    fn play_while_playing_enqueues_instead_of_interrupting() {
        let player = headless_player();
        let first = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=30".to_string()),
            ..Default::default()
        };
        player.play(&first).expect("first play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "the first clip to start playing",
        );

        let second = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };
        player
            .play(&second)
            .expect("second play must be accepted (enqueued)");

        // mpv must still be on the first clip: the second Play only queued.
        let path: String = player.mpv.get_property("path").unwrap_or_default();
        assert!(
            path.contains("duration=30"),
            "a Play while something is playing must not interrupt it, got path {path:?}"
        );

        let state = player.queue_state();
        assert_eq!(state.items.len(), 2, "both items must be in the queue");
        assert_eq!(
            state.current_index,
            Some(0),
            "the currently playing item stays current; the second is only queued"
        );
    }

    /// When the current item reaches genuine end-of-file, the daemon must
    /// automatically start the next queued item instead of falling back to
    /// the idle clock (see `auto_advance_queue`).
    #[test]
    fn eof_auto_advances_to_the_next_queued_item() {
        let player = headless_player();
        let first = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=1".to_string()),
            ..Default::default()
        };
        let second = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=5".to_string()),
            ..Default::default()
        };
        player.play(&first).expect("first play");
        player.play(&second).expect("second play enqueues");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "the first clip to start playing",
        );

        // Let the first clip run out on its own; auto-advance should move
        // the queue's position to the second item without ever needing an
        // incoming command.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut advanced = false;
        while Instant::now() < deadline {
            if player.queue_state().current_index == Some(1) {
                advanced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            advanced,
            "the queue must auto-advance to the second item once the first reaches eof"
        );
        assert_eq!(
            player.idle_screen(),
            None,
            "auto-advance must not fall back to the idle clock when a next item exists"
        );
    }

    /// `QueueJumpForward`/`QueueJumpBackward` (`Player::queue_jump_forward`/
    /// `queue_jump_backward`) move within the queue on demand, and are a
    /// no-op (not an error) past either edge.
    #[test]
    fn queue_jump_forward_and_backward_move_between_queued_items() {
        let player = headless_player();
        let clip = || PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=30".to_string()),
            ..Default::default()
        };
        player.play(&clip()).expect("first play");
        wait_until(
            &player.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "the first clip to start playing",
        );
        player.play(&clip()).expect("second play enqueues");
        assert_eq!(player.queue_state().current_index, Some(0));

        player.queue_jump_forward().expect("jump forward");
        assert_eq!(player.queue_state().current_index, Some(1));

        player.queue_jump_backward().expect("jump backward");
        assert_eq!(player.queue_state().current_index, Some(0));

        // Past either edge, a jump changes nothing and is not an error.
        player
            .queue_jump_backward()
            .expect("jump backward at the start is a no-op");
        assert_eq!(player.queue_state().current_index, Some(0));
    }

    /// The queue (its items and current position) must survive a daemon
    /// restart: `Player::build` reloads whatever `Queue::save` last wrote.
    /// Simulates the restart by dropping one `Player` and constructing a
    /// fresh one against the same state file, exactly as `main` would after
    /// a real process restart -- mpv itself always starts fresh and idle
    /// either way, so this also checks that a reload does not, by itself,
    /// resume playback.
    #[test]
    fn queue_persists_across_a_simulated_restart() {
        let dir = scratch_dir("queue-persistence");
        let state_path = dir.join("queue.json");
        let browser = "/nonexistent/castoff-test-browser";

        let before = Player::with_browser_and_state(headless_mpv(), browser, state_path.clone())
            .expect("create player");
        before
            .show_idle_screen(IdleScreen::Clock)
            .expect("show initial idle screen");

        let first = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=30".to_string()),
            ..Default::default()
        };
        let second = PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=30".to_string()),
            ..Default::default()
        };
        before.play(&first).expect("first play");
        wait_until(
            &before.mpv,
            |mpv| mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.05,
            "the first clip to start playing",
        );
        before.play(&second).expect("second play enqueues");

        let state_before = before.queue_state();
        assert_eq!(state_before.items.len(), 2);
        assert_eq!(state_before.current_index, Some(0));
        drop(before);

        let after = Player::with_browser_and_state(headless_mpv(), browser, state_path)
            .expect("recreate player after simulated restart");
        after
            .show_idle_screen(IdleScreen::Clock)
            .expect("show initial idle screen");

        let state_after = after.queue_state();
        assert_eq!(
            state_after.items.len(),
            state_before.items.len(),
            "the reloaded queue must have the same items"
        );
        assert_eq!(
            state_after.current_index, state_before.current_index,
            "the reloaded queue must have the same current position"
        );
        for (before_item, after_item) in state_before.items.iter().zip(&state_after.items) {
            assert_eq!(before_item.url, after_item.url);
        }
        assert_eq!(
            after.idle_screen(),
            Some(IdleScreen::Clock),
            "a reload must not itself resume playback"
        );
    }
}
