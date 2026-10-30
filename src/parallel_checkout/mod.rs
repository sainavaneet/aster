//! Isolated checkouts for parallel Orbit steps. A checkout starts from a
//! snapshot of the working tree taken with `git stash create`, which touches
//! neither the tree nor the index. Where the OS can clone a directory
//! copy-on-write (APFS), the whole repository directory is cloned, build output
//! included, and the clone is moved onto the snapshot; otherwise a detached
//! `git worktree` is added. When the step ends, its changes are captured as a
//! patch (tracked edits and new files) and the checkout is removed.
//!
//! Everything here runs git and copies files: call it only off the event loop.
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

pub(crate) mod checked;
pub(crate) mod integrate;
pub(crate) mod integrate_resume;
pub(crate) mod journal;
mod reclaim;
mod safety;
#[cfg(test)]
mod tests;

use checked::{git, run, run_bytes};
pub(crate) use reclaim::{reclaim_orphans, RECLAIM_MIN_AGE};
pub(crate) use safety::RemoveError;
use safety::{clonable, remove_unchecked, verify_clone, write_marker};

/// Removes a checkout Aster created under `directory` (see `safety::remove`),
/// and the Codex trust Aster gave it.
pub(crate) fn remove(checkout: &Checkout, directory: &Path) -> Result<(), RemoveError> {
    safety::remove(checkout, directory)?;
    crate::codex_trust::untrust_checkout(&checkout.path);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutMethod {
    /// A copy-on-write clone of the repository directory.
    CopyOnWrite,
    /// A detached `git worktree`.
    Worktree,
}

/// One step's checkout, with what is needed to capture its changes and remove it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    /// The checkout's root (a copy of the repository root).
    pub(crate) path: PathBuf,
    /// The repository the snapshot came from.
    pub(crate) repo_root: PathBuf,
    /// The project directory relative to the repository root.
    #[serde(default)]
    pub(crate) subdir: PathBuf,
    /// The commit the checkout is on: the working-tree snapshot, or HEAD.
    pub(crate) snapshot: String,
    /// The tree of everything in the checkout when the step started; the patch
    /// is the difference from it.
    pub(crate) baseline: String,
    pub(crate) method: CheckoutMethod,
}

impl Checkout {
    /// Where the step's pane starts: the project directory inside the checkout.
    pub(crate) fn workdir(&self) -> PathBuf {
        self.path.join(&self.subdir)
    }
}

/// What a captured patch holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchSummary {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    /// Changed paths, relative to the repository root, at most `MAX_LISTED_FILES`.
    pub(crate) files: Vec<String>,
    /// How many paths changed in all.
    pub(crate) file_count: usize,
}

pub(crate) const MAX_LISTED_FILES: usize = 200;

/// Copies the directory `source` to the new path `destination`.
pub(crate) type CloneFn = fn(&Path, &Path) -> std::io::Result<()>;

/// Snapshots and clones of one repository run one at a time: concurrent
/// `git stash create` calls fight over the index lock, and a clone made while
/// another Git command runs could copy its lock file.
static CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The commit a checkout starts from. `git stash create` records tracked edits
/// in a commit without changing the tree, the index, or any ref; it prints
/// nothing when the tree is clean, and the snapshot is then HEAD.
pub(crate) fn snapshot(repo_root: &Path) -> Result<String, String> {
    let created = run(git_stash_create(repo_root))?;
    if !created.is_empty() {
        return Ok(created);
    }
    run({
        let mut head = git(repo_root);
        head.args(["rev-parse", "--verify", "HEAD"]);
        head
    })
}

fn git_stash_create(repo_root: &Path) -> Command {
    let mut command = git(repo_root);
    command.args(["stash", "create"]);
    command
}

/// Runs `step` with a private index file, so staging everything to read the
/// tree leaves the checkout's own index alone.
fn with_private_index<T>(
    checkout: &Path,
    step: impl FnOnce(&dyn Fn() -> Command) -> Result<T, String>,
) -> Result<T, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let index = std::env::temp_dir().join(format!(
        "aster-parallel-index-{}-{nanos}",
        std::process::id()
    ));
    let command = || {
        let mut command = git(checkout);
        command.env("GIT_INDEX_FILE", &index);
        command
    };
    let result = (|| {
        let mut add = command();
        add.args(["add", "-A"]);
        run(add)?;
        step(&command)
    })();
    let _ = checked::remove_file_inside(&std::env::temp_dir(), &index);
    result
}

/// The tree of everything in `checkout`: tracked files and untracked files that
/// are not ignored.
fn working_tree(checkout: &Path) -> Result<String, String> {
    with_private_index(checkout, |command| {
        let mut write = command();
        write.arg("write-tree");
        run(write)
    })
}

fn checkout_snapshot(clone: &Path, snapshot: &str) -> Result<(), String> {
    // Forced: the clone's files already equal the snapshot, but its index and
    // HEAD are the original's.
    let mut checkout = git(clone);
    checkout.args(["checkout", "-q", "-f", "--detach", snapshot]);
    run(checkout).map(|_| ())
}

/// Applies an earlier step's patch in the checkout, so this step's own patch is
/// relative to that step's result.
fn apply_patch(checkout: &Path, patch: &Path) -> Result<(), String> {
    if checked::is_empty_file(patch) {
        return Ok(());
    }
    let mut apply = git(checkout);
    apply.args(["apply", "--whitespace=nowarn"]).arg(patch);
    run(apply)
        .map(|_| ())
        .map_err(|error| format!("could not apply {}: {error}", patch.display()))
}

fn add_worktree(repo_root: &Path, path: &Path, snapshot: &str) -> Result<(), String> {
    let mut add = git(repo_root);
    add.args(["worktree", "add", "--detach"])
        .arg(path)
        .arg(snapshot);
    run(add).map(|_| ())
}

/// Creates the checkout `name` under `directory/<repo_name>/`, then applies the
/// `apply` patches in order: the earlier steps' results that this one builds on.
/// `owner` is the session recorded in the ownership marker; only that session's
/// startup may reclaim the checkout later (see `reclaim`).
pub(crate) fn create(
    repo_root: &Path,
    project: &Path,
    directory: &Path,
    repo_name: &str,
    name: &str,
    apply: &[PathBuf],
    owner: &str,
) -> Result<Checkout, String> {
    let place = Placement {
        directory,
        repo_name,
        name,
        owner,
    };
    create_placed(
        repo_root,
        project,
        place,
        apply,
        crate::platform::clone_directory_cow,
    )
}

/// `create` with the copy-on-write clone replaceable, so tests can fail it.
/// The checkout has no owner, so no startup reclaims it.
#[cfg(test)]
pub(crate) fn create_with(
    repo_root: &Path,
    project: &Path,
    directory: &Path,
    repo_name: &str,
    name: &str,
    apply: &[PathBuf],
    clone: CloneFn,
) -> Result<Checkout, String> {
    let place = Placement {
        directory,
        repo_name,
        name,
        owner: "",
    };
    create_placed(repo_root, project, place, apply, clone)
}

/// Where a checkout goes and whose it is.
#[derive(Clone, Copy)]
pub(crate) struct Placement<'a> {
    pub(crate) directory: &'a Path,
    pub(crate) repo_name: &'a str,
    pub(crate) name: &'a str,
    pub(crate) owner: &'a str,
}

pub(crate) fn create_placed(
    repo_root: &Path,
    project: &Path,
    place: Placement<'_>,
    apply: &[PathBuf],
    clone: CloneFn,
) -> Result<Checkout, String> {
    let Placement {
        directory,
        repo_name,
        name,
        owner,
    } = place;
    let lock = CREATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let subdir = project
        .strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    // The repository is known by its real path from here on, so a symlinked
    // path to it is the same repository (see `integrate`).
    let real_root = checked::real_path(repo_root).map_err(|error| error.to_string())?;
    let repo_root = real_root.as_path();
    let snapshot = snapshot(repo_root)?;
    let path = crate::worktree::default_checkout_path(directory, repo_name, name);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    checked::ensure_root(directory).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        checked::create_dir_inside(directory, parent).map_err(|error| error.to_string())?;
    }
    // First, before anything exists at the name: the marker is what lets
    // `remove` recognise the directory as ours, and an existing marker path
    // (a file or a symlink someone else put there) refuses the whole checkout.
    write_marker(&path, repo_root, owner)?;
    let cloned = clonable(repo_root, directory)
        && match clone(repo_root, &path).map_err(|error| error.to_string()) {
            // Nothing in the clone is touched until it is proven to be a
            // repository of its own; then whatever lock the original held at
            // that moment (copied with it) is dropped.
            Ok(()) => {
                verify_clone(&path).is_ok()
                    && safety::remove_copied_lock(&path)
                    && checkout_snapshot(&path, &snapshot).is_ok()
            }
            Err(_) => false,
        };
    let method = if cloned {
        CheckoutMethod::CopyOnWrite
    } else {
        // A failed clone leaves a partial copy that Git would refuse to add over.
        let _ = checked::remove_dir_inside(directory, &path);
        if let Err(error) = add_worktree(repo_root, &path, &snapshot) {
            safety::remove_marker(directory, &path);
            return Err(error);
        }
        CheckoutMethod::Worktree
    };
    drop(lock);
    let prepared = apply
        .iter()
        .try_for_each(|patch| apply_patch(&path, patch))
        .and_then(|()| working_tree(&path))
        .inspect_err(|_| remove_unchecked(directory, &path, repo_root, method));
    let baseline = prepared?;
    Ok(Checkout {
        path,
        repo_root: repo_root.to_path_buf(),
        subdir,
        snapshot,
        baseline,
        method,
    })
}

/// Saves what the step changed since it started to `patch_path`: a patch for
/// `git apply` covering edits to tracked files and new files, binary-safe.
pub(crate) fn capture_patch(
    checkout: &Checkout,
    patch_path: &Path,
) -> Result<PatchSummary, String> {
    let (patch, names) = with_private_index(&checkout.path, |command| {
        let mut diff = command();
        diff.args(["diff", "--cached", "--binary", "--full-index"])
            .arg(&checkout.baseline);
        let patch = run_bytes(diff)?;
        let mut names = command();
        names
            .args(["diff", "--cached", "--name-only", "-z"])
            .arg(&checkout.baseline);
        Ok((patch, run_bytes(names)?))
    })?;
    let files: Vec<String> = names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    let directory = patch_path.parent().unwrap_or(Path::new("."));
    checked::ensure_dir(directory).map_err(|error| error.to_string())?;
    checked::write_replace_inside(directory, patch_path, &patch)
        .map_err(|error| error.to_string())?;
    Ok(PatchSummary {
        path: patch_path.to_path_buf(),
        bytes: patch.len() as u64,
        file_count: files.len(),
        files: files.into_iter().take(MAX_LISTED_FILES).collect(),
    })
}
