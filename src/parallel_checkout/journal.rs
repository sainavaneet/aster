//! The durable journal of an integration: one line per fact, each made durable
//! before the caller goes on. `record` either succeeds (the line is on disk,
//! file and directory flushed) or returns an error the caller must act on;
//! `read` either returns every line or an error for anything it cannot parse,
//! a torn last line included. Nothing is skipped silently.
use std::path::{Path, PathBuf};

use super::checked;

/// One fact about a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    /// About to touch the tree for this patch.
    About(usize),
    /// Applied cleanly by Aster.
    Applied(usize),
    /// Found in the tree already; Aster did not apply it.
    Present(usize),
}

impl Entry {
    fn line(self) -> String {
        match self {
            Self::About(step) => format!("about {step}\n"),
            Self::Applied(step) => format!("applied {step}\n"),
            Self::Present(step) => format!("present {step}\n"),
        }
    }

    fn parse(line: &str) -> Option<Self> {
        let (word, step) = line.split_once(' ')?;
        let step = step.parse::<usize>().ok()?;
        match word {
            "about" => Some(Self::About(step)),
            "applied" => Some(Self::Applied(step)),
            "present" => Some(Self::Present(step)),
            _ => None,
        }
    }
}

/// Why a journal could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JournalError {
    /// Reading or writing failed.
    Io(String),
    /// A line is not a record, or the last one is cut off.
    Malformed(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) | Self::Malformed(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Journal {
    path: PathBuf,
}

impl Journal {
    /// The journal at `path`, wherever the caller keeps it.
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The journal of `task_id` in `directory`; the id must be a plain file name.
    pub(crate) fn in_dir(directory: &Path, task_id: &str) -> Result<Self, JournalError> {
        let plain = !task_id.is_empty() && task_id != ".." && !task_id.contains(['/', '\\', '\0']);
        if !plain {
            return Err(JournalError::Io(format!(
                "{task_id:?} is not a usable task id"
            )));
        }
        Ok(Self::at(
            directory.join(format!("{task_id}-integrate.journal")),
        ))
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Makes the record durable: appended and flushed to disk, or an error. A
    /// journal that is a link or not a regular file is refused.
    pub(crate) fn record(&self, entry: Entry) -> Result<(), JournalError> {
        checked::append_durably(&self.path, entry.line().as_bytes())
            .map_err(|error| JournalError::Io(error.to_string()))
    }

    /// Every record, in order; empty when there is no journal.
    pub(crate) fn read(&self) -> Result<Vec<Entry>, JournalError> {
        if !checked::exists_no_follow(&self.path) {
            return Ok(Vec::new());
        }
        let text = checked::read_plain(&self.path).map_err(|e| JournalError::Io(e.to_string()))?;
        if !text.is_empty() && !text.ends_with('\n') {
            return Err(JournalError::Malformed("the last line is cut off".into()));
        }
        text.lines()
            .map(|line| {
                Entry::parse(line)
                    .ok_or_else(|| JournalError::Malformed(format!("unreadable line {line:?}")))
            })
            .collect()
    }
}
