//! The checked helpers every file and git operation goes through, and the journal.
use super::*;
use crate::parallel_checkout::checked;
use crate::parallel_checkout::journal::{Entry, Journal, JournalError};

fn dir(name: &str) -> PathBuf {
    let scratch = Scratch::new();
    let path = scratch.root.join(name);
    std::fs::create_dir_all(&path).unwrap();
    // The scratch directory must outlive the test body: leak it to the OS temp dir cleanup.
    std::mem::forget(scratch);
    path
}

#[cfg(unix)]
#[test]
fn removal_and_writes_refuse_links_and_paths_outside_the_root() {
    let root = dir("root");
    let outside = dir("outside");
    std::fs::write(outside.join("keep"), "keep").unwrap();
    let inside_dir = root.join("child");
    std::fs::create_dir_all(&inside_dir).unwrap();
    std::fs::write(inside_dir.join("file"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

    // Not inside the root, the root itself, and a link to a directory elsewhere.
    assert!(checked::remove_dir_inside(&root, &outside).is_err());
    assert!(checked::remove_dir_inside(&root, &root).is_err());
    assert!(checked::remove_dir_inside(&root, &root.join("link")).is_err());
    assert_eq!(
        std::fs::read_to_string(outside.join("keep")).unwrap(),
        "keep"
    );
    // A path that climbs out is resolved first, then refused.
    assert!(checked::remove_dir_inside(&root, &root.join("child/../../outside")).is_err());
    assert!(checked::remove_dir_inside(&root, &inside_dir).is_ok());
    assert!(!inside_dir.exists());

    // A link at the destination is replaced, never written through.
    let target_file = outside.join("target");
    std::fs::write(&target_file, "original").unwrap();
    std::os::unix::fs::symlink(&target_file, root.join("patch")).unwrap();
    checked::write_replace_inside(&root, &root.join("patch"), b"new").unwrap();
    assert_eq!(std::fs::read_to_string(&target_file).unwrap(), "original");
    assert_eq!(std::fs::read_to_string(root.join("patch")).unwrap(), "new");
    // Nor into a directory that is a link out of the root.
    assert!(checked::write_replace_inside(&root, &root.join("link/evil"), b"x").is_err());
    assert!(!outside.join("evil").exists());

    // Removing a link unlinks it and leaves its target.
    std::os::unix::fs::symlink(&target_file, root.join("alias")).unwrap();
    checked::remove_file_inside(&root, &root.join("alias")).unwrap();
    assert!(target_file.exists() && !checked::exists_no_follow(&root.join("alias")));
    // Exclusive creation never overwrites.
    assert!(checked::write_new(&root.join("patch"), b"again").is_err());
    assert!(checked::read_plain(&root.join("link")).is_err());
}

#[cfg(unix)]
#[test]
fn directories_are_ensured_without_following_links() {
    let root = dir("ensure");
    let outside = dir("ensure-outside");
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    assert!(checked::ensure_dir(&root.join("link")).is_err());
    assert!(checked::ensure_dir(&root.join("fresh/deeper")).is_ok());
    assert!(checked::create_dir_inside(&root, &root.join("a/b")).is_ok());
    assert!(checked::create_dir_inside(&root, &root.join("link/x")).is_err());
    assert!(!outside.join("x").exists());
}

#[test]
fn the_journal_is_durable_strict_and_never_skips_a_line() {
    let root = dir("journal");
    let journal = Journal::in_dir(&root, "task-1").unwrap();
    assert_eq!(journal.read(), Ok(vec![]));

    journal.record(Entry::About(0)).unwrap();
    journal.record(Entry::Applied(0)).unwrap();
    journal.record(Entry::Present(1)).unwrap();

    assert_eq!(
        journal.read(),
        Ok(vec![Entry::About(0), Entry::Applied(0), Entry::Present(1)])
    );
    for torn in [
        "about 0\napplied",
        "about 0\nabout",
        "about 0",
        "bogus 1\n",
        "about x\n",
        "about 0\n\n",
    ] {
        std::fs::write(journal.path(), torn).unwrap();
        assert!(
            matches!(journal.read(), Err(JournalError::Malformed(_))),
            "{torn:?}"
        );
    }
    for bad in ["", "..", "a/b", "a\\b"] {
        assert!(Journal::in_dir(&root, bad).is_err(), "{bad:?}");
    }
}

#[cfg(unix)]
#[test]
fn the_journal_refuses_to_be_a_link_and_reports_a_write_that_fails() {
    let root = dir("journal-link");
    let elsewhere = dir("journal-elsewhere");
    let victim = elsewhere.join("victim");
    std::fs::write(&victim, "precious\n").unwrap();
    let journal = Journal::in_dir(&root, "task").unwrap();
    std::os::unix::fs::symlink(&victim, journal.path()).unwrap();

    assert!(matches!(
        journal.record(Entry::About(0)),
        Err(JournalError::Io(_))
    ));
    assert!(matches!(journal.read(), Err(JournalError::Io(_))));
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious\n");

    // A journal directory that cannot exist.
    std::fs::write(root.join("blocker"), "file").unwrap();
    let unusable = Journal::at(root.join("blocker/task.journal"));
    assert!(unusable.record(Entry::About(0)).is_err());
}
