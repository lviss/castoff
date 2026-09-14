mod fcast;
mod idle_screen;
mod overlay;
mod player;
mod webpage;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, info, warn};

use fcast::{
    Opcode, PlaybackErrorMessage, PlaybackState, PlaybackUpdateMessage, PlayMessage, SeekMessage,
    SetSpeedMessage, SetVolumeMessage, VersionMessage,
};
use player::Player;

/// The write half of one FCast sender's socket, shared between the
/// per-connection command reply path (`dispatch`) and that same connection's
/// background push task (`push_updates`) so the two can never interleave
/// bytes of two different frames onto the wire.
///
/// Both paths serialize on the same lock, but they snapshot their status at
/// different moments, so a frame that was current when it was snapshotted can
/// still reach the lock after a newer one. The newest `PlaybackUpdate`
/// generation written is tracked here and a strictly older one is dropped, so
/// the frames a sender sees stay monotonically fresh.
struct ConnectionWriter {
    write: OwnedWriteHalf,
    last_generation: u64,
}

impl ConnectionWriter {
    /// Write `status` unless a newer one was already written on this
    /// connection. Returns whether it was written.
    async fn write_playback_update(&mut self, status: &PlaybackUpdateMessage) -> Result<bool> {
        if status.generation_time < self.last_generation {
            return Ok(false);
        }
        self.last_generation = status.generation_time;
        fcast::write_message(&mut self.write, Opcode::PlaybackUpdate, status).await?;
        Ok(true)
    }
}

type SharedWriter = Arc<AsyncMutex<ConnectionWriter>>;

/// How often a push task re-sends `PlaybackUpdate` while the last known state
/// is `Playing`. No tick fires at all while idle/paused -- see [Design
/// principles](../../README.md#design-principles).
const PUSH_TICK_INTERVAL: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<()> {
    // The daemon's own log goes to stderr, with mpv's, Cage's and the browser
    // engine's diagnostics -- journald captures it on the appliance, and it is
    // the stream a caller reading "the console" sees. (stdout is left free,
    // and not every environment that runs the daemon under a compositor
    // forwards a child's stdout at all; `tracing`'s default is stdout.)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let port: u16 = std::env::var("CASTOFF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fcast::DEFAULT_PORT);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let player = Arc::new(Player::new()?);
    info!("mpv core ready");

    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "FCast control server listening");

    loop {
        let (socket, peer) = listener.accept().await?;
        let player = Arc::clone(&player);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, Arc::clone(&player)).await {
                warn!(%peer, error = %e, "connection ended");
            }
        });
    }
}

/// Own one FCast sender's persistent connection: replies to its own commands
/// (the pre-existing behavior) while a sibling task pushes unprompted
/// `PlaybackUpdate`s onto the same socket whenever playback state changes for
/// any reason, including a different connection's command (see
/// `push_updates`). Splitting the socket (`into_split`) is what lets the two
/// write independently without one blocking on the other's read.
async fn handle_connection(socket: TcpStream, player: Arc<Player>) -> Result<()> {
    let (mut read_half, write_half) = socket.into_split();
    let writer: SharedWriter = Arc::new(AsyncMutex::new(ConnectionWriter {
        write: write_half,
        last_generation: 0,
    }));

    let push_task = tokio::spawn(push_updates(Arc::clone(&writer), Arc::clone(&player)));

    let result: Result<()> = async {
        loop {
            let frame = match fcast::read_frame(&mut read_half).await? {
                Some(f) => f,
                None => return Ok(()), // peer closed cleanly
            };

            if let Err(e) = dispatch(&writer, &player, frame).await {
                error!(error = %e, "failed to handle FCast message");
                let msg = PlaybackErrorMessage {
                    message: e.to_string(),
                };
                let mut w = writer.lock().await;
                fcast::write_message(&mut w.write, Opcode::PlaybackError, &msg).await?;
            }
        }
    }
    .await;

    // The read loop has ended (peer closed, or a write/protocol error), so
    // there's no longer a connection to push updates onto.
    push_task.abort();
    result
}

/// Push an unprompted `PlaybackUpdate` onto `writer` whenever `player`'s
/// broadcast fires (a state change from *any* connected sender's command, or
/// an async mpv/idle-screen transition -- see `Player::subscribe_status`),
/// plus once every `PUSH_TICK_INTERVAL` while the last known state is
/// `Playing`. No periodic tick at all while idle/paused: the `tokio::select!`
/// arm below is only even considered when `last_state == Playing`, so this
/// task is purely event-driven (an `.await` on the channel) the rest of the
/// time, consistent with the daemon's no-polling design principle. Returns
/// (ending the task) once the socket can no longer be written to; `player`'s
/// `watch::Sender` outliving every connection means `rx.changed()` itself
/// never errors here in practice.
async fn push_updates(writer: SharedWriter, player: Arc<Player>) {
    let mut rx = player.subscribe_status();
    let mut last_state = rx.borrow().state;

    let mut interval = tokio::time::interval(PUSH_TICK_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // `interval`'s first tick fires immediately; consume it here so a fresh
    // connection doesn't get an extra push at connect time on top of
    // whatever `dispatch` already replied with.
    interval.tick().await;

    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return; // Player dropped (daemon shutting down).
                }
                let status = rx.borrow_and_update().clone();
                let written = match write_update(&writer, &status).await {
                    Ok(written) => written,
                    Err(_) => return,
                };
                // A frame can be dropped as older than a command reply that
                // overtook it; in that case re-read the latest published
                // state so `last_state` (which arms the tick) reflects the
                // frame actually on the wire, not the superseded one.
                last_state = if written {
                    status.state
                } else {
                    rx.borrow().state
                };
            }
            _ = interval.tick(), if last_state == PlaybackState::Playing => {
                // A *fresh* snapshot, not `rx.borrow()`: the tick exists so a
                // sender's progress bar can advance without polling, and the
                // watch value only changes on a state change -- re-sending it
                // would repeat the same `time`/`generationTime` forever. Read
                // mpv off the async worker, exactly as `send_status` does.
                let p = Arc::clone(&player);
                let Ok(status) = tokio::task::spawn_blocking(move || p.status()).await else {
                    return;
                };
                // Fold the live state back into `last_state` here too, not
                // only in the `changed()` arm: this is how a transition
                // nothing publishes (today, the browser engine exiting on its
                // own -- `webpage.rs` clears its active page with no callback)
                // is noticed, and what stops the tick once playback is over.
                // Without it the tick would keep firing for the life of the
                // connection, the standing wake loop the power-efficiency
                // design principle forbids.
                let written = match write_update(&writer, &status).await {
                    Ok(written) => written,
                    Err(_) => return,
                };
                last_state = if written {
                    status.state
                } else {
                    rx.borrow().state
                };
            }
        }
    }
}

async fn write_update(writer: &SharedWriter, status: &PlaybackUpdateMessage) -> Result<bool> {
    let mut w = writer.lock().await;
    w.write_playback_update(status).await
}

async fn dispatch(writer: &SharedWriter, player: &Arc<Player>, frame: fcast::Frame) -> Result<()> {
    match frame.opcode {
        Opcode::Play => {
            let msg: PlayMessage = serde_json::from_slice(&frame.body)?;
            info!(
                url = msg.url.as_deref().unwrap_or(""),
                has_content = msg.content.is_some(),
                "received Play"
            );
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.play(&msg)).await??;
            send_status(writer, player).await
        }
        Opcode::Pause => {
            info!("received Pause");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.pause()).await??;
            send_status(writer, player).await
        }
        Opcode::Resume => {
            info!("received Resume");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.resume()).await??;
            send_status(writer, player).await
        }
        Opcode::Stop => {
            info!("received Stop");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.stop()).await??;
            send_status(writer, player).await
        }
        Opcode::Seek => {
            let msg: SeekMessage = serde_json::from_slice(&frame.body)?;
            info!(time = msg.time, "received Seek");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.seek(msg.time)).await??;
            send_status(writer, player).await
        }
        Opcode::SetVolume => {
            let msg: SetVolumeMessage = serde_json::from_slice(&frame.body)?;
            info!(volume = msg.volume, "received SetVolume");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.set_volume(msg.volume)).await??;
            send_volume(writer, player).await
        }
        Opcode::SetSpeed => {
            let msg: SetSpeedMessage = serde_json::from_slice(&frame.body)?;
            info!(speed = msg.speed, "received SetSpeed");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.set_speed(msg.speed)).await??;
            send_status(writer, player).await
        }
        Opcode::Version => {
            debug!("received Version");
            let reply = VersionMessage {
                version: fcast::PROTOCOL_VERSION,
            };
            let mut w = writer.lock().await;
            fcast::write_message(&mut w.write, Opcode::Version, &reply).await?;
            Ok(())
        }
        Opcode::Ping => {
            // Debug, not info: a sender may ping on a short heartbeat-like
            // cadence, and that traffic isn't itself a diagnostically
            // interesting "request" the way Play/Seek/etc. are.
            debug!("received Ping");
            let mut w = writer.lock().await;
            fcast::write_empty(&mut w.write, Opcode::Pong).await?;
            Ok(())
        }
        other => {
            warn!(?other, "ignoring unsupported/receiver-only opcode");
            Ok(())
        }
    }
}

async fn send_status(writer: &SharedWriter, player: &Arc<Player>) -> Result<()> {
    // Snapshot *under* the writer lock: `dispatch`'s reply and
    // `push_updates`' unprompted push share this lock, and a status taken
    // outside it could be written after a newer pushed state. The lock plus
    // the generation check in `ConnectionWriter` keep the frames on this
    // connection monotonically fresh.
    let mut w = writer.lock().await;
    let player = Arc::clone(player);
    let status = tokio::task::spawn_blocking(move || player.status()).await?;
    w.write_playback_update(&status).await?;
    Ok(())
}

async fn send_volume(writer: &SharedWriter, player: &Arc<Player>) -> Result<()> {
    let player = Arc::clone(player);
    let volume = tokio::task::spawn_blocking(move || player.volume()).await?;
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct VolumeUpdateMessage {
        generation_time: u64,
        volume: f64,
    }
    let mut w = writer.lock().await;
    fcast::write_message(
        &mut w.write,
        Opcode::VolumeUpdate,
        &VolumeUpdateMessage {
            generation_time: player::now_millis(),
            volume,
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream as ClientStream;

    use super::*;
    use player::headless_player;

    /// Bind a listener on an ephemeral loopback port and spawn the same
    /// per-connection accept loop `main` runs, so tests exercise the real
    /// `handle_connection`/`dispatch`/`push_updates` wiring over a real TCP
    /// socket rather than calling `Player` methods directly. Returns the
    /// address to connect test clients to.
    async fn spawn_server(player: Arc<Player>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let Ok((socket, _peer)) = listener.accept().await else {
                    return;
                };
                let player = Arc::clone(&player);
                tokio::spawn(async move {
                    let _ = handle_connection(socket, player).await;
                });
            }
        });
        addr
    }

    async fn read_update(client: &mut ClientStream) -> PlaybackUpdateMessage {
        let frame = fcast::read_frame(client)
            .await
            .expect("read frame")
            .expect("frame present, not EOF");
        assert_eq!(frame.opcode, Opcode::PlaybackUpdate, "expected PlaybackUpdate");
        serde_json::from_slice(&frame.body).expect("decode PlaybackUpdate body")
    }

    /// A second connection that never sends a command of its own must still
    /// see a `PlaybackUpdate` pushed onto its socket the moment a *different*
    /// connection's command changes playback state -- the core ask of this
    /// task: no more polling the daemon to find out something happened
    /// elsewhere.
    #[tokio::test]
    async fn a_command_on_one_connection_pushes_an_update_to_another() {
        let player = Arc::new(headless_player());
        let addr = spawn_server(player).await;

        let mut commander = ClientStream::connect(addr).await.expect("connect commander");
        let mut observer = ClientStream::connect(addr).await.expect("connect observer");
        // Give both connections' push tasks a moment to subscribe before the
        // state change below, so the observer can't miss it.
        tokio::time::sleep(Duration::from_millis(50)).await;

        fcast::write_empty(&mut commander, Opcode::Pause)
            .await
            .expect("send Pause");

        // The commander gets its usual synchronous reply...
        let commander_reply = read_update(&mut commander).await;
        assert_eq!(commander_reply.state, PlaybackState::Idle);

        // ...and the observer, which sent nothing, gets an unprompted push
        // for the same state change.
        let observer_update = tokio::time::timeout(Duration::from_secs(5), read_update(&mut observer))
            .await
            .expect("observer should receive a pushed PlaybackUpdate");
        assert_eq!(observer_update.state, PlaybackState::Idle);
    }

    /// The periodic ~1s push must fire only while the last known state is
    /// `Playing`, never while idle: no busy-polling a connection that isn't
    /// doing anything (see README's Design principles).
    #[tokio::test]
    async fn periodic_push_only_fires_while_playing() {
        let player = Arc::new(headless_player());
        let addr = spawn_server(player).await;

        let mut client = ClientStream::connect(addr).await.expect("connect");

        // Idle at connect time: no command was ever sent, so no synchronous
        // reply and no periodic tick should arrive either.
        let idle_wait = tokio::time::timeout(Duration::from_millis(1500), read_update(&mut client)).await;
        assert!(
            idle_wait.is_err(),
            "no PlaybackUpdate should be pushed while idle"
        );

        // A synthetic clip long enough (30s, same idiom as player.rs's own
        // tests) that this test's few seconds of reading periodic pushes
        // can't run past it into a natural end-of-file -- that would flip
        // the state to Idle/Paused mid-assertion and fail the test for the
        // wrong reason.
        let play = fcast::PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=30".to_string()),
            ..Default::default()
        };
        let body = serde_json::to_vec(&play).expect("encode Play");
        fcast::write_frame(&mut client, Opcode::Play, &body)
            .await
            .expect("send Play");

        // The synchronous reply to Play itself (and any immediate push from
        // `play()`'s own state-change broadcast) can still report Idle: mpv's
        // `idle-active` property updates asynchronously and may not have
        // flipped yet the instant `loadfile`'s command call returns. Keep
        // reading until an update genuinely reports Playing before measuring
        // the periodic-tick behavior below.
        let mut saw_playing = false;
        for _ in 0..10 {
            let update = tokio::time::timeout(Duration::from_secs(3), read_update(&mut client))
                .await
                .expect("an update should arrive while Play is starting");
            if update.state == PlaybackState::Playing {
                saw_playing = true;
                break;
            }
        }
        assert!(saw_playing, "expected an update reporting Playing after Play");

        // Reaching Playing can itself produce a short burst of near-
        // simultaneous change-driven pushes (`play()`'s own publish plus the
        // lifecycle watcher's `PlaybackRestart` publish), which would pollute
        // gap measurements below with sub-second spacing that isn't the
        // periodic tick. Let that settle, then drain anything already
        // buffered from the settle window before measuring.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        while let Ok(update) =
            tokio::time::timeout(Duration::from_millis(50), read_update(&mut client)).await
        {
            assert_eq!(update.state, PlaybackState::Playing);
        }

        // With no further command changing state, every update from here on
        // must come from the periodic tick, so every gap between consecutive
        // arrivals should sit close to `PUSH_TICK_INTERVAL`.
        let mut timestamps = Vec::new();
        let mut times = Vec::new();
        for _ in 0..3 {
            let update = tokio::time::timeout(Duration::from_secs(3), read_update(&mut client))
                .await
                .expect("a periodic PlaybackUpdate should keep arriving while playing");
            assert_eq!(update.state, PlaybackState::Playing);
            timestamps.push(Instant::now());
            times.push(
                update
                    .time
                    .expect("a playing update must carry a play position"),
            );
        }
        // The whole point of the tick: carry an *advancing* position, so a
        // sender's progress bar can track playback without polling. A tick
        // that merely re-sent the last broadcast snapshot would repeat the
        // same `time` forever and fail this.
        assert!(
            times.windows(2).all(|w| w[1] > w[0]),
            "periodic pushes must carry an advancing play position, got {times:?}"
        );
        let gaps: Vec<Duration> = timestamps.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps
                .iter()
                .all(|g| *g >= Duration::from_millis(700) && *g <= Duration::from_millis(2000)),
            "expected every gap between periodic pushes to be ~{PUSH_TICK_INTERVAL:?}, got {gaps:?}"
        );

        client.shutdown().await.ok();
    }

    /// The tick must also stop when playback ends on its own: once the last
    /// known state leaves `Playing`, no further periodic write may go out on
    /// this connection (see README's Design principles). Complements
    /// `periodic_push_only_fires_while_playing`, which covers a connection
    /// that was never playing at all.
    #[tokio::test]
    async fn periodic_push_stops_when_playback_ends() {
        let player = Arc::new(headless_player());
        let addr = spawn_server(player).await;

        let mut client = ClientStream::connect(addr).await.expect("connect");

        // A short synthetic clip that reaches end-of-file on its own, so the
        // daemon's eof watcher (not a client Stop) is what ends playback.
        let play = fcast::PlayMessage {
            url: Some("av://lavfi:testsrc=size=64x64:rate=10:duration=3".to_string()),
            ..Default::default()
        };
        let body = serde_json::to_vec(&play).expect("encode Play");
        fcast::write_frame(&mut client, Opcode::Play, &body)
            .await
            .expect("send Play");

        // Wait for Playing first: the synchronous reply to Play itself (and
        // any immediate push from `play()`'s own broadcast) can still report
        // Idle, because mpv resolves `idle-active` off that call stack.
        let mut saw_playing = false;
        for _ in 0..10 {
            let update = tokio::time::timeout(Duration::from_secs(3), read_update(&mut client))
                .await
                .expect("an update should arrive while Play is starting");
            if update.state == PlaybackState::Playing {
                saw_playing = true;
                break;
            }
        }
        assert!(saw_playing, "expected an update reporting Playing after Play");

        // Now wait until some update reports something other than Playing:
        // that is the clip's end, reported by the eof watcher's idle-screen
        // transition.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut ended = None;
        while Instant::now() < deadline {
            let update = tokio::time::timeout(Duration::from_secs(5), read_update(&mut client))
                .await
                .expect("updates should keep arriving until the clip ends");
            if update.state != PlaybackState::Playing {
                ended = Some(update.state);
                break;
            }
        }
        assert!(
            matches!(ended, Some(PlaybackState::Idle) | Some(PlaybackState::Paused)),
            "the clip reaching end-of-file must be reported as no longer playing, got {ended:?}"
        );

        // The end transition can legitimately produce one near-simultaneous
        // duplicate: the `changed()` arm's publish and a tick that was
        // already due both report the same end state. Drain that immediately,
        // then require silence.
        let settle_until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < settle_until {
            let _ = tokio::time::timeout(Duration::from_millis(50), read_update(&mut client)).await;
        }

        // With nothing playing any more, the periodic tick must have stopped:
        // no update may arrive on this idle connection.
        let quiet =
            tokio::time::timeout(Duration::from_millis(2500), read_update(&mut client)).await;
        assert!(
            quiet.is_err(),
            "no PlaybackUpdate should be pushed once playback has ended"
        );

        client.shutdown().await.ok();
    }
}
