//! Compact elapsed-time text and the age bar's buckets.

/// Longest age label: four digits and a unit.
pub(crate) const AGE_BUFFER_LEN: usize = 5;
const MAX_AGE_VALUE: u64 = 9999;

/// `elapsed_ms` as `45s`, `3m`, `2h` or `4d`, written into `buffer`.
pub(crate) fn age_text(elapsed_ms: u64, buffer: &mut [u8; AGE_BUFFER_LEN]) -> &str {
    let seconds = elapsed_ms / 1000;
    let (value, unit) = match seconds {
        0..=59 => (seconds, b's'),
        60..=3599 => (seconds / 60, b'm'),
        3600..=86399 => (seconds / 3600, b'h'),
        _ => (seconds / 86400, b'd'),
    };
    let mut value = value.min(MAX_AGE_VALUE);
    let mut start = buffer.len() - 1;
    buffer[start] = unit;
    loop {
        start -= 1;
        buffer[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    std::str::from_utf8(&buffer[start..]).unwrap_or_default()
}

/// How urgent an elapsed time looks on the age bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AgeTone {
    Fresh,
    Slow,
    Stale,
}

const MINUTE_MS: u64 = 60_000;

/// Filled cells (0 to 3) of the age bar: `<1m`, `<5m`, `<15m`, `<1h`, `>=1h`
/// map to 0, 1, 2, 3 (the last two buckets both fill the bar).
pub(crate) const fn age_bar_cells(elapsed_ms: u64) -> u8 {
    match elapsed_ms / MINUTE_MS {
        0 => 0,
        1..=4 => 1,
        5..=14 => 2,
        _ => 3,
    }
}

/// The tone of an age: gentle color steps at 15 minutes and 1 hour.
pub(crate) const fn age_tone(elapsed_ms: u64) -> AgeTone {
    match elapsed_ms / MINUTE_MS {
        0..=14 => AgeTone::Fresh,
        15..=59 => AgeTone::Slow,
        _ => AgeTone::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(ms: u64) -> String {
        let mut buffer = [0u8; AGE_BUFFER_LEN];
        age_text(ms, &mut buffer).to_owned()
    }

    #[test]
    fn age_text_covers_every_unit_and_saturates() {
        assert_eq!(text(0), "0s");
        assert_eq!(text(59_999), "59s");
        assert_eq!(text(60_000), "1m");
        assert_eq!(text(3_599_999), "59m");
        assert_eq!(text(3_600_000), "1h");
        assert_eq!(text(86_400_000), "1d");
        assert_eq!(text(u64::MAX), "9999d");
    }

    #[test]
    fn age_bar_buckets_and_tones_follow_the_spec() {
        assert_eq!(age_bar_cells(0), 0);
        assert_eq!(age_bar_cells(MINUTE_MS), 1);
        assert_eq!(age_bar_cells(5 * MINUTE_MS), 2);
        assert_eq!(age_bar_cells(15 * MINUTE_MS), 3);
        assert_eq!(age_tone(14 * MINUTE_MS), AgeTone::Fresh);
        assert_eq!(age_tone(15 * MINUTE_MS), AgeTone::Slow);
        assert_eq!(age_tone(60 * MINUTE_MS), AgeTone::Stale);
    }
}
