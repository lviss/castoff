//! Jellyfin Session REST client.
//!
//! Castoff shows Jellyfin's own web client (jellyfin-web) as a browser tab
//! through the existing `webpage.rs` mechanism -- no second mpv process, no
//! native reimplementation of Jellyfin's WebSocket session protocol. Once
//! that tab is logged in it already registers a normal, remote-controllable
//! Jellyfin session (`SupportsMediaControl: true`) with its own open
//! WebSocket to the server; this module is castoff's own authenticated REST
//! client *of that same session*, driven by FCast's existing opcodes
//! (`Pause`/`Resume`/`Seek`/`SetVolume`) plus a new directional-nav one, so
//! Jellyfin becomes one more controllable source through the control surface
//! castoff already has rather than a parallel, disconnected one. See
//! `README.md`'s Jellyfin section and the `castoff-jellyfin-cast-research`/
//! `castoff-jellyfin-integration-ux` research reports this follows.
//!
//! # Why one client identity for everything
//!
//! A Quick Connect secret, once a human approves its code on another
//! device, resolves to exactly *one* server-side `AuthenticationResult` --
//! tied to the `Device`/`DeviceId` that called `/QuickConnect/Initiate`, not
//! to whoever later redeems it (confirmed by reading the server's own
//! `QuickConnectManager.cs`: `GetAuthorizedRequest` just returns the cached
//! result by secret, idempotently, until it expires). So whichever device
//! performs the Initiate/poll/Authenticate dance becomes *the* identity for
//! that whole login; there is no separate "backend session" distinct from
//! "the browser tab's session" to juggle. `JellyfinSession` here is written
//! to be that one identity (`DEVICE_ID` persisted alongside the resulting
//! credentials), ready for whatever wires the human-facing pairing step to
//! it -- see the worklog/status notes for the open question of how the
//! resulting token reaches the browser tab's own (origin-scoped)
//! `localStorage`, which this module deliberately does not attempt to
//! solve by itself.
//!
//! # Scope
//!
//! Everything here assumes a `JellyfinCredentials` already exists (however
//! it got there) and uses it to: resolve the tab's own session id, send
//! playback/volume/navigation commands to it, and poll its now-playing
//! state. The Quick Connect functions at the bottom produce that
//! `JellyfinCredentials` in the first place, independent of how/when a
//! caller decides to invoke them or show the resulting code -- that
//! decision is out of scope for this module.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};

use crate::fcast::RemoteCommand;

/// Castoff's fixed client/device identity presented to Jellyfin for every
/// request -- the Quick Connect login and every Session REST call
/// afterwards, so they are all unambiguously the same client to the server.
const CLIENT_NAME: &str = "castoff";

/// How long a single Jellyfin REST call may take before this daemon gives up
/// on it. Generous but bounded: a hung server must not wedge a `Player`
/// method (these all run on `spawn_blocking`, see `main.rs`, but a stuck
/// thread there is still a real resource leak over time).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to poll `/QuickConnect/Connect` while waiting for a human to
/// approve the on-screen code, matching jellyfin-web's own polling cadence
/// (`authenticateQuickConnect`'s `setInterval(..., 5000)`).
///
/// Not read anywhere in the daemon itself yet: *when* a login is triggered
/// and *how* its code reaches the screen is an open product decision (no
/// existing castoff affordance shows arbitrary status text once a web page
/// -- which would include Jellyfin's own -- is stacked over mpv's window;
/// see this module's doc comment). `login_with_quick_connect` below and
/// everything it calls are complete and tested in isolation, ready for
/// whatever that decision wires them to.
#[cfg_attr(not(test), allow(dead_code))]
const QUICK_CONNECT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The server's own Quick Connect request timeout
/// (`QuickConnectManager.cs`'s `Timeout = 10` minutes) -- waiting any longer
/// than this for an approval is pointless, the secret has already expired
/// server-side. Same pending-wiring note as `QUICK_CONNECT_POLL_INTERVAL`.
#[cfg_attr(not(test), allow(dead_code))]
const QUICK_CONNECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const STATE_FILE_NAME: &str = "jellyfin.json";

/// Persisted Jellyfin login: castoff's own device id (chosen once, before
/// any login exists, so it has to be stored independently of
/// `credentials`) plus the credentials a completed Quick Connect login
/// produced, if any.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct JellyfinState {
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    credentials: Option<JellyfinCredentials>,
}

/// A completed Jellyfin login: enough to act as that session's client for
/// every Session REST call below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JellyfinCredentials {
    /// The server's base URL, e.g. `https://jellyfin.example.com`, with no
    /// trailing slash. Also doubles as the key that decides whether a given
    /// displayed page is *this* Jellyfin session: see `controls`.
    pub(crate) server_url: String,
    pub(crate) access_token: String,
    pub(crate) user_id: String,
}

/// Jellyfin's now-playing state for the controlled session, in the shape
/// `Player` needs to fold into a `PlaybackUpdateMessage`/`QueueItemMessage`
/// -- see README's "Queueing (private extension)" and the
/// `castoff-jellyfin-integration-ux` report's recommendation of one
/// evolving placeholder entry rather than a mirrored per-item queue.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct NowPlaying {
    pub(crate) title: Option<String>,
    pub(crate) duration_secs: Option<f64>,
    pub(crate) position_secs: Option<f64>,
    pub(crate) is_paused: bool,
}

/// Jellyfin ticks are 100ns units (confirmed against jellyfin-web's own
/// `seekMs`: `const ticks = ms * 10000`, i.e. 10,000,000 ticks/second).
const TICKS_PER_SECOND: f64 = 10_000_000.0;

fn secs_to_ticks(secs: f64) -> i64 {
    (secs * TICKS_PER_SECOND).round() as i64
}

fn ticks_to_secs(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

/// The origin (`scheme://host[:port]`) of `url`, for comparing a displayed
/// page's URL against a Jellyfin server's base URL without pulling in a full
/// URL-parsing crate -- matches `webpage.rs`'s own "small, manual, good
/// enough for this daemon's own URLs" style (see its `validate_url`).
fn origin_of(url: &str) -> Option<&str> {
    let after_scheme = url.find("://").map(|i| i + 3)?;
    let authority_end = url[after_scheme..]
        .find(['/', '?', '#'])
        .map(|i| after_scheme + i)
        .unwrap_or(url.len());
    Some(&url[..authority_end])
}

/// One `Playing` sub-command (`POST /Sessions/{id}/Playing/{command}`,
/// `PlaystateCommand` server-side). Only the ones FCast's existing opcodes
/// need are named; Jellyfin has more (`NextTrack`, `Rewind`, ...) that
/// castoff has no FCast opcode to drive yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaystateCommand {
    Pause,
    Unpause,
}

impl PlaystateCommand {
    fn as_str(self) -> &'static str {
        match self {
            PlaystateCommand::Pause => "Pause",
            PlaystateCommand::Unpause => "Unpause",
        }
    }
}

impl RemoteCommand {
    /// The `GeneralCommand.Name` value this maps to
    /// (`MediaBrowser.Model/Session/GeneralCommandType.cs`), dispatched by
    /// the receiving jellyfin-web tab exactly like a physical remote's D-pad
    /// (`serverNotifications.js`'s `processGeneralCommand`).
    fn jellyfin_name(self) -> &'static str {
        match self {
            RemoteCommand::MoveUp => "MoveUp",
            RemoteCommand::MoveDown => "MoveDown",
            RemoteCommand::MoveLeft => "MoveLeft",
            RemoteCommand::MoveRight => "MoveRight",
            RemoteCommand::Select => "Select",
            RemoteCommand::Back => "Back",
        }
    }
}

struct SessionState {
    device_id: String,
    credentials: Option<JellyfinCredentials>,
    /// Cached `Sessions` id for `credentials`' device, resolved lazily
    /// (`resolve_session`) and invalidated whenever `credentials` changes.
    session_id: Option<String>,
}

/// Owns castoff's Jellyfin login (if any) and every Session REST call made
/// against it: passthrough playback control, now-playing polling, and
/// directional/select remote navigation. One instance lives on `Player`,
/// mirroring how `WebpageController`/`ImageStore` are owned.
pub(crate) struct JellyfinSession {
    agent: ureq::Agent,
    state: Mutex<SessionState>,
    state_path: Option<PathBuf>,
}

impl JellyfinSession {
    pub(crate) fn from_env() -> Self {
        Self::with_state_path(crate::queue::default_state_dir().map(|dir| dir.join(STATE_FILE_NAME)))
    }

    fn with_state_path(state_path: Option<PathBuf>) -> Self {
        let loaded = state_path.as_deref().and_then(load_state);
        let (device_id, credentials) = match loaded {
            Some(state) => (
                state.device_id.unwrap_or_else(generate_device_id),
                state.credentials,
            ),
            None => (generate_device_id(), None),
        };
        let session = Self {
            agent: ureq::AgentBuilder::new()
                .timeout(REQUEST_TIMEOUT)
                .build(),
            state: Mutex::new(SessionState {
                device_id,
                credentials,
                session_id: None,
            }),
            state_path,
        };
        // Persist a freshly generated device id immediately, so a login
        // started against it (which embeds it in the Quick Connect
        // `Initiate` call) is reproducible across a restart even before any
        // credentials exist.
        session.persist();
        session
    }

    /// An in-memory-only session (no persisted state file), for tests --
    /// both this module's own and `player.rs`'s routing tests.
    #[cfg(test)]
    pub(crate) fn ephemeral_for_test() -> Self {
        Self::with_state_path(None)
    }

    fn persist(&self) {
        let Some(path) = &self.state_path else { return };
        let state = {
            let s = self.state.lock().unwrap();
            JellyfinState {
                device_id: Some(s.device_id.clone()),
                credentials: s.credentials.clone(),
            }
        };
        if let Err(e) = try_save(&state, path) {
            warn!(error = %e, ?path, "failed to persist Jellyfin login state");
        }
    }

    pub(crate) fn device_id(&self) -> String {
        self.state.lock().unwrap().device_id.clone()
    }

    pub(crate) fn credentials(&self) -> Option<JellyfinCredentials> {
        self.state.lock().unwrap().credentials.clone()
    }

    /// Adopt a completed login, persist it, and drop any cached session id
    /// (it belonged to the previous credentials, if any). Only called by
    /// `login_with_quick_connect` and tests today -- see that method's
    /// pending-wiring note.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_credentials(&self, credentials: JellyfinCredentials) {
        {
            let mut s = self.state.lock().unwrap();
            s.credentials = Some(credentials);
            s.session_id = None;
        }
        self.persist();
    }

    /// Whether `url` is a page this session's credentials would control --
    /// i.e. it shares the logged-in Jellyfin server's origin. This is how
    /// `Player` decides whether an FCast `Pause`/`Resume`/`Seek`/`SetVolume`
    /// (or the new remote-nav opcode) should be routed here instead of to
    /// mpv: see `player.rs`.
    pub(crate) fn controls(&self, url: &str) -> bool {
        let Some(creds) = self.credentials() else {
            return false;
        };
        match (origin_of(url), origin_of(&creds.server_url)) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }

    fn auth_header(&self, token: Option<&str>) -> String {
        let device_id = self.device_id();
        let mut header = format!(
            r#"MediaBrowser Client="{CLIENT_NAME}", Device="{CLIENT_NAME}", DeviceId="{device_id}", Version="{}""#,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(token) = token {
            header.push_str(&format!(r#", Token="{token}""#));
        }
        header
    }

    /// Resolve (and cache) this login's own `Sessions` id: the jellyfin-web
    /// tab logged in with the same credentials registers a session under
    /// the same `DeviceId`, so filtering `GET /Sessions` by it is sufficient
    /// -- the same query `sessionPlayer.getTargets()` uses for the official
    /// app's own "Play On" list, just filtered *to* this device instead of
    /// excluding it (see the `castoff-jellyfin-integration-ux` report).
    fn resolve_session(&self) -> Result<SessionInfoDto> {
        let creds = self
            .credentials()
            .context("no Jellyfin login yet; nothing to control")?;
        let device_id = self.device_id();
        let url = format!(
            "{}/Sessions?DeviceId={}",
            creds.server_url,
            urlencode(&device_id)
        );
        let sessions: Vec<SessionInfoDto> = self
            .agent
            .get(&url)
            .set("Authorization", &self.auth_header(Some(&creds.access_token)))
            .call()
            .context("GET /Sessions failed")?
            .into_json()
            .context("GET /Sessions returned unparsable JSON")?;
        let session = sessions.into_iter().next().with_context(|| {
            format!(
                "no Jellyfin session is registered for device {device_id:?} yet -- is the \
                 jellyfin-web tab logged in and still open?"
            )
        })?;
        self.state.lock().unwrap().session_id = Some(session.id.clone());
        Ok(session)
    }

    /// The cached session id, resolving it first if this is the first call
    /// since login (or since the last `set_credentials`).
    fn session_id(&self) -> Result<String> {
        if let Some(id) = self.state.lock().unwrap().session_id.clone() {
            return Ok(id);
        }
        Ok(self.resolve_session()?.id)
    }

    fn playing_command(&self, command: PlaystateCommand) -> Result<()> {
        let creds = self
            .credentials()
            .context("no Jellyfin login yet; nothing to control")?;
        let id = self.session_id()?;
        let url = format!("{}/Sessions/{id}/Playing/{}", creds.server_url, command.as_str());
        self.agent
            .post(&url)
            .set("Authorization", &self.auth_header(Some(&creds.access_token)))
            .call()
            .with_context(|| format!("POST /Sessions/{id}/Playing/{} failed", command.as_str()))?;
        Ok(())
    }

    pub(crate) fn pause(&self) -> Result<()> {
        self.playing_command(PlaystateCommand::Pause)
    }

    pub(crate) fn resume(&self) -> Result<()> {
        self.playing_command(PlaystateCommand::Unpause)
    }

    pub(crate) fn seek(&self, position_secs: f64) -> Result<()> {
        let creds = self
            .credentials()
            .context("no Jellyfin login yet; nothing to control")?;
        let id = self.session_id()?;
        let url = format!(
            "{}/Sessions/{id}/Playing/Seek?seekPositionTicks={}",
            creds.server_url,
            secs_to_ticks(position_secs)
        );
        self.agent
            .post(&url)
            .set("Authorization", &self.auth_header(Some(&creds.access_token)))
            .call()
            .with_context(|| format!("POST /Sessions/{id}/Playing/Seek failed"))?;
        Ok(())
    }

    fn general_command(&self, name: &str, arguments: Option<HashMap<&str, String>>) -> Result<()> {
        let creds = self
            .credentials()
            .context("no Jellyfin login yet; nothing to control")?;
        let id = self.session_id()?;
        let url = format!("{}/Sessions/{id}/Command", creds.server_url);
        let body = json!({ "Name": name, "Arguments": arguments.unwrap_or_default() });
        self.agent
            .post(&url)
            .set("Authorization", &self.auth_header(Some(&creds.access_token)))
            .send_json(body)
            .with_context(|| format!("POST /Sessions/{id}/Command ({name}) failed"))?;
        Ok(())
    }

    /// FCast's `SetVolume` is 0.0-1.0; Jellyfin's `SetVolume` general
    /// command takes a 0-100 integer percentage.
    pub(crate) fn set_volume(&self, volume: f64) -> Result<()> {
        let mut args = HashMap::new();
        args.insert("Volume", ((volume * 100.0).round() as i64).to_string());
        self.general_command("SetVolume", Some(args))
    }

    /// Directional/select navigation (castoff's new FCast
    /// `Opcode::RemoteCommand`): dispatched inside the tab exactly like a
    /// real remote's D-pad, via `GeneralCommand`'s own first-class
    /// `MoveUp`/`MoveDown`/`MoveLeft`/`MoveRight`/`Select`/`Back` values --
    /// see the `castoff-jellyfin-integration-ux` report §4.
    pub(crate) fn remote_command(&self, command: RemoteCommand) -> Result<()> {
        self.general_command(command.jellyfin_name(), None)
    }

    /// The controlled session's current now-playing state, or `None` if
    /// nothing is playing there. Re-resolves the session id as a side
    /// effect (cheap: it's the same `GET /Sessions?DeviceId=...` call), so
    /// this alone is enough to recover from the tab having re-registered
    /// under a new session id (e.g. after a reload).
    pub(crate) fn now_playing(&self) -> Result<Option<NowPlaying>> {
        let session = self.resolve_session()?;
        let Some(item) = session.now_playing_item else {
            return Ok(None);
        };
        let play_state = session.play_state.unwrap_or_default();
        Ok(Some(NowPlaying {
            title: item.name,
            duration_secs: item.run_time_ticks.map(ticks_to_secs),
            position_secs: play_state.position_ticks.map(ticks_to_secs),
            is_paused: play_state.is_paused,
        }))
    }

    /// Complete a one-time Quick Connect login against `server_url`:
    /// `POST /QuickConnect/Initiate`, hand the resulting code to `on_code`
    /// (so a caller can show it on screen) and poll `GET /QuickConnect/Connect`
    /// until a human approves it elsewhere, then redeem the secret
    /// (`POST /Users/AuthenticateWithQuickConnect`) and adopt the resulting
    /// credentials. Blocks for up to [`QUICK_CONNECT_TIMEOUT`] (matching the
    /// server's own secret lifetime); callers run this off whatever thread
    /// can afford to block (e.g. `spawn_blocking`, like every other `Player`
    /// method).
    ///
    /// Deliberately does not attempt to also log the *browser tab* in with
    /// the resulting token -- see this module's doc comment.
    ///
    /// Not called anywhere in the daemon yet -- see
    /// `QUICK_CONNECT_POLL_INTERVAL`'s doc comment for why.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn login_with_quick_connect(
        &self,
        server_url: &str,
        mut on_code: impl FnMut(&str),
    ) -> Result<JellyfinCredentials> {
        let server_url = server_url.trim_end_matches('/');
        let device_id = self.device_id();
        let pending = quick_connect_initiate(&self.agent, server_url, &device_id)?;
        on_code(&pending.code);

        let deadline = std::time::Instant::now() + QUICK_CONNECT_TIMEOUT;
        loop {
            if quick_connect_check(&self.agent, server_url, &pending.secret)? {
                break;
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "Jellyfin Quick Connect code {} was never approved within {:?}",
                    pending.code,
                    QUICK_CONNECT_TIMEOUT
                );
            }
            std::thread::sleep(QUICK_CONNECT_POLL_INTERVAL);
        }

        let credentials = quick_connect_authenticate(&self.agent, server_url, &pending.secret)?;
        self.set_credentials(credentials.clone());
        Ok(credentials)
    }
}

fn generate_device_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("castoff-{nanos:x}-{:x}", std::process::id())
}

fn load_state(path: &std::path::Path) -> Option<JellyfinState> {
    match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).ok(),
        Err(_) => None,
    }
}

/// Persist via write-then-rename, the same crash-safety pattern
/// `queue.rs`'s `Queue::save` uses.
fn try_save(state: &JellyfinState, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(state)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Minimal query-string escaping for the one kind of value this module ever
/// puts in a URL (a device id this daemon generated itself, see
/// `generate_device_id`) -- not a general-purpose encoder.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionInfoDto {
    id: String,
    #[serde(default)]
    play_state: Option<PlayStateDto>,
    #[serde(default)]
    now_playing_item: Option<NowPlayingItemDto>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlayStateDto {
    #[serde(default)]
    position_ticks: Option<i64>,
    #[serde(default)]
    is_paused: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NowPlayingItemDto {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    run_time_ticks: Option<i64>,
}

// Everything below (through `probe_server`) is the Quick Connect login
// itself: complete and unit-tested, but not called anywhere in the daemon
// yet pending the open product decision of when a login is triggered and
// how its code reaches the screen -- see `QUICK_CONNECT_POLL_INTERVAL`'s
// doc comment above.

/// A Quick Connect request that has been created but not yet approved.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct QuickConnectPending {
    pub(crate) secret: String,
    pub(crate) code: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[cfg_attr(not(test), allow(dead_code))]
struct QuickConnectInitiateResponse {
    secret: String,
    code: String,
}

/// `POST /QuickConnect/Initiate` needs no prior auth, just the four
/// non-empty `Client`/`Device`/`DeviceId`/`Version` header fields
/// (`QuickConnectManager.cs`'s `TryConnect`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn quick_connect_initiate(
    agent: &ureq::Agent,
    server_url: &str,
    device_id: &str,
) -> Result<QuickConnectPending> {
    let header = format!(
        r#"MediaBrowser Client="{CLIENT_NAME}", Device="{CLIENT_NAME}", DeviceId="{device_id}", Version="{}""#,
        env!("CARGO_PKG_VERSION")
    );
    let response: QuickConnectInitiateResponse = agent
        .post(&format!("{server_url}/QuickConnect/Initiate"))
        .set("Authorization", &header)
        .call()
        .context("POST /QuickConnect/Initiate failed")?
        .into_json()
        .context("POST /QuickConnect/Initiate returned unparsable JSON")?;
    Ok(QuickConnectPending {
        secret: response.secret,
        code: response.code,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[cfg_attr(not(test), allow(dead_code))]
struct QuickConnectStateResponse {
    #[serde(default)]
    authenticated: bool,
}

/// `GET /QuickConnect/Connect?secret=...`: whether a human has approved this
/// request yet. Needs no auth either -- the secret itself is the capability.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn quick_connect_check(agent: &ureq::Agent, server_url: &str, secret: &str) -> Result<bool> {
    let response: QuickConnectStateResponse = agent
        .get(&format!(
            "{server_url}/QuickConnect/Connect?secret={}",
            urlencode(secret)
        ))
        .call()
        .context("GET /QuickConnect/Connect failed")?
        .into_json()
        .context("GET /QuickConnect/Connect returned unparsable JSON")?;
    debug!(authenticated = response.authenticated, "Quick Connect poll");
    Ok(response.authenticated)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[cfg_attr(not(test), allow(dead_code))]
struct AuthenticationResultResponse {
    access_token: String,
    user: AuthenticatedUser,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[cfg_attr(not(test), allow(dead_code))]
struct AuthenticatedUser {
    id: String,
}

/// `POST /Users/AuthenticateWithQuickConnect`: redeem an approved secret for
/// the `AuthenticationResult` the approving human authorized
/// (`QuickConnectManager.cs`'s `GetAuthorizedRequest` -- idempotent, so this
/// can safely be retried).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn quick_connect_authenticate(
    agent: &ureq::Agent,
    server_url: &str,
    secret: &str,
) -> Result<JellyfinCredentials> {
    let response: AuthenticationResultResponse = agent
        .post(&format!("{server_url}/Users/AuthenticateWithQuickConnect"))
        .send_json(json!({ "Secret": secret }))
        .context("POST /Users/AuthenticateWithQuickConnect failed")?
        .into_json()
        .context("POST /Users/AuthenticateWithQuickConnect returned unparsable JSON")?;
    Ok(JellyfinCredentials {
        server_url: server_url.to_string(),
        access_token: response.access_token,
        user_id: response.user.id,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(test), allow(dead_code))]
struct PublicSystemInfo {
    #[serde(default, rename = "ProductName")]
    product_name: String,
}

/// Whether `origin` looks like a Jellyfin server: `GET /System/Info/Public`
/// needs no auth and, on a real Jellyfin server, reports
/// `"ProductName":"Jellyfin Server"` (confirmed live against
/// `demo.jellyfin.org`). Meant to recognize a cast target automatically
/// rather than requiring separate Jellyfin-specific setup, once the pending
/// login-trigger decision wires it in -- not called anywhere yet.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn probe_server(agent: &ureq::Agent, origin: &str) -> bool {
    let Ok(response) = agent
        .get(&format!("{origin}/System/Info/Public"))
        .call()
    else {
        return false;
    };
    let Ok(info) = response.into_json::<PublicSystemInfo>() else {
        return false;
    };
    info.product_name.eq_ignore_ascii_case("Jellyfin Server")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use tiny_http::{Header, Response, Server};

    fn json_header() -> Header {
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
    }

    #[test]
    fn origin_of_strips_path_and_keeps_scheme_host_port() {
        assert_eq!(
            origin_of("https://jellyfin.example.com:8096/web/index.html"),
            Some("https://jellyfin.example.com:8096")
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8096"),
            Some("http://127.0.0.1:8096")
        );
        assert_eq!(origin_of("not-a-url"), None);
    }

    #[test]
    fn ticks_round_trip_through_seconds() {
        assert_eq!(secs_to_ticks(1.5), 15_000_000);
        assert_eq!(ticks_to_secs(15_000_000), 1.5);
    }

    #[test]
    fn controls_matches_only_the_logged_in_servers_origin() {
        let session = JellyfinSession::ephemeral_for_test();
        session.set_credentials(JellyfinCredentials {
            server_url: "http://jellyfin.lan:8096".to_string(),
            access_token: "tok".to_string(),
            user_id: "user".to_string(),
        });
        assert!(session.controls("http://jellyfin.lan:8096/web/index.html"));
        // Case-insensitive, matching `webpage.rs`'s own URL handling.
        assert!(session.controls("HTTP://JELLYFIN.LAN:8096/web/"));
        // The bare origin itself (no path) is also a match.
        assert!(session.controls("http://jellyfin.lan:8096"));
        assert!(!session.controls("http://other.example.com/"));
    }

    #[test]
    fn controls_is_false_before_any_login() {
        let session = JellyfinSession::ephemeral_for_test();
        assert!(!session.controls("http://jellyfin.lan:8096/web/"));
    }

    /// A tiny single-shot mock Jellyfin server: reads one request, hands it
    /// to `respond` to decide the response body, replies, and exits. Good
    /// enough for the one-call-at-a-time methods under test here; multi-call
    /// flows spawn one of these per expected call via `MockServer::sequence`.
    struct MockServer {
        addr: String,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        /// Serve `responses.len()` requests in order, one JSON body per
        /// request (by index), then shut down.
        fn sequence(responses: Vec<serde_json::Value>) -> Self {
            let server = Server::http("127.0.0.1:0").expect("bind mock Jellyfin server");
            let addr = server.server_addr().to_string();
            let handle = std::thread::spawn(move || {
                for body in responses {
                    let Ok(request) = server.recv() else { return };
                    let json = body.to_string();
                    let _ = request.respond(
                        Response::from_string(json)
                            .with_header(json_header()),
                    );
                }
            });
            Self {
                addr,
                handle: Some(handle),
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn logged_in_session(server_url: &str) -> JellyfinSession {
        let session = JellyfinSession::ephemeral_for_test();
        session.set_credentials(JellyfinCredentials {
            server_url: server_url.to_string(),
            access_token: "test-token".to_string(),
            user_id: "test-user".to_string(),
        });
        session
    }

    #[test]
    fn resolve_session_picks_the_first_session_for_this_device() {
        let mock = MockServer::sequence(vec![json!([
            { "Id": "session-1" },
        ])]);
        let session = logged_in_session(&mock.base_url());
        let id = session.session_id().expect("a session id");
        assert_eq!(id, "session-1");
    }

    #[test]
    fn resolve_session_fails_legibly_when_nothing_is_registered_yet() {
        let mock = MockServer::sequence(vec![json!([])]);
        let session = logged_in_session(&mock.base_url());
        let err = session.session_id().expect_err("no session registered");
        assert!(
            err.to_string().contains("no Jellyfin session is registered"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pause_and_resume_send_the_right_playstate_commands() {
        let mock = MockServer::sequence(vec![
            json!([{ "Id": "s1" }]), // resolve_session (pause)
            json!({}),               // the Playing/Pause POST
            json!({}),               // the Playing/Unpause POST (session cached)
        ]);
        let session = logged_in_session(&mock.base_url());
        session.pause().expect("pause");
        session.resume().expect("resume");
    }

    #[test]
    fn seek_converts_seconds_to_ticks_in_the_query_string() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let r1 = server.recv().unwrap();
            let _ = r1.respond(
                Response::from_string(json!([{ "Id": "s1" }]).to_string())
                    .with_header(json_header()),
            );
            let r2 = server.recv().unwrap();
            tx.send(r2.url().to_string()).unwrap();
            let _ = r2.respond(Response::from_string("{}"));
        });
        let session = logged_in_session(&format!("http://{addr}"));
        session.seek(12.5).expect("seek");
        let seek_url = rx.recv().unwrap();
        assert!(
            seek_url.contains("seekPositionTicks=125000000"),
            "unexpected seek URL: {seek_url}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn set_volume_sends_a_zero_to_hundred_percentage() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let r1 = server.recv().unwrap();
            let _ = r1.respond(
                Response::from_string(json!([{ "Id": "s1" }]).to_string())
                    .with_header(json_header()),
            );
            let mut r2 = server.recv().unwrap();
            let mut body = String::new();
            r2.as_reader().read_to_string(&mut body).unwrap();
            tx.send(body).unwrap();
            let _ = r2.respond(Response::from_string("{}"));
        });
        let session = logged_in_session(&format!("http://{addr}"));
        session.set_volume(0.42).expect("set_volume");
        let body = rx.recv().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["Name"], "SetVolume");
        assert_eq!(parsed["Arguments"]["Volume"], "42");
        handle.join().unwrap();
    }

    #[test]
    fn remote_command_dispatches_the_general_command_name() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let r1 = server.recv().unwrap();
            let _ = r1.respond(
                Response::from_string(json!([{ "Id": "s1" }]).to_string())
                    .with_header(json_header()),
            );
            let mut r2 = server.recv().unwrap();
            let mut body = String::new();
            r2.as_reader().read_to_string(&mut body).unwrap();
            tx.send(body).unwrap();
            let _ = r2.respond(Response::from_string("{}"));
        });
        let session = logged_in_session(&format!("http://{addr}"));
        session
            .remote_command(RemoteCommand::MoveUp)
            .expect("remote_command");
        let body = rx.recv().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["Name"], "MoveUp");
        handle.join().unwrap();
    }

    #[test]
    fn now_playing_is_none_when_nothing_is_playing() {
        let mock = MockServer::sequence(vec![json!([{ "Id": "s1" }])]);
        let session = logged_in_session(&mock.base_url());
        assert_eq!(session.now_playing().expect("now_playing"), None);
    }

    #[test]
    fn now_playing_reports_title_duration_and_position_from_ticks() {
        let mock = MockServer::sequence(vec![json!([{
            "Id": "s1",
            "PlayState": { "PositionTicks": 50_000_000, "IsPaused": true },
            "NowPlayingItem": { "Name": "Big Buck Bunny", "RunTimeTicks": 6_000_000_000i64 },
        }])]);
        let session = logged_in_session(&mock.base_url());
        let now_playing = session
            .now_playing()
            .expect("now_playing")
            .expect("something is playing");
        assert_eq!(now_playing.title, Some("Big Buck Bunny".to_string()));
        assert_eq!(now_playing.duration_secs, Some(600.0));
        assert_eq!(now_playing.position_secs, Some(5.0));
        assert!(now_playing.is_paused);
    }

    #[test]
    fn quick_connect_initiate_parses_the_secret_and_code() {
        let mock = MockServer::sequence(vec![
            json!({ "Authenticated": false, "Secret": "sek", "Code": "123456" }),
        ]);
        let agent = ureq::Agent::new();
        let pending = quick_connect_initiate(&agent, &mock.base_url(), "device-1")
            .expect("initiate");
        assert_eq!(pending.secret, "sek");
        assert_eq!(pending.code, "123456");
    }

    #[test]
    fn quick_connect_check_reports_whether_it_was_approved() {
        let mock = MockServer::sequence(vec![json!({ "Authenticated": true })]);
        let agent = ureq::Agent::new();
        let approved = quick_connect_check(&agent, &mock.base_url(), "sek").expect("check");
        assert!(approved);
    }

    #[test]
    fn quick_connect_authenticate_returns_usable_credentials() {
        let mock = MockServer::sequence(vec![json!({
            "AccessToken": "tok-123",
            "User": { "Id": "user-456" },
        })]);
        let agent = ureq::Agent::new();
        let creds = quick_connect_authenticate(&agent, &mock.base_url(), "sek")
            .expect("authenticate");
        assert_eq!(creds.access_token, "tok-123");
        assert_eq!(creds.user_id, "user-456");
        assert_eq!(creds.server_url, mock.base_url());
    }

    #[test]
    fn login_with_quick_connect_polls_until_approved_then_authenticates() {
        let mock = MockServer::sequence(vec![
            json!({ "Authenticated": false, "Secret": "sek", "Code": "654321" }),
            json!({ "Authenticated": false }), // first poll: not yet
            json!({ "Authenticated": true }),  // second poll: approved
            json!({ "AccessToken": "tok", "User": { "Id": "user" } }),
        ]);
        let session = JellyfinSession::ephemeral_for_test();
        let mut shown_code = None;
        // Avoid the real poll cadence slowing this test down: temporarily
        // not possible without an injectable clock, so this test instead
        // relies on the mock only ever returning two polls before approval.
        let creds = {
            // Shrink the interval for this one call via the env override
            // tests use elsewhere in this daemon is overkill for a two-poll
            // wait; accept the fixed real `QUICK_CONNECT_POLL_INTERVAL` here.
            session.login_with_quick_connect(&mock.base_url(), |code| {
                shown_code = Some(code.to_string());
            })
        }
        .expect("login");
        assert_eq!(shown_code, Some("654321".to_string()));
        assert_eq!(creds.access_token, "tok");
        assert_eq!(creds.user_id, "user");
    }

    #[test]
    fn probe_server_recognizes_a_jellyfin_response() {
        let mock = MockServer::sequence(vec![json!({ "ProductName": "Jellyfin Server" })]);
        let agent = ureq::Agent::new();
        assert!(probe_server(&agent, &mock.base_url()));
    }

    #[test]
    fn probe_server_rejects_a_non_jellyfin_response() {
        let mock = MockServer::sequence(vec![json!({ "ProductName": "Something Else" })]);
        let agent = ureq::Agent::new();
        assert!(!probe_server(&agent, &mock.base_url()));
    }

    #[test]
    fn jellyfin_state_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!(
            "castoff-jellyfin-test-{}-{}",
            std::process::id(),
            generate_device_id()
        ));
        let path = dir.join(STATE_FILE_NAME);
        let session = JellyfinSession::with_state_path(Some(path.clone()));
        let device_id = session.device_id();
        session.set_credentials(JellyfinCredentials {
            server_url: "http://jellyfin.lan".to_string(),
            access_token: "tok".to_string(),
            user_id: "user".to_string(),
        });
        drop(session);

        let reloaded = JellyfinSession::with_state_path(Some(path.clone()));
        assert_eq!(reloaded.device_id(), device_id);
        assert_eq!(
            reloaded.credentials(),
            Some(JellyfinCredentials {
                server_url: "http://jellyfin.lan".to_string(),
                access_token: "tok".to_string(),
                user_id: "user".to_string(),
            })
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
