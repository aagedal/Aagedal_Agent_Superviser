//! Calls the guard didn't simply pass, kept as JSON lines for later inspection. Arguments can
//! hold secrets, so the file is readable by its owner only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

pub struct Journal {
    path: PathBuf,
    next_id: Mutex<Option<u64>>,
}

/// The recorded calls, oldest first; unreadable lines are skipped.
pub fn read(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.get("id").is_some_and(Value::is_u64))
        .collect()
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), next_id: Mutex::new(None) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `entry` with the next id, and returns that id.
    pub fn append(&self, entry: Map<String, Value>) -> std::io::Result<u64> {
        let mut next = self.next_id.lock().unwrap();
        let id = match *next {
            Some(id) => id,
            None => {
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent)?;
                    #[cfg(unix)]
                    std::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
                }
                read(&self.path).iter().filter_map(|e| e["id"].as_u64()).max().unwrap_or(0) + 1
            }
        };
        let mut line = Map::new();
        line.insert("id".into(), id.into());
        line.extend(entry);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&self.path)?;
        writeln!(file, "{}", Value::Object(line))?;
        *next = Some(id + 1);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_after_existing_entries() {
        let dir = std::env::temp_dir().join(format!("guard-journal-{}", std::process::id()));
        let path = dir.join("calls.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{\"id\": 7}\nnot json\n").unwrap();
        let journal = Journal::new(&path);
        let mut entry = Map::new();
        entry.insert("verdict".into(), "blocked".into());
        assert_eq!(journal.append(entry.clone()).unwrap(), 8);
        assert_eq!(journal.append(entry).unwrap(), 9);
        assert_eq!(read(&path).len(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
