//! The one place where checkout and integration code touches the file system
//! or runs git. Everything here is deliberate about three things: paths that
//! are written or removed must resolve strictly inside a root the caller names;
//! symlinks are never followed (a link is refused, or unlinked itself, never
//! read or written through); and files are created exclusively, so nothing that
//! was already there is overwritten. Call sites use these helpers instead of
//! `std::fs` and `Command`, so a rule fixed here is fixed for all of them.
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---- git ----

/// Git in `repo`, with a fixed identity and no prompts. The only way this
/// module family starts git.
pub(crate) fn git(repo: &Path) -> Command {
    let mut command = crate::noninteractive_process::command("git");
    command
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "user.name=Aster", "-c", "user.email=aster@localhost"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.quotepath=off"])
        .arg("-C")
        .arg(repo);
    command
}

pub(crate) fn run_bytes(mut command: Command) -> Result<Vec<u8>, String> {
    let output = command.output().map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if stderr.is_empty() {
        format!("git failed with status {}", output.status)
    } else {
        stderr
    })
}

pub(crate) fn run(command: Command) -> Result<String, String> {
    run_bytes(command).map(|out| String::from_utf8_lossy(&out).trim().to_owned())
}

// ---- looking, without following links ----

/// The path with every link resolved; fails if it does not exist.
pub(crate) fn real_path(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Like `real_path`, but a path that is gone resolves through its nearest
/// existing ancestor, with the missing names appended as they are (a `..` among
/// them is refused).
pub(crate) fn real_target(path: &Path) -> io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
            let mut current = path;
            loop {
                let name = current
                    .file_name()
                    .ok_or_else(|| refuse(path, "it has a component that cannot be resolved"))?;
                missing.push(name);
                current = current
                    .parent()
                    .ok_or_else(|| refuse(path, "it has no existing ancestor"))?;
                match std::fs::canonicalize(current) {
                    Ok(base) => {
                        return Ok(missing.iter().rev().fold(base, |acc, name| acc.join(name)))
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        other => other,
    }
}

/// Whether anything is at the path (a dangling link counts).
pub(crate) fn exists_no_follow(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Whether the path is a real directory, not a link to one.
pub(crate) fn is_plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

/// Whether the path is a real file, not a link.
pub(crate) fn is_plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// Whether the path is a real file with nothing in it.
pub(crate) fn is_empty_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file() && meta.len() == 0)
}

/// Whether any entry directly inside `dir` is a link; `None` if it cannot be listed.
pub(crate) fn has_link_inside(dir: &Path) -> Option<bool> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.file_type().is_ok_and(|kind| kind.is_symlink())),
    )
}

/// The text of a real file; a link or anything else is refused.
pub(crate) fn read_plain(path: &Path) -> io::Result<String> {
    if !is_plain_file(path) {
        return Err(refuse(path, "not a regular file"));
    }
    std::fs::read_to_string(path)
}

fn refuse(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("refusing {}: {why}", path.display()),
    )
}

// ---- containment ----

/// The real path of `path` when it is strictly inside the real `root`.
pub(crate) fn inside(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let root = std::fs::canonicalize(root)?;
    let target = real_target(path)?;
    if target.starts_with(&root) && target != root {
        Ok(target)
    } else {
        Err(refuse(path, "it is not inside the allowed directory"))
    }
}

/// Makes `dir` (and what is missing above it) a real directory, not a link.
pub(crate) fn ensure_dir(dir: &Path) -> io::Result<()> {
    if is_plain_dir(dir) {
        return Ok(());
    }
    if exists_no_follow(dir) {
        return Err(refuse(dir, "it exists and is not a real directory"));
    }
    std::fs::create_dir_all(dir)?;
    if is_plain_dir(dir) {
        Ok(())
    } else {
        Err(refuse(dir, "it is not a real directory"))
    }
}

/// Makes the configured directory `dir` exist. Unlike `ensure_dir` it may be a
/// link to a directory, since the user chose it; what is created below it is
/// always checked against its real path.
pub(crate) fn ensure_root(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if std::fs::metadata(dir)?.is_dir() {
        Ok(())
    } else {
        Err(refuse(dir, "it is not a directory"))
    }
}

/// Creates `dir` below `root`; both must stay real directories.
pub(crate) fn create_dir_inside(root: &Path, dir: &Path) -> io::Result<()> {
    let target = inside(root, dir)?;
    std::fs::create_dir_all(&target)?;
    inside(root, &target).map(|_| ())
}

/// Deletes the directory tree at `path`, which must be a real directory
/// strictly inside the real `root`. A link at `path` is refused.
pub(crate) fn remove_dir_inside(root: &Path, path: &Path) -> io::Result<()> {
    if exists_no_follow(path) && !is_plain_dir(path) {
        return Err(refuse(path, "it is not a real directory"));
    }
    let target = inside(root, path)?;
    match std::fs::remove_dir_all(target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Deletes the file (or link, unlinking it, never its target) at `path`, whose
/// directory must be strictly inside the real `root` or be the root itself.
pub(crate) fn remove_file_inside(root: &Path, path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| refuse(path, "it has no directory"))?;
    let real_parent = std::fs::canonicalize(parent)?;
    let real_root = std::fs::canonicalize(root)?;
    if !real_parent.starts_with(&real_root) {
        return Err(refuse(path, "it is not inside the allowed directory"));
    }
    match std::fs::remove_file(real_parent.join(path.file_name().unwrap_or_default())) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

// ---- writing ----

/// Creates the file with `bytes`; fails if anything, even a link, is there.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)
}

/// Replaces the file at `path` (directly inside `root`, or below it) with
/// `bytes` through a new temporary file and a rename, so a reader never sees
/// half of it and a link at `path` is replaced, never written through.
pub(crate) fn write_replace_inside(root: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| refuse(path, "it has no directory"))?;
    let real_root = std::fs::canonicalize(root)?;
    let real_parent = real_target(parent)?;
    if !real_parent.starts_with(&real_root) {
        return Err(refuse(path, "it is not inside the allowed directory"));
    }
    let name = path
        .file_name()
        .ok_or_else(|| refuse(path, "it has no name"))?;
    let target = real_parent.join(name);
    let mut partial = target.clone().into_os_string();
    partial.push(".tmp");
    let partial = PathBuf::from(partial);
    remove_file_inside(root, &partial)?;
    write_new(&partial, bytes)?;
    std::fs::rename(&partial, &target)
}

/// Appends `bytes` to the file at `path` and flushes them to disk (and, for a
/// new file, its directory), or fails. The file must be a real file, not a link.
pub(crate) fn append_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or(Path::new("."));
    ensure_dir(parent)?;
    let existed = exists_no_follow(path);
    if existed && !is_plain_file(path) {
        return Err(refuse(path, "it is not a regular file"));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if !existed {
        // Windows cannot open a directory with a plain File::open; the
        // platform helper syncs where that is possible and is a no-op there.
        crate::platform::sync_parent_directory(parent)?;
    }
    Ok(())
}
