//! Toast slide-in and fade-out phases.
use std::time::Instant;

use crate::config::UiAnimationsConfig;

use super::phase::linear_progress_phase;
use super::{NOTIFICATION_FADE_OUT_DURATION, NOTIFICATION_SLIDE_IN_DURATION};

/// Maximum slide distance, in cells, of an entering toast.
const SLIDE_IN_MAX_COLS: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NotificationTransitionPhase {
    /// Horizontal slide offset in cells (0 when settled or not in full mode).
    pub(crate) slide_offset_cols: u16,
    /// Contrast factor in `[0.0, 1.0]`; `1.0` is full contrast.
    pub(crate) opacity: f32,
}

impl NotificationTransitionPhase {
    pub(crate) const SETTLED: Self = Self {
        slide_offset_cols: 0,
        opacity: 1.0,
    };
}

pub(crate) fn notification_transition_phase(
    shown_at: Instant,
    deadline: Instant,
    now: Instant,
    mode: UiAnimationsConfig,
) -> NotificationTransitionPhase {
    if !mode.is_enabled() {
        return NotificationTransitionPhase::SETTLED;
    }
    let slide_in = linear_progress_phase(
        now.saturating_duration_since(shown_at),
        NOTIFICATION_SLIDE_IN_DURATION,
    );
    let fade_out = linear_progress_phase(
        deadline.saturating_duration_since(now),
        NOTIFICATION_FADE_OUT_DURATION,
    );
    let slide_offset_cols = if mode.allows_looping_motion() && slide_in < 1.0 {
        ((1.0 - slide_in) * SLIDE_IN_MAX_COLS).ceil() as u16
    } else {
        0
    };
    NotificationTransitionPhase {
        slide_offset_cols,
        opacity: slide_in.min(fade_out),
    }
}

pub(crate) fn notification_is_animating(
    shown_at: Instant,
    deadline: Instant,
    now: Instant,
    mode: UiAnimationsConfig,
) -> bool {
    mode.is_enabled()
        && (now.saturating_duration_since(shown_at) < NOTIFICATION_SLIDE_IN_DURATION
            || deadline.saturating_duration_since(now) <= NOTIFICATION_FADE_OUT_DURATION)
}
