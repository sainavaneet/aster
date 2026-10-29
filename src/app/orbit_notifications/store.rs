//! Durable ledger file:
//! `{ "version": 1, "lead_terminal_id": ..., "tasks": [...], "tombstones": [...] }`.
//! Writes go to a private temporary file, are fsynced, then renamed over the
//! ledger (see `orbit_files`). Loading is record by record: an entry this build
//! cannot read (a newer state, a damaged record) is skipped and kept in a side
//! file instead of resetting the ledger. Token hashes are written here only;
//! API serialization of tasks omits them.
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::state::{Task, TokenHash, Tombstone};

pub(super) const FILE_NAME: &str = "orbit-tasks.json";
const VERSION: u32 = 1;

/// A record plus the token hash its own serialization skips.
#[derive(Serialize)]
struct WithHash<'a, T> {
    #[serde(flatten)]
    record: &'a T,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_hash: Option<&'a TokenHash>,
}

#[derive(Serialize)]
struct LedgerRef<'a> {
    version: u32,
    lead_terminal_id: Option<&'a str>,
    tasks: Vec<WithHash<'a, Task>>,
    tombstones: Vec<WithHash<'a, Tombstone>>,
}

#[derive(Debug)]
pub(super) struct Loaded {
    pub(super) lead_terminal_id: Option<String>,
    pub(super) tasks: Vec<Task>,
    pub(super) tombstones: VecDeque<Tombstone>,
    /// Entries that could not be read (an unknown state from a newer build,
    /// a damaged record), kept aside instead of failing the whole ledger.
    pub(super) skipped: Skipped,
}

/// Unreadable ledger entries, by list.
#[derive(Debug, Default)]
pub(super) struct Skipped {
    pub(super) tasks: Vec<Value>,
    pub(super) tombstones: Vec<Value>,
}

impl Skipped {
    pub(super) fn len(&self) -> usize {
        self.tasks.len() + self.tombstones.len()
    }
}

/// What `save` writes.
pub(super) struct Snapshot<'a> {
    pub(super) lead_terminal_id: Option<&'a str>,
    pub(super) tasks: &'a [Task],
    pub(super) tombstones: &'a VecDeque<Tombstone>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LoadError {
    /// The invalid ledger was moved aside; start empty and keep saving.
    Reset(String),
    /// The ledger exists but could not be read or moved aside. Saving must stay
    /// disabled so the original is never overwritten.
    Unreadable(String),
}

/// `Ok(None)` when no ledger exists. An invalid ledger is moved aside to
/// `<file>.corrupt-<now_ms>` so it is never overwritten, and the error is returned.
pub(super) fn load(path: &Path, now_ms: u64) -> Result<Option<Loaded>, LoadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(LoadError::Unreadable(format!(
            "Cannot read Orbit task ledger at {}: {error}; saving is disabled until it is fixed",
            path.display()
        )))
        }
    };
    let problem = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => match parse_ledger(value) {
            Ok(loaded) => return Ok(Some(loaded)),
            Err(problem) => problem,
        },
        Err(error) => error.to_string(),
    };
    match quarantine(path, now_ms) {
        Ok(kept) => Err(LoadError::Reset(format!(
            "Invalid Orbit task ledger ({problem}); started empty, original kept at {kept}"
        ))),
        Err(error) => Err(LoadError::Unreadable(format!(
            "Invalid Orbit task ledger ({problem}) at {} could not be moved aside ({error}); started empty with saving disabled",
            path.display()
        ))),
    }
}

/// The ledger record by record: the file shape and version must be right, but
/// an entry that does not parse is skipped rather than failing the rest.
fn parse_ledger(value: Value) -> Result<Loaded, String> {
    let Value::Object(mut file) = value else {
        return Err("not a JSON object".into());
    };
    let version = file
        .get("version")
        .and_then(Value::as_u64)
        .ok_or("missing version")?;
    if version != u64::from(VERSION) {
        return Err(format!("unsupported version {version}"));
    }
    let lead_terminal_id = match file.remove("lead_terminal_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id),
        Some(_) => return Err("lead_terminal_id is not a string".into()),
    };
    let (tasks, skipped_tasks) = parse_records::<Task>(file.remove("tasks"), "tasks")?;
    let (tombstones, skipped_tombstones) =
        parse_records::<Tombstone>(file.remove("tombstones"), "tombstones")?;
    Ok(Loaded {
        lead_terminal_id,
        tasks,
        tombstones: tombstones.into(),
        skipped: Skipped {
            tasks: skipped_tasks,
            tombstones: skipped_tombstones,
        },
    })
}

/// The readable records of a list and the raw entries that were not.
fn parse_records<T: DeserializeOwned>(
    list: Option<Value>,
    name: &str,
) -> Result<(Vec<T>, Vec<Value>), String> {
    let entries = match list {
        None | Some(Value::Null) => return Ok((Vec::new(), Vec::new())),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(format!("{name} is not a list")),
    };
    let mut records = Vec::with_capacity(entries.len());
    let mut skipped = Vec::new();
    for entry in entries {
        match T::deserialize(&entry) {
            Ok(record) => records.push(record),
            Err(error) => {
                let task_id = entry.get("task_id").and_then(Value::as_str).unwrap_or("?");
                tracing::warn!(%error, task_id, list = name, "unreadable Orbit ledger entry skipped");
                skipped.push(entry);
            }
        }
    }
    Ok((records, skipped))
}

/// Writes skipped entries next to the ledger, `<file>.skipped-<now_ms>`, so a
/// newer build's records are never silently lost; returns its path.
pub(super) fn keep_skipped(path: &Path, skipped: &Skipped, now_ms: u64) -> std::io::Result<String> {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".skipped-{now_ms}"));
    let target = PathBuf::from(name);
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": VERSION,
        "tasks": skipped.tasks,
        "tombstones": skipped.tombstones,
    }))
    .map_err(std::io::Error::other)?;
    let mut file = crate::app::orbit_files::create_private(&target)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(target.display().to_string())
}

fn quarantine(path: &Path, now_ms: u64) -> std::io::Result<String> {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".corrupt-{now_ms}"));
    let target = PathBuf::from(name);
    std::fs::rename(path, &target).inspect_err(|error| {
        tracing::warn!(%error, path = %path.display(), "cannot quarantine Orbit task ledger");
    })?;
    Ok(target.display().to_string())
}

/// The ledger file contents for `snapshot`, token hashes included.
pub(super) fn serialize(snapshot: &Snapshot<'_>) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec(&LedgerRef {
        version: VERSION,
        lead_terminal_id: snapshot.lead_terminal_id,
        tasks: snapshot
            .tasks
            .iter()
            .map(|task| WithHash {
                record: task,
                token_hash: task.token_hash.as_ref(),
            })
            .collect(),
        tombstones: snapshot
            .tombstones
            .iter()
            .map(|entry| WithHash {
                record: entry,
                token_hash: entry.token_hash.as_ref(),
            })
            .collect(),
    })
    .map_err(std::io::Error::other)
}

/// Writes the ledger in place (tests; the server writes through `OrbitWriter`).
#[cfg(test)]
pub(super) fn save(path: &Path, snapshot: &Snapshot<'_>) -> std::io::Result<()> {
    crate::app::orbit_files::replace_atomically(path, &serialize(snapshot)?)
}

#[cfg(test)]
pub(super) fn test_path(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "aster-orbit-ledger-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    directory.join(FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::super::state::{Delivery, LiveState, Submission};
    use super::*;

    fn task(id: &str) -> Task {
        Task {
            task_id: id.into(),
            worker_id: "worker".into(),
            lead_id: "lead".into(),
            submission: Submission::Submitted,
            delivery: Delivery::AwaitingReport,
            report: None,
            origin: None,
            notice_for: None,
            error: None,
            created_at: 1,
            updated_at: 2,
            title: None,
            attempts: 3,
            recoveries: 0,
            redelivered: false,
            nudged_at: Some(4),
            deadline: Some(5),
            late: false,
            steps: Vec::new(),
            timeout_ms: None,
            role: None,
            late_from: None,
            extra: Default::default(),
            token_hash: Some(TokenHash("abc123".into())),
            live: LiveState::default(),
        }
    }

    #[test]
    fn ledger_round_trips_tasks_tombstones_hashes_and_lead() {
        let path = test_path("round-trip");
        let tombstones = VecDeque::from([Tombstone {
            task_id: "old".into(),
            worker_id: "worker".into(),
            report: None,
            final_delivery: Delivery::Cancelled,
            resolved_at: 9,
            token_hash: Some(TokenHash("def456".into())),
            extra: Default::default(),
        }]);
        let tasks = [task("a")];
        let snapshot = Snapshot {
            lead_terminal_id: Some("lead-terminal"),
            tasks: &tasks,
            tombstones: &tombstones,
        };
        save(&path, &snapshot).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        let loaded = load(&path, 10).unwrap().unwrap();
        assert_eq!(loaded.lead_terminal_id.as_deref(), Some("lead-terminal"));
        assert_eq!(loaded.tasks[0].task_id, "a");
        assert_eq!(loaded.tasks[0].attempts, 3);
        assert_eq!(loaded.tasks[0].deadline, Some(5));
        assert_eq!(loaded.tasks[0].token_hash, tasks[0].token_hash);
        assert_eq!(loaded.tombstones, tombstones);
        // The API view of the same records never carries the hash.
        assert!(serde_json::to_value(&tasks[0])
            .unwrap()
            .get("token_hash")
            .is_none());
        assert!(serde_json::to_value(&tombstones[0])
            .unwrap()
            .get("token_hash")
            .is_none());
    }

    #[test]
    fn steps_round_trip_and_a_ledger_from_before_steps_loads_unchanged() {
        use super::super::steps::{Step, StepState};
        use crate::api::schema::OrbitHandoff;
        let path = test_path("steps");
        let mut relayed = task("relayed");
        relayed.steps = vec![
            Step {
                state: StepState::HandedOff,
                finished_at: Some(3),
                handoff: Some(OrbitHandoff {
                    done: "built".into(),
                    next: "review".into(),
                    changed_files: vec!["src/a.rs".into()],
                    known_issues: Some("none yet".into()),
                    to: Some("rx".into()),
                    parallel: None,
                }),
                ..Step::active(0, "rc", "claude", 1)
            },
            Step::active(1, "rx", "codex", 3),
        ];
        let tasks = [relayed, task("plain")];
        let tombstones = VecDeque::new();
        let snapshot = Snapshot {
            lead_terminal_id: None,
            tasks: &tasks,
            tombstones: &tombstones,
        };
        save(&path, &snapshot).unwrap();
        let loaded = load(&path, 10).unwrap().unwrap();
        assert_eq!(loaded.tasks[0].steps, tasks[0].steps);
        assert!(loaded.tasks[1].steps.is_empty());
        assert!(serde_json::to_value(&tasks[1])
            .unwrap()
            .get("steps")
            .is_none());

        // A ledger written before steps existed, with a stored report.
        let old = test_path("steps-old");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(
            &old,
            r#"{"version":1,"lead_terminal_id":"lead","tasks":[{"task_id":"t","worker_id":"w","lead_id":"lead","submission":"submitted","delivery":"queued","report":{"task_id":"t","worker_id":"w","outcome":"completed","summary":"done","tests_run":true},"origin":"worker","error":null,"created_at":1,"updated_at":2,"attempts":1}],"tombstones":[]}"#,
        )
        .unwrap();
        let loaded = load(&old, 10).unwrap().unwrap();
        assert!(loaded.skipped.len() == 0 && loaded.tasks.len() == 1);
        assert!(loaded.tasks[0].steps.is_empty());
        assert!(loaded.tasks[0].report.as_ref().unwrap().handoff.is_none());
        assert!(serde_json::to_value(&loaded.tasks[0])
            .unwrap()
            .get("steps")
            .is_none());
    }

    #[test]
    fn missing_ledger_is_empty_and_minimal_ledger_uses_defaults() {
        let path = test_path("minimal");
        assert!(load(&path, 1).unwrap().is_none());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"tasks":[{"task_id":"t","worker_id":"w","lead_id":"l","submission":"pending","delivery":"queued","report":null,"error":null,"token_hash":"future"}]}"#,
        )
        .unwrap();
        let loaded = load(&path, 1).unwrap().unwrap();
        assert!(loaded.lead_terminal_id.is_none());
        assert_eq!(loaded.tasks[0].attempts, 0);
        assert_eq!(loaded.tasks[0].deadline, None);
        assert!(loaded.tombstones.is_empty());
    }

    #[test]
    fn corrupt_or_unknown_version_ledger_is_kept_aside() {
        for (name, contents) in [
            ("corrupt", "{not json"),
            ("version", r#"{"version":99,"tasks":[]}"#),
        ] {
            let path = test_path(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            let Err(LoadError::Reset(error)) = load(&path, 42) else {
                panic!("expected a reset");
            };
            assert!(error.contains("started empty"), "{error}");
            assert!(!path.exists());
            let kept = path.with_file_name(format!("{FILE_NAME}.corrupt-42"));
            assert_eq!(std::fs::read_to_string(kept).unwrap(), contents);
        }
    }

    #[test]
    fn unreadable_ledger_disables_saving_and_is_left_in_place() {
        // A directory at the ledger path exists but cannot be read as a file.
        let path = test_path("unreadable");
        std::fs::create_dir_all(&path).unwrap();
        let Err(LoadError::Unreadable(error)) = load(&path, 7) else {
            panic!("expected an unreadable ledger");
        };
        assert!(error.contains("saving is disabled"), "{error}");
        assert!(path.is_dir(), "unreadable ledger was moved or replaced");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
