//! Client-side presentation animations for the shell chrome.
//!
//! All animation state is TUI presentation state: it lives in the client, is
//! advanced from snapshots and the client timer, and is only read during
//! render. [`ClientAnimationClock`] schedules ticks only while something is
//! actually animating; `ui.animations = "off"` never schedules a tick.

mod clock;
mod notification;
mod phase;
mod shell_state;

use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::config::{StatusIndicatorStyle, UiAnimationsConfig};

use super::ClientEndpointId;

pub(crate) use clock::{AgentRowAnimationPhase, ClientAnimationClock};
pub(crate) use notification::{
    notification_is_animating, notification_transition_phase, NotificationTransitionPhase,
};
pub(crate) use phase::blend_color;

/// Animation tick interval (12.5 Hz, within the 10 to 15 Hz budget).
pub(crate) const ANIMATION_TICK_INTERVAL: Duration = Duration::from_millis(80);
/// Duration of the short flash when an agent's status changes.
pub(crate) const AGENT_FLASH_DURATION: Duration = Duration::from_millis(320);
/// Duration of the decaying highlight on the most recently changed agent.
pub(crate) const AGENT_RECENCY_DECAY_DURATION: Duration = Duration::from_millis(2000);
/// Period of the slow pulse for blocked agents.
pub(crate) const BLOCKED_PULSE_PERIOD: Duration = Duration::from_millis(1600);
/// Duration of the highlight sweep on a tab whose agent reported or became blocked.
pub(crate) const TAB_SWEEP_DURATION: Duration = Duration::from_millis(640);
/// Duration of the notification slide-in transition.
pub(crate) const NOTIFICATION_SLIDE_IN_DURATION: Duration = Duration::from_millis(240);
/// Duration of the notification fade-out transition before expiry.
pub(crate) const NOTIFICATION_FADE_OUT_DURATION: Duration = Duration::from_millis(320);

/// Read-only view of the animation clock for one composed frame.
///
/// Render receives this by value so every row samples the same instant and
/// mode without reading the wall clock or mutating animation state.
#[derive(Clone, Copy)]
pub(crate) struct AnimationFrame<'a> {
    clock: &'a ClientAnimationClock,
    mode: UiAnimationsConfig,
    now: Instant,
}

impl<'a> AnimationFrame<'a> {
    pub(crate) fn new(
        clock: &'a ClientAnimationClock,
        mode: UiAnimationsConfig,
        now: Instant,
    ) -> Self {
        Self { clock, mode, now }
    }

    pub(crate) fn mode(self) -> UiAnimationsConfig {
        self.mode
    }

    pub(crate) fn agent_row(
        self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        status: AgentStatus,
    ) -> AgentRowAnimationPhase {
        self.clock
            .agent_row_phase(endpoint_id, pane_id, status, self.mode, self.now)
    }

    pub(crate) fn tab_sweep(
        self,
        endpoint_id: &ClientEndpointId,
        tab_id: &str,
    ) -> Option<(f32, AgentStatus)> {
        self.clock
            .tab_sweep_phase(endpoint_id, tab_id, self.mode, self.now)
    }

    pub(crate) fn spinner_index(self) -> Option<u8> {
        self.clock.spinner_index(self.mode, self.now)
    }
}

/// The working glyph for `spinner_index`, or the static status icon when the
/// spinner is not cycling.
pub(crate) fn working_spinner_glyph(
    spinner_index: Option<u8>,
    indicator_style: StatusIndicatorStyle,
) -> &'static str {
    crate::ui::working_spinner_glyph(
        spinner_index,
        super::status_icon(AgentStatus::Working, indicator_style),
    )
}

#[cfg(test)]
mod tests;
