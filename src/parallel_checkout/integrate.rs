//! Applying the saved patches of parallel steps to the main tree, in step
//! order, with `git apply --3way` (which also stages them). Runs git: call it
//! only off the event loop.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::checked::{git, run, run_bytes};

/// One saved patch to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchToApply {
    /// The step that made it.
    pub step: usize,
    pub path: PathBuf,
    /// The paths it changes, for a patch that fails without leaving conflicts.
    pub files: Vec<String>,
    /// The steps whose results this patch is built on (its chain predecessors):
    /// if it is in the tree, so are they.
    pub after: Vec<usize>,
}

/// The patch that stopped the integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stopped {
    pub step: usize,
    pub path: PathBuf,
    /// The files in conflict, whose markers were left in the tree, or, when
    /// the patch did not apply at all, the files it changes.
    pub files: Vec<String>,
    /// Conflict markers were left (`false`: the patch did not apply and
    /// changed nothing).
    pub conflict: bool,
    /// Whether the patch was applied is not known: the server stopped while
    /// it was being applied, and the tree does not settle it.
    #[serde(default)]
    pub unclear: bool,
    pub message: String,
}

/// How far the integration got.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Steps whose patches were applied cleanly, in order.
    pub applied: Vec<usize>,
    /// Steps whose patches were already in the tree, so Aster neither applied
    /// nor staged them (the same change had been made some other way).
    pub present: Vec<usize>,
    /// Steps found in the tree after a restart with no journal, and assumed
    /// applied (a patch in the tree implies the earlier ones of its chain).
    #[serde(default)]
    pub recovered: Vec<usize>,
    pub stopped: Option<Stopped>,
}

/// Paths with unresolved conflicts in the tree.
fn conflicted_files(repo_root: &Path) -> Vec<String> {
    let mut diff = git(repo_root);
    diff.args(["diff", "--name-only", "--diff-filter=U", "-z"]);
    run_bytes(diff)
        .map(|names| {
            names
                .split(|byte| *byte == 0)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The conflicts a failed patch caused: unmerged paths that were not unmerged
/// before it, among the patch's own files (all of them when the patch lists
/// more files than are recorded).
fn new_conflicts(before: &[String], after: Vec<String>, patch: &PatchToApply) -> Vec<String> {
    let complete = patch.files.len() < super::MAX_LISTED_FILES;
    after
        .into_iter()
        .filter(|file| !before.contains(file))
        .filter(|file| !complete || patch.files.contains(file))
        .collect()
}

/// Applies each patch to `repo_root` in order, exactly once, and stops at the
/// first that does not apply cleanly. A conflicting patch leaves its markers
/// in the tree; the patches after it are not tried.
pub(crate) fn apply_in_order(repo_root: &Path, patches: &[PatchToApply]) -> ApplyReport {
    let mut applied = Vec::new();
    for patch in patches {
        if super::checked::is_empty_file(&patch.path) {
            applied.push(patch.step);
            continue;
        }
        // Conflicts that were already there are not this patch's.
        let before = conflicted_files(repo_root);
        let mut apply = git(repo_root);
        apply
            .args(["apply", "--3way", "--whitespace=nowarn"])
            .arg(&patch.path);
        let Err(message) = run(apply) else {
            applied.push(patch.step);
            continue;
        };
        let conflicts = new_conflicts(&before, conflicted_files(repo_root), patch);
        let conflict = !conflicts.is_empty();
        let files = if conflict {
            conflicts
        } else {
            patch.files.clone()
        };
        let stopped = Stopped {
            step: patch.step,
            path: patch.path.clone(),
            files,
            conflict,
            unclear: false,
            message,
        };
        return ApplyReport {
            present: Vec::new(),
            recovered: Vec::new(),
            applied,
            stopped: Some(stopped),
        };
    }
    ApplyReport {
        present: Vec::new(),
        recovered: Vec::new(),
        applied,
        stopped: None,
    }
}

/// What the tree says about a patch.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The patch is in the tree (it can be reversed exactly, and not applied).
    Applied,
    /// The patch is not in the tree and applies to it as it is.
    NotApplied,
    /// Neither: it was cut off part way, conflicts with the tree, or needs a
    /// three-way merge.
    Unclear,
}

/// The readings behind a `Probe`.
pub(super) struct Reading {
    /// Conflict markers in the patch's own files.
    pub(super) own_conflicts: bool,
    pub(super) reversible: bool,
    pub(super) applicable: bool,
}

pub(super) fn read_tree(repo_root: &Path, patch: &PatchToApply) -> Reading {
    let check = |reverse: bool| {
        let mut command = git(repo_root);
        command.args(["apply", "--check", "--whitespace=nowarn"]);
        if reverse {
            command.arg("-R");
        }
        command.arg(&patch.path);
        run(command).is_ok()
    };
    Reading {
        own_conflicts: !new_conflicts(&[], conflicted_files(repo_root), patch).is_empty(),
        reversible: check(true),
        applicable: check(false),
    }
}

fn empty_patch(patch: &PatchToApply) -> bool {
    super::checked::is_empty_file(&patch.path)
}

/// Whether `patch` is in the tree: it reverses exactly (`git apply -R
/// --check`) if so, and applies exactly (`--check`) if not.
#[cfg(test)]
pub(crate) fn probe(repo_root: &Path, patch: &PatchToApply) -> Probe {
    if empty_patch(patch) {
        return Probe::Applied;
    }
    let tree = read_tree(repo_root, patch);
    match (tree.own_conflicts, tree.reversible, tree.applicable) {
        (false, true, false) => Probe::Applied,
        (false, false, true) => Probe::NotApplied,
        _ => Probe::Unclear,
    }
}

/// Held by tests to keep an apply thread from touching the tree.
#[cfg(test)]
pub(crate) static HOLD_APPLY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Applies one patch, safely to repeat: the tree is read first, so a patch
/// already in it (applied before a crash that lost the record of it) is never
/// applied again, whatever was or was not persisted. A patch that merely needs
/// a three-way merge is applied as usual and may conflict; one whose own files
/// hold conflict markers, or that both reverses and applies, is left alone and
/// reported as unclear.
pub(crate) fn apply_once(repo_root: &Path, patch: &PatchToApply) -> ApplyReport {
    #[cfg(test)]
    while HOLD_APPLY.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if empty_patch(patch) {
        return ApplyReport {
            present: Vec::new(),
            recovered: Vec::new(),
            applied: vec![patch.step],
            stopped: None,
        };
    }
    let tree = read_tree(repo_root, patch);
    if !tree.own_conflicts && tree.reversible && !tree.applicable {
        return ApplyReport {
            present: vec![patch.step],
            recovered: Vec::new(),
            applied: Vec::new(),
            stopped: None,
        };
    }
    if tree.own_conflicts || (tree.reversible && tree.applicable) {
        return ApplyReport {
            present: Vec::new(),
            recovered: Vec::new(),
            applied: Vec::new(),
            stopped: Some(Stopped {
                step: patch.step,
                path: patch.path.clone(),
                files: patch.files.clone(),
                conflict: false,
                unclear: true,
                message: "its files already hold conflict markers, or the patch both reverses and applies".into(),
            }),
        };
    }
    apply_in_order(repo_root, std::slice::from_ref(patch))
}
