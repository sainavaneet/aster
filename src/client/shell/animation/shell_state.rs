//! Animation hooks on [`ClientShellState`]: clock ticks and timer scheduling.
use std::time::{Duration, Instant};

use super::super::ClientShellState;
use super::clock::LiveAgentActivity;
use super::notification_is_animating;

impl ClientShellState {
    fn live_agent_activity(&self) -> LiveAgentActivity {
        let local = self
            .snapshot
            .as_deref()
            .map_or(LiveAgentActivity::default(), |snapshot| {
                LiveAgentActivity::default().include(snapshot)
            });
        self.endpoints
            .iter()
            .filter_map(|endpoint| endpoint.snapshot.as_deref())
            .fold(local, LiveAgentActivity::include)
    }

    fn notification_is_animating(&self, now: Instant) -> bool {
        let mode = self.config.animations;
        let notice = self.visible_endpoint_notice.as_ref().is_some_and(|notice| {
            notification_is_animating(notice.shown_at, notice.deadline, now, mode)
        });
        notice
            || self
                .visible_notification
                .as_ref()
                .is_some_and(|notification| {
                    notification_is_animating(
                        notification.shown_at,
                        notification.deadline,
                        now,
                        mode,
                    )
                })
    }

    /// Advances the animation clock; returns whether the shell should repaint.
    pub(crate) fn tick_animations(&mut self, now: Instant) -> bool {
        let mode = self.config.animations;
        if !mode.is_enabled() {
            return false;
        }
        let activity = self.live_agent_activity();
        let notification_animating = self.notification_is_animating(now);
        self.animations
            .tick(mode, activity, notification_animating, now)
    }

    /// Delay until the next animation frame, or `None` while nothing animates.
    pub(crate) fn animation_timer_delay(&self, now: Instant) -> Option<Duration> {
        let mode = self.config.animations;
        if !mode.is_enabled() {
            return None;
        }
        self.animations.timer_delay(
            mode,
            self.live_agent_activity(),
            self.notification_is_animating(now),
            now,
        )
    }

    #[cfg(test)]
    pub(crate) fn has_active_animations(&self, now: Instant) -> bool {
        self.animation_timer_delay(now).is_some()
    }
}
