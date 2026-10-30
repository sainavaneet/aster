//! Startup reclamation of checkouts nothing owns any more: a server killed
//! between creating a clone and saving the ledger, or a ledger lost or reset,
//! leaves a clone (often gigabytes of build output) that no step will ever
//! remove. Only directories carrying the ownership marker Aster writes before
//! it clones are considered, and only once the marker is old enough that no
//! creation can still be running. The worktrees directory is shared by every
//! session and repository, so a marker is acted on only when it names this
//! session as its owner; markers with no owner (older builds) or another
//! owner are left alone.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::checked;
use super::safety::{marker_path, read_marker, read_owner};
use super::{Checkout, CheckoutMethod};

/// How old a marker must be before its checkout counts as abandoned.
pub(crate) const RECLAIM_MIN_AGE: Duration = Duration::from_secs(60 * 60);

const MARKER_SUFFIX: &str = ".aster-checkout";

/// What happened to one orphaned checkout.
#[derive(Debug)]
pub(crate) struct Reclaimed {
    pub(crate) path: PathBuf,
    pub(crate) result: Result<(), String>,
}

/// Removes the marked checkouts below `directory/<repo>/` that are not in
/// `owned`, whose marker names `owner` and is at least `min_age` old. Everything
/// else (unmarked directories, young, owned, foreign or ownerless checkouts) is
/// left alone. Removal goes through the same ownership checks as `remove`, which
/// refuse links.
pub(crate) fn reclaim_orphans(
    directory: &Path,
    owner: &str,
    owned: &HashSet<PathBuf>,
    min_age: Duration,
) -> Vec<Reclaimed> {
    let Ok(repos) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut reclaimed = Vec::new();
    for repo in repos.flatten().map(|entry| entry.path()) {
        if !checked::is_plain_dir(&repo) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&repo) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(path) = orphan(
                &repo,
                &entry.file_name().to_string_lossy(),
                owner,
                owned,
                min_age,
            ) {
                let result = super::remove(&checkout_at(&path), directory);
                reclaimed.push(Reclaimed {
                    path,
                    result: result.map_err(|error| error.to_string()),
                });
            }
        }
    }
    reclaimed
}

/// The checkout a marker file named `name` in `repo` stands for, when it is an
/// old, unowned, regular marker.
fn orphan(
    repo: &Path,
    name: &str,
    owner: &str,
    owned: &HashSet<PathBuf>,
    min_age: Duration,
) -> Option<PathBuf> {
    let stem = name.strip_suffix(MARKER_SUFFIX).filter(|s| !s.is_empty())?;
    let path = repo.join(stem);
    let marker = marker_path(&path);
    if owner.is_empty() || !checked::is_plain_file(&marker) {
        return None;
    }
    if read_marker(&path).is_none() {
        tracing::info!(marker = %marker.display(), "left an unreadable checkout marker alone");
        return None;
    }
    match read_owner(&path) {
        Some(recorded) if recorded == owner => {}
        recorded => {
            tracing::info!(marker = %marker.display(), owner = ?recorded, "left a checkout of another or unknown owner alone");
            return None;
        }
    }
    let age = std::fs::symlink_metadata(&marker)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())?;
    if age < min_age {
        return None;
    }
    let named = checked::real_target(&path).unwrap_or_else(|_| path.clone());
    (!owned.contains(&path) && !owned.contains(&named)).then_some(path)
}

/// A checkout record rebuilt from what is on disk: removal needs only the path,
/// the repository named by the marker and how the clone was made.
fn checkout_at(path: &Path) -> Checkout {
    let repo_root = read_marker(path)
        .map(|(_, repo)| PathBuf::from(repo))
        .unwrap_or_default();
    let method = if checked::is_plain_dir(&path.join(".git")) {
        CheckoutMethod::CopyOnWrite
    } else {
        CheckoutMethod::Worktree
    };
    Checkout {
        path: path.to_path_buf(),
        repo_root,
        subdir: PathBuf::new(),
        snapshot: String::new(),
        baseline: String::new(),
        method,
    }
}
