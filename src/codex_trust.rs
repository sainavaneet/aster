//! Codex folder trust for Orbit's own checkouts. codex-cli asks to trust every
//! new folder and ignores `-c projects."<path>".trust_level` overrides, so a
//! Codex worker in a fresh Orbit worktree or parallel checkout would stop at
//! that dialog. When, and only when, the source project is already trusted in
//! `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`), the checkout's
//! counterpart path is trusted too, with a marked entry appended to the file;
//! the entry is removed again with the checkout. Every other byte of the file
//! is left as it was, and a missing file is never created.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Starts every entry Aster appends; removal matches it exactly.
const MARKER: &str = "\n# Trusted by Aster for an Orbit checkout; removed with the checkout.\n";

/// `$CODEX_HOME/config.toml`, else `~/.codex/config.toml`.
pub(crate) fn config_path() -> Option<PathBuf> {
    config_path_from(|key| std::env::var_os(key))
}

fn config_path_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let home = env("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".codex"))
        })?;
    Some(home.join("config.toml"))
}

fn entry(path: &Path) -> String {
    let key = toml::Value::String(path.display().to_string());
    format!("{MARKER}[projects.{key}]\ntrust_level = \"trusted\"\n")
}

/// Whether `config` trusts exactly `path` (as written, or canonical).
fn trusts(config: &toml::Table, path: &Path) -> bool {
    let Some(projects) = config.get("projects").and_then(toml::Value::as_table) else {
        return false;
    };
    let canonical = fs::canonicalize(path).ok();
    projects.iter().any(|(key, project)| {
        let key = Path::new(key);
        (key == path || canonical.as_deref() == Some(key))
            && project.get("trust_level").and_then(toml::Value::as_str) == Some("trusted")
    })
}

/// Trusts, in `config`, the checkout counterpart of the source project:
/// `checkout` mirrors `repo_root`, and the project is `project` inside it.
/// Returns the paths trusted; nothing is written when the source is not
/// trusted, the file is missing, or the result would not be valid TOML.
pub(crate) fn trust_checkout_in(
    config: &Path,
    repo_root: &Path,
    project: &Path,
    checkout: &Path,
) -> Result<Vec<PathBuf>, String> {
    let Some(original) = read(config)? else {
        tracing::info!(config = %config.display(), "no Codex config; Orbit checkout not trusted");
        return Ok(Vec::new());
    };
    let table: toml::Table = toml::from_str(&original)
        .map_err(|error| format!("Codex config is not valid TOML: {error}"))?;
    let inside = project.strip_prefix(repo_root).unwrap_or(Path::new(""));
    let mut wanted: Vec<PathBuf> = Vec::new();
    for (source, counterpart) in [
        (repo_root, checkout.to_path_buf()),
        (project, checkout.join(inside).components().collect()),
    ] {
        if trusts(&table, source) && !trusts(&table, &counterpart) && !wanted.contains(&counterpart)
        {
            wanted.push(counterpart);
        }
    }
    if wanted.is_empty() {
        tracing::info!(project = %project.display(), "Codex does not trust the Orbit project; checkout not trusted");
        return Ok(Vec::new());
    }
    let mut updated = original;
    for path in &wanted {
        updated.push_str(&entry(path));
    }
    toml::from_str::<toml::Table>(&updated)
        .map_err(|error| format!("Codex config would not stay valid TOML: {error}"))?;
    write_atomic(config, &updated)?;
    Ok(wanted)
}

/// Removes the entries Aster added for `checkout` or a directory inside it.
/// Returns how many were removed.
pub(crate) fn untrust_checkout_in(config: &Path, checkout: &Path) -> Result<usize, String> {
    let Some(original) = read(config)? else {
        return Ok(0);
    };
    let mut updated = original.clone();
    let mut removed = 0;
    let mut from = 0;
    while let Some(offset) = updated[from..].find(MARKER) {
        let start = from + offset;
        let end = entry_end(&updated, start);
        let ours = toml::from_str::<toml::Table>(&updated[start..end])
            .ok()
            .and_then(|table| {
                let projects = table.get("projects")?.as_table()?.clone();
                let (key, _) = projects.into_iter().next()?;
                Some(PathBuf::from(key))
            })
            .filter(|path| updated[start..end] == entry(path) && path.starts_with(checkout));
        if ours.is_some() {
            updated.replace_range(start..end, "");
            removed += 1;
        } else {
            from = start + MARKER.len();
        }
    }
    if removed > 0 {
        write_atomic(config, &updated)?;
    }
    Ok(removed)
}

/// The end of the entry starting at `start`: the marker and the two lines after it.
fn entry_end(text: &str, start: usize) -> usize {
    let body = start + MARKER.len();
    let mut end = body;
    for _ in 0..2 {
        end = text[end..]
            .find('\n')
            .map_or(text.len(), |offset| end + offset + 1);
    }
    end
}

/// [`trust_checkout_in`] for the user's Codex config; failures are logged.
pub(crate) fn trust_checkout(repo_root: &Path, project: &Path, checkout: &Path) {
    let Some(config) = config_path() else {
        return;
    };
    match trust_checkout_in(&config, repo_root, project, checkout) {
        Ok(paths) if !paths.is_empty() => {
            tracing::info!(?paths, "Codex trust added for an Orbit checkout");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not trust the Orbit checkout in Codex"),
    }
}

/// [`untrust_checkout_in`] for the user's Codex config; failures are logged.
pub(crate) fn untrust_checkout(checkout: &Path) {
    let Some(config) = config_path() else {
        return;
    };
    match untrust_checkout_in(&config, checkout) {
        Ok(0) => {}
        Ok(removed) => {
            tracing::info!(removed, checkout = %checkout.display(), "Codex trust removed with an Orbit checkout");
        }
        Err(error) => tracing::warn!(%error, "could not remove the Orbit checkout's Codex trust"),
    }
}

fn read(config: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(config) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not read {}: {error}", config.display())),
    }
}

/// Replaces the file (through a symlink, its target) with `text`, keeping its
/// permissions.
fn write_atomic(config: &Path, text: &str) -> Result<(), String> {
    let target = fs::canonicalize(config).map_err(|error| error.to_string())?;
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent", target.display()))?;
    let permissions = fs::metadata(&target)
        .map_err(|error| error.to_string())?
        .permissions();
    let temp = parent.join(format!(".config.toml.aster-{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temp, permissions)?;
        crate::platform::replace_file(&temp, &target)
    })();
    written.map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("could not write {}: {error}", target.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "model = \"gpt-5-codex\"\n\n# my projects\n[projects.\"/work/aster\"]\ntrust_level = \"trusted\"\n\n[projects.\"/work/other\"]\ntrust_level = \"untrusted\"\n";

    /// A temp CODEX_HOME holding `config` (when given).
    fn codex_home(name: &str, config: Option<&str>) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("aster-codex-trust-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        if let Some(config) = config {
            fs::write(home.join("config.toml"), config).unwrap();
        }
        home
    }

    fn read_config(home: &Path) -> String {
        fs::read_to_string(home.join("config.toml")).unwrap()
    }

    #[test]
    fn codex_home_wins_over_home() {
        let env = |home: Option<&'static str>| {
            move |key: &str| match key {
                "CODEX_HOME" => home.map(Into::into),
                "HOME" => Some("/users/dev".into()),
                _ => None,
            }
        };
        assert_eq!(
            config_path_from(env(Some("/tmp/codex"))),
            Some(PathBuf::from("/tmp/codex/config.toml"))
        );
        assert_eq!(
            config_path_from(env(None)),
            Some(PathBuf::from("/users/dev/.codex/config.toml"))
        );
    }

    #[test]
    fn a_checkout_of_a_trusted_project_is_trusted_and_untrusted_byte_for_byte() {
        let home = codex_home("trusted", Some(USER));
        let config = home.join("config.toml");
        let checkout = Path::new("/wt/aster/par012");
        let trusted = trust_checkout_in(
            &config,
            Path::new("/work/aster"),
            Path::new("/work/aster/crates/core"),
            checkout,
        )
        .unwrap();
        assert_eq!(
            trusted,
            [checkout.to_path_buf()],
            "only the root is trusted in the source"
        );
        let written = read_config(&home);
        assert!(written.starts_with(USER), "{written}");
        let table: toml::Table = toml::from_str(&written).unwrap();
        assert!(trusts(&table, checkout));
        assert!(!trusts(&table, Path::new("/wt/aster")), "no other path");
        // Trusting again adds nothing.
        trust_checkout_in(
            &config,
            Path::new("/work/aster"),
            Path::new("/work/aster"),
            checkout,
        )
        .unwrap();
        assert_eq!(read_config(&home), written);

        assert_eq!(untrust_checkout_in(&config, checkout).unwrap(), 1);
        assert_eq!(read_config(&home), USER, "every other byte is unchanged");
        assert_eq!(untrust_checkout_in(&config, checkout).unwrap(), 0);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn a_checkout_of_an_untrusted_project_is_left_alone() {
        let home = codex_home("untrusted", Some(USER));
        let config = home.join("config.toml");
        for source in ["/work/other", "/work/unknown", "/work"] {
            let trusted = trust_checkout_in(
                &config,
                Path::new(source),
                Path::new(source),
                Path::new("/wt/x/par001"),
            )
            .unwrap();
            assert!(trusted.is_empty(), "{source}");
        }
        assert_eq!(read_config(&home), USER);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn a_missing_config_is_never_created() {
        let home = codex_home("missing", None);
        let config = home.join("config.toml");
        let trusted = trust_checkout_in(
            &config,
            Path::new("/work/aster"),
            Path::new("/work/aster"),
            Path::new("/wt/aster/par002"),
        )
        .unwrap();
        assert!(trusted.is_empty());
        assert_eq!(
            untrust_checkout_in(&config, Path::new("/wt/aster/par002")).unwrap(),
            0
        );
        assert!(!config.exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn removal_keeps_a_user_edited_entry_and_other_checkouts() {
        let home = codex_home("edited", Some(USER.trim_end()));
        let config = home.join("config.toml");
        let root = Path::new("/work/aster");
        for checkout in ["/wt/aster/par003", "/wt/aster/par004"] {
            trust_checkout_in(&config, root, root, Path::new(checkout)).unwrap();
        }
        let both = read_config(&home);
        assert!(both.starts_with(USER.trim_end()));
        assert_eq!(
            untrust_checkout_in(&config, Path::new("/wt/aster/par003")).unwrap(),
            1
        );
        let one = read_config(&home);
        assert_eq!(
            one,
            format!(
                "{}{}",
                USER.trim_end(),
                entry(Path::new("/wt/aster/par004"))
            )
        );
        // An entry the user changed is no longer Aster's to remove.
        let edited = one.replace(
            "trust_level = \"trusted\"\n",
            "trust_level = \"trusted\" # keep\n",
        );
        let edited = edited.replacen(
            "trust_level = \"trusted\" # keep\n",
            "trust_level = \"trusted\"\n",
            1,
        );
        fs::write(&config, &edited).unwrap();
        assert_eq!(
            untrust_checkout_in(&config, Path::new("/wt/aster/par004")).unwrap(),
            0
        );
        assert_eq!(read_config(&home), edited);
        let _ = fs::remove_dir_all(home);
    }
}
