//! `orbit-team.json` in the session data directory:
//! `{ "version": 1, "roles": [...] }`, written privately (mode 0600) to a
//! temporary file, fsynced, and renamed over the roster so a crash never leaves
//! a partial file; the server writes it through `OrbitWriter`, off the event
//! loop. Unknown fields from a newer build are kept and written back.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::roster::{Role, Roster};

pub(crate) const FILE_NAME: &str = "orbit-team.json";
const VERSION: u32 = 1;

#[derive(Serialize)]
struct FileRef<'a> {
    version: u32,
    roles: &'a [Role],
    #[serde(flatten)]
    extra: &'a Map<String, Value>,
}

#[derive(Deserialize)]
struct File {
    version: u32,
    roles: Vec<Role>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

pub(crate) fn path() -> PathBuf {
    crate::session::data_dir().join(FILE_NAME)
}

/// `Ok(None)` when no roster exists. An unreadable or invalid roster is an
/// error; the caller must not overwrite it.
pub(crate) fn load(path: &Path) -> Result<Option<Roster>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Cannot read Orbit team roster at {}: {error}",
                path.display()
            ))
        }
    };
    let file: File = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid Orbit team roster at {}: {error}", path.display()))?;
    if file.version != VERSION {
        return Err(format!(
            "Invalid Orbit team roster at {}: unsupported version {}",
            path.display(),
            file.version
        ));
    }
    let roster = Roster {
        roles: file.roles,
        extra: file.extra,
    };
    roster
        .check_loaded()
        .map_err(|error| format!("Invalid Orbit team roster at {}: {error}", path.display()))?;
    Ok(Some(roster))
}

/// The roster file contents.
pub(crate) fn serialize(roster: &Roster) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&FileRef {
        version: VERSION,
        roles: &roster.roles,
        extra: &roster.extra,
    })
    .map_err(std::io::Error::other)
}

/// Writes the roster in place (tests; the server writes through `OrbitWriter`).
#[cfg(test)]
pub(crate) fn save(path: &Path, roster: &Roster) -> std::io::Result<()> {
    crate::app::orbit_files::replace_atomically(path, &serialize(roster)?)
}
