//! Making integration safe across crashes. The thread that applies a patch
//! writes a durable journal (see `journal`) before it touches the tree and after
//! it is done, so a restart knows how far it got, whatever the ledger had time
//! to record. Without a journal, the tree itself is read, and only patches that
//! are chain predecessors of one found in the tree are taken as applied.
//! Anything that cannot be settled stops as unclear; nothing is skipped silently.
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::integrate::{apply_once, read_tree, ApplyReport, PatchToApply, Stopped};
use super::journal::{Entry, Journal, JournalError};

/// The journal file of one task's integration, for tests that write their own.
#[cfg(test)]
pub(crate) fn journal_path(directory: &Path, task_id: &str) -> std::path::PathBuf {
    Journal::in_dir(directory, task_id)
        .map(|journal| journal.path().to_path_buf())
        .unwrap_or_else(|_| directory.join("invalid.journal"))
}

fn unclear(patch: &PatchToApply, message: &str) -> Stopped {
    Stopped {
        step: patch.step,
        path: patch.path.clone(),
        files: patch.files.clone(),
        conflict: false,
        unclear: true,
        message: message.into(),
    }
}

/// `report`, stopped at `patch` as unclear.
fn stop(mut report: ApplyReport, patch: &PatchToApply, message: &str) -> ApplyReport {
    report.stopped = Some(unclear(patch, message));
    report
}

/// `apply_once` with the journal written first: `about N` before the tree is
/// touched, `applied N` or `present N` after. If the first record cannot be made
/// durable, nothing is applied. If the second cannot, the patch is reported
/// applied but the integration stops there, as unclear, so the next patch is
/// never applied on a journal that has fallen behind the tree.
pub(crate) fn apply_journaled(
    repo_root: &Path,
    patch: &PatchToApply,
    journal: &Journal,
) -> ApplyReport {
    if let Err(error) = journal.record(Entry::About(patch.step)) {
        let message = format!("cannot write the integration journal ({error})");
        let mut stopped = unclear(patch, &message);
        stopped.unclear = false;
        return ApplyReport {
            stopped: Some(stopped),
            ..ApplyReport::default()
        };
    }
    let report = apply_once(repo_root, patch);
    if report.stopped.is_some() {
        // The "about" record stays: the patch is in flight until someone settles it.
        return report;
    }
    let entry = if report.present.is_empty() {
        Entry::Applied(patch.step)
    } else {
        Entry::Present(patch.step)
    };
    match journal.record(entry) {
        Ok(()) => report,
        Err(error) => {
            let message = format!(
                "it was applied, but its journal record could not be made durable ({error}); no further patch was applied"
            );
            ApplyReport {
                stopped: Some(unclear(patch, &message)),
                ..report
            }
        }
    }
}

/// Works out how far a previous run got among `pending` (the patches the
/// ledger does not record as done), without applying anything. The journal
/// decides when there is one; the tree decides otherwise.
pub(crate) fn reconcile(
    repo_root: &Path,
    pending: &[PatchToApply],
    journal: &Journal,
) -> ApplyReport {
    let Some(first) = pending.first() else {
        return ApplyReport::default();
    };
    match journal.read() {
        Err(JournalError::Io(error) | JournalError::Malformed(error)) => stop(
            ApplyReport::default(),
            first,
            &format!("the integration journal cannot be trusted ({error}); check git status and git diff"),
        ),
        Ok(entries) if entries.is_empty() => from_the_tree(repo_root, pending),
        Ok(entries) => from_journal(repo_root, pending, &entries),
    }
}

fn from_journal(repo_root: &Path, pending: &[PatchToApply], entries: &[Entry]) -> ApplyReport {
    let mut outcome: HashMap<usize, Entry> = HashMap::new();
    let mut started: HashSet<usize> = HashSet::new();
    for entry in entries {
        match entry {
            Entry::About(step) => {
                started.insert(*step);
            }
            Entry::Applied(step) | Entry::Present(step) => {
                outcome.insert(*step, *entry);
            }
        }
    }
    // A patch another one was built on is in the tree if that one is.
    let implied: HashSet<usize> = pending
        .iter()
        .filter(|p| matches!(outcome.get(&p.step), Some(Entry::Applied(_))))
        .flat_map(|p| p.after.iter().copied())
        .collect();
    let mut report = ApplyReport::default();
    for (at, patch) in pending.iter().enumerate() {
        match outcome.get(&patch.step) {
            Some(Entry::Applied(_)) => report.applied.push(patch.step),
            Some(_) => report.present.push(patch.step),
            // Started, with no outcome recorded: whatever else was recorded since.
            None if started.contains(&patch.step) => {
                if implied.contains(&patch.step) {
                    report.applied.push(patch.step);
                    continue;
                }
                let tree = read_tree(repo_root, patch);
                if tree.own_conflicts || (tree.reversible && tree.applicable) {
                    return stop(report, patch, "the server stopped while it was being applied and the tree does not settle it");
                }
                if tree.reversible {
                    report.applied.push(patch.step);
                } else if tree.applicable {
                    break;
                } else {
                    return stop(report, patch, "the server stopped while it was being applied and the tree has changed since");
                }
            }
            None if pending[at + 1..]
                .iter()
                .any(|q| outcome.contains_key(&q.step)) =>
            {
                return stop(
                    report,
                    patch,
                    "the journal records a later patch as done but never started this one",
                );
            }
            None => break,
        }
    }
    report
}

/// What the tree says about one patch.
enum Standing {
    InTree,
    NotApplied,
    /// Neither: the files changed since, or it needs a three-way merge.
    Neither,
}

fn from_the_tree(repo_root: &Path, pending: &[PatchToApply]) -> ApplyReport {
    let mut standing = Vec::with_capacity(pending.len());
    for patch in pending {
        if super::checked::is_empty_file(&patch.path) {
            standing.push(Standing::InTree);
            continue;
        }
        let tree = read_tree(repo_root, patch);
        if tree.own_conflicts || (tree.reversible && tree.applicable) {
            return unclear_from(patch, "there is no usable integration journal, and the tree does not settle whether it was applied");
        }
        standing.push(match (tree.reversible, tree.applicable) {
            (true, _) => Standing::InTree,
            (false, true) => Standing::NotApplied,
            (false, false) => Standing::Neither,
        });
    }
    // Nothing of this integration in the tree: a fresh start, whatever else is
    // true of the patches (one may need a three-way merge; applying finds out).
    if !standing.iter().any(|s| matches!(s, Standing::InTree)) {
        return ApplyReport::default();
    }
    // A patch in the tree implies the patches it was built on, and only those.
    let implied: HashSet<usize> = pending
        .iter()
        .zip(&standing)
        .filter(|(_, s)| matches!(s, Standing::InTree))
        .flat_map(|(p, _)| p.after.iter().copied())
        .collect();
    let mut report = ApplyReport::default();
    let mut unapplied_seen = false;
    for (patch, standing) in pending.iter().zip(&standing) {
        match standing {
            Standing::InTree if unapplied_seen => report.present.push(patch.step),
            Standing::InTree => report.recovered.push(patch.step),
            Standing::NotApplied => unapplied_seen = true,
            Standing::Neither if implied.contains(&patch.step) => report.recovered.push(patch.step),
            // Independent of everything found in the tree, yet neither there nor
            // applicable: something else changed its files. Surface it.
            Standing::Neither => {
                return stop(
                    report,
                    patch,
                    "other patches of this integration are in the tree, but this one neither applies nor reverses (its files changed since); check git status and git diff",
                );
            }
        }
    }
    report
}

fn unclear_from(patch: &PatchToApply, message: &str) -> ApplyReport {
    stop(ApplyReport::default(), patch, message)
}
