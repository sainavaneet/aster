//! Which `[[polaris.team]]` roles a session pins: a role with `projects`
//! belongs to those directories and everything inside them; a role without
//! `projects` belongs to every session.
use std::path::{Path, PathBuf};

use crate::config::PolarisTeamRoleConfig;

/// The diagnostic shown once when a config role has no `projects`.
pub(crate) fn unscoped_hint(names: &[&str]) -> String {
    format!(
        "[[polaris.team]] {} has no projects and is pinned in every session; add projects = [\"~/path/to/project\"] to keep it to its project",
        names.join(", ")
    )
}

/// A configured project directory, `~` expanded and canonicalised; `None` for
/// a blank entry.
fn project_root(path: &str) -> Option<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    Some(canonical_project(
        &crate::worktree::expand_tilde_absolute_path(path),
    ))
}

/// `path` with its nearest existing ancestor canonicalised, so a directory
/// that does not exist (yet) still compares like one that does.
pub(crate) fn canonical_project(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|ancestor| {
            let base = std::fs::canonicalize(ancestor).ok()?;
            let rest = path.strip_prefix(ancestor).ok()?;
            Some(base.join(rest))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// Whether `config` applies to a session whose canonical project directory is
/// `project`: it has no `projects`, or `project` is one of them or inside one.
pub(crate) fn applies(config: &PolarisTeamRoleConfig, project: &Path) -> bool {
    config.projects.is_empty()
        || config
            .projects
            .iter()
            .filter_map(|root| project_root(root))
            .any(|root| project.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(projects: &[&str]) -> PolarisTeamRoleConfig {
        PolarisTeamRoleConfig {
            name: "core".into(),
            projects: projects.iter().map(|path| (*path).to_owned()).collect(),
            ..PolarisTeamRoleConfig::default()
        }
    }

    #[test]
    fn a_role_without_projects_applies_everywhere() {
        assert!(applies(&role(&[]), Path::new("/anywhere")));
    }

    #[test]
    fn a_role_applies_to_its_project_and_directories_inside_it() {
        let config = role(&["/work/aster", "/work/other"]);
        assert!(applies(&config, Path::new("/work/aster")));
        assert!(applies(&config, Path::new("/work/aster/crates/core")));
        assert!(applies(&config, Path::new("/work/other")));
        assert!(
            !applies(&config, Path::new("/work/asterix")),
            "components, not prefixes"
        );
        assert!(!applies(&config, Path::new("/work")));
        assert!(
            !applies(&role(&[" "]), Path::new("/work")),
            "a blank entry matches nothing"
        );
    }

    #[test]
    fn a_tilde_project_is_expanded_against_home() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let home = canonical_project(&home);
        let dir = format!("aster-orbit-projects-{}", std::process::id());
        let config = role(&[&format!("~/{dir}")]);
        assert!(applies(&config, &home.join(&dir).join("src")));
        assert!(!applies(&config, &home));
        assert!(applies(&role(&["~"]), &home));
    }
}
