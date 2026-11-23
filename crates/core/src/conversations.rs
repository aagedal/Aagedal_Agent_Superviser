//! Each model's last conversation, saved to disk when the model is unloaded or the app quits and
//! loaded back when the model starts again, so an agent picking up a long conversation doesn't
//! wait for the model to read it all again (24k tokens: 10 s → 0.1 s for Gemma 3 4B on an M5 Pro).
//!
//! llama-server writes and reads the files (`--slot-save-path`, `/slots/0?action=save|restore`);
//! this module decides which to keep. One per model, at most [`MAX_BYTES`] in all and never
//! more than a tenth of the free disk space; unused ones go after [`MAX_AGE`]. They live in the
//! cache folder, which the sandbox hides from agents: they hold every project's chats, and a
//! file planted there would be read as a model's context.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::catalog::KvType;
use crate::runtime::{LlamaServer, Saved};

/// The most all saved conversations take together (10 GB, as Finder counts).
pub const MAX_BYTES: u64 = 10_000_000_000;
/// ...and never more than 1/this of the free space, counting the files already saved so they
/// don't shrink their own room.
const FREE_SHARE: u64 = 10;
/// A conversation not picked up for this long is deleted.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Shorter conversations are read again in a few seconds: not worth the disk. OpenCode's
/// system prompt alone is about 24k tokens, so an agent's conversation is always longer.
pub const MIN_TOKENS: u64 = 4096;
/// A save still being written is younger than this; an older one was left by a crash.
const SAVING_AGE: Duration = Duration::from_secs(600);
/// Files hold context checkpoints (sliding-window and hybrid models) beyond the KV cells.
const ESTIMATE_MARGIN: u64 = 4;

/// What a conversation was saved with. Any change (a new model file, another cache type or
/// context) makes it unusable, so it is deleted rather than loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Key {
    pub model: PathBuf,
    pub size: u64,
    pub modified: u64,
    pub kv: KvType,
    pub context: u64,
}

impl Key {
    pub fn of(model: &Path, kv: KvType, context: u64) -> Option<Self> {
        let meta = std::fs::metadata(model).ok()?;
        let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
        Some(Self { model: model.to_path_buf(), size: meta.len(), modified, kv, context })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    /// The installed model's id.
    id: String,
    key: Key,
    tokens: u64,
    bytes: u64,
    /// When it was saved or last loaded, in seconds since 1970.
    used: u64,
}

/// For Settings.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub count: usize,
    pub bytes: u64,
    pub limit: u64,
}

pub struct Conversations {
    dir: PathBuf,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Ids name files without leaking paths, and llama-server only takes plain file names.
fn stem(id: &str) -> String {
    hex::encode(&Sha256::digest(id.as_bytes())[..8])
}

impl Conversations {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The folder llama-server saves to, made readable by the owner only.
    pub fn prepare(&self) -> std::io::Result<&Path> {
        crate::paths::private_dir(&self.dir)?;
        Ok(&self.dir)
    }

    /// The name llama-server knows `id`'s conversation by.
    pub fn file_name(id: &str) -> String {
        format!("{}.bin", stem(id))
    }

    /// Where a save goes until it is complete, so a failed one never replaces a good one.
    fn temp_name(id: &str) -> String {
        format!("{}-saving.bin", stem(id))
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", stem(id)))
    }

    fn read_meta(path: &Path) -> Option<Meta> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Saved conversations, least recently used first.
    fn saved(&self) -> Vec<Meta> {
        let mut saved: Vec<Meta> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| Self::read_meta(&e.path()))
            .filter(|m| self.dir.join(Self::file_name(&m.id)).is_file())
            .collect();
        saved.sort_by_key(|m| m.used);
        saved
    }

    fn remove(&self, id: &str) {
        let _ = std::fs::remove_file(self.dir.join(Self::file_name(id)));
        let _ = std::fs::remove_file(self.meta_path(id));
    }

    /// Bytes in the folder, whatever they are.
    fn bytes_on_disk(&self) -> u64 {
        std::fs::read_dir(&self.dir).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
    }

    /// What all conversations may take now.
    pub fn limit(&self) -> u64 {
        match crate::installed::available_space(&self.dir) {
            Some(free) => MAX_BYTES.min((free + self.bytes_on_disk()) / FREE_SHARE),
            None => MAX_BYTES,
        }
    }

    pub fn summary(&self) -> Summary {
        let saved = self.saved();
        Summary { count: saved.len(), bytes: self.bytes_on_disk(), limit: self.limit() }
    }

    /// `id`'s conversation if it was saved with `key`, else nothing (and a stale one is deleted).
    pub fn restorable(&self, id: &str, key: &Key) -> Option<String> {
        let meta = Self::read_meta(&self.meta_path(id))?;
        let file = Self::file_name(id);
        if meta.key != *key || !self.dir.join(&file).is_file() {
            self.remove(id);
            return None;
        }
        Some(file)
    }

    fn mark_used(&self, id: &str) {
        if let Some(mut meta) = Self::read_meta(&self.meta_path(id)) {
            meta.used = now();
            let _ = Self::write_meta(&self.meta_path(id), &meta);
        }
    }

    fn write_meta(path: &Path, meta: &Meta) -> std::io::Result<()> {
        crate::paths::write_private(path, serde_json::to_string(meta)?.as_bytes())
    }

    /// Makes room for `bytes` more for `id`, whose old conversation it replaces, by deleting
    /// the least recently used others. False if it can't fit at all.
    fn make_room(&self, id: &str, bytes: u64) -> bool {
        let limit = self.limit();
        if bytes > limit {
            return false;
        }
        let others: Vec<Meta> = self.saved().into_iter().filter(|m| m.id != id).collect();
        let mut total: u64 = others.iter().map(|m| m.bytes).sum();
        for old in others {
            if total + bytes <= limit {
                break;
            }
            tracing::info!("deleting the saved conversation of {} for room", old.id);
            self.remove(&old.id);
            total -= old.bytes;
        }
        true
    }

    /// Deletes conversations unused for [`MAX_AGE`], files no conversation accounts for, and
    /// the least recently used ones while all take more than the limit.
    pub fn prune(&self) {
        let cutoff = now().saturating_sub(MAX_AGE.as_secs());
        let mut saved = self.saved();
        saved.retain(|m| {
            let keep = m.used >= cutoff;
            if !keep {
                self.remove(&m.id);
            }
            keep
        });
        let known: Vec<String> = saved.iter().flat_map(|m| [Self::file_name(&m.id), format!("{}.json", stem(&m.id))]).collect();
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Another model's save or atomic metadata write in progress.
            let in_progress = (name.ends_with("-saving.bin") || name.ends_with(".tmp"))
                && entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age < SAVING_AGE));
            if !in_progress && !known.contains(&name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let limit = self.limit();
        let mut total: u64 = saved.iter().fold(0u64, |acc, m| acc.saturating_add(m.bytes));
        for old in saved {
            if total <= limit {
                break;
            }
            self.remove(&old.id);
            total = total.saturating_sub(old.bytes);
        }
    }

    pub fn forget(&self, id: &str) {
        self.remove(id);
    }

    pub fn clear(&self) {
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    /// Saves `server`'s conversation as `id`'s, replacing the one before. Nothing is saved while
    /// a reply is being written, for a short conversation, or when it wouldn't fit;
    /// `bytes_for(tokens)` estimates its size beforehand.
    pub async fn save(&self, server: &LlamaServer, id: &str, key: &Key, bytes_for: impl Fn(u64) -> u64) -> anyhow::Result<Option<Saved>> {
        let slot = server.slot().await?;
        if slot.busy || slot.tokens < MIN_TOKENS {
            return Ok(None);
        }
        let estimate = bytes_for(slot.tokens);
        if !self.make_room(id, estimate + estimate / ESTIMATE_MARGIN) {
            tracing::info!("{id}: a conversation of {} tokens (about {} MB) doesn't fit the room for saved ones", slot.tokens, estimate / 1_000_000);
            return Ok(None);
        }
        self.prepare()?;
        let temp = self.dir.join(Self::temp_name(id));
        let saved = match server.save_slot(&Self::temp_name(id)).await {
            Ok(saved) => saved,
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(e);
            }
        };
        std::fs::rename(&temp, self.dir.join(Self::file_name(id)))?;
        let meta = Meta { id: id.to_string(), key: key.clone(), tokens: saved.tokens, bytes: saved.bytes, used: now() };
        Self::write_meta(&self.meta_path(id), &meta)?;
        self.prune();
        Ok(Some(saved))
    }

    /// Loads `id`'s conversation into `server` if one was saved with `key`; returns its tokens.
    /// One that won't load is deleted.
    pub async fn restore(&self, server: &LlamaServer, id: &str, key: &Key) -> Option<u64> {
        let file = self.restorable(id, key)?;
        match server.restore_slot(&file).await {
            Ok(tokens) => {
                self.mark_used(id);
                Some(tokens)
            }
            Err(e) => {
                tracing::warn!("{id}: {e:#}; deleting the saved conversation");
                self.remove(id);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("superviser-conversations-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(context: u64) -> Key {
        Key { model: "/m/a.gguf".into(), size: 1, modified: 2, kv: KvType::Q8, context }
    }

    /// A conversation as [`Conversations::save`] leaves it, `age` seconds after it was used.
    fn put(c: &Conversations, id: &str, bytes: u64, age: u64) {
        std::fs::write(c.dir.join(Conversations::file_name(id)), vec![0u8; bytes as usize]).unwrap();
        let meta = Meta { id: id.into(), key: key(65_536), tokens: 5000, bytes, used: now() - age };
        Conversations::write_meta(&c.meta_path(id), &meta).unwrap();
    }

    #[test]
    fn only_a_conversation_saved_the_same_way_is_loaded() {
        let c = Conversations::new(temp());
        put(&c, "model-a", 10, 0);
        assert_eq!(c.restorable("model-a", &key(65_536)), Some(Conversations::file_name("model-a")));
        // Loaded at another context: deleted, not loaded.
        assert_eq!(c.restorable("model-a", &key(32_768)), None);
        assert_eq!(c.summary().count, 0);
        std::fs::remove_dir_all(&c.dir).ok();
    }

    #[test]
    fn room_is_made_from_the_least_recently_used() {
        let c = Conversations::new(temp());
        put(&c, "old", 100, 300);
        put(&c, "newer", 100, 200);
        put(&c, "mine", 100, 100);
        // Plenty of room: nothing goes.
        assert!(c.make_room("mine", 100));
        assert_eq!(c.summary().count, 3);
        // More than the limit alone: refused, nothing deleted.
        assert!(!c.make_room("mine", c.limit() + 1));
        assert_eq!(c.summary().count, 3);
        // Room for all but 150 bytes beside the others: the oldest goes first, and `mine`
        // doesn't count, since the new one replaces it.
        assert!(c.make_room("mine", c.limit() - 150));
        let left: Vec<String> = c.saved().into_iter().map(|m| m.id).collect();
        assert_eq!(left, ["newer", "mine"]);
        std::fs::remove_dir_all(&c.dir).ok();
    }

    #[test]
    fn prune_drops_old_and_unknown_files() {
        let c = Conversations::new(temp());
        put(&c, "fresh", 10, 60);
        put(&c, "stale", 10, MAX_AGE.as_secs() + 60);
        std::fs::write(c.dir.join("unknown.bin"), b"x").unwrap();
        // Being written by another model's save: left alone.
        std::fs::write(c.dir.join("0123456789abcdef-saving.bin"), b"x").unwrap();
        c.prune();
        let left: Vec<String> = c.saved().into_iter().map(|m| m.id).collect();
        assert_eq!(left, ["fresh"]);
        assert_eq!(std::fs::read_dir(&c.dir).unwrap().count(), 3);
        c.clear();
        assert_eq!(c.summary().bytes, 0);
        std::fs::remove_dir_all(&c.dir).ok();
    }

    #[test]
    fn the_limit_is_10_gb_or_a_tenth_of_free_space() {
        let c = Conversations::new(temp());
        assert!(c.limit() <= MAX_BYTES && c.limit() > 0);
        std::fs::remove_dir_all(&c.dir).ok();
    }

    #[test]
    fn file_names_are_plain() {
        let name = Conversations::file_name("unsloth/Qwen3.5-4B-GGUF:Q4_K_M");
        assert!(name.chars().all(|c| c.is_ascii_hexdigit() || c == '.' || c.is_ascii_lowercase()), "{name}");
        assert_ne!(name, Conversations::file_name("another"));
    }
}
