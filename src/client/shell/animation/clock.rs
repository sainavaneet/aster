//! Per-agent and per-tab animation state derived from shell snapshots.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::config::UiAnimationsConfig;
use crate::protocol::ClientShellSnapshot;

use super::phase::{linear_decay_phase, linear_progress_phase, triangle_wave_phase};
use super::{
    ClientEndpointId, AGENT_FLASH_DURATION, AGENT_RECENCY_DECAY_DURATION, ANIMATION_TICK_INTERVAL,
    BLOCKED_PULSE_PERIOD, TAB_SWEEP_DURATION,
};

const WORKING_SPINNER_FRAMES: u128 = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AgentRowAnimationPhase {
    pub(crate) spinner_index: Option<u8>,
    pub(crate) blocked_pulse: f32,
    pub(crate) status_flash: f32,
    pub(crate) recency_highlight: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TrackedAgentState {
    status: AgentStatus,
    state_change_seq: u64,
    flash_started_at: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq)]
struct RecentChangedAgent {
    endpoint_id: ClientEndpointId,
    pane_id: String,
    changed_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TabSweepEntry {
    started_at: Instant,
    status: AgentStatus,
}

/// Whether a snapshot's agents are working or blocked; drives looping effects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveAgentActivity {
    pub(crate) working: bool,
    pub(crate) blocked: bool,
}

/// Maps are keyed by endpoint first so render can look rows up by `&str`
/// without building an owned key per row.
#[derive(Clone, Debug)]
pub(crate) struct ClientAnimationClock {
    origin: Instant,
    last_tick_at: Option<Instant>,
    tracked_agents: HashMap<ClientEndpointId, HashMap<String, TrackedAgentState>>,
    seeded_endpoints: HashMap<ClientEndpointId, Option<u64>>,
    recent_changed_agent: Option<RecentChangedAgent>,
    tab_sweeps: HashMap<ClientEndpointId, HashMap<String, TabSweepEntry>>,
}

impl ClientAnimationClock {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            origin: now,
            last_tick_at: None,
            tracked_agents: HashMap::new(),
            seeded_endpoints: HashMap::new(),
            recent_changed_agent: None,
            tab_sweeps: HashMap::new(),
        }
    }

    pub(crate) fn clear_endpoint(&mut self, endpoint_id: &ClientEndpointId) {
        self.seeded_endpoints.remove(endpoint_id);
        self.tracked_agents.remove(endpoint_id);
        self.tab_sweeps.remove(endpoint_id);
        if self
            .recent_changed_agent
            .as_ref()
            .is_some_and(|recent| &recent.endpoint_id == endpoint_id)
        {
            self.recent_changed_agent = None;
        }
    }

    /// Records status transitions in `snapshot`. The first snapshot of an
    /// endpoint generation is a baseline: reconnecting or restoring never
    /// flashes rows that merely became visible again.
    pub(crate) fn observe_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
        snapshot: &ClientShellSnapshot,
        now: Instant,
    ) {
        let is_initial_seed = self
            .seeded_endpoints
            .insert(endpoint_id.clone(), generation)
            != Some(generation);
        let live_panes: HashSet<&str> = snapshot
            .agents
            .iter()
            .map(|agent| agent.pane_id.as_str())
            .collect();
        let live_tabs: HashSet<&str> = snapshot
            .tabs
            .iter()
            .map(|tab| tab.tab_id.as_str())
            .collect();
        let agents = self.tracked_agents.entry(endpoint_id.clone()).or_default();
        agents.retain(|pane_id, _| live_panes.contains(pane_id.as_str()));
        let sweeps = self.tab_sweeps.entry(endpoint_id.clone()).or_default();
        sweeps.retain(|tab_id, _| live_tabs.contains(tab_id.as_str()));
        if self.recent_changed_agent.as_ref().is_some_and(|recent| {
            &recent.endpoint_id == endpoint_id && !live_panes.contains(recent.pane_id.as_str())
        }) {
            self.recent_changed_agent = None;
        }

        let mut latest_change: Option<(&str, u64)> = None;
        for agent in &snapshot.agents {
            let status = agent.agent_status;
            let status_changed = match agents.get_mut(agent.pane_id.as_str()) {
                Some(previous) => {
                    let status_changed = previous.status != status;
                    if !status_changed && previous.state_change_seq == agent.state_change_seq {
                        continue;
                    }
                    previous.status = status;
                    previous.state_change_seq = agent.state_change_seq;
                    if status_changed && !is_initial_seed {
                        previous.flash_started_at = Some(now);
                    }
                    status_changed
                }
                None => {
                    agents.insert(
                        agent.pane_id.clone(),
                        TrackedAgentState {
                            status,
                            state_change_seq: agent.state_change_seq,
                            flash_started_at: (!is_initial_seed).then_some(now),
                        },
                    );
                    true
                }
            };
            if is_initial_seed {
                continue;
            }
            if status_changed && matches!(status, AgentStatus::Blocked | AgentStatus::Done) {
                sweeps.insert(
                    agent.tab_id.clone(),
                    TabSweepEntry {
                        started_at: now,
                        status,
                    },
                );
            }
            if latest_change.is_none_or(|(_, seq)| agent.state_change_seq >= seq) {
                latest_change = Some((&agent.pane_id, agent.state_change_seq));
            }
        }

        if let Some((pane_id, _)) = latest_change {
            self.recent_changed_agent = Some(RecentChangedAgent {
                endpoint_id: endpoint_id.clone(),
                pane_id: pane_id.to_owned(),
                changed_at: now,
            });
        }
        self.prune_expired(now);
    }

    /// Starts a tab sweep for an agent report delivered as a notification.
    pub(crate) fn record_tab_report(
        &mut self,
        endpoint_id: &ClientEndpointId,
        tab_id: &str,
        status: AgentStatus,
        now: Instant,
    ) {
        self.tab_sweeps
            .entry(endpoint_id.clone())
            .or_default()
            .insert(
                tab_id.to_owned(),
                TabSweepEntry {
                    started_at: now,
                    status,
                },
            );
    }

    fn prune_expired(&mut self, now: Instant) {
        for state in self
            .tracked_agents
            .values_mut()
            .flat_map(HashMap::values_mut)
        {
            if state.flash_started_at.is_some_and(|started| {
                now.saturating_duration_since(started) >= AGENT_FLASH_DURATION
            }) {
                state.flash_started_at = None;
            }
        }
        if self.recent_changed_agent.as_ref().is_some_and(|recent| {
            now.saturating_duration_since(recent.changed_at) >= AGENT_RECENCY_DECAY_DURATION
        }) {
            self.recent_changed_agent = None;
        }
        for sweeps in self.tab_sweeps.values_mut() {
            sweeps.retain(|_, sweep| {
                now.saturating_duration_since(sweep.started_at) < TAB_SWEEP_DURATION
            });
        }
    }

    pub(crate) fn has_active_animations(
        &self,
        mode: UiAnimationsConfig,
        activity: LiveAgentActivity,
        notification_animating: bool,
        now: Instant,
    ) -> bool {
        if !mode.is_enabled() {
            return false;
        }
        let looping = mode.allows_looping_motion();
        let active_flash = self
            .tracked_agents
            .values()
            .flat_map(HashMap::values)
            .any(|state| {
                state.flash_started_at.is_some_and(|started| {
                    now.saturating_duration_since(started) < AGENT_FLASH_DURATION
                })
            });
        let active_recency = self.recent_changed_agent.as_ref().is_some_and(|recent| {
            now.saturating_duration_since(recent.changed_at) < AGENT_RECENCY_DECAY_DURATION
        });
        let active_sweep = looping
            && self
                .tab_sweeps
                .values()
                .flat_map(HashMap::values)
                .any(|sweep| now.saturating_duration_since(sweep.started_at) < TAB_SWEEP_DURATION);
        (looping && (activity.working || activity.blocked))
            || notification_animating
            || active_flash
            || active_recency
            || active_sweep
    }

    /// Delay until the next animation frame, or `None` when nothing animates.
    pub(crate) fn timer_delay(
        &self,
        mode: UiAnimationsConfig,
        activity: LiveAgentActivity,
        notification_animating: bool,
        now: Instant,
    ) -> Option<Duration> {
        if !self.has_active_animations(mode, activity, notification_animating, now) {
            return None;
        }
        Some(self.last_tick_at.map_or(ANIMATION_TICK_INTERVAL, |last| {
            ANIMATION_TICK_INTERVAL.saturating_sub(now.saturating_duration_since(last))
        }))
    }

    /// Advances the clock; returns whether the shell should repaint.
    pub(crate) fn tick(
        &mut self,
        mode: UiAnimationsConfig,
        activity: LiveAgentActivity,
        notification_animating: bool,
        now: Instant,
    ) -> bool {
        let had_transient = self.has_transient_effects();
        self.prune_expired(now);
        if !self.has_active_animations(mode, activity, notification_animating, now) {
            self.last_tick_at = None;
            // One final repaint clears the last frame of an expired effect.
            return had_transient && mode.is_enabled();
        }
        if self
            .last_tick_at
            .is_none_or(|last| now.saturating_duration_since(last) >= ANIMATION_TICK_INTERVAL)
        {
            self.last_tick_at = Some(now);
            return true;
        }
        false
    }

    fn has_transient_effects(&self) -> bool {
        self.recent_changed_agent.is_some()
            || self.tab_sweeps.values().any(|sweeps| !sweeps.is_empty())
            || self
                .tracked_agents
                .values()
                .flat_map(HashMap::values)
                .any(|state| state.flash_started_at.is_some())
    }

    pub(crate) fn spinner_index(&self, mode: UiAnimationsConfig, now: Instant) -> Option<u8> {
        if !mode.allows_looping_motion() {
            return None;
        }
        let elapsed_ms = now.saturating_duration_since(self.origin).as_millis();
        let step_ms = ANIMATION_TICK_INTERVAL.as_millis().max(1);
        Some(((elapsed_ms / step_ms) % WORKING_SPINNER_FRAMES) as u8)
    }

    pub(crate) fn agent_row_phase(
        &self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        status: AgentStatus,
        mode: UiAnimationsConfig,
        now: Instant,
    ) -> AgentRowAnimationPhase {
        if !mode.is_enabled() {
            return AgentRowAnimationPhase::default();
        }
        let spinner_index = (status == AgentStatus::Working)
            .then(|| self.spinner_index(mode, now))
            .flatten();
        let blocked_pulse = if status == AgentStatus::Blocked && mode.allows_looping_motion() {
            triangle_wave_phase(
                now.saturating_duration_since(self.origin),
                BLOCKED_PULSE_PERIOD,
            )
        } else {
            0.0
        };
        let status_flash = self
            .tracked_agents
            .get(endpoint_id)
            .and_then(|agents| agents.get(pane_id))
            .and_then(|state| state.flash_started_at)
            .map_or(0.0, |started| {
                linear_decay_phase(now.saturating_duration_since(started), AGENT_FLASH_DURATION)
            });
        let recency_highlight = self
            .recent_changed_agent
            .as_ref()
            .filter(|recent| &recent.endpoint_id == endpoint_id && recent.pane_id == pane_id)
            .map_or(0.0, |recent| {
                linear_decay_phase(
                    now.saturating_duration_since(recent.changed_at),
                    AGENT_RECENCY_DECAY_DURATION,
                )
            });
        AgentRowAnimationPhase {
            spinner_index,
            blocked_pulse,
            status_flash,
            recency_highlight,
        }
    }

    pub(crate) fn tab_sweep_phase(
        &self,
        endpoint_id: &ClientEndpointId,
        tab_id: &str,
        mode: UiAnimationsConfig,
        now: Instant,
    ) -> Option<(f32, AgentStatus)> {
        if !mode.allows_looping_motion() {
            return None;
        }
        let entry = self.tab_sweeps.get(endpoint_id)?.get(tab_id)?;
        let elapsed = now.saturating_duration_since(entry.started_at);
        (elapsed < TAB_SWEEP_DURATION).then(|| {
            (
                linear_progress_phase(elapsed, TAB_SWEEP_DURATION),
                entry.status,
            )
        })
    }
}

impl LiveAgentActivity {
    /// Folds the working and blocked agents of `snapshot` into `self`.
    pub(crate) fn include(self, snapshot: &ClientShellSnapshot) -> Self {
        let agents = snapshot.agents.iter().map(|agent| agent.agent_status);
        let tabs = snapshot.tabs.iter().map(|tab| tab.agent_status);
        agents.chain(tabs).fold(self, |activity, status| Self {
            working: activity.working || status == AgentStatus::Working,
            blocked: activity.blocked || status == AgentStatus::Blocked,
        })
    }
}
