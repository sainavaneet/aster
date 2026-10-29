//! `orbit-decisions.jsonl` in the session data directory: one JSON object per
//! decision, rotated at `MAX_BYTES` keeping one `.1`, plus the most recent
//! entries in memory for `orbit.activity` and `aster orbit decisions`. Writes
//! happen on decision threads only; the event loop reads the in-memory copy.
use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const FILE_NAME: &str = "orbit-decisions.jsonl";
pub(crate) const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Entries kept in memory and returned by `orbit.decisions`.
pub(crate) const RECENT: usize = 50;
/// Bound on the redacted input summary of one entry.
pub(crate) const MAX_SUMMARY_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) ts: u64,
    pub(crate) kind: String,
    pub(crate) provider: String,
    /// Task, notice, or worker the decision is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subject: Option<String>,
    pub(crate) inputs_summary: String,
    pub(crate) answers: Option<Value>,
    pub(crate) thresholds: Value,
    pub(crate) action: String,
    pub(crate) latency_ms: u64,
    pub(crate) error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Journal {
    path: Option<PathBuf>,
    /// Serialises appends and rotation across decision threads.
    file: Mutex<()>,
    recent: Mutex<VecDeque<Entry>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `text` cut to at most `max_bytes` on a character boundary, marked with `…`.
pub(crate) fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

fn open_append(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Appends `line` and a newline, first moving a file that would pass `max_bytes`
/// to `.1` (replacing an older one).
pub(crate) fn append_line(path: &Path, line: &str, max_bytes: u64) -> io::Result<()> {
    let size = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
    if size > 0 && size + line.len() as u64 + 1 > max_bytes {
        std::fs::rename(path, rotated(path))?;
    }
    let mut file = open_append(path)?;
    file.write_all(format!("{line}\n").as_bytes())
}

impl Journal {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Records `entry`; a write failure is logged and the entry still kept in memory.
    pub(crate) fn append(&self, entry: Entry) {
        if let Some(path) = &self.path {
            let line = serde_json::to_string(&entry).unwrap_or_default();
            let _guard = lock(&self.file);
            if let Err(error) = append_line(path, &line, MAX_BYTES) {
                tracing::warn!(%error, path = %path.display(), "cannot write Orbit decision journal");
            }
        }
        let mut recent = lock(&self.recent);
        recent.push_back(entry);
        while recent.len() > RECENT {
            recent.pop_front();
        }
    }

    /// The newest `RECENT` entries, oldest first.
    pub(crate) fn recent(&self) -> Vec<Entry> {
        lock(&self.recent).iter().cloned().collect()
    }

    /// Fills memory from the file's last entries; run once, on a decision thread.
    pub(crate) fn load_recent(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let entries = {
            let _guard = lock(&self.file);
            match read_tail(path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Err(error) => {
                    tracing::warn!(%error, "cannot read Orbit decision journal");
                    return;
                }
            }
        };
        let mut recent = lock(&self.recent);
        let newer: Vec<Entry> = recent.drain(..).collect();
        recent.extend(entries);
        recent.extend(newer);
        while recent.len() > RECENT {
            recent.pop_front();
        }
    }
}

/// The last `RECENT` readable entries. A line that is not an entry (damaged,
/// not UTF-8, or a kind this build cannot read) is skipped, never fatal;
/// unknown fields of a newer build are ignored.
fn read_tail(path: &Path) -> io::Result<VecDeque<Entry>> {
    let file = std::fs::File::open(path)?;
    let mut tail = VecDeque::with_capacity(RECENT);
    let mut skipped = 0_usize;
    for line in io::BufReader::new(file).split(b'\n') {
        let Ok(entry) = serde_json::from_slice::<Entry>(&line?) else {
            skipped += 1;
            continue;
        };
        tail.push_back(entry);
        if tail.len() > RECENT {
            tail.pop_front();
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "unreadable Orbit decision journal lines skipped");
    }
    Ok(tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: u64) -> Entry {
        Entry {
            ts,
            kind: "approval".into(),
            provider: "rules".into(),
            subject: Some("task-1".into()),
            inputs_summary: "command: date".into(),
            answers: None,
            thresholds: serde_json::json!({}),
            action: "approve".into(),
            latency_ms: 0,
            error: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aster-decision-journal-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn append_rotates_at_the_limit_keeping_one_old_file() {
        let dir = temp_dir("rotate");
        let path = dir.join(FILE_NAME);
        for index in 0..4 {
            append_line(&path, &format!("line-{index}-{}", "x".repeat(60)), 100).unwrap();
        }
        let current = std::fs::read_to_string(&path).unwrap();
        let old = std::fs::read_to_string(rotated(&path)).unwrap();
        assert_eq!(current.lines().count(), 1, "{current}");
        assert!(current.starts_with("line-3"));
        assert!(
            old.starts_with("line-2"),
            "only the newest rotation is kept"
        );
        assert!(!dir.join(format!("{FILE_NAME}.2")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_keeps_the_last_entries_and_reloads_them_from_disk() {
        let dir = temp_dir("recent");
        let path = dir.join(FILE_NAME);
        let journal = Journal::new(Some(path.clone()));
        for ts in 0..(RECENT as u64 + 5) {
            journal.append(entry(ts));
        }
        let recent = journal.recent();
        assert_eq!(recent.len(), RECENT);
        assert_eq!(recent[0].ts, 5);
        let reloaded = Journal::new(Some(path.clone()));
        reloaded.load_recent();
        assert_eq!(reloaded.recent(), recent);
        let line = std::fs::read_to_string(&path).unwrap();
        let first: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
        for key in [
            "ts",
            "kind",
            "provider",
            "inputs_summary",
            "answers",
            "thresholds",
            "action",
            "latency_ms",
            "error",
        ] {
            assert!(first.get(key).is_some(), "{key} missing from {first}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "journal is private");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_builds_journal_format_loads_from_the_fixture() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/orbit/state/orbit-decisions.v1.jsonl"
        ));
        let entries = read_tail(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "approval");
        assert_eq!(entries[1].provider, "jev");
        assert_eq!(entries[1].subject.as_deref(), Some("task-2"));
    }

    #[test]
    fn unreadable_journal_lines_are_skipped_and_the_rest_load() {
        let dir = temp_dir("tolerant");
        let path = dir.join(FILE_NAME);
        let good = serde_json::to_string(&entry(1)).unwrap();
        let mut future: Value = serde_json::to_value(entry(2)).unwrap();
        future["added_by_a_newer_build"] = serde_json::json!({"x": 1});
        let mut bytes = Vec::new();
        bytes.extend_from_slice(format!("{good}\n{{\"ts\": broken\n").as_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
        bytes.extend_from_slice(format!("{future}\n\n").as_bytes());
        std::fs::write(&path, bytes).unwrap();
        let journal = Journal::new(Some(path));
        journal.load_recent();
        let recent = journal.recent();
        assert_eq!(
            recent.iter().map(|entry| entry.ts).collect::<Vec<_>>(),
            [1, 2]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounded_cuts_on_a_character_boundary() {
        assert_eq!(bounded("abc", 3), "abc");
        let cut = bounded(&"é".repeat(600), MAX_SUMMARY_BYTES);
        assert!(cut.len() <= MAX_SUMMARY_BYTES);
        assert!(cut.ends_with('…'));
    }
}
