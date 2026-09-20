//! The image upload HTTP endpoint: a small local server, separate from the
//! FCast TCP control port, that accepts one image per request and hands it
//! to `images::ImageStore`.
//!
//! FCast's own wire format caps a frame at 32 KiB (`fcast.rs`'s doc
//! comment), nowhere near enough for a phone photo, and this daemon only
//! ever implements FCast as a private single-user protocol anyway (see
//! `queue.rs`'s doc comment) -- so rather than inventing a chunked-frame
//! extension to FCast, images travel over an ordinary local HTTP `POST`
//! instead, on their own port (see README's "Image uploads (private
//! extension)"). The Android app (a later, separate task) makes one request
//! per shared image.

use std::io::Read;
use std::sync::Arc;

use tiny_http::{Header, Method, Response, Server};
use tracing::{error, info, warn};

use crate::images::ImageStore;

/// Default port the image upload server listens on, distinct from FCast's
/// `fcast::DEFAULT_PORT` (46899). Overridable with `CASTOFF_IMAGE_PORT`, the
/// same override pattern `fcast::DEFAULT_PORT`/`CASTOFF_PORT` already use.
pub const DEFAULT_UPLOAD_PORT: u16 = 46900;

/// The only route this server serves.
const UPLOAD_PATH: &str = "/images";

/// Upper bound on one upload's body, generous enough for a full-resolution
/// phone photo while bounding how much memory one request can hold in
/// flight.
const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

/// Start the upload server on `port`, blocking to bind before returning so a
/// bad port (already in use) is reported immediately rather than inside a
/// background thread nobody is watching -- the same contract `main.rs`'s
/// `TcpListener::bind` for the FCast port already has. The accept loop itself
/// runs on its own thread (`tiny_http::Server` is a synchronous, blocking
/// API, not a `tokio` one) for the daemon's lifetime; production never joins
/// it, the same as every other `main.rs`-spawned background task.
pub fn spawn(images: Arc<ImageStore>, port: u16) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow::anyhow!("failed to bind image upload server on port {port}: {e}"))?;
    info!(port, path = UPLOAD_PATH, "image upload server listening");
    Ok(std::thread::spawn(move || {
        for request in server.incoming_requests() {
            handle_request(&images, request);
        }
    }))
}

/// Handle one HTTP request: only `POST /images` with an `image/*`
/// `Content-Type` is accepted. Every response is best-effort (`request`'s own
/// error, if any, is logged rather than propagated -- there is no client left
/// to hand a further error to once `respond` itself has failed).
fn handle_request(images: &ImageStore, mut request: tiny_http::Request) {
    if request.method() != &Method::Post || request.url() != UPLOAD_PATH {
        respond(request, Response::empty(404));
        return;
    }

    if request
        .body_length()
        .is_some_and(|len| len > MAX_UPLOAD_BYTES)
    {
        respond(
            request,
            Response::from_string("upload too large").with_status_code(413),
        );
        return;
    }

    let content_type = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("content-type"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();

    let mut body = Vec::new();
    // Read one more byte than the limit so an over-limit body is detected
    // here even when `Content-Length` was absent or understated, rather than
    // buffering an unbounded request into memory.
    match request
        .as_reader()
        .take(MAX_UPLOAD_BYTES as u64 + 1)
        .read_to_end(&mut body)
    {
        Ok(_) => {}
        Err(e) => {
            warn!(error = %e, "failed to read image upload body");
            respond(
                request,
                Response::from_string("failed to read request body").with_status_code(400),
            );
            return;
        }
    }
    if body.len() > MAX_UPLOAD_BYTES {
        respond(
            request,
            Response::from_string("upload too large").with_status_code(413),
        );
        return;
    }

    match images.store(&body, &content_type) {
        Ok(stored) => {
            let body = UploadResponse {
                id: stored.id.clone(),
                url: images.file_url_for(&stored.id).unwrap_or_default(),
                container: stored.content_type.clone(),
            };
            let json = serde_json::to_string(&body).unwrap_or_default();
            let mut response = Response::from_string(json);
            if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            {
                response.add_header(header);
            }
            respond(request, response);
        }
        Err(e) => {
            warn!(error = %e, "rejected an image upload");
            respond(request, Response::from_string(e.to_string()).with_status_code(400));
        }
    }
}

fn respond<R: std::io::Read>(request: tiny_http::Request, response: Response<R>) {
    if let Err(e) = request.respond(response) {
        error!(error = %e, "failed to write image upload response");
    }
}

/// The upload endpoint's success response: `id` is the stable identifier the
/// sender then references in an ordinary FCast `Play` (`url`/`container`
/// already filled in and ready to send as-is), or in `SetImageWallpaper` to
/// tag it for idle-screen rotation.
#[derive(Debug, serde::Serialize)]
struct UploadResponse {
    id: String,
    url: String,
    container: String,
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use tiny_http::ListenAddr;

    use super::*;

    /// Bind the upload server on an OS-assigned ephemeral port and return
    /// that port, mirroring `main.rs`'s own `spawn_server` test helper.
    fn spawn_test_server(images: Arc<ImageStore>) -> u16 {
        let server = Server::http("127.0.0.1:0").expect("bind upload server");
        let ListenAddr::IP(addr) = server.server_addr() else {
            panic!("expected an IP listen address");
        };
        let port = addr.port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                handle_request(&images, request);
            }
        });
        port
    }

    /// A minimal raw HTTP/1.1 POST, since pulling in an HTTP client crate
    /// just for this one test would be more machinery than the request
    /// itself.
    fn post(port: u16, path: &str, content_type: &str, body: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {content_type}\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n",
            len = body.len(),
        );
        stream.write_all(request.as_bytes()).expect("write request line");
        stream.write_all(body).expect("write body");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("read response");
        let status: u16 = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("status line");
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    #[test]
    fn uploading_an_image_stores_it_and_returns_a_usable_id() {
        let dir = std::env::temp_dir().join(format!(
            "castoff-upload-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let images = Arc::new(ImageStore::with_dir(dir.clone()));
        let port = spawn_test_server(Arc::clone(&images));

        let (status, body) = post(port, "/images", "image/png", b"fake png bytes");
        assert_eq!(status, 200, "body: {body}");

        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
        let id = parsed["id"].as_str().expect("response has an id");
        assert!(!id.is_empty());
        assert_eq!(parsed["container"], "image/png");
        assert!(parsed["url"].as_str().unwrap().starts_with("file://"));

        // The id is immediately usable to look the file back up, and the
        // bytes on disk are exactly what was uploaded.
        let path = images.path_for(id).expect("stored id must resolve");
        assert_eq!(std::fs::read(path).expect("read stored file"), b"fake png bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_image_content_type_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "castoff-upload-test-reject-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let images = Arc::new(ImageStore::with_dir(dir.clone()));
        let port = spawn_test_server(images);

        let (status, _) = post(port, "/images", "text/plain", b"not an image");
        assert_eq!(status, 400);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_method_or_path_is_not_found() {
        let dir = std::env::temp_dir().join(format!(
            "castoff-upload-test-404-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let images = Arc::new(ImageStore::with_dir(dir.clone()));
        let port = spawn_test_server(images);

        let (status, _) = post(port, "/not-images", "image/png", b"x");
        assert_eq!(status, 404);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
