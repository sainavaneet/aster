//! The team panel's row list, cached on its `TeamSidebar`.
//!
//! The rows (lead, role groups, the `other` group's agent rows, the work board)
//! with their heights, gaps, and click targets are rebuilt only when something
//! they depend on changes: the snapshot, the collapsed groups, the sidebar
//! width, the agent row or sort settings, or the moment a resolved task's
//! ten-minute window ends. `ClientShellState::refresh_team_rows` checks that
//! before the frame is drawn; the render path only borrows the result. Lines
//! are indices into the model, so the cache never copies a name or pane id the
//! model already holds.

use std::collections::HashSet;

use super::agent_sidebar::{agent_row, list_gaps, ordered_agent_pane_ids, row_height, AgentRow};
use super::team_sidebar::{TeamSidebar, OTHER_GROUP_TITLE, WORK_GROUP_KEY};
use super::*;
use crate::api::schema::AgentStatus;

const TASK_ROW_HEIGHT: usize = 2;
const TEAM_ROW_GAP: u16 = 0;

/// One cached line; indices point into the owning `TeamSidebar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TeamLineRef {
    Lead,
    /// A role or `scratch` group header: an index into `TeamSidebar::groups`.
    Group {
        group: usize,
        collapsed: bool,
    },
    Member {
        group: usize,
        member: usize,
    },
    OtherHeader {
        count: usize,
        status: AgentStatus,
        collapsed: bool,
    },
    /// An index into `TeamRows::others`.
    Agent(usize),
    WorkHeader {
        collapsed: bool,
    },
    /// An index into the work board's active tasks.
    Task(usize),
    /// An index into the work board's resolved tasks.
    Recent(usize),
}

/// What the rows depend on, borrowed from the shell state for the check.
pub(super) struct TeamRowsInput<'a> {
    pub(super) snapshot_serial: u64,
    pub(super) snapshot: &'a ClientShellSnapshot,
    pub(super) collapsed: &'a HashSet<String>,
    pub(super) sidebar_width: u16,
    pub(super) config: &'a ClientShellConfig,
    /// The frame's time on the endpoint's clock, in Unix milliseconds.
    pub(super) now_ms: u64,
}

struct TeamRowsKey {
    snapshot_serial: u64,
    collapsed: HashSet<String>,
    sidebar_width: u16,
    sort: crate::config::AgentPanelSortConfig,
    sidebar_work: bool,
    agents: crate::config::AgentsSidebarConfig,
}

impl TeamRowsKey {
    fn matches(&self, input: &TeamRowsInput<'_>) -> bool {
        self.snapshot_serial == input.snapshot_serial
            && self.sidebar_width == input.sidebar_width
            && self.sort == input.config.agent_panel_sort
            && self.sidebar_work == input.config.sidebar_work
            && self.collapsed == *input.collapsed
            && self.agents == input.config.agents
    }
}

#[derive(Default)]
pub(super) struct TeamRows {
    key: Option<TeamRowsKey>,
    /// The first time a visible resolved task leaves the board.
    expires_at_ms: u64,
    pub(super) lines: Vec<TeamLineRef>,
    /// Agent rows of the `other` group; empty while it is collapsed.
    pub(super) others: Vec<AgentRow>,
    pub(super) heights: Vec<u16>,
    pub(super) gaps: Vec<u16>,
    #[cfg(test)]
    builds: u32,
}

impl TeamRows {
    fn is_current(&self, input: &TeamRowsInput<'_>) -> bool {
        input.now_ms < self.expires_at_ms && self.key.as_ref().is_some_and(|key| key.matches(input))
    }
}

impl TeamSidebar {
    /// Rebuilds the cached rows when their inputs changed; a no-op otherwise.
    pub(super) fn refresh_rows(&mut self, input: &TeamRowsInput<'_>) {
        if self.rows.is_current(input) {
            return;
        }
        let mut lines = vec![TeamLineRef::Lead];
        for (group, entry) in self.groups.iter().enumerate() {
            let collapsed = input.collapsed.contains(&entry.key);
            lines.push(TeamLineRef::Group { group, collapsed });
            if !collapsed {
                lines.extend(
                    (0..entry.members.len()).map(|member| TeamLineRef::Member { group, member }),
                );
            }
        }
        let others = self.push_other_lines(&mut lines, input);
        let expires_at_ms = if input.config.sidebar_work {
            self.work.push_line_refs(
                &mut lines,
                input.collapsed.contains(WORK_GROUP_KEY),
                input.now_ms,
            )
        } else {
            u64::MAX
        };
        let heights = lines
            .iter()
            .map(|line| {
                row_height(match line {
                    TeamLineRef::Agent(index) => others[*index].rows.len(),
                    TeamLineRef::Task(_) => TASK_ROW_HEIGHT,
                    _ => 1,
                })
            })
            .collect();
        let gaps = list_gaps(lines.len(), TEAM_ROW_GAP);
        #[cfg(test)]
        let builds = self.rows.builds + 1;
        self.rows = TeamRows {
            key: Some(TeamRowsKey {
                snapshot_serial: input.snapshot_serial,
                collapsed: input.collapsed.clone(),
                sidebar_width: input.sidebar_width,
                sort: input.config.agent_panel_sort,
                sidebar_work: input.config.sidebar_work,
                agents: input.config.agents.clone(),
            }),
            expires_at_ms,
            lines,
            others,
            heights,
            gaps,
            #[cfg(test)]
            builds,
        };
    }

    /// The `other` group: agents outside the team, in sidebar sort order.
    /// Returns its agent rows, kept only while the group is expanded.
    fn push_other_lines(
        &self,
        lines: &mut Vec<TeamLineRef>,
        input: &TeamRowsInput<'_>,
    ) -> Vec<AgentRow> {
        let snapshot = input.snapshot;
        let rows = ordered_agent_pane_ids(snapshot, input.config.agent_panel_sort)
            .into_iter()
            .filter(|pane_id| !self.team_panes.contains(pane_id))
            .filter_map(|pane_id| agent_row(snapshot, &pane_id, input.config, None))
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return rows;
        }
        let collapsed = input.collapsed.contains(OTHER_GROUP_TITLE);
        lines.push(TeamLineRef::OtherHeader {
            count: rows.len(),
            status: rows
                .iter()
                .map(|row| row.status)
                .max_by_key(|status| status_priority(*status))
                .unwrap_or(AgentStatus::Unknown),
            collapsed,
        });
        if collapsed {
            return Vec::new();
        }
        lines.extend((0..rows.len()).map(TeamLineRef::Agent));
        rows
    }

    /// How many times the cached rows were built.
    #[cfg(test)]
    pub(super) fn row_builds(&self) -> u32 {
        self.rows.builds
    }
}

impl ClientShellState {
    /// The frame's time for Orbit ages: the endpoint's clock when it sent one,
    /// else this client's.
    pub(super) fn orbit_frame_unix_ms(&self, now: std::time::Instant) -> u64 {
        self.orbit_clock.unwrap_or(self.wall_clock).unix_ms_at(now)
    }

    /// Brings the team panel's cached rows up to date before a frame is drawn.
    /// Skipped while the panel is not shown.
    pub(super) fn refresh_team_rows(&mut self, sidebar_width: u16, now: std::time::Instant) {
        let now_ms = self.orbit_frame_unix_ms(now);
        let (Some(team), Some(snapshot)) = (self.team_sidebar.as_mut(), self.snapshot.as_deref())
        else {
            return;
        };
        if self.sidebar_collapsed
            || TeamSidebar::active(Some(&*team), &self.config, snapshot).is_none()
        {
            return;
        }
        team.refresh_rows(&TeamRowsInput {
            snapshot_serial: self.snapshot_serial,
            snapshot,
            collapsed: &self.collapsed_team_groups,
            sidebar_width,
            config: &self.config,
            now_ms,
        });
    }
}
