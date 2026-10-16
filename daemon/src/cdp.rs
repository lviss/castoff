//! A minimal, blocking Chrome DevTools Protocol client.
//!
//! This exists for exactly one caller (`jellyfin.rs`'s `login_via_browser_tab`):
//! driving the Jellyfin login page already displayed by `webpage.rs`'s
//! browser engine -- clicking its Quick Connect button and reading back the
//! token its own login flow obtained once a human approves it elsewhere. It
//! is not a general browser-automation layer, and it is not how Jellyfin
//! playback/remote-nav commands reach the tab (those go through
//! `jellyfin.rs`'s own REST client and the tab's already-open WebSocket to
//! the Jellyfin server, never through this).

use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tungstenite::{Message, WebSocket};

/// How long a single CDP call may block reading a reply before giving up.
/// Bounds a single `evaluate` call, not the overall login wait -- callers
/// that need to wait for a human (e.g. approving a Quick Connect code) do
/// so by calling `evaluate` repeatedly in their own, much longer, poll loop.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Read Chromium's own `DevToolsActivePort` file -- `<profile dir>/DevToolsActivePort`,
/// written once at startup when launched with `--remote-debugging-port` --
/// to discover the real, OS-assigned debugging port. Polls rather than
/// reading once: the file does not exist until the engine's DevTools HTTP
/// server has actually bound, shortly after the process starts.
///
/// Callers must have already cleared any stale file left by a *previous*
/// engine in the same profile directory before this engine was spawned
/// (`webpage.rs`'s `Browser::clear_singleton_artifacts` does this), or this
/// can return a dead port from an engine that has already exited.
pub(crate) fn wait_for_devtools_port(profile_dir: &Path, timeout: Duration) -> Result<u16> {
    let path = profile_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(port) = contents.lines().next().and_then(|l| l.trim().parse().ok()) {
                return Ok(port);
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "Chromium's DevToolsActivePort file never appeared at {path:?} within {timeout:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[derive(Debug, Deserialize)]
struct CdpTarget {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    web_socket_debugger_url: Option<String>,
}

/// Discover the open page target's own DevTools WebSocket URL via
/// Chromium's `GET /json/list` (its standard, undocumented-but-stable
/// DevTools HTTP endpoint). The engine is launched with exactly one page
/// (`--app=<url>`), so the first `page`-type target is the one to drive.
pub(crate) fn discover_page_ws_url(agent: &ureq::Agent, http_port: u16) -> Result<String> {
    let targets: Vec<CdpTarget> = agent
        .get(&format!("http://127.0.0.1:{http_port}/json/list"))
        .call()
        .context("GET /json/list (Chromium DevTools target discovery) failed")?
        .into_json()
        .context("GET /json/list returned unparsable JSON")?;
    targets
        .into_iter()
        .find(|t| t.kind == "page")
        .and_then(|t| t.web_socket_debugger_url)
        .context("Chromium reported no page target with a DevTools WebSocket URL")
}

/// One CDP session over a page's own DevTools WebSocket: just enough to
/// evaluate a JS expression and get its value back.
pub(crate) struct CdpSession {
    socket: WebSocket<TcpStream>,
    next_id: u64,
}

impl CdpSession {
    /// Connect to `ws_url` (as reported by [`discover_page_ws_url`]) on
    /// `http_port`'s host.
    pub(crate) fn connect(http_port: u16, ws_url: &str) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", http_port))
            .with_context(|| format!("failed to connect to Chromium's DevTools port {http_port}"))?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        let (socket, _response) = tungstenite::client(ws_url, stream)
            .with_context(|| format!("DevTools WebSocket handshake to {ws_url} failed"))?;
        Ok(Self { socket, next_id: 1 })
    }

    /// Evaluate `expression` in the page's own main-frame JS context and
    /// return its value (via CDP's `Runtime.evaluate` with
    /// `returnByValue: true`). Errors on a CDP-level failure, a thrown JS
    /// exception, or a read timing out (see [`READ_TIMEOUT`]) -- the last
    /// of which is also what a torn-down engine (e.g. a superseding `Stop`)
    /// eventually surfaces as, since nothing closes this socket explicitly.
    pub(crate) fn evaluate(&mut self, expression: &str) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": {
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }
        });
        self.socket
            .send(Message::Text(request.to_string()))
            .context("failed to send a CDP Runtime.evaluate request")?;
        loop {
            let message = self
                .socket
                .read()
                .context("failed to read a CDP reply (the engine may have exited)")?;
            let Message::Text(text) = message else { continue };
            let reply: serde_json::Value =
                serde_json::from_str(&text).context("CDP reply was not valid JSON")?;
            // CDP also pushes unsolicited events (no matching id) on this
            // same socket; skip anything that isn't this call's own reply.
            if reply.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                anyhow::bail!("CDP rejected {expression:?}: {error}");
            }
            let result = &reply["result"];
            if let Some(exception) = result.get("exceptionDetails") {
                anyhow::bail!("JS exception evaluating {expression:?}: {exception}");
            }
            return Ok(result["result"]["value"].clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A single-connection mock DevTools WebSocket server: accepts one
    /// connection, performs the WS handshake, reads one `Runtime.evaluate`
    /// request and replies with a canned CDP result -- enough to test
    /// `CdpSession::evaluate` end to end without a real Chromium.
    fn serve_one_evaluate(port: u16, result_value: serde_json::Value) -> std::thread::JoinHandle<serde_json::Value> {
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock DevTools port");
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept mock DevTools connection");
            let mut socket = tungstenite::accept(stream).expect("mock DevTools WS handshake");
            let request = loop {
                match socket.read().expect("read CDP request") {
                    Message::Text(text) => break text,
                    _ => continue,
                }
            };
            let parsed: serde_json::Value = serde_json::from_str(&request).unwrap();
            let id = parsed["id"].clone();
            let reply = serde_json::json!({ "id": id, "result": { "result": { "value": result_value } } });
            socket
                .send(Message::Text(reply.to_string()))
                .expect("send mock CDP reply");
            parsed
        })
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn evaluate_sends_the_expression_and_returns_the_value() {
        let port = free_port();
        let server = serve_one_evaluate(port, serde_json::json!(42));
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/page/test");
        let mut session = CdpSession::connect(port, &ws_url).expect("connect");

        let value = session.evaluate("1 + 41").expect("evaluate");
        assert_eq!(value, serde_json::json!(42));

        let request = server.join().expect("server thread");
        assert_eq!(request["method"], "Runtime.evaluate");
        assert_eq!(request["params"]["expression"], "1 + 41");
    }

    #[test]
    fn evaluate_surfaces_a_js_exception_as_an_error() {
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let Message::Text(text) = socket.read().unwrap() else {
                panic!("expected a text frame")
            };
            let id = serde_json::from_str::<serde_json::Value>(&text).unwrap()["id"].clone();
            let reply = serde_json::json!({
                "id": id,
                "result": { "result": { "type": "undefined" }, "exceptionDetails": { "text": "boom" } }
            });
            socket.send(Message::Text(reply.to_string())).unwrap();
        });
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/page/test");
        let mut session = CdpSession::connect(port, &ws_url).expect("connect");

        let err = session
            .evaluate("throw new Error('boom')")
            .expect_err("a thrown exception must surface as an error");
        assert!(err.to_string().contains("boom"), "unexpected error: {err}");
        handle.join().unwrap();
    }

    #[test]
    fn wait_for_devtools_port_reads_the_first_line_as_a_port_number() {
        let dir = std::env::temp_dir().join(format!(
            "castoff-cdp-test-{}-{}",
            std::process::id(),
            free_port()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("DevToolsActivePort"), "54321\n/devtools/browser/abc\n").unwrap();

        let port = wait_for_devtools_port(&dir, Duration::from_secs(1)).expect("read port");
        assert_eq!(port, 54321);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_for_devtools_port_times_out_legibly_when_the_file_never_appears() {
        let dir = std::env::temp_dir().join(format!(
            "castoff-cdp-test-missing-{}-{}",
            std::process::id(),
            free_port()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let err = wait_for_devtools_port(&dir, Duration::from_millis(200))
            .expect_err("the file never appears");
        assert!(err.to_string().contains("DevToolsActivePort"));
    }
}
