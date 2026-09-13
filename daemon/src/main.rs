mod fcast;
mod idle_screen;
mod player;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use fcast::{
    Opcode, PlayMessage, PlaybackErrorMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage,
    VersionMessage,
};
use player::Player;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

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

async fn handle_connection(mut socket: TcpStream, player: Arc<Player>) -> Result<()> {
    loop {
        let frame = match fcast::read_frame(&mut socket).await? {
            Some(f) => f,
            None => return Ok(()), // peer closed cleanly
        };

        if let Err(e) = dispatch(&mut socket, &player, frame).await {
            error!(error = %e, "failed to handle FCast message");
            let msg = PlaybackErrorMessage {
                message: e.to_string(),
            };
            fcast::write_message(&mut socket, Opcode::PlaybackError, &msg).await?;
        }
    }
}

async fn dispatch(socket: &mut TcpStream, player: &Arc<Player>, frame: fcast::Frame) -> Result<()> {
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
            send_status(socket, player).await
        }
        Opcode::Pause => {
            info!("received Pause");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.pause()).await??;
            send_status(socket, player).await
        }
        Opcode::Resume => {
            info!("received Resume");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.resume()).await??;
            send_status(socket, player).await
        }
        Opcode::Stop => {
            info!("received Stop");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.stop()).await??;
            send_status(socket, player).await
        }
        Opcode::Seek => {
            let msg: SeekMessage = serde_json::from_slice(&frame.body)?;
            info!(time = msg.time, "received Seek");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.seek(msg.time)).await??;
            send_status(socket, player).await
        }
        Opcode::SetVolume => {
            let msg: SetVolumeMessage = serde_json::from_slice(&frame.body)?;
            info!(volume = msg.volume, "received SetVolume");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.set_volume(msg.volume)).await??;
            send_volume(socket, player).await
        }
        Opcode::SetSpeed => {
            let msg: SetSpeedMessage = serde_json::from_slice(&frame.body)?;
            info!(speed = msg.speed, "received SetSpeed");
            let p = Arc::clone(player);
            tokio::task::spawn_blocking(move || p.set_speed(msg.speed)).await??;
            send_status(socket, player).await
        }
        Opcode::Version => {
            debug!("received Version");
            let reply = VersionMessage {
                version: fcast::PROTOCOL_VERSION,
            };
            fcast::write_message(socket, Opcode::Version, &reply).await?;
            Ok(())
        }
        Opcode::Ping => {
            // Debug, not info: a sender may ping on a short heartbeat-like
            // cadence, and that traffic isn't itself a diagnostically
            // interesting "request" the way Play/Seek/etc. are.
            debug!("received Ping");
            fcast::write_empty(socket, Opcode::Pong).await?;
            Ok(())
        }
        other => {
            warn!(?other, "ignoring unsupported/receiver-only opcode");
            Ok(())
        }
    }
}

async fn send_status(socket: &mut TcpStream, player: &Arc<Player>) -> Result<()> {
    let player = Arc::clone(player);
    let status = tokio::task::spawn_blocking(move || player.status()).await?;
    fcast::write_message(socket, Opcode::PlaybackUpdate, &status).await?;
    Ok(())
}

async fn send_volume(socket: &mut TcpStream, player: &Arc<Player>) -> Result<()> {
    let player = Arc::clone(player);
    let volume = tokio::task::spawn_blocking(move || player.volume()).await?;
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct VolumeUpdateMessage {
        generation_time: u64,
        volume: f64,
    }
    fcast::write_message(
        socket,
        Opcode::VolumeUpdate,
        &VolumeUpdateMessage {
            generation_time: player::now_millis(),
            volume,
        },
    )
    .await?;
    Ok(())
}
