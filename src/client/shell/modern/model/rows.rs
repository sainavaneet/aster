//! The modern team panel's row list: the work section and each row's height.
//! Built by `TeamSidebar::refresh_rows` when its key changes (snapshot, width,
//! section height, collapsed groups and lanes, style, glyphs, done-lane
//! expiry); the draw path only reads the result.

use super::super::super::agent_sidebar::{row_height, AgentRow};
use super::super::super::team_rows::{TeamLineRef, TeamRowsInput};
use super::super::super::team_sidebar::TeamSidebar;
use super::super::super::work_board::RECENT_VISIBLE_MS;
use crate::ui::chrome::state::Lane;

/// Sidebar content width below which cards drop their detail rows.
const NARROW_BELOW: u16 = 22;
/// Content width from which member rows show the open task and cards the next step.
const WIDE_FROM: u16 = 32;
/// Section height from which blank rows separate TEAM, WORK and the lanes.
const GAP_MIN_HEIGHT: u16 = 30;
/// Rows the agent panel header takes above the list.
const PANEL_HEADER_ROWS: u16 = 3;

/// Density tier of the sidebar, from its content width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum Tier {
    Narrow,
    Normal,
    Wide,
}

impl Tier {
    /// `sidebar_width` includes the separator column.
    pub(in crate::client::shell) fn of(sidebar_width: u16) -> Self {
        match sidebar_width.saturating_sub(1) {
            0..NARROW_BELOW => Self::Narrow,
            NARROW_BELOW..WIDE_FROM => Self::Normal,
            _ => Self::Wide,
        }
    }
}

pub(in crate::client::shell) struct Built {
    pub(in crate::client::shell) visible_done: usize,
    pub(in crate::client::shell) expires_at_ms: u64,
    pub(in crate::client::shell) heights: Vec<u16>,
    pub(in crate::client::shell) tier: Tier,
}

fn height_of(team: &TeamSidebar, others: &[AgentRow], tier: Tier, line: &TeamLineRef) -> u16 {
    match *line {
        TeamLineRef::Member { group, member } => {
            let titled = team.modern.roles[group].members[member]
                .task_title
                .is_some();
            1 + u16::from(titled && tier == Tier::Wide)
        }
        TeamLineRef::Agent(index) => row_height(others[index].rows.len()),
        TeamLineRef::LaneTask { lane, index } => {
            let card = &team.modern.lanes.tasks(lane)[index];
            match tier {
                Tier::Narrow => 1,
                _ => 2 + u16::from(card.strip.is_some()),
            }
        }
        _ => 1,
    }
}

/// Appends the work section to `lines` and returns the heights of all lines.
pub(in crate::client::shell) fn finish(
    team: &TeamSidebar,
    lines: &mut Vec<TeamLineRef>,
    others: &[AgentRow],
    input: &TeamRowsInput<'_>,
) -> Built {
    let tier = Tier::of(input.sidebar_width);
    let mut expires_at_ms = u64::MAX;
    let mut done_count = 0;
    if input.config.sidebar_work {
        let lanes = &team.modern.lanes;
        let visible_done = lanes
            .done
            .iter()
            .enumerate()
            .filter(|(_, card)| {
                input.now_ms < card.finished_at_ms.saturating_add(RECENT_VISIBLE_MS)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        done_count = visible_done.len();
        expires_at_ms = lanes
            .done
            .iter()
            .map(|card| card.finished_at_ms.saturating_add(RECENT_VISIBLE_MS))
            .filter(|hidden_at| input.now_ms < *hidden_at)
            .min()
            .unwrap_or(u64::MAX);
        if lanes.has_active() || !visible_done.is_empty() {
            let spaced = input.section_height >= GAP_MIN_HEIGHT;
            if spaced {
                lines.push(TeamLineRef::Gap);
            }
            lines.push(TeamLineRef::WorkLabel);
            let mut first = true;
            for lane in Lane::ALL {
                let count = if lane == Lane::Done {
                    visible_done.len()
                } else {
                    lanes.len(lane)
                };
                if count == 0 {
                    continue;
                }
                if spaced && !first {
                    lines.push(TeamLineRef::Gap);
                }
                first = false;
                let collapsed = input.collapsed_lanes.contains(lane);
                lines.push(TeamLineRef::LaneHeader {
                    lane,
                    collapsed,
                    hidden: 0,
                });
                if collapsed {
                    continue;
                }
                if lane == Lane::Done {
                    lines.extend(visible_done.iter().copied().map(TeamLineRef::LaneDone));
                } else {
                    lines.extend((0..count).map(|index| TeamLineRef::LaneTask { lane, index }));
                }
            }
        }
    }
    let mut heights = lines
        .iter()
        .map(|line| height_of(team, others, tier, line))
        .collect::<Vec<_>>();
    // A list taller than its section drops resolved tasks first, the least
    // urgent rows; everything else stays reachable by scrolling.
    let body = input.section_height.saturating_sub(PANEL_HEADER_ROWS);
    let total = heights.iter().map(|height| u32::from(*height)).sum::<u32>();
    if input.section_height > 0 && total > u32::from(body) {
        let excess = usize::try_from(total - u32::from(body)).unwrap_or(usize::MAX);
        let done_rows = lines
            .iter()
            .rev()
            .take_while(|line| matches!(line, TeamLineRef::LaneDone(_)))
            .count();
        let removed = done_rows.min(excess);
        if removed > 0 {
            lines.truncate(lines.len() - removed);
            heights.truncate(lines.len());
            let header = lines.len() - (done_rows - removed) - 1;
            if let TeamLineRef::LaneHeader { hidden, .. } = &mut lines[header] {
                *hidden = u16::try_from(removed).unwrap_or(u16::MAX);
            }
        }
    }
    Built {
        visible_done: done_count,
        expires_at_ms,
        heights,
        tier,
    }
}
