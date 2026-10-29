use std::borrow::Borrow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Opaque identity for a server-owned terminal.
///
/// During the pane-backed transition this is stored one-to-one beside panes,
/// but callers must not derive it from a pane id or layout position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalId(String);

static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

/// Upper bound for an identifier accepted from a persisted session file.
const MAX_PERSISTED_TERMINAL_ID_LEN: usize = 128;

/// Prefix of every identifier produced by [`TerminalId::alloc`].
const ALLOCATED_TERMINAL_ID_PREFIX: &str = "term_";

impl TerminalId {
    pub fn alloc() -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let counter = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
        Self(format!(
            "{ALLOCATED_TERMINAL_ID_PREFIX}{micros:x}{counter:x}"
        ))
    }

    /// Rehydrate an identifier read from a persisted session file.
    ///
    /// Returns `None` unless the value is non-empty, at most
    /// [`MAX_PERSISTED_TERMINAL_ID_LEN`] bytes, and limited to ASCII
    /// alphanumerics, `_`, and `-`. Uniqueness is the caller's responsibility.
    pub(crate) fn from_persisted(value: String) -> Option<Self> {
        let is_valid = !value.is_empty()
            && value.len() <= MAX_PERSISTED_TERMINAL_ID_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        is_valid.then_some(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for TerminalId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn terminal_ids_support_borrowed_lookup_without_changing_identity() {
        let id = TerminalId::alloc();
        let serialized = serde_json::to_string(&id).unwrap();
        let restored: TerminalId = serde_json::from_str(&serialized).unwrap();
        let mut terminals = HashMap::from([(id.clone(), 1)]);

        assert_eq!(terminals.get(restored.as_str()), Some(&1));
        assert_eq!(terminals.get("missing-terminal"), None);
        *terminals.get_mut(restored.as_str()).unwrap() = 2;
        assert_eq!(terminals.remove(id.as_str()), Some(2));
        assert!(terminals.is_empty());
    }

    #[test]
    fn from_persisted_accepts_allocated_ids_and_rejects_malformed_values() {
        let allocated = TerminalId::alloc();
        assert_eq!(
            TerminalId::from_persisted(allocated.to_string()),
            Some(allocated)
        );
        for valid in [
            "a",
            "term_ABC-123",
            &"x".repeat(MAX_PERSISTED_TERMINAL_ID_LEN),
        ] {
            assert!(
                TerminalId::from_persisted(valid.to_string()).is_some(),
                "{valid:?} should be accepted"
            );
        }
        let too_long = "x".repeat(MAX_PERSISTED_TERMINAL_ID_LEN + 1);
        for invalid in [
            "",
            too_long.as_str(),
            "term 1",
            "term/1",
            "term:1",
            "term.1",
            "term\n1",
            "t\u{e9}rm",
            "term\0",
        ] {
            assert_eq!(
                TerminalId::from_persisted(invalid.to_string()),
                None,
                "{invalid:?} should be rejected"
            );
        }
    }

    /// Restored ids are reused verbatim, so a fresh `alloc()` in a later
    /// process must not reproduce one. This holds because the id begins with
    /// the wall-clock microsecond timestamp in hex: a later process starts
    /// from a strictly later timestamp of the same hex width (until 2112), so
    /// the prefix differs. A wall clock that moves backwards is the only
    /// theoretical collision path; restore additionally refuses to hand out an
    /// id that is already claimed in the restored session.
    #[test]
    fn alloc_format_embeds_wall_clock_micros_prefix() {
        let now_micros = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros()
        };
        let before = now_micros();
        let id = TerminalId::alloc();
        let after = now_micros();

        let hex = id
            .as_str()
            .strip_prefix(ALLOCATED_TERMINAL_ID_PREFIX)
            .expect("allocated ids keep the term_ prefix");
        assert!(hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        let width = format!("{after:x}").len();
        assert_eq!(width, format!("{before:x}").len());
        let micros = u128::from_str_radix(&hex[..width], 16).unwrap();
        assert!(
            (before..=after).contains(&micros),
            "allocated id {id} must start with the allocation timestamp"
        );
        assert!(TerminalId::from_persisted(id.to_string()).is_some());
    }
}
