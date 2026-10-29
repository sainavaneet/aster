//! Resuming an integration after a crash: the journal, and the tree as a fallback.
use std::process::Command;

use super::*;
use crate::parallel_checkout::integrate::{apply_once, PatchToApply};
use crate::parallel_checkout::integrate_resume::{apply_journaled, journal_path, reconcile};
use crate::parallel_checkout::journal::Journal;

fn to_apply(step: usize, summary: &PatchSummary) -> PatchToApply {
    PatchToApply {
        step,
        path: summary.path.clone(),
        files: summary.files.clone(),
        after: Vec::new(),
    }
}

impl Scratch {
    fn chain_patch(&self, name: &str, after: &[PathBuf], edits: &[(&str, &str)]) -> PatchSummary {
        let checkout = self.create_named(name, after, no_clone);
        for (file, text) in edits {
            std::fs::write(checkout.path.join(file), text).unwrap();
        }
        let summary =
            capture_patch(&checkout, &self.root.join(format!("patches/{name}.patch"))).unwrap();
        remove(&checkout, &self.checkouts()).unwrap();
        summary
    }

    fn apply_by_hand(&self, patch: &PatchSummary) {
        let status = Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(["apply", "--3way"])
            .arg(&patch.path)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

/// old -> one -> two, both applied to the tree, as after a crash.
fn applied_chain(scratch: &Scratch) -> Vec<PatchToApply> {
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a one\n")]);
    let two = scratch.chain_patch(
        "two",
        std::slice::from_ref(&one.path),
        &[("a.txt", "a one\na two\n")],
    );
    scratch.apply_by_hand(&one);
    scratch.apply_by_hand(&two);
    // The second was made from the first's result.
    let second = PatchToApply {
        after: vec![0],
        ..to_apply(1, &two)
    };
    vec![to_apply(0, &one), second]
}

#[test]
fn an_applied_chain_is_recognised_from_the_end_without_a_journal() {
    let scratch = Scratch::new();
    let patches = applied_chain(&scratch);
    let tree = scratch.state();

    let report = reconcile(
        &scratch.repo(),
        &patches,
        &Journal::at(scratch.root.join("none.journal")),
    );

    assert_eq!(report.recovered, [0, 1], "{report:?}");
    assert!(report.applied.is_empty() && report.present.is_empty() && report.stopped.is_none());
    assert_eq!(scratch.state(), tree, "nothing is replayed");
    assert!(!std::fs::read_to_string(scratch.repo().join("a.txt"))
        .unwrap()
        .contains("<<<<<<<"));
}

#[test]
fn nothing_is_recovered_from_an_untouched_tree() {
    let scratch = Scratch::new();
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a one\n")]);
    let two = scratch.chain_patch("two", &[], &[("b.txt", "b two\n")]);

    let report = reconcile(
        &scratch.repo(),
        &[to_apply(0, &one), to_apply(1, &two)],
        &Journal::at(scratch.root.join("none.journal")),
    );

    assert_eq!(
        report,
        crate::parallel_checkout::integrate::ApplyReport::default()
    );
}

#[test]
fn a_patch_put_there_by_hand_does_not_hide_an_earlier_patch_that_still_applies() {
    let scratch = Scratch::new();
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a one\n")]);
    let two = scratch.chain_patch("two", &[], &[("b.txt", "b two\n")]);
    scratch.apply_by_hand(&two);

    let report = reconcile(
        &scratch.repo(),
        &[to_apply(0, &one), to_apply(1, &two)],
        &Journal::at(scratch.root.join("none.journal")),
    );

    assert!(report.recovered.is_empty(), "{report:?}");
    assert_eq!(report.present, [1]);
}

#[test]
fn the_journal_decides_how_far_a_crashed_run_got() {
    let scratch = Scratch::new();
    let patches = applied_chain(&scratch);
    let journal = journal_path(&scratch.root.join("journals"), "task");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();

    // Patch 0 done; patch 1 was being applied (and is in the tree).
    std::fs::write(&journal, "about 0\napplied 0\nabout 1\n").unwrap();
    let report = reconcile(&scratch.repo(), &patches, &Journal::at(journal.clone()));
    assert_eq!(report.applied, [0, 1], "{report:?}");
    assert!(report.stopped.is_none() && report.recovered.is_empty());

    // Both done.
    std::fs::write(&journal, "about 0\napplied 0\nabout 1\napplied 1\n").unwrap();
    assert_eq!(
        reconcile(&scratch.repo(), &patches, &Journal::at(journal.clone())).applied,
        [0, 1]
    );

    // Only patch 0 was started, and the tree has moved on since (patch 1 is
    // in it): the journal cannot say, and the tree no longer does either.
    std::fs::write(&journal, "about 0\n").unwrap();
    let report = reconcile(
        &scratch.repo(),
        &patches[..1],
        &Journal::at(journal.clone()),
    );
    assert!(report.stopped.is_some_and(|s| s.unclear), "never guessed");
}

#[test]
fn an_in_flight_patch_that_left_conflict_markers_is_unclear() {
    let scratch = Scratch::new();
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a from step\n")]);
    scratch.write("a.txt", "a from main\n");
    scratch.git(&["commit", "-q", "-am", "main moves"]);
    let patch = to_apply(0, &one);
    // The crashed run left the patch half applied: markers in the tree.
    assert!(!Command::new("git")
        .arg("-C")
        .arg(scratch.repo())
        .args(["apply", "--3way"])
        .arg(&one.path)
        .status()
        .unwrap()
        .success());
    let journal = journal_path(&scratch.root.join("journals"), "task");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, "about 0\n").unwrap();

    let stopped = reconcile(
        &scratch.repo(),
        std::slice::from_ref(&patch),
        &Journal::at(journal.clone()),
    )
    .stopped;

    assert!(stopped.unwrap().unclear);
}

#[test]
fn applying_writes_the_journal_before_and_after_and_refuses_without_it() {
    let scratch = Scratch::new();
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a one\n")]);
    let patch = to_apply(0, &one);
    let journal = journal_path(&scratch.root.join("journals"), "task");

    let report = apply_journaled(&scratch.repo(), &patch, &Journal::at(journal.clone()));

    assert_eq!(report.applied, [0]);
    assert_eq!(
        std::fs::read_to_string(&journal).unwrap(),
        "about 0\napplied 0\n"
    );

    // A journal that cannot be written: nothing is applied.
    let scratch = Scratch::new();
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a one\n")]);
    std::fs::write(scratch.root.join("blocker"), "a file, not a directory").unwrap();
    let unwritable = scratch.root.join("blocker/task.journal");

    let report = apply_journaled(
        &scratch.repo(),
        &to_apply(0, &one),
        &Journal::at(unwritable),
    );

    assert!(report.applied.is_empty() && report.stopped.is_some());
    assert_eq!(
        std::fs::read_to_string(scratch.repo().join("a.txt")).unwrap(),
        "a\n"
    );
}

#[test]
fn a_patch_already_present_is_reported_present_not_applied() {
    let scratch = Scratch::new();
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a one\n")]);
    scratch.apply_by_hand(&one);
    let before = scratch.state();

    let report = apply_once(&scratch.repo(), &to_apply(0, &one));

    assert_eq!(report.present, [0]);
    assert!(report.applied.is_empty());
    assert_eq!(scratch.state(), before);
}

// ---- Round 4: written first, and failing, before the fixes ----

#[test]
fn an_independent_patch_that_neither_applies_nor_reverses_is_never_skipped_because_a_later_one_reverses(
) {
    let scratch = Scratch::new();
    let a = scratch.chain_patch("a", &[], &[("a.txt", "a from step\n")]);
    let b = scratch.chain_patch("b", &[], &[("b.txt", "b from step\n")]);
    // B is in the tree. A is independent of B, but the user edited its file
    // meanwhile, so A neither applies nor reverses.
    scratch.apply_by_hand(&b);
    scratch.write("a.txt", "a edited by the user\n");

    let report = reconcile(
        &scratch.repo(),
        &[to_apply(0, &a), to_apply(1, &b)],
        &Journal::at(scratch.root.join("none.journal")),
    );

    assert!(
        report.recovered.is_empty(),
        "A was silently skipped: {report:?}"
    );
    let stopped = report.stopped.expect("A must surface");
    assert_eq!((stopped.step, stopped.files), (0, vec!["a.txt".to_owned()]));
}

#[test]
fn a_journal_with_an_unfinished_about_does_not_replay_it_after_a_later_patch_finished() {
    let scratch = Scratch::new();
    let patches = applied_chain(&scratch);
    let journal = journal_path(&scratch.root.join("journals"), "task");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    // The completion record of patch 0 was lost; patch 1 (built on it) finished.
    std::fs::write(&journal, "about 0\nabout 1\napplied 1\n").unwrap();

    let report = reconcile(&scratch.repo(), &patches, &Journal::at(journal.clone()));

    assert!(report.stopped.is_none(), "{report:?}");
    let mut done: Vec<usize> = [report.applied.clone(), report.recovered.clone()].concat();
    done.sort_unstable();
    assert_eq!(done, [0, 1], "patch 0 would be replayed: {report:?}");
}

#[test]
fn a_truncated_journal_line_is_unclear_never_skipped() {
    let scratch = Scratch::new();
    let patches = applied_chain(&scratch);
    let journal = journal_path(&scratch.root.join("journals"), "task");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    for torn in [
        "about 0\napplied 0\nabout 1\napplied",
        "about 0\napplied 0\nabout 1",
        "about 0\napplied 0\nabout 1\napplied 1",
    ] {
        std::fs::write(&journal, torn).unwrap();

        let report = reconcile(&scratch.repo(), &patches, &Journal::at(journal.clone()));

        assert!(
            report.stopped.as_ref().is_some_and(|s| s.unclear),
            "{torn:?} must stop as unclear: {report:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_completion_record_that_cannot_be_made_durable_stops_further_applies() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let one = scratch.chain_patch("one", &[], &[("a.txt", "a one\n")]);
    let patch = to_apply(0, &one);
    let journal = journal_path(&scratch.root.join("journals"), "task");
    crate::parallel_checkout::integrate::HOLD_APPLY
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let repo = scratch.repo();
    let thread_journal = journal.clone();
    let thread_patch = patch.clone();
    let worker = std::thread::spawn(move || {
        apply_journaled(&repo, &thread_patch, &Journal::at(thread_journal))
    });
    // The "about" record is written before the gate; then the file stops accepting writes.
    for _ in 0..400 {
        if journal.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .is_ok()
    {
        // Rights that ignore the mode (root): nothing to prove.
        crate::parallel_checkout::integrate::HOLD_APPLY
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = worker.join();
        return;
    }
    crate::parallel_checkout::integrate::HOLD_APPLY
        .store(false, std::sync::atomic::Ordering::SeqCst);

    let report = worker.join().unwrap();

    let stopped = report.stopped.expect("the lost record must be surfaced");
    assert!(stopped.unclear, "{stopped:?}");
    assert_eq!(report.applied, [0], "the patch itself was applied");
}
