//! The play queue: items queued after (or as) the currently playing one, and
//! the daemon's position within it -- see README's "Queueing (private
//! extension)" for the wire contract and `player.rs` for how `Player::play`
//! decides between playing an item immediately and only enqueueing it.
//!
//! Persisted as JSON so the queue survives a daemon restart (see
//! `default_state_path`); `Player::build` reloads it at startup, and every
//! mutation in `player.rs` is followed by a `save` call. A restart does not
//! resume playback on its own -- only the queue's contents and position are
//! remembered; mpv itself always starts fresh and idle, and nothing here
//! auto-plays until a client sends a command.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::fcast::{PlayMessage, QueueItemMessage, QueueStateMessage};
use crate::player::now_millis;

const STATE_FILE_NAME: &str = "queue.json";

/// The play queue: every item ever queued, in queue order, plus the index of
/// the current one. A queued item is stored as the exact `PlayMessage` it was
/// enqueued with, so replaying it (auto-advance, a jump, or a reload after
/// restart) plays it identically to how a fresh `Play` would have.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    items: Vec<PlayMessage>,
    position: Option<usize>,
}

impl Queue {
    /// Append `item` to the queue and return its index.
    pub fn push(&mut self, item: PlayMessage) -> usize {
        self.items.push(item);
        self.items.len() - 1
    }

    pub fn set_position(&mut self, position: Option<usize>) {
        self.position = position;
    }

    /// Not read by the daemon itself outside of tests (`to_state_message`'s
    /// `current_index` is what callers actually want); exists so tests can
    /// assert on the position directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn position(&self) -> Option<usize> {
        self.position
    }

    /// Move to the item after the current one, if any, and return it. Leaves
    /// the position unchanged (and returns `None`) when nothing has played
    /// yet, the queue is empty, or the current item is already last --
    /// callers (an explicit jump, or auto-advance on end-of-file) both treat
    /// that as "nothing to do" rather than an error.
    pub fn jump_forward(&mut self) -> Option<&PlayMessage> {
        let next = self.position?.checked_add(1)?;
        if next >= self.items.len() {
            return None;
        }
        self.position = Some(next);
        self.items.get(next)
    }

    /// Move to the item before the current one, if any. Same "no-op past the
    /// edge" contract as `jump_forward`.
    pub fn jump_backward(&mut self) -> Option<&PlayMessage> {
        let prev = self.position?.checked_sub(1)?;
        self.position = Some(prev);
        self.items.get(prev)
    }

    /// This queue's state as the wire shape sent to FCast senders (see
    /// `Opcode::QueueState`).
    pub fn to_state_message(&self) -> QueueStateMessage {
        QueueStateMessage {
            generation_time: now_millis(),
            items: self
                .items
                .iter()
                .map(|item| QueueItemMessage {
                    url: item.url.clone().unwrap_or_default(),
                    container: item.container.clone(),
                })
                .collect(),
            current_index: self.position,
        }
    }

    /// Load the persisted queue from `path`, or start empty if there is
    /// nothing there yet (first run), `path` is `None` (no writable state
    /// directory could be resolved -- see `default_state_path`), the file
    /// can't be read, or its contents don't parse. A corrupt or foreign state
    /// file must never stop the daemon from starting, only cost it the
    /// remembered queue.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                warn!(
                    error = %e,
                    ?path,
                    "persisted queue state did not parse; starting with an empty queue"
                );
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!(
                    error = %e,
                    ?path,
                    "could not read persisted queue state; starting with an empty queue"
                );
                Self::default()
            }
        }
    }

    /// Persist this queue to `path` via a write-then-rename, so a crash
    /// mid-write can never leave a half-written file the next startup would
    /// fail to parse. A no-op when `path` is `None` (see `load`); failures
    /// are logged, not propagated, since a queue that can't be saved should
    /// not stop playback.
    pub fn save(&self, path: Option<&Path>) {
        let Some(path) = path else { return };
        if let Err(e) = self.try_save(path) {
            warn!(error = %e, ?path, "failed to persist queue state");
        }
    }

    fn try_save(&self, path: &Path) -> anyhow::Result<()> {
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

/// Default queue persistence file: `queue.json` under the daemon's state
/// directory, resolved (first match wins):
/// 1. `CASTOFF_STATE_DIR` -- explicit override, matching this daemon's other
///    `CASTOFF_*` env knobs.
/// 2. `STATE_DIRECTORY` -- set automatically by systemd when the unit
///    declares `StateDirectory=` (see `nix/tv-box.nix`'s `cage-tty1`
///    service); may list several colon-separated directories, of which the
///    first is used.
/// 3. `XDG_STATE_HOME` (the XDG Base Directory spec's state-file location).
/// 4. `$HOME/.local/state/castoff` (XDG's own fallback for an unset
///    `XDG_STATE_HOME`).
///
/// `None` when none of these resolve (no `HOME` either) -- the queue then
/// stays in-memory only for that run; `Player::new` logs this once at
/// startup rather than on every save.
pub fn default_state_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CASTOFF_STATE_DIR") {
        return Some(PathBuf::from(dir).join(STATE_FILE_NAME));
    }
    if let Ok(dirs) = std::env::var("STATE_DIRECTORY") {
        if let Some(first) = dirs.split(':').find(|s| !s.is_empty()) {
            return Some(PathBuf::from(first).join(STATE_FILE_NAME));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return Some(PathBuf::from(xdg).join("castoff").join(STATE_FILE_NAME));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".local/state/castoff")
                .join(STATE_FILE_NAME),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(url: &str) -> PlayMessage {
        PlayMessage {
            url: Some(url.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn jump_forward_and_backward_move_within_bounds_only() {
        let mut queue = Queue::default();
        queue.push(item("a"));
        queue.push(item("b"));
        queue.push(item("c"));
        queue.set_position(Some(0));

        assert_eq!(queue.jump_forward().and_then(|i| i.url.clone()), Some("b".to_string()));
        assert_eq!(queue.position(), Some(1));
        assert_eq!(queue.jump_forward().and_then(|i| i.url.clone()), Some("c".to_string()));
        assert_eq!(queue.position(), Some(2));
        assert!(queue.jump_forward().is_none(), "no item after the last one");
        assert_eq!(
            queue.position(),
            Some(2),
            "position must not move past the end"
        );

        assert_eq!(queue.jump_backward().and_then(|i| i.url.clone()), Some("b".to_string()));
        assert_eq!(queue.jump_backward().and_then(|i| i.url.clone()), Some("a".to_string()));
        assert!(queue.jump_backward().is_none(), "no item before the first one");
        assert_eq!(
            queue.position(),
            Some(0),
            "position must not move before the start"
        );
    }

    #[test]
    fn jump_on_empty_or_unstarted_queue_is_a_no_op() {
        let mut queue = Queue::default();
        assert!(queue.jump_forward().is_none());
        assert!(queue.jump_backward().is_none());

        queue.push(item("a"));
        // Pushed but never started (position still `None`): a jump has
        // nothing to move relative to.
        assert!(queue.jump_forward().is_none());
        assert!(queue.jump_backward().is_none());
    }

    #[test]
    fn persisted_queue_round_trips_through_a_file() {
        let dir =
            std::env::temp_dir().join(format!("castoff-queue-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("queue.json");

        let mut queue = Queue::default();
        queue.push(item("a"));
        queue.push(item("b"));
        queue.set_position(Some(1));
        queue.save(Some(&path));

        let reloaded = Queue::load(Some(&path));
        assert_eq!(reloaded.position(), Some(1));
        let state = reloaded.to_state_message();
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[1].url, "b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_state_file_loads_as_an_empty_queue() {
        let path = std::env::temp_dir().join(format!(
            "castoff-queue-test-missing-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let queue = Queue::load(Some(&path));
        assert!(queue.to_state_message().items.is_empty());
        assert_eq!(queue.position(), None);
    }
}
