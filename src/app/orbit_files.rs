//! Orbit's own state files (`orbit-tasks.json`, `orbit-team.json`,
//! `orbit-binding.json`, skipped-entry side files): created with mode 0600 on
//! Unix, replaced atomically (temporary file, fsync, rename), and for the ledger
//! and roster written by a dedicated thread so the event loop only serializes.
//!
//! The writer coalesces bursts: replacements of one file within `COALESCE` are
//! written once, last one wins. `flush` writes everything pending and waits,
//! for server shutdown and live handoff. Each write's result comes back as
//! `AppEvent::OrbitFileSaved`; when the thread cannot start, writes happen
//! inline instead.
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::events::AppEvent;

/// At most one write per file per window, unless a flush is requested.
pub(crate) const COALESCE: Duration = Duration::from_millis(250);
/// How long a flush waits for the writer before giving up.
const FLUSH_WAIT: Duration = Duration::from_secs(5);

/// A new private file; any existing file at `path` is replaced first, so a
/// leftover with wider permissions never keeps them.
pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Writes `bytes` to `<path>.tmp` (mode 0600), fsyncs it, and renames it over
/// `path`, so a crash never leaves a partial file and the result is private.
pub(crate) fn replace_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let mut pending = path.as_os_str().to_owned();
    pending.push(".tmp");
    let pending = PathBuf::from(pending);
    let result = (|| {
        let mut file = create_private(&pending)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&pending, path)
    })();
    if result.is_err() {
        // Never leave a partial temporary file behind.
        let _ = std::fs::remove_file(&pending);
    }
    result
}

/// Which state file a write was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrbitFile {
    Ledger,
    Roster,
}

/// The outcome of one background write.
#[derive(Debug)]
pub(crate) struct OrbitFileSaved {
    pub(crate) file: OrbitFile,
    pub(crate) path: PathBuf,
    pub(crate) result: Result<(), String>,
}

enum Job {
    Replace {
        file: OrbitFile,
        path: PathBuf,
        bytes: Vec<u8>,
    },
    Flush(mpsc::Sender<()>),
}

/// Replacements waiting for the coalescing window, one per path, oldest first.
#[derive(Default)]
struct Pending(Vec<(OrbitFile, PathBuf, Vec<u8>)>);

impl Pending {
    fn add(&mut self, file: OrbitFile, path: PathBuf, bytes: Vec<u8>) {
        match self.0.iter_mut().find(|(_, queued, _)| *queued == path) {
            Some(slot) => slot.2 = bytes,
            None => self.0.push((file, path, bytes)),
        }
    }

    fn write(&mut self, report: &dyn Fn(OrbitFileSaved), writes: &Counter) {
        for (file, path, bytes) in self.0.drain(..) {
            let result = replace_atomically(&path, &bytes).map_err(|error| error.to_string());
            writes.bump();
            report(OrbitFileSaved { file, path, result });
        }
    }
}

/// Completed writes, counted for tests.
#[derive(Clone, Default)]
struct Counter(#[cfg(test)] std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Counter {
    fn bump(&self) {
        #[cfg(test)]
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn run(jobs: mpsc::Receiver<Job>, report: &dyn Fn(OrbitFileSaved), writes: &Counter) {
    let mut pending = Pending::default();
    while let Ok(job) = jobs.recv() {
        let mut flushes = Vec::new();
        match job {
            Job::Replace { file, path, bytes } => pending.add(file, path, bytes),
            Job::Flush(done) => flushes.push(done),
        }
        let deadline = Instant::now() + COALESCE;
        let mut closed = false;
        while flushes.is_empty() {
            match jobs.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Job::Replace { file, path, bytes }) => pending.add(file, path, bytes),
                Ok(Job::Flush(done)) => flushes.push(done),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        pending.write(report, writes);
        for done in flushes {
            let _ = done.send(());
        }
        if closed {
            return;
        }
    }
    pending.write(report, writes);
}

/// The ledger and roster writer owned by the App.
pub(crate) struct OrbitWriter {
    events: Option<tokio::sync::mpsc::Sender<AppEvent>>,
    jobs: Option<mpsc::Sender<Job>>,
    /// Writes happen on the caller's thread (unit tests, or no writer thread).
    inline: bool,
    writes: Counter,
}

impl OrbitWriter {
    /// A writer that starts its thread on first use and reports to `events`.
    pub(crate) fn threaded(events: tokio::sync::mpsc::Sender<AppEvent>) -> Self {
        Self {
            events: Some(events),
            jobs: None,
            inline: false,
            writes: Counter::default(),
        }
    }

    /// A writer that writes synchronously; results return from `replace`.
    #[cfg(test)]
    pub(crate) fn inline() -> Self {
        Self {
            events: None,
            jobs: None,
            inline: true,
            writes: Counter::default(),
        }
    }

    /// Completed writes so far.
    #[cfg(test)]
    pub(crate) fn writes(&self) -> usize {
        self.writes.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn sender(&mut self) -> Option<&mpsc::Sender<Job>> {
        if self.inline {
            return None;
        }
        if self.jobs.is_none() {
            let events = self.events.clone()?;
            let (tx, rx) = mpsc::channel();
            let writes = self.writes.clone();
            let spawned = crate::thread_spawn::spawn_named("aster-orbit-writer", move || {
                let report = |saved: OrbitFileSaved| {
                    if let Err(error) = &saved.result {
                        tracing::warn!(%error, path = %saved.path.display(), "cannot save Orbit state file");
                    }
                    let _ = events.blocking_send(AppEvent::OrbitFileSaved(Box::new(saved)));
                };
                run(rx, &report, &writes);
            });
            match spawned {
                Ok(_) => self.jobs = Some(tx),
                Err(error) => {
                    tracing::warn!(%error, "cannot start the Orbit writer; saving inline");
                    self.inline = true;
                    return None;
                }
            }
        }
        self.jobs.as_ref()
    }

    /// Queues `bytes` to replace `path`. `Some(result)` when written inline;
    /// otherwise the result arrives later as `AppEvent::OrbitFileSaved`.
    pub(crate) fn replace(
        &mut self,
        file: OrbitFile,
        path: PathBuf,
        bytes: Vec<u8>,
    ) -> Option<io::Result<()>> {
        let job = Job::Replace { file, path, bytes };
        let job = match self.sender() {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return None,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        // No thread (or it is gone): write here.
        self.jobs = None;
        self.inline = true;
        let Job::Replace { path, bytes, .. } = job else {
            unreachable!("only replacements are queued here")
        };
        let result = replace_atomically(&path, &bytes);
        self.writes.bump();
        Some(result)
    }

    /// Writes everything queued now and waits for it, at most `FLUSH_WAIT`.
    pub(crate) fn flush(&mut self) {
        let Some(jobs) = self.jobs.as_ref() else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if jobs.send(Job::Flush(done)).is_err() || wait.recv_timeout(FLUSH_WAIT).is_err() {
            tracing::warn!("Orbit writer did not confirm its flush");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aster-orbit-files-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn replacement_is_private_atomic_and_replaces_a_wide_leftover_tmp() {
        let dir = temp_dir("replace");
        let path = dir.join("state.json");
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, "partial from a crash").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        replace_atomically(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert!(!tmp.exists(), "temporary file renamed");
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_burst_is_coalesced_into_one_write_and_flush_writes_at_once() {
        let dir = temp_dir("coalesce");
        let path = dir.join("ledger.json");
        let (events, mut results) = tokio::sync::mpsc::channel(16);
        let mut writer = OrbitWriter::threaded(events);
        for index in 0..3 {
            let bytes = format!("{{\"n\":{index}}}").into_bytes();
            assert!(writer
                .replace(OrbitFile::Ledger, path.clone(), bytes)
                .is_none());
        }
        writer.flush();
        assert_eq!(writer.writes(), 1, "three changes, one write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":2}");
        let saved = results.try_recv().unwrap();
        let AppEvent::OrbitFileSaved(saved) = saved else {
            panic!("unexpected event");
        };
        assert_eq!(saved.file, OrbitFile::Ledger);
        assert!(saved.result.is_ok());
        assert!(results.try_recv().is_err(), "one result per write");
        // Later changes coalesce again; without a flush they land within the window.
        writer.replace(OrbitFile::Ledger, path.clone(), b"{\"n\":3}".to_vec());
        let deadline = Instant::now() + Duration::from_secs(2);
        while writer.writes() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(writer.writes(), 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":3}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_is_reported_back() {
        let dir = temp_dir("error");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "a file, not a directory").unwrap();
        let (events, mut results) = tokio::sync::mpsc::channel(16);
        let mut writer = OrbitWriter::threaded(events);
        writer.replace(OrbitFile::Roster, blocker.join("team.json"), b"{}".to_vec());
        writer.flush();
        let Ok(AppEvent::OrbitFileSaved(saved)) = results.try_recv() else {
            panic!("no result reported");
        };
        assert_eq!(saved.file, OrbitFile::Roster);
        assert!(saved.result.is_err(), "{saved:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
