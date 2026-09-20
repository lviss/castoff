//! Storage for images uploaded from the Android app's share flow (see
//! `upload.rs` and README's "Image uploads (private extension)"), plus which
//! of them are tagged for idle-screen wallpaper rotation
//! (`idle_screen.rs`'s rotation timer, via `pick_next_wallpaper`).
//!
//! Persisted the same way `queue.rs` persists the play queue: a small JSON
//! manifest (`images.json`) next to the queue's own state file, written via
//! write-then-rename, loading as empty when the file is missing or corrupt
//! rather than failing the daemon. The image *files* themselves live
//! alongside it in an `images/` subdirectory. Both resolve their location
//! through `queue::default_state_dir`, so an operator has one state-directory
//! knob for the whole daemon, not one per subsystem.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

const MANIFEST_FILE_NAME: &str = "images.json";
const IMAGES_DIR_NAME: &str = "images";

/// One uploaded image: its stable id, the filename its bytes live under
/// inside the store's image directory, the MIME type it was uploaded with,
/// and whether it is currently tagged for idle-screen wallpaper rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredImage {
    pub id: String,
    pub file_name: String,
    pub content_type: String,
    #[serde(default)]
    pub wallpaper: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Manifest {
    images: Vec<StoredImage>,
}

impl Manifest {
    /// Load the persisted manifest from `path`, or start empty if there is
    /// nothing there yet, `path` is `None`, the file can't be read, or its
    /// contents don't parse -- same contract as `queue::Queue::load`: a
    /// corrupt or foreign state file must never stop the daemon from
    /// starting, only cost it the remembered images.
    fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                warn!(
                    error = %e,
                    ?path,
                    "persisted image manifest did not parse; starting with no stored images"
                );
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!(
                    error = %e,
                    ?path,
                    "could not read persisted image manifest; starting with no stored images"
                );
                Self::default()
            }
        }
    }

    /// Persist via write-then-rename, same as `queue::Queue::save`. A no-op
    /// when `path` is `None`; failures are logged, not propagated, since a
    /// manifest that can't be saved should not stop an upload or a tag change
    /// from taking effect for the rest of this run.
    fn save(&self, path: Option<&Path>) {
        let Some(path) = path else { return };
        if let Err(e) = self.try_save(path) {
            warn!(error = %e, ?path, "failed to persist image manifest");
        }
    }

    fn try_save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

struct State {
    manifest: Manifest,
    /// The id most recently picked for wallpaper rotation, so
    /// `pick_next_wallpaper` can avoid repeating it immediately (see
    /// `pick_random_avoiding_repeat`).
    last_wallpaper_id: Option<String>,
}

/// Stored images and their wallpaper tag: shared by the upload HTTP endpoint
/// (`upload.rs`), the `SetImageWallpaper` FCast opcode (`player.rs`), and the
/// idle-screen wallpaper rotation timer (`idle_screen.rs`).
pub struct ImageStore {
    /// Directory the image files themselves live in.
    dir: PathBuf,
    /// Where the manifest is persisted, or `None` if no writable state
    /// directory could be resolved -- uploads/tags then stay in-memory only
    /// for this run, same fallback contract as `queue::Queue`.
    manifest_path: Option<PathBuf>,
    state: Mutex<State>,
}

static ID_SEQ: AtomicU64 = AtomicU64::new(0);

impl ImageStore {
    pub fn from_env() -> Self {
        let (dir, manifest_path) = match crate::queue::default_state_dir() {
            Some(state_dir) => (
                state_dir.join(IMAGES_DIR_NAME),
                Some(state_dir.join(MANIFEST_FILE_NAME)),
            ),
            None => {
                warn!(
                    "could not resolve a state directory (checked CASTOFF_STATE_DIR, \
                     STATE_DIRECTORY, XDG_STATE_HOME, HOME); uploaded images will be stored \
                     under a temporary directory and will not survive a restart this run"
                );
                (std::env::temp_dir().join("castoff-images"), None)
            }
        };
        Self::with_paths(dir, manifest_path)
    }

    /// A store scoped to its own scratch directory, for tests that don't want
    /// to touch (or share) the real default state directory -- the same
    /// reason `player.rs`'s test constructors pass `state_path: None` to
    /// `Queue`. Unlike `Queue`, an `ImageStore` always needs a real directory
    /// to write files into, so this gets one rather than opting out of
    /// persistence entirely.
    #[cfg(test)]
    pub fn ephemeral_for_test() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "castoff-images-test-{}-{}",
            std::process::id(),
            ID_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        Self::with_dir(dir)
    }

    #[cfg(test)]
    pub fn with_dir(dir: PathBuf) -> Self {
        Self::with_paths(
            dir.join(IMAGES_DIR_NAME),
            Some(dir.join(MANIFEST_FILE_NAME)),
        )
    }

    fn with_paths(dir: PathBuf, manifest_path: Option<PathBuf>) -> Self {
        let manifest = Manifest::load(manifest_path.as_deref());
        Self {
            dir,
            manifest_path,
            state: Mutex::new(State {
                manifest,
                last_wallpaper_id: None,
            }),
        }
    }

    /// Store `bytes` (an uploaded image; `content_type` e.g. `image/jpeg`) as
    /// a new stable entry and return it. `content_type` must be an `image/*`
    /// MIME type -- checked here (not left to the HTTP layer) so any future
    /// caller gets the same validation.
    pub fn store(&self, bytes: &[u8], content_type: &str) -> Result<StoredImage> {
        let media_type = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !media_type.starts_with("image/") {
            anyhow::bail!("upload content type {content_type:?} is not an image/* MIME type");
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create image storage directory {:?}", self.dir))?;
        let id = generate_id();
        let file_name = format!("{id}.{}", extension_for_content_type(&media_type));
        let path = self.dir.join(&file_name);
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write uploaded image to {path:?}"))?;
        let stored = StoredImage {
            id,
            file_name,
            content_type: media_type,
            wallpaper: false,
        };
        let mut state = self.state.lock().unwrap();
        state.manifest.images.push(stored.clone());
        state.manifest.save(self.manifest_path.as_deref());
        Ok(stored)
    }

    /// Tag or untag `id` for wallpaper rotation. Returns whether `id` was
    /// found.
    pub fn set_wallpaper(&self, id: &str, wallpaper: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(image) = state.manifest.images.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        image.wallpaper = wallpaper;
        state.manifest.save(self.manifest_path.as_deref());
        true
    }

    /// The absolute path to `id`'s stored file, or `None` if `id` is unknown.
    pub fn path_for(&self, id: &str) -> Option<PathBuf> {
        let state = self.state.lock().unwrap();
        state
            .manifest
            .images
            .iter()
            .find(|i| i.id == id)
            .map(|i| self.dir.join(&i.file_name))
    }

    /// A `file://` URL for `id`'s stored file, for the sender to reference in
    /// an ordinary `Play` (see README's "Image uploads (private extension)").
    pub fn file_url_for(&self, id: &str) -> Option<String> {
        self.path_for(id).map(|path| format!("file://{}", path.display()))
    }

    /// Pick the next wallpaper image to show (see `idle_screen.rs`'s rotation
    /// timer): a random image currently tagged for wallpaper, avoiding an
    /// immediate repeat of the last pick when more than one is tagged.
    /// `None` when nothing is tagged.
    pub fn pick_next_wallpaper(&self) -> Option<PathBuf> {
        let mut state = self.state.lock().unwrap();
        let tagged: Vec<StoredImage> = state
            .manifest
            .images
            .iter()
            .filter(|i| i.wallpaper)
            .cloned()
            .collect();
        let previous = state.last_wallpaper_id.clone();
        let chosen = pick_random_avoiding_repeat(&tagged, previous.as_deref())?.clone();
        state.last_wallpaper_id = Some(chosen.id.clone());
        Some(self.dir.join(&chosen.file_name))
    }
}

fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{seq:x}")
}

/// Map an already-lowercased, parameter-stripped `image/*` MIME type to a
/// file extension mpv's `--image-exts` list recognizes, so mpv detects the
/// stored file as an image purely from its own extension/content sniffing --
/// no daemon-side special-casing needed on the playback side. Falls back to
/// a generic extension for a MIME type not in the common set; mpv's own
/// content probing still generally recognizes such a file's real format.
fn extension_for_content_type(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/tiff" => "tiff",
        "image/svg+xml" => "svg",
        _ => "img",
    }
}

/// Pick a random entry from `candidates`, avoiding `previous`'s id when more
/// than one candidate exists -- there is nothing else to pick when there's
/// only one, so it is returned even if it matches `previous`. `None` when
/// `candidates` is empty.
fn pick_random_avoiding_repeat<'a>(
    candidates: &'a [StoredImage],
    previous: Option<&str>,
) -> Option<&'a StoredImage> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return candidates.first();
    }
    let pool: Vec<&StoredImage> = match previous {
        Some(prev) => {
            let filtered: Vec<&StoredImage> =
                candidates.iter().filter(|c| c.id != prev).collect();
            if filtered.is_empty() {
                candidates.iter().collect()
            } else {
                filtered
            }
        }
        None => candidates.iter().collect(),
    };
    Some(pool[random_index(pool.len())])
}

/// A cheap, non-cryptographic random index in `0..len`, seeded from the
/// system clock plus a per-process counter (so two calls in the same
/// nanosecond still diverge). Good enough for "random wallpaper order"; no
/// need to pull in a `rand` dependency for this.
fn random_index(len: usize) -> usize {
    debug_assert!(len > 0);
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    // splitmix64's finisher, applied to an xor of the two varying inputs.
    let mut x = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x as usize) % len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "castoff-images-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn image(id: &str, wallpaper: bool) -> StoredImage {
        StoredImage {
            id: id.to_string(),
            file_name: format!("{id}.png"),
            content_type: "image/png".to_string(),
            wallpaper,
        }
    }

    #[test]
    fn store_writes_the_file_and_returns_a_usable_id() {
        let dir = scratch_dir("store");
        let store = ImageStore::with_dir(dir.clone());

        let stored = store
            .store(b"fake image bytes", "image/png")
            .expect("store must accept an image/* upload");

        assert!(!stored.id.is_empty());
        assert_eq!(stored.file_name, format!("{}.png", stored.id));
        assert!(!stored.wallpaper, "a freshly uploaded image starts untagged");

        let path = store
            .path_for(&stored.id)
            .expect("path_for must resolve a stored id");
        assert_eq!(
            std::fs::read(&path).expect("stored file must exist"),
            b"fake image bytes"
        );

        let url = store
            .file_url_for(&stored.id)
            .expect("file_url_for must resolve a stored id");
        assert!(url.starts_with("file://"));
        assert!(url.ends_with(&stored.file_name));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_rejects_non_image_content_types() {
        let dir = scratch_dir("reject");
        let store = ImageStore::with_dir(dir.clone());

        let err = store
            .store(b"not an image", "text/html")
            .expect_err("a non-image/* content type must be refused");
        assert!(err.to_string().contains("image/*"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wallpaper_tag_persists_across_a_reload() {
        let dir = scratch_dir("tag-persist");
        let store = ImageStore::with_dir(dir.clone());
        let stored = store.store(b"bytes", "image/jpeg").expect("store");

        assert!(store.set_wallpaper(&stored.id, true), "id must be found");

        let reloaded = ImageStore::with_dir(dir.clone());
        let tagged = reloaded.pick_next_wallpaper();
        assert!(
            tagged.is_some(),
            "the wallpaper tag must survive reloading the manifest from disk"
        );

        assert!(reloaded.set_wallpaper(&stored.id, false), "id must be found");
        let reloaded_again = ImageStore::with_dir(dir.clone());
        assert!(
            reloaded_again.pick_next_wallpaper().is_none(),
            "untagging must also persist"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_wallpaper_on_an_unknown_id_reports_not_found() {
        let dir = scratch_dir("unknown-id");
        let store = ImageStore::with_dir(dir.clone());
        assert!(!store.set_wallpaper("does-not-exist", true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_manifest_loads_as_no_stored_images() {
        let dir = scratch_dir("missing-manifest");
        let _ = std::fs::remove_dir_all(&dir);
        let store = ImageStore::with_dir(dir.clone());
        assert!(store.pick_next_wallpaper().is_none());
        assert!(store.path_for("anything").is_none());
    }

    #[test]
    fn pick_next_wallpaper_is_none_when_nothing_is_tagged() {
        let dir = scratch_dir("none-tagged");
        let store = ImageStore::with_dir(dir.clone());
        store.store(b"a", "image/png").unwrap();
        store.store(b"b", "image/png").unwrap();
        assert!(store.pick_next_wallpaper().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single tagged image is always returned, even though it necessarily
    /// repeats the previous pick every time.
    #[test]
    fn pick_random_avoiding_repeat_returns_the_only_candidate_even_if_it_was_previous() {
        let images = vec![image("solo", true)];
        for _ in 0..5 {
            let picked = pick_random_avoiding_repeat(&images, Some("solo"));
            assert_eq!(picked.map(|i| i.id.as_str()), Some("solo"));
        }
    }

    #[test]
    fn pick_random_avoiding_repeat_returns_none_for_no_candidates() {
        assert!(pick_random_avoiding_repeat(&[], None).is_none());
        assert!(pick_random_avoiding_repeat(&[], Some("x")).is_none());
    }

    /// With more than one candidate, the previous pick must never repeat
    /// immediately, and -- to prove this isn't just hardcoded to "the other
    /// one" -- more than one distinct id must actually appear across many
    /// picks.
    #[test]
    fn pick_random_avoiding_repeat_never_repeats_and_is_actually_random() {
        let images = vec![image("a", true), image("b", true), image("c", true)];
        let mut previous = "a".to_string();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..300 {
            let picked = pick_random_avoiding_repeat(&images, Some(&previous))
                .expect("candidates are non-empty");
            assert_ne!(
                picked.id, previous,
                "must never immediately repeat the previous pick"
            );
            seen.insert(picked.id.clone());
            previous = picked.id.clone();
        }
        assert!(
            seen.len() > 1,
            "expected genuine variety across 300 picks, got only {seen:?}"
        );
    }
}
