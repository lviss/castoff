//! Jellyfin login and Session REST client.
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
//! # How the login itself happens
//!
//! The daemon never calls `/QuickConnect/Initiate` itself. `localStorage`
//! (where jellyfin-web keeps its login) is strictly origin-scoped, so no
//! page the daemon serves from its own origin could write it for the
//! Jellyfin server's -- the only way to get the *browser tab's own* session
//! logged in is to let jellyfin-web run its *own* Quick Connect flow inside
//! itself. `maybe_start_login`/`login_via_browser_tab` drive that flow via a
//! minimal Chrome DevTools Protocol client (`cdp.rs`): click the login
//! page's own Quick Connect button (so its own JS runs the Initiate/poll/
//! Authenticate dance and shows the code in its own dialog -- nothing here
//! needs to display one), wait for a human to approve it elsewhere, then
//! read back the token/user/device id that flow already obtained via
//! `window.ApiClient`, which jellyfin-web deliberately exposes globally
//! (confirmed in the `castoff-jellyfin-integration-ux` report). Everything
//! below this point in the module -- resolving the tab's own session,
//! sending it playback/volume/navigation commands, polling its now-playing
//! state -- just needs the resulting `JellyfinCredentials` and does not care
//! how they were obtained.
//!
//! The browser tab's own device id (read back the same way) is what makes
//! this work: the credentials the daemon uses for its REST calls are for
//! *the same device* jellyfin-web registered its session under, so
//! `GET /Sessions?DeviceId=<that id>` resolves to the tab's own session --
//! there is no separate "daemon device" identity to keep in sync with it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::cdp;
use crate::fcast::RemoteCommand;

/// Castoff's fixed client identity presented to Jellyfin for every Session
/// REST call (the `DeviceId` comes from the logged-in credentials instead --
/// see this module's doc comment).
const CLIENT_NAME: &str = "castoff";

/// How long a single Jellyfin REST call may take before this daemon gives up
/// on it. Generous but bounded: a hung server must not wedge a `Player`
/// method (these all run on `spawn_blocking`, see `main.rs`, but a stuck
/// thread there is still a real resource leak over time).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for Chromium's DevTools HTTP server to come up after the
/// engine is spawned (see `cdp::wait_for_devtools_port`).
const DEVTOOLS_PORT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the login page's own Quick Connect button to become
/// clickable. It starts hidden (`class="... btnQuick hide"`); jellyfin-web's
/// own `viewshow` handler removes `hide` once its own
/// `GET /QuickConnect/Enabled` call resolves true (confirmed from
/// `jellyfin-web`'s `login/index.js`), so this is page-load plus one of the
/// page's own network round trips, not instant.
const QUICK_CONNECT_BUTTON_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a human to approve the Quick Connect code
/// jellyfin-web's own dialog is showing, matching the server's own secret
/// lifetime (`QuickConnectManager.cs`'s `Timeout = 10` minutes) -- waiting
/// any longer is pointless, the secret has already expired server-side.
const LOGIN_WAIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often `wait_until` re-evaluates its condition.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

const STATE_FILE_NAME: &str = "jellyfin.json";

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
    /// The browser tab's own device id (`window.ApiClient.deviceId()`),
    /// read back from the same login that produced `access_token`. Every
    /// REST call below presents this exact id in its own `Authorization`
    /// header and uses it to find the tab's session
    /// (`GET /Sessions?DeviceId=...`) -- see this module's doc comment for
    /// why there is no separate daemon-chosen device id.
    pub(crate) device_id: String,
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

#[derive(Default)]
struct SessionState {
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
        let credentials = state_path.as_deref().and_then(load_credentials);
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(REQUEST_TIMEOUT)
                .build(),
            state: Mutex::new(SessionState {
                credentials,
                session_id: None,
            }),
            state_path,
        }
    }

    /// An in-memory-only session (no persisted state file), for tests --
    /// both this module's own and `player.rs`'s routing tests.
    #[cfg(test)]
    pub(crate) fn ephemeral_for_test() -> Self {
        Self::with_state_path(None)
    }

    fn persist(&self) {
        let Some(path) = &self.state_path else { return };
        let credentials = self.state.lock().unwrap().credentials.clone();
        if let Err(e) = try_save(&credentials, path) {
            warn!(error = %e, ?path, "failed to persist Jellyfin login state");
        }
    }

    pub(crate) fn credentials(&self) -> Option<JellyfinCredentials> {
        self.state.lock().unwrap().credentials.clone()
    }

    /// Adopt a completed login, persist it, and drop any cached session id
    /// (it belonged to the previous credentials, if any).
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

    /// `DeviceId` here is `creds.device_id` -- the *browser tab's own*
    /// device id, read back from the login that produced `creds` -- not a
    /// separate identity of the daemon's own (see this module's doc
    /// comment for why there is no such thing).
    fn auth_header(&self, creds: &JellyfinCredentials) -> String {
        format!(
            r#"MediaBrowser Client="{CLIENT_NAME}", Device="{CLIENT_NAME}", DeviceId="{}", Version="{}", Token="{}""#,
            creds.device_id,
            env!("CARGO_PKG_VERSION"),
            creds.access_token
        )
    }

    /// Resolve (and cache) this login's own `Sessions` id: the jellyfin-web
    /// tab logged in with these credentials registers a session under the
    /// same `DeviceId`, so filtering `GET /Sessions` by it is sufficient --
    /// the same query `sessionPlayer.getTargets()` uses for the official
    /// app's own "Play On" list, just filtered *to* this device instead of
    /// excluding it (see the `castoff-jellyfin-integration-ux` report).
    fn resolve_session(&self) -> Result<SessionInfoDto> {
        let creds = self
            .credentials()
            .context("no Jellyfin login yet; nothing to control")?;
        let url = format!(
            "{}/Sessions?DeviceId={}",
            creds.server_url,
            urlencode(&creds.device_id)
        );
        let sessions: Vec<SessionInfoDto> = self
            .agent
            .get(&url)
            .set("Authorization", &self.auth_header(&creds))
            .call()
            .context("GET /Sessions failed")?
            .into_json()
            .context("GET /Sessions returned unparsable JSON")?;
        let session = sessions.into_iter().next().with_context(|| {
            format!(
                "no Jellyfin session is registered for device {:?} yet -- is the \
                 jellyfin-web tab logged in and still open?",
                creds.device_id
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
            .set("Authorization", &self.auth_header(&creds))
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
            .set("Authorization", &self.auth_header(&creds))
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
            .set("Authorization", &self.auth_header(&creds))
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

}

/// If `url` is a page served by a Jellyfin server this session isn't
/// already logged into, probe whether it really is one (`probe_server`)
/// and, if so, start the browser-driven login dance
/// (`login_via_browser_tab`) on its own detached thread -- never on the
/// caller's own stack, since it can block for minutes waiting on a human to
/// approve the on-screen code. Called right after `webpage.show(url)`
/// succeeds, from both the explicit webpage `Play` path and the async
/// media-to-webpage fallback path (`player.rs`).
pub(crate) fn maybe_start_login(jellyfin: &Arc<JellyfinSession>, profile_dir: PathBuf, url: &str) {
    if jellyfin.controls(url) {
        return; // cheap, in-memory check: no need for a thread at all.
    }
    let Some(origin) = origin_of(url).map(str::to_string) else {
        return;
    };
    // Everything past this point touches the network (the probe, then the
    // login dance itself), so none of it runs on the caller's own stack --
    // `play_webpage`/`fall_back_to_browser` must stay as fast as they were
    // for every *other* displayed page, Jellyfin or not.
    let jellyfin = Arc::clone(jellyfin);
    std::thread::spawn(move || {
        if !probe_server(&jellyfin.agent, &origin) {
            return;
        }
        info!(origin, "detected a Jellyfin server; starting its Quick Connect login");
        match login_via_browser_tab(&profile_dir, &origin) {
            Ok(credentials) => {
                info!(origin, "Jellyfin login completed");
                jellyfin.set_credentials(credentials);
            }
            Err(e) => warn!(
                origin,
                error = %e,
                "Jellyfin login via the displayed browser tab failed"
            ),
        }
    });
}

/// The login page's own Quick Connect button selector (confirmed from
/// `jellyfin-web`'s `login/index.html`/`index.js`): hidden by default,
/// shown once the page's own `GET /QuickConnect/Enabled` call resolves true.
const QUICK_CONNECT_BUTTON_READY_JS: &str =
    "(() => { const b = document.querySelector('.btnQuick'); return !!(b && !b.classList.contains('hide')); })()";
const CLICK_QUICK_CONNECT_JS: &str = "document.querySelector('.btnQuick').click()";
/// `window.ApiClient` is jellyfin-web's own deliberately-global live client
/// instance (`ServerConnections.js`'s `window.ApiClient = apiClient`);
/// `accessToken()` returns `null` until its internal login flow succeeds.
const IS_LOGGED_IN_JS: &str =
    "(() => !!(window.ApiClient && typeof window.ApiClient.accessToken === 'function' && window.ApiClient.accessToken()))()";
const READ_CREDENTIALS_JS: &str = "({ accessToken: window.ApiClient.accessToken(), userId: window.ApiClient.getCurrentUserId(), deviceId: window.ApiClient.deviceId() })";

/// Log in to Jellyfin by driving the login page already displayed in the
/// browser engine via CDP: wait for its own Quick Connect button to become
/// clickable, click it -- which runs jellyfin-web's *own* Initiate/poll/
/// Authenticate dance and shows the code in its own dialog, so nothing here
/// needs to display one -- then wait for a human to approve it elsewhere
/// and read back the token/user/device id that flow already obtained. See
/// this module's doc comment for why the daemon never calls Jellyfin's
/// Quick Connect REST endpoints itself.
///
/// Blocks for up to [`QUICK_CONNECT_BUTTON_TIMEOUT`] plus
/// [`LOGIN_WAIT_TIMEOUT`]; `maybe_start_login` is what keeps this off
/// `Player::operation`'s lock and every other caller's own stack.
fn login_via_browser_tab(profile_dir: &Path, server_origin: &str) -> Result<JellyfinCredentials> {
    let http_port = cdp::wait_for_devtools_port(profile_dir, DEVTOOLS_PORT_TIMEOUT)?;
    let agent = ureq::Agent::new();
    let ws_url = cdp::discover_page_ws_url(&agent, http_port)?;
    let mut session = cdp::CdpSession::connect(http_port, &ws_url)?;

    wait_until(&mut session, QUICK_CONNECT_BUTTON_READY_JS, QUICK_CONNECT_BUTTON_TIMEOUT)
        .context("Jellyfin's own Quick Connect button never became available on the login page")?;
    session
        .evaluate(CLICK_QUICK_CONNECT_JS)
        .context("failed to click Jellyfin's Quick Connect button")?;

    wait_until(&mut session, IS_LOGGED_IN_JS, LOGIN_WAIT_TIMEOUT)
        .context("Jellyfin Quick Connect login was never approved")?;
    let raw = session
        .evaluate(READ_CREDENTIALS_JS)
        .context("failed to read back Jellyfin's own login credentials")?;
    let field = |name: &str| -> Result<String> {
        raw.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("window.ApiClient did not report a usable {name:?} after login"))
    };
    Ok(JellyfinCredentials {
        server_url: server_origin.to_string(),
        access_token: field("accessToken")?,
        user_id: field("userId")?,
        device_id: field("deviceId")?,
    })
}

/// Poll `expression` (via CDP) every [`POLL_INTERVAL`] until it evaluates
/// truthy or `timeout` elapses.
fn wait_until(session: &mut cdp::CdpSession, expression: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if session.evaluate(expression)?.as_bool() == Some(true) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out after {timeout:?} waiting for {expression:?} to become true");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn load_credentials(path: &Path) -> Option<JellyfinCredentials> {
    match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).ok(),
        Err(_) => None,
    }
}

/// Persist via write-then-rename, the same crash-safety pattern
/// `queue.rs`'s `Queue::save` uses. A no-op when there is nothing to
/// persist yet (no login completed), so no empty/placeholder file is ever
/// written before one exists.
fn try_save(credentials: &Option<JellyfinCredentials>, path: &Path) -> Result<()> {
    let Some(credentials) = credentials else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(credentials)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Minimal query-string escaping for the one kind of value this module ever
/// puts in a URL (the browser tab's own device id, read back via CDP) --
/// not a general-purpose encoder.
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

#[derive(Debug, Clone, Deserialize)]
struct PublicSystemInfo {
    #[serde(default, rename = "ProductName")]
    product_name: String,
}

/// Whether `origin` looks like a Jellyfin server: `GET /System/Info/Public`
/// needs no auth and, on a real Jellyfin server, reports
/// `"ProductName":"Jellyfin Server"` (confirmed live against
/// `demo.jellyfin.org`). This is how `maybe_start_login` recognizes a cast
/// target automatically rather than requiring separate Jellyfin-specific
/// setup.
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
            device_id: "device".to_string(),
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
            device_id: "test-device".to_string(),
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
    fn jellyfin_credentials_round_trip_through_a_file() {
        let dir = std::env::temp_dir().join(format!(
            "castoff-jellyfin-test-{}-{}",
            std::process::id(),
            free_port()
        ));
        let path = dir.join(STATE_FILE_NAME);
        let session = JellyfinSession::with_state_path(Some(path.clone()));
        assert_eq!(session.credentials(), None, "nothing is persisted before a login");
        session.set_credentials(JellyfinCredentials {
            server_url: "http://jellyfin.lan".to_string(),
            access_token: "tok".to_string(),
            user_id: "user".to_string(),
            device_id: "device".to_string(),
        });
        drop(session);

        let reloaded = JellyfinSession::with_state_path(Some(path.clone()));
        assert_eq!(
            reloaded.credentials(),
            Some(JellyfinCredentials {
                server_url: "http://jellyfin.lan".to_string(),
                access_token: "tok".to_string(),
                user_id: "user".to_string(),
                device_id: "device".to_string(),
            })
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A single-connection mock Chromium DevTools endpoint: answers one
    /// `GET /json/list` with a single page target, then performs the real
    /// WebSocket upgrade for that target and answers each
    /// `Runtime.evaluate` call in `script`, in order, with its value.
    fn serve_mock_devtools(port: u16, script: Vec<serde_json::Value>) -> std::thread::JoinHandle<()> {
        use std::io::{Read, Write};
        use tungstenite::Message;

        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind mock DevTools port");
        std::thread::spawn(move || {
            // `GET /json/list`.
            let (mut stream, _) = listener.accept().expect("accept json/list connection");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = json!([{
                "type": "page",
                "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/page/test"),
            }])
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write json/list response");

            // The DevTools WebSocket connection for that page.
            let (stream, _) = listener.accept().expect("accept devtools websocket");
            let mut socket = tungstenite::accept(stream).expect("mock DevTools WS handshake");
            for value in script {
                let text = loop {
                    match socket.read().expect("read CDP request") {
                        Message::Text(text) => break text,
                        _ => continue,
                    }
                };
                let id = serde_json::from_str::<serde_json::Value>(&text).unwrap()["id"].clone();
                let reply = json!({ "id": id, "result": { "result": { "value": value } } });
                socket
                    .send(Message::Text(reply.to_string()))
                    .expect("send mock CDP reply");
            }
        })
    }

    fn write_devtools_active_port(profile_dir: &Path, port: u16) {
        std::fs::create_dir_all(profile_dir).unwrap();
        std::fs::write(
            profile_dir.join("DevToolsActivePort"),
            format!("{port}\n/devtools/browser/mock\n"),
        )
        .unwrap();
    }

    #[test]
    fn login_via_browser_tab_waits_for_the_button_then_reads_back_credentials() {
        let port = free_port();
        let profile_dir = std::env::temp_dir().join(format!(
            "castoff-jellyfin-login-test-{}-{port}",
            std::process::id()
        ));
        write_devtools_active_port(&profile_dir, port);
        let server = serve_mock_devtools(
            port,
            vec![
                json!(false), // Quick Connect button not ready yet
                json!(true),  // button ready
                json!(serde_json::Value::Null), // the click itself
                json!(false), // not logged in yet
                json!(true),  // approved
                json!({ "accessToken": "tok", "userId": "user", "deviceId": "device" }),
            ],
        );

        let creds =
            login_via_browser_tab(&profile_dir, "http://jellyfin.lan").expect("login via tab");
        assert_eq!(creds.server_url, "http://jellyfin.lan");
        assert_eq!(creds.access_token, "tok");
        assert_eq!(creds.user_id, "user");
        assert_eq!(creds.device_id, "device");

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&profile_dir);
    }

    #[test]
    fn login_via_browser_tab_fails_legibly_when_the_devtools_port_never_appears() {
        let profile_dir = std::env::temp_dir().join(format!(
            "castoff-jellyfin-login-missing-test-{}-{}",
            std::process::id(),
            free_port()
        ));
        let _ = std::fs::remove_dir_all(&profile_dir);

        let err = login_via_browser_tab(&profile_dir, "http://jellyfin.lan")
            .expect_err("no DevTools port ever appears");
        assert!(err.to_string().contains("DevToolsActivePort"), "unexpected error: {err}");
    }

    #[test]
    fn maybe_start_login_does_nothing_when_already_logged_into_this_origin() {
        // No mock server at all: if this reached the network it would hang
        // or error inside the background thread, which this test cannot
        // observe directly, but it also must not panic or block the caller.
        let session = Arc::new(logged_in_session("http://jellyfin.lan"));
        maybe_start_login(
            &session,
            PathBuf::from("/nonexistent"),
            "http://jellyfin.lan/web/index.html",
        );
        // Still the same credentials; nothing was reset or re-triggered.
        assert_eq!(session.credentials().unwrap().access_token, "test-token");
    }

    #[test]
    fn maybe_start_login_does_nothing_for_a_non_jellyfin_origin() {
        let mock = MockServer::sequence(vec![json!({ "ProductName": "Grafana" })]);
        let session = Arc::new(JellyfinSession::ephemeral_for_test());
        maybe_start_login(
            &session,
            PathBuf::from("/nonexistent"),
            &format!("{}/d/dashboard", mock.base_url()),
        );
        // The probe (and everything after it) now always runs on its own
        // background thread, never on this call's own stack -- see
        // `maybe_start_login`'s doc comment for why. `MockServer`'s `Drop`
        // joins its one-request server thread, which only returns once the
        // probe this test cares about has actually landed, so waiting for
        // `mock` to drop here is itself the synchronization: no arbitrary
        // sleep needed.
        drop(mock);
        assert_eq!(session.credentials(), None);
    }
}
