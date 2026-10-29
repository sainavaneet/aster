use std::time::{Duration, Instant};

use super::clock::LiveAgentActivity;
use super::*;
use crate::api::schema::AgentStatus;
use crate::config::UiAnimationsConfig;
use crate::protocol::{
    ClientShellAgent, ClientShellSnapshot, ClientShellTab, ClientShellWorkspace,
};

const IDLE: LiveAgentActivity = LiveAgentActivity {
    working: false,
    blocked: false,
};
const WORKING: LiveAgentActivity = LiveAgentActivity {
    working: true,
    blocked: false,
};
const BLOCKED: LiveAgentActivity = LiveAgentActivity {
    working: false,
    blocked: true,
};

fn test_snapshot(status: AgentStatus, seq: u64) -> ClientShellSnapshot {
    ClientShellSnapshot {
        boot_id: "boot-1".into(),
        revision: seq,
        config_diagnostic: None,
        product_announcement: None,
        update_available: None,
        update_install_command: "aster update".into(),
        server_keybindings_toml: None,
        latest_release_notes_available: false,
        integration_updates_available: false,
        worktree_directory: "/tmp/aster-worktrees".into(),
        release_notes: None,
        focused_workspace_id: Some("ws_1".into()),
        focused_tab_id: Some("tab_1".into()),
        focused_pane_id: Some("pane_1".into()),
        tab_bar_right: Vec::new(),
        tab_bar_right_separator: " ".into(),
        agent_view_label: None,
        agent_order: Vec::new(),
        workspaces: vec![ClientShellWorkspace {
            workspace_id: "ws_1".into(),
            active_tab_id: "tab_1".into(),
            new_workspace_cwd: "/repo".into(),
            number: 1,
            label: "alpha".into(),
            custom_label: false,
            branch: None,
            git_ahead_behind: None,
            tokens: Vec::new(),
            worktree: None,
            focused: true,
            agent_status: status,
        }],
        tabs: vec![ClientShellTab {
            tab_id: "tab_1".into(),
            workspace_id: "ws_1".into(),
            number: 1,
            label: "tab 1".into(),
            custom_label: false,
            zoomed: false,
            focused: true,
            agent_status: status,
        }],
        panes: Vec::new(),
        agents: vec![ClientShellAgent {
            pane_id: "pane_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some("agent".into()),
            display_agent: None,
            agent: Some("pi".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: seq,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: true,
        }],
        commands: Vec::new(),
        orbit: None,
    }
}

fn seeded_clock(status: AgentStatus, start: Instant) -> ClientAnimationClock {
    let mut clock = ClientAnimationClock::new(start);
    clock.observe_snapshot(
        &ClientEndpointId::Local,
        Some(1),
        &test_snapshot(status, 1),
        start,
    );
    clock
}

fn local_phase(
    clock: &ClientAnimationClock,
    status: AgentStatus,
    mode: UiAnimationsConfig,
    now: Instant,
) -> AgentRowAnimationPhase {
    clock.agent_row_phase(&ClientEndpointId::Local, "pane_1", status, mode, now)
}

#[test]
fn clock_never_schedules_when_idle_or_disabled() {
    let now = Instant::now();
    let mut clock = seeded_clock(AgentStatus::Idle, now);

    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Full, IDLE, false, now),
        None
    );
    assert!(!clock.tick(UiAnimationsConfig::Full, IDLE, false, now));

    let everything = LiveAgentActivity {
        working: true,
        blocked: true,
    };
    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Off, everything, true, now),
        None
    );
    assert!(!clock.tick(UiAnimationsConfig::Off, everything, true, now));
}

#[test]
fn clock_ticks_at_tick_interval_only_while_looping_motion_is_allowed() {
    let now = Instant::now();
    let mut clock = ClientAnimationClock::new(now);

    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Full, WORKING, false, now),
        Some(ANIMATION_TICK_INTERVAL)
    );
    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Full, BLOCKED, false, now),
        Some(ANIMATION_TICK_INTERVAL)
    );
    // Reduced mode has no looping motion, so long-lived states never keep it ticking.
    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Reduced, WORKING, false, now),
        None
    );
    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Reduced, BLOCKED, false, now),
        None
    );
    // A transient fade still ticks in reduced mode.
    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Reduced, IDLE, true, now),
        Some(ANIMATION_TICK_INTERVAL)
    );

    let half = ANIMATION_TICK_INTERVAL / 2;
    assert!(clock.tick(UiAnimationsConfig::Full, WORKING, false, now));
    assert!(!clock.tick(UiAnimationsConfig::Full, WORKING, false, now + half));
    assert_eq!(
        clock.timer_delay(UiAnimationsConfig::Full, WORKING, false, now + half),
        Some(ANIMATION_TICK_INTERVAL - half)
    );
    assert!(clock.tick(
        UiAnimationsConfig::Full,
        WORKING,
        false,
        now + ANIMATION_TICK_INTERVAL
    ));
}

#[test]
fn status_change_triggers_flash_recency_decay_and_tab_sweep() {
    let start = Instant::now();
    let mut clock = seeded_clock(AgentStatus::Working, start);
    let full = UiAnimationsConfig::Full;

    let initial = local_phase(&clock, AgentStatus::Working, full, start);
    assert_eq!(initial.status_flash, 0.0);
    assert_eq!(initial.recency_highlight, 0.0);
    assert!(clock
        .tab_sweep_phase(&ClientEndpointId::Local, "tab_1", full, start)
        .is_none());

    let change_at = start + Duration::from_millis(100);
    clock.observe_snapshot(
        &ClientEndpointId::Local,
        Some(1),
        &test_snapshot(AgentStatus::Blocked, 2),
        change_at,
    );
    let at_change = local_phase(&clock, AgentStatus::Blocked, full, change_at);
    assert!((at_change.status_flash - 1.0).abs() < 1e-4);
    assert!((at_change.recency_highlight - 1.0).abs() < 1e-4);

    let mid_sweep = change_at + TAB_SWEEP_DURATION / 2;
    let (sweep_phase, sweep_status) = clock
        .tab_sweep_phase(&ClientEndpointId::Local, "tab_1", full, mid_sweep)
        .expect("active tab sweep");
    assert_eq!(sweep_status, AgentStatus::Blocked);
    assert!((sweep_phase - 0.5).abs() < 0.05);
    assert!(clock
        .tab_sweep_phase(
            &ClientEndpointId::Local,
            "tab_1",
            UiAnimationsConfig::Reduced,
            mid_sweep
        )
        .is_none());

    let mid_decay = local_phase(
        &clock,
        AgentStatus::Blocked,
        full,
        change_at + AGENT_RECENCY_DECAY_DURATION / 2,
    );
    assert_eq!(mid_decay.status_flash, 0.0);
    assert!((mid_decay.recency_highlight - 0.5).abs() < 0.05);

    let after = local_phase(
        &clock,
        AgentStatus::Idle,
        full,
        change_at + AGENT_RECENCY_DECAY_DURATION,
    );
    assert_eq!(after.recency_highlight, 0.0);
}

#[test]
fn new_generation_and_cleared_endpoint_rebaseline_without_flashing() {
    let start = Instant::now();
    let mut clock = seeded_clock(AgentStatus::Working, start);
    let reconnect_at = start + Duration::from_millis(50);

    // A reconnect (new generation) whose first snapshot differs is a baseline.
    clock.observe_snapshot(
        &ClientEndpointId::Local,
        Some(2),
        &test_snapshot(AgentStatus::Done, 7),
        reconnect_at,
    );
    let phase = local_phase(
        &clock,
        AgentStatus::Done,
        UiAnimationsConfig::Full,
        reconnect_at,
    );
    assert_eq!(phase.status_flash, 0.0);
    assert_eq!(phase.recency_highlight, 0.0);

    clock.clear_endpoint(&ClientEndpointId::Local);
    clock.observe_snapshot(
        &ClientEndpointId::Local,
        Some(2),
        &test_snapshot(AgentStatus::Blocked, 8),
        reconnect_at,
    );
    let phase = local_phase(
        &clock,
        AgentStatus::Blocked,
        UiAnimationsConfig::Full,
        reconnect_at,
    );
    assert_eq!(phase.status_flash, 0.0);
    assert!(clock
        .tab_sweep_phase(
            &ClientEndpointId::Local,
            "tab_1",
            UiAnimationsConfig::Full,
            reconnect_at
        )
        .is_none());
}

#[test]
fn reduced_mode_keeps_one_shot_flashes_but_no_spinner_or_pulse() {
    let start = Instant::now();
    let mut clock = seeded_clock(AgentStatus::Working, start);
    clock.observe_snapshot(
        &ClientEndpointId::Local,
        Some(1),
        &test_snapshot(AgentStatus::Blocked, 2),
        start,
    );
    let pulse_peak = start + BLOCKED_PULSE_PERIOD / 2;
    let reduced = local_phase(
        &clock,
        AgentStatus::Blocked,
        UiAnimationsConfig::Reduced,
        start,
    );
    assert!(reduced.status_flash > 0.0);
    assert_eq!(reduced.spinner_index, None);
    assert_eq!(
        local_phase(
            &clock,
            AgentStatus::Blocked,
            UiAnimationsConfig::Reduced,
            pulse_peak
        )
        .blocked_pulse,
        0.0
    );
    assert!(
        local_phase(
            &clock,
            AgentStatus::Blocked,
            UiAnimationsConfig::Full,
            pulse_peak
        )
        .blocked_pulse
            > 0.9
    );
    assert_eq!(
        local_phase(&clock, AgentStatus::Blocked, UiAnimationsConfig::Off, start),
        AgentRowAnimationPhase::default()
    );
}

#[test]
fn notification_slide_and_fade_phases_follow_mode() {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(4);
    let entering = start + NOTIFICATION_SLIDE_IN_DURATION / 4;

    let enter = notification_transition_phase(start, deadline, entering, UiAnimationsConfig::Full);
    assert!(enter.slide_offset_cols > 0);
    assert!(enter.opacity > 0.0 && enter.opacity < 1.0);

    let reduced =
        notification_transition_phase(start, deadline, entering, UiAnimationsConfig::Reduced);
    assert_eq!(reduced.slide_offset_cols, 0);
    assert!(reduced.opacity > 0.0 && reduced.opacity < 1.0);

    let exiting = deadline - NOTIFICATION_FADE_OUT_DURATION / 4;
    let exit = notification_transition_phase(start, deadline, exiting, UiAnimationsConfig::Full);
    assert_eq!(exit.slide_offset_cols, 0);
    assert!(exit.opacity > 0.0 && exit.opacity < 1.0);

    let settled = start + Duration::from_secs(2);
    assert_eq!(
        notification_transition_phase(start, deadline, settled, UiAnimationsConfig::Full),
        NotificationTransitionPhase::SETTLED
    );
    assert!(!notification_is_animating(
        start,
        deadline,
        settled,
        UiAnimationsConfig::Full
    ));
    assert_eq!(
        notification_transition_phase(start, deadline, entering, UiAnimationsConfig::Off),
        NotificationTransitionPhase::SETTLED
    );
    assert!(!notification_is_animating(
        start,
        deadline,
        entering,
        UiAnimationsConfig::Off
    ));
}

#[test]
fn blend_color_interpolates_rgb_and_snaps_non_rgb_at_midpoint() {
    let palette = crate::app::state::Palette::catppuccin();
    let from = ratatui::style::Color::Rgb(0, 0, 0);
    let to = ratatui::style::Color::Rgb(200, 100, 50);
    assert_eq!(
        blend_color(from, to, 0.5, &palette),
        ratatui::style::Color::Rgb(100, 50, 25)
    );
    assert_eq!(blend_color(from, to, 0.0, &palette), from);
    assert_eq!(blend_color(from, to, 1.5, &palette), to);
    let named = ratatui::style::Color::Red;
    assert_eq!(blend_color(named, to, 0.4, &palette), named);
    assert_eq!(blend_color(named, to, 0.6, &palette), to);
}
