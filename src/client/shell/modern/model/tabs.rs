//! Per-tab activity for the modern tab bar, built once per snapshot: how many
//! agents in the tab work or need you, and one chip per role that has a pane in
//! it. The tab bar only reads this; it never scans the snapshot's agents.

use std::collections::HashMap;

use super::team::pane_state;
use crate::client::shell::team_sidebar::TeamSidebar;
use crate::protocol::ClientShellSnapshot;
use crate::ui::chrome::state::StateKind;

/// Role chips shown per tab.
pub(in crate::client::shell) const MAX_CHIPS: usize = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct TabChip {
    /// Role index, which picks the role tint.
    pub(in crate::client::shell) tint: u8,
    /// Filled while a member of the role in this tab is working.
    pub(in crate::client::shell) working: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct TabActivity {
    pub(in crate::client::shell) working: u16,
    /// Agents blocked or waiting on an approval.
    pub(in crate::client::shell) attention: u16,
    /// At least one of the attention agents waits on an approval.
    pub(in crate::client::shell) approval: bool,
    pub(in crate::client::shell) chips: [TabChip; MAX_CHIPS],
    pub(in crate::client::shell) chip_len: u8,
}

fn digits(value: u16) -> u16 {
    match value {
        0..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        1000..=9999 => 4,
        _ => 5,
    }
}

impl TabActivity {
    /// Cells the tab's suffix needs, with the gap that separates it from the
    /// label: chips, then ` ◐N` and ` ▲N` or ` ⚑N`. Every glyph is one cell
    /// wide in both glyph sets, so the width does not depend on them. Zero when
    /// there is nothing to show.
    pub(in crate::client::shell) fn suffix_width(&self) -> u16 {
        let mut width = u16::from(self.chip_len);
        if self.working > 0 {
            width += 2 + digits(self.working);
        }
        if self.attention > 0 {
            width += 2 + digits(self.attention);
        }
        if width == 0 {
            0
        } else {
            width + 1
        }
    }
}

/// What the modern tab bar reads: per-tab activity and the chip totals.
pub(in crate::client::shell) struct TabsView<'a> {
    pub(in crate::client::shell) activity: &'a HashMap<String, TabActivity>,
    pub(in crate::client::shell) summary: AttentionSummary,
}

/// Totals over every agent of the snapshot, for the chip at the tab bar's
/// right edge: `▲1 ⚑1 ◐3`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct AttentionSummary {
    pub(in crate::client::shell) approval: u16,
    pub(in crate::client::shell) blocked: u16,
    pub(in crate::client::shell) working: u16,
}

impl AttentionSummary {
    /// Cells the chip needs (single-cell glyphs in both glyph sets, one space
    /// between parts); zero when there is nothing to show.
    pub(in crate::client::shell) fn width(&self) -> u16 {
        let parts = [self.blocked, self.approval, self.working];
        let shown = parts.iter().filter(|count| **count > 0).count() as u16;
        let digits: u16 = parts
            .iter()
            .filter(|count| **count > 0)
            .map(|count| digits(*count))
            .sum();
        if shown == 0 {
            0
        } else {
            shown + digits + (shown - 1)
        }
    }
}

pub(in crate::client::shell) fn summarize(
    snapshot: &ClientShellSnapshot,
    team: Option<&TeamSidebar>,
) -> AttentionSummary {
    let attention = team.map(|team| &team.modern.attention);
    let mut summary = AttentionSummary::default();
    for agent in &snapshot.agents {
        let state = team
            .and_then(|team| team.modern.headers.get(&agent.pane_id))
            .map_or_else(
                || pane_state(attention, &agent.pane_id, agent.agent_status),
                |header| header.state,
            );
        match state {
            StateKind::Approval => summary.approval = summary.approval.saturating_add(1),
            StateKind::Blocked => summary.blocked = summary.blocked.saturating_add(1),
            StateKind::Working => summary.working = summary.working.saturating_add(1),
            _ => {}
        }
    }
    summary
}

/// Activity by tab id, for tabs that have an agent.
pub(in crate::client::shell) fn build(
    snapshot: &ClientShellSnapshot,
    team: Option<&TeamSidebar>,
) -> HashMap<String, TabActivity> {
    let mut tabs = HashMap::<String, TabActivity>::new();
    let attention = team.map(|team| &team.modern.attention);
    for agent in &snapshot.agents {
        let header = team.and_then(|team| team.modern.headers.get(&agent.pane_id));
        let state = header.map_or_else(
            || pane_state(attention, &agent.pane_id, agent.agent_status),
            |header| header.state,
        );
        let activity = tabs.entry(agent.tab_id.clone()).or_default();
        match state {
            StateKind::Working => activity.working = activity.working.saturating_add(1),
            StateKind::Blocked | StateKind::Approval => {
                activity.attention = activity.attention.saturating_add(1);
                activity.approval |= state == StateKind::Approval;
            }
            _ => {}
        }
        let Some(tint) = header.and_then(|header| header.tint) else {
            continue;
        };
        let tint = u8::try_from(tint).unwrap_or(u8::MAX);
        let chips = &mut activity.chips[..usize::from(activity.chip_len)];
        match chips.iter_mut().find(|chip| chip.tint == tint) {
            Some(chip) => chip.working |= state == StateKind::Working,
            None if usize::from(activity.chip_len) < MAX_CHIPS => {
                activity.chips[usize::from(activity.chip_len)] = TabChip {
                    tint,
                    working: state == StateKind::Working,
                };
                activity.chip_len += 1;
            }
            None => {}
        }
    }
    tabs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attention_chip_counts_only_what_it_shows() {
        let mut summary = AttentionSummary::default();
        assert_eq!(summary.width(), 0);
        summary.working = 3;
        assert_eq!(summary.width(), 2);
        summary.blocked = 1;
        summary.approval = 12;
        assert_eq!(summary.width(), 2 + 2 + 3 + 2);
    }

    #[test]
    fn suffix_width_counts_chips_and_counts_with_one_gap() {
        let mut activity = TabActivity::default();
        assert_eq!(activity.suffix_width(), 0);
        activity.chip_len = 2;
        assert_eq!(activity.suffix_width(), 3);
        activity.working = 12;
        assert_eq!(activity.suffix_width(), 2 + 2 + 2 + 1);
        activity.attention = 1;
        assert_eq!(activity.suffix_width(), 2 + 4 + 3 + 1);
    }
}
