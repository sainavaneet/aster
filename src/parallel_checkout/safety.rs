//! Checks that keep checkout operations inside what Aster itself created: a
//! clone is proven to be a repository of its own before anything in it is
//! touched, and a checkout is deleted only when it is strictly inside the
//! configured worktrees directory and carries the marker written at creation.
//! All file-system access goes through `checked`.
use std::path::{Path, PathBuf};

use super::checked::{self, git, run};
use super::CheckoutMethod;

fn plain_git_dir(git_dir: &Path) -> bool {
    checked::is_plain_dir(git_dir)
}

/// Whether the repository keeps Git state outside its own `.git` directory or
/// reaches it through a link: a symlink, a shared common directory, alternate
/// object stores. A clone of such a repository would share that state with the
/// original, and Git commands run in the clone could write to it.
pub(super) fn git_state_is_shared(git_dir: &Path) -> bool {
    if checked::exists_no_follow(&git_dir.join("commondir"))
        || checked::exists_no_follow(&git_dir.join("objects/info/alternates"))
    {
        return true;
    }
    checked::has_link_inside(git_dir).unwrap_or(true)
}

/// A clone needs a plain repository whose `.git` is a real directory of its
/// own (not a symlink, which `is_dir` would follow, and not a gitfile).
pub(super) fn clonable(repo_root: &Path, directory: &Path) -> bool {
    let git_dir = repo_root.join(".git");
    plain_git_dir(&git_dir) && !directory.starts_with(repo_root) && !git_state_is_shared(&git_dir)
}

/// Before anything in a clone is touched: its `.git` is a real directory with
/// no shared state or links inside it, and Git resolves its Git directory and
/// work tree inside the clone, so nothing done there reaches the original.
pub(super) fn verify_clone(clone: &Path) -> Result<(), String> {
    let git_dir = clone.join(".git");
    if !plain_git_dir(&git_dir) || git_state_is_shared(&git_dir) {
        return Err("the clone's .git is a link or shares state with another repository".into());
    }
    let ask = |what: &str| {
        let mut command = git(clone);
        command.args(["rev-parse", what]);
        run(command).and_then(|path| {
            checked::real_path(Path::new(&path)).map_err(|error| format!("{path}: {error}"))
        })
    };
    let root = checked::real_path(clone).map_err(|error| error.to_string())?;
    let (git_dir, top) = (ask("--absolute-git-dir")?, ask("--show-toplevel")?);
    if git_dir.starts_with(&root) && top == root {
        return Ok(());
    }
    Err(format!(
        "the clone's Git directory {} is not inside {}",
        git_dir.display(),
        root.display()
    ))
}

/// Removes the index lock a clone copied from the original, if the file really
/// is inside the clone. `false` if it could not be removed.
pub(super) fn remove_copied_lock(clone: &Path) -> bool {
    let lock = clone.join(".git/index.lock");
    if !checked::exists_no_follow(&lock) {
        return true;
    }
    let Ok(root) = checked::real_path(clone) else {
        return false;
    };
    checked::remove_file_inside(&root, &lock).is_ok()
}

/// The ownership marker of a checkout: a sibling file naming its canonical path.
fn marker_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}.aster-checkout"))
}

/// Records that Aster created the checkout at `path`. The marker is created
/// exclusively (`write_new`): a marker path someone else put there, a link
/// included, is an error, never written through.
pub(super) fn write_marker(path: &Path, repo_root: &Path) -> Result<(), String> {
    let target = checked::real_target(path).map_err(|error| error.to_string())?;
    let marker = marker_path(path);
    let text = format!("{}\n{}\n", target.display(), repo_root.display());
    checked::write_new(&marker, text.as_bytes()).map_err(|error| {
        format!(
            "cannot create the ownership marker {}: {error}",
            marker.display()
        )
    })
}

/// Deletes the marker this process created for `path` (below `directory`).
pub(super) fn remove_marker(directory: &Path, path: &Path) {
    let _ = checked::remove_file_inside(directory, &marker_path(path));
}

/// The marker's two lines (canonical checkout path, source repository), read
/// only from a regular file, never through a symlink.
fn read_marker(path: &Path) -> Option<(String, String)> {
    let text = checked::read_plain(&marker_path(path)).ok()?;
    let mut lines = text.lines();
    Some((lines.next()?.to_owned(), lines.next()?.to_owned()))
}

/// Why a checkout was not removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveError {
    /// The recorded path is not a checkout Aster owns inside the worktrees
    /// directory; nothing was touched and trying again will not change that.
    Refused(String),
    /// Removal failed and may succeed later.
    Failed(String),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

fn refusal(path: &Path, why: &str) -> RemoveError {
    RemoveError::Refused(format!("refusing to remove {}: {why}", path.display()))
}

/// The canonical path of `path` when it is strictly inside the canonical
/// worktrees `directory` and does not contain the repository.
fn contained(path: &Path, repo_root: &Path, directory: &Path) -> Result<PathBuf, RemoveError> {
    if !path.is_absolute() {
        return Err(refusal(path, "the path is not absolute"));
    }
    let directory = checked::real_path(directory).map_err(|error| {
        refusal(
            path,
            &format!("the worktrees directory is unavailable ({error})"),
        )
    })?;
    let target = checked::real_target(path).map_err(|error| refusal(path, &error.to_string()))?;
    if !target.starts_with(&directory) || target == directory {
        return Err(refusal(path, "it is not inside the worktrees directory"));
    }
    if repo_root.starts_with(&target) {
        return Err(refusal(path, "it contains the repository"));
    }
    Ok(target)
}

/// Whether the marker (if any) names exactly this checkout and its repository.
fn marker_matches(path: &Path, target: &Path, repo_root: &Path) -> bool {
    read_marker(path).is_some_and(|(named, repo)| {
        named == target.display().to_string() && repo == repo_root.display().to_string()
    })
}

/// The canonical path to delete, once it is proven to be ours: contained (see
/// `contained`) and named by the marker written when it was created.
fn authorize(path: &Path, repo_root: &Path, directory: &Path) -> Result<PathBuf, RemoveError> {
    let target = contained(path, repo_root, directory)?;
    if !marker_matches(path, &target, repo_root) {
        return Err(refusal(path, "it has no matching ownership marker"));
    }
    Ok(target)
}

/// Deletes a checkout this process just created, on a failed creation.
pub(super) fn remove_unchecked(
    directory: &Path,
    path: &Path,
    repo_root: &Path,
    method: CheckoutMethod,
) {
    if method == CheckoutMethod::Worktree {
        let command = crate::worktree::build_worktree_remove_command(repo_root, path, true, false);
        let _ = crate::worktree::run_worktree_remove_command_with_recovery(
            &command, repo_root, path, true, false,
        );
    }
    let _ = checked::remove_dir_inside(directory, path);
    remove_marker(directory, path);
    if method == CheckoutMethod::Worktree {
        prune(repo_root);
    }
}

fn prune(repo_root: &Path) {
    let mut prune = git(repo_root);
    prune.args(["worktree", "prune"]);
    let _ = run(prune);
}

/// Removes a checkout from disk, and its worktree entry from the repository,
/// after proving it is one Aster created under `directory`.
pub(crate) fn remove(checkout: &super::Checkout, directory: &Path) -> Result<(), RemoveError> {
    let (path, repo_root) = (&checkout.path, &checkout.repo_root);
    let gone = !checked::exists_no_follow(path);
    if path.is_absolute() && gone {
        // Nothing to delete, but the leftovers are only touched once the path
        // is proven to be inside the worktrees directory and the marker agrees.
        let target = contained(path, repo_root, directory)?;
        if read_marker(path).is_none() {
            return Ok(());
        }
        if !marker_matches(path, &target, repo_root) {
            return Err(refusal(path, "its ownership marker names something else"));
        }
        remove_marker(directory, path);
        if checkout.method == CheckoutMethod::Worktree {
            prune(repo_root);
        }
        return Ok(());
    }
    let target = authorize(path, repo_root, directory)?;
    if checkout.method == CheckoutMethod::Worktree {
        let command =
            crate::worktree::build_worktree_remove_command(repo_root, &target, true, false);
        let removed = crate::worktree::run_worktree_remove_command_with_recovery(
            &command, repo_root, &target, true, false,
        );
        if removed.is_ok() && !checked::exists_no_follow(&target) {
            remove_marker(directory, path);
            return Ok(());
        }
    }
    checked::remove_dir_inside(directory, &target)
        .map_err(|error| RemoveError::Failed(error.to_string()))?;
    if checkout.method == CheckoutMethod::Worktree {
        prune(repo_root);
    }
    remove_marker(directory, path);
    Ok(())
}
