//! Background-only membership scan. The unreaped leader pins the group identity
//! throughout the scan, so disappearance cannot cause a signal to a reused PGID.
pub(crate) fn service_group_has_members(leader: u32) -> std::io::Result<bool> {
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == leader {
            continue;
        }
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(2))
            .and_then(|group| group.parse::<u32>().ok())
            == Some(leader)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
