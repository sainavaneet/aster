//! Orbit team tree for the agent sidebar.
//!
//! `TeamSidebar` is the precomputed row model. It is rebuilt only when the
//! snapshot's `orbit` projection changes (see `apply_active_snapshot`); the row
//! list it shows is cached beside it (`team_rows`), so the render loop formats
//! and allocates nothing: it walks the cached lines and writes borrowed text
//! into the buffer. Focus comes from the snapshot at render time; collapse
//! state is client-side presentation state held in
//! `ClientShellState::collapsed_team_groups`.
//!
//! Layout, top to bottom: the lead row, one collapsible group per role, a
//! `scratch` group for role-less workers, then an `other` group holding agents
//! that are not part of the team (ordered by `ui.agent_panel_sort`, rendered as
//! ordinary agent rows). Members are listed in roster order; the sort setting
//! never reorders them.

use std::collections::HashSet;

use ratatui::{buffer::Buffer, layout::Rect, style::Modifier, style::Style};

use super::agent_sidebar::{ordered_agent_pane_ids, AgentRow};
use super::render::{put_segment, put_text};
use super::team_rows::TeamLineRef;
use super::*;
use crate::api::schema::AgentStatus;
use crate::protocol::{OrbitShellMember, OrbitShellView};

const SCRATCH_GROUP_KEY: &str = "scratch";
pub(super) const OTHER_GROUP_TITLE: &str = "other";
pub(super) const WORK_GROUP_KEY: &str = "work";
const LEAD_DETAIL: &str = "lead";
const LEAD_INDENT: u16 = 1;
const MEMBER_INDENT: u16 = 3;
const EXPANDED_GLYPH: &str = "▾";
const COLLAPSED_GLYPH: &str = "▸";
const PINNED_LABEL: &str = " pinned";

/// One clickable team row: the lead or a member.
pub(super) struct TeamRow {
    pub(super) pane_id: Option<String>,
    pub(super) status: AgentStatus,
    /// False when the member has no live agent (supervision phase shown instead).
    pub(super) live: bool,
    pub(super) name: String,
    pub(super) detail: String,
}

pub(super) struct TeamGroup {
    pub(super) key: String,
    pub(super) title: String,
    pub(super) pinned: bool,
    /// Highest-priority member status, shown on the header while collapsed.
    pub(super) status: AgentStatus,
    pub(super) members: Vec<TeamRow>,
}

pub(crate) struct TeamSidebar {
    pub(super) lead: TeamRow,
    pub(super) groups: Vec<TeamGroup>,
    pub(super) team_panes: HashSet<String>,
    pub(super) work: super::work_board::WorkBoard,
    pub(super) rows: super::team_rows::TeamRows,
    /// Role cards and work lanes of the modern style, built with the rest.
    pub(super) modern: super::modern::model::team::ModernTeam,
}

pub(super) enum TeamLine<'a> {
    Lead(&'a TeamRow),
    Header {
        /// Index into `TeamSidebar::groups`; `groups.len()` is the `other` group.
        group: usize,
        title: &'a str,
        count: usize,
        pinned: bool,
        status: AgentStatus,
        collapsed: bool,
    },
    Member(&'a TeamRow),
    Agent(&'a AgentRow),
    WorkHeader {
        group: usize,
        active: usize,
        blocked: usize,
        collapsed: bool,
    },
    Task(&'a super::work_board::WorkTask),
    Recent(&'a super::work_board::WorkRecent),
    /// A modern-only row; the modern drawer handles it, classic never sees one.
    Modern,
}

/// Everything the render loop needs besides the line itself.
pub(super) struct TeamRenderContext<'a> {
    pub(super) config: &'a ClientShellConfig,
    pub(super) endpoint_id: &'a ClientEndpointId,
    pub(super) animation: super::animation::AnimationFrame<'a>,
    pub(super) focused_pane_id: Option<&'a str>,
    /// Wall-clock reading for this frame, in Unix milliseconds.
    pub(super) now_unix_ms: u64,
}

/// One collapsed-sidebar entry, in the same order as the keyboard navigation.
pub(super) struct CollapsedEntry<'a> {
    pub(super) pane_id: &'a str,
    pub(super) status: AgentStatus,
    pub(super) focused: bool,
}

pub(super) fn agent_state(state: &str) -> Option<AgentStatus> {
    match state {
        "idle" => Some(AgentStatus::Idle),
        "working" => Some(AgentStatus::Working),
        "blocked" => Some(AgentStatus::Blocked),
        "unknown" => Some(AgentStatus::Unknown),
        _ => None,
    }
}

fn member_row(member: &OrbitShellMember) -> TeamRow {
    let status = agent_state(&member.state);
    let detail = match status {
        Some(_) => member.kind.clone(),
        None => format!("{} · {}", member.kind, member.state.replace('_', " ")),
    };
    TeamRow {
        pane_id: member.pane_id.clone(),
        status: status.unwrap_or(AgentStatus::Unknown),
        live: status.is_some(),
        name: member.name.clone(),
        detail,
    }
}

fn group(key: String, title: &str, pinned: bool, members: &[OrbitShellMember]) -> TeamGroup {
    let members = members.iter().map(member_row).collect::<Vec<_>>();
    let status = members
        .iter()
        .filter(|member| member.live)
        .map(|member| member.status)
        .max_by_key(|status| status_priority(*status))
        .unwrap_or(AgentStatus::Unknown);
    TeamGroup {
        key,
        title: title.to_owned(),
        pinned,
        status,
        members,
    }
}

impl TeamSidebar {
    pub(crate) fn build(orbit: &OrbitShellView) -> Self {
        let lead_status = agent_state(&orbit.lead.state);
        let lead = TeamRow {
            pane_id: orbit.lead.pane_id.clone(),
            status: lead_status.unwrap_or(AgentStatus::Unknown),
            live: lead_status.is_some(),
            name: orbit.lead.name.clone(),
            detail: LEAD_DETAIL.to_owned(),
        };
        let mut groups = orbit
            .roles
            .iter()
            .map(|role| {
                group(
                    format!("role:{}", role.name),
                    &role.name,
                    role.pinned,
                    &role.members,
                )
            })
            .collect::<Vec<_>>();
        if !orbit.scratch.is_empty() {
            groups.push(group(
                SCRATCH_GROUP_KEY.to_owned(),
                SCRATCH_GROUP_KEY,
                false,
                &orbit.scratch,
            ));
        }
        let team_panes = std::iter::once(&lead)
            .chain(groups.iter().flat_map(|group| &group.members))
            .filter_map(|row| row.pane_id.clone())
            .collect();
        Self {
            lead,
            groups,
            team_panes,
            work: super::work_board::WorkBoard::build(orbit),
            rows: super::team_rows::TeamRows::default(),
            modern: super::modern::model::team::ModernTeam::build(orbit),
        }
    }

    /// The borrowed line a cached line stands for.
    pub(super) fn line(&self, line: &TeamLineRef) -> TeamLine<'_> {
        match *line {
            TeamLineRef::Lead => TeamLine::Lead(&self.lead),
            TeamLineRef::Group { group, collapsed } => {
                let entry = &self.groups[group];
                TeamLine::Header {
                    group,
                    title: &entry.title,
                    count: entry.members.len(),
                    pinned: entry.pinned,
                    status: entry.status,
                    collapsed,
                }
            }
            TeamLineRef::Member { group, member } => {
                TeamLine::Member(&self.groups[group].members[member])
            }
            TeamLineRef::OtherHeader {
                count,
                status,
                collapsed,
            } => TeamLine::Header {
                group: self.groups.len(),
                title: OTHER_GROUP_TITLE,
                count,
                pinned: false,
                status,
                collapsed,
            },
            TeamLineRef::Agent(index) => TeamLine::Agent(&self.rows.others[index]),
            TeamLineRef::WorkHeader { collapsed } => TeamLine::WorkHeader {
                group: self.groups.len() + 1,
                active: self.work.active_count(),
                blocked: self.work.blocked_count(),
                collapsed,
            },
            TeamLineRef::Task(index) => TeamLine::Task(self.work.task(index)),
            TeamLineRef::Recent(index) => TeamLine::Recent(self.work.recent(index)),
            TeamLineRef::WorkLabel
            | TeamLineRef::LaneHeader { .. }
            | TeamLineRef::LaneTask { .. }
            | TeamLineRef::LaneDone(_)
            | TeamLineRef::Gap => TeamLine::Modern,
        }
    }

    /// The pane a cached line focuses when clicked, if any.
    pub(super) fn line_pane_id(&self, index: usize) -> Option<&str> {
        if let TeamLineRef::LaneTask { lane, index } = *self.rows.lines.get(index)? {
            return self.modern.lanes.tasks(lane)[index].pane_id.as_deref();
        }
        match self.line(self.rows.lines.get(index)?) {
            TeamLine::Lead(row) | TeamLine::Member(row) => row.pane_id.as_deref(),
            TeamLine::Agent(row) => Some(row.pane_id.as_str()),
            TeamLine::Task(task) => task.pane_id(),
            TeamLine::Header { .. }
            | TeamLine::WorkHeader { .. }
            | TeamLine::Recent(_)
            | TeamLine::Modern => None,
        }
    }

    /// The model to use for this frame, or `None` when the flat list applies:
    /// `ui.sidebar_team` is off, the agent list is a filtered view, or the
    /// session has no Orbit team.
    pub(super) fn active<'a>(
        model: Option<&'a Self>,
        config: &ClientShellConfig,
        snapshot: &ClientShellSnapshot,
    ) -> Option<&'a Self> {
        model.filter(|_| config.sidebar_team && snapshot.agent_view_label.is_none())
    }

    pub(super) fn group_key(&self, group: usize) -> &str {
        match self.groups.get(group) {
            Some(group) => group.key.as_str(),
            None if group == self.groups.len() => OTHER_GROUP_TITLE,
            None => WORK_GROUP_KEY,
        }
    }

    fn team_rows(&self) -> impl Iterator<Item = &TeamRow> {
        std::iter::once(&self.lead).chain(self.groups.iter().flat_map(|group| &group.members))
    }

    fn other_pane_ids(
        &self,
        snapshot: &ClientShellSnapshot,
        sort: crate::config::AgentPanelSortConfig,
    ) -> Vec<String> {
        ordered_agent_pane_ids(snapshot, sort)
            .into_iter()
            .filter(|pane_id| !self.team_panes.contains(pane_id))
            .collect()
    }

    /// Focusable panes in sidebar order. Keyboard navigation and the collapsed
    /// sidebar's numbering both use this, so index N means the same pane in both.
    pub(super) fn pane_order(
        &self,
        snapshot: &ClientShellSnapshot,
        sort: crate::config::AgentPanelSortConfig,
    ) -> Vec<String> {
        self.team_rows()
            .filter_map(|row| row.pane_id.clone())
            .chain(self.other_pane_ids(snapshot, sort))
            .collect()
    }

    /// The list row that shows `pane_id`, so keyboard navigation can scroll to it.
    /// `None` when the pane sits inside a collapsed group or is not listed.
    pub(super) fn row_index_of(
        &self,
        snapshot: &ClientShellSnapshot,
        sort: crate::config::AgentPanelSortConfig,
        collapsed: &HashSet<String>,
        pane_id: &str,
    ) -> Option<usize> {
        if self.rows.tier.is_some() {
            // Modern rows have spacers and multi-row cards, so the order is read
            // from the cached lines instead of being recomputed here.
            return self.rows.lines.iter().position(|line| match *line {
                TeamLineRef::Lead => self.lead.pane_id.as_deref() == Some(pane_id),
                TeamLineRef::Member { group, member } => {
                    self.groups[group].members[member].pane_id.as_deref() == Some(pane_id)
                }
                TeamLineRef::Agent(other) => self.rows.others[other].pane_id == pane_id,
                _ => false,
            });
        }
        let mut index = 0;
        if self.lead.pane_id.as_deref() == Some(pane_id) {
            return Some(index);
        }
        index += 1;
        for group in &self.groups {
            index += 1;
            if collapsed.contains(&group.key) {
                continue;
            }
            for member in &group.members {
                if member.pane_id.as_deref() == Some(pane_id) {
                    return Some(index);
                }
                index += 1;
            }
        }
        let others = self.other_pane_ids(snapshot, sort);
        if others.is_empty() || collapsed.contains(OTHER_GROUP_TITLE) {
            return None;
        }
        others
            .iter()
            .position(|other| other == pane_id)
            .map(|position| index + 1 + position)
    }

    /// Entries for the collapsed sidebar: every focusable team pane, then the rest.
    pub(super) fn collapsed_entries<'a>(
        &'a self,
        snapshot: &'a ClientShellSnapshot,
        sort: crate::config::AgentPanelSortConfig,
    ) -> Vec<CollapsedEntry<'a>> {
        let focused_pane_id = snapshot.focused_pane_id.as_deref();
        let mut entries = self
            .team_rows()
            .filter_map(|row| {
                Some(CollapsedEntry {
                    pane_id: row.pane_id.as_deref()?,
                    status: row.status,
                    focused: focused_pane_id == row.pane_id.as_deref(),
                })
            })
            .collect::<Vec<_>>();
        let others = ordered_agent_pane_ids(snapshot, sort);
        entries.extend(others.iter().filter_map(|pane_id| {
            if self.team_panes.contains(pane_id) {
                return None;
            }
            let agent = snapshot
                .agents
                .iter()
                .find(|agent| &agent.pane_id == pane_id)?;
            Some(CollapsedEntry {
                pane_id: agent.pane_id.as_str(),
                status: agent.agent_status,
                focused: agent.focused,
            })
        }));
        entries
    }
}

/// Draws one line. `index` is its position in the cached rows: a line that
/// focuses a pane records it as its click target, resolved on click.
pub(super) fn render_team_line(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    line: &TeamLine<'_>,
    context: &TeamRenderContext<'_>,
    hits: &mut ShellHitMap,
) {
    match line {
        TeamLine::Modern => {}
        TeamLine::Lead(row) => {
            render_team_row(buffer, rect, row, LEAD_INDENT, true, context);
            if row.pane_id.is_some() {
                hits.team_agents.push((rect, index));
            }
        }
        TeamLine::Member(row) => {
            render_team_row(buffer, rect, row, MEMBER_INDENT, false, context);
            if row.pane_id.is_some() {
                hits.team_agents.push((rect, index));
            }
        }
        TeamLine::Header {
            group,
            title,
            count,
            pinned,
            status,
            collapsed,
        } => {
            render_header(
                buffer, rect, title, *count, *pinned, *status, *collapsed, context,
            );
            hits.team_groups.push((rect, *group));
        }
        TeamLine::Agent(row) => {
            let phase = context
                .animation
                .agent_row(context.endpoint_id, &row.pane_id, row.status);
            hits.team_agents.push((rect, index));
            super::agent_sidebar::render_agent_row(buffer, rect, row, context.config, phase);
        }
        TeamLine::WorkHeader {
            group,
            active,
            blocked,
            collapsed,
        } => {
            super::work_board::render_work_header(
                buffer, rect, *active, *blocked, *collapsed, context,
            );
            hits.team_groups.push((rect, *group));
        }
        TeamLine::Task(task) => {
            super::work_board::render_task(buffer, rect, task, context);
            if task.pane_id().is_some() {
                hits.team_agents.push((rect, index));
            }
        }
        TeamLine::Recent(task) => super::work_board::render_recent(buffer, rect, task, context),
    }
}

fn render_team_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &TeamRow,
    indent: u16,
    is_lead: bool,
    context: &TeamRenderContext<'_>,
) {
    let config = context.config;
    let palette = &config.palette;
    let focused = row
        .pane_id
        .as_deref()
        .is_some_and(|pane_id| context.focused_pane_id == Some(pane_id));
    let phase = context.animation.agent_row(
        context.endpoint_id,
        row.pane_id.as_deref().unwrap_or_default(),
        row.status,
    );
    let (icon, icon_fg) = super::agent_sidebar::agent_status_icon_and_color(
        row.status,
        config.status_indicators,
        palette,
        phase,
    );
    if let Some(bg) = row_background(palette, focused, phase) {
        buffer.set_style(rect, Style::default().bg(bg));
    }
    let name_fg = if focused || is_lead {
        palette.text
    } else {
        palette.subtext0
    };
    let mut name_style = Style::default().fg(name_fg);
    if focused || is_lead {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    let mut detail_style = Style::default().fg(palette.overlay0);
    if !row.live {
        name_style = name_style.add_modifier(Modifier::DIM);
        detail_style = detail_style.add_modifier(Modifier::DIM);
    }
    let right = rect.right();
    let mut x = rect.x.saturating_add(indent);
    x = put_segment(buffer, x, rect.y, right, icon, Style::default().fg(icon_fg));
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(buffer, x, rect.y, right, &row.name, name_style);
    x = put_segment(buffer, x, rect.y, right, "  ", Style::default());
    put_segment(buffer, x, rect.y, right, &row.detail, detail_style);
}

fn row_background(
    palette: &Palette,
    focused: bool,
    phase: super::animation::AgentRowAnimationPhase,
) -> Option<ratatui::style::Color> {
    if focused {
        Some(palette.active_row_bg)
    } else if phase.status_flash > 0.0 || phase.recency_highlight > 0.0 {
        Some(super::animation::blend_color(
            palette.sidebar_bg,
            palette.surface0,
            phase.status_flash.max(phase.recency_highlight * 0.65),
            palette,
        ))
    } else {
        None
    }
}

fn render_header(
    buffer: &mut Buffer,
    rect: Rect,
    title: &str,
    count: usize,
    pinned: bool,
    status: AgentStatus,
    collapsed: bool,
    context: &TeamRenderContext<'_>,
) {
    let config = context.config;
    let palette = &config.palette;
    let right = rect.right();
    let dim = Style::default().fg(palette.overlay0);
    let mut x = rect.x.saturating_add(LEAD_INDENT);
    let glyph = if collapsed {
        COLLAPSED_GLYPH
    } else {
        EXPANDED_GLYPH
    };
    x = put_segment(buffer, x, rect.y, right, glyph, dim);
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = put_segment(
        buffer,
        x,
        rect.y,
        right,
        title,
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD),
    );
    x = put_segment(buffer, x, rect.y, right, " ", Style::default());
    x = super::work_board::put_number(buffer, x, rect.y, right, count, dim);
    if pinned {
        put_segment(buffer, x, rect.y, right, PINNED_LABEL, dim);
    }
    // A collapsed group still reports its most urgent member.
    if collapsed && status != AgentStatus::Unknown && rect.width > 2 {
        put_text(
            buffer,
            right.saturating_sub(2),
            rect.y,
            1,
            status_icon(status, config.status_indicators),
            Style::default().fg(status_color(status, palette)),
        );
    }
}

/// Renders the team tree below the agent panel header from the cached rows
/// (`ClientShellState::refresh_team_rows` runs before the frame). Returns
/// nothing: the scroll metrics and hit rectangles land in `hits` as for the
/// flat list.
pub(super) fn render_team_panel(
    buffer: &mut Buffer,
    area: Rect,
    team: &TeamSidebar,
    context: &TeamRenderContext<'_>,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    if !super::agent_sidebar::render_agent_panel_header(
        buffer,
        area,
        TEAM_PANEL_TITLE,
        None,
        context.config,
        hits,
    ) {
        return;
    }
    let rows = &team.rows;
    super::agent_sidebar::render_measured_agent_list(
        buffer,
        area,
        super::agent_sidebar::AgentListRows {
            rows: &rows.lines,
            heights: &rows.heights,
            gaps: &rows.gaps,
        },
        None,
        context.config,
        agent_scroll,
        hits,
        |buffer, rect, index, line, hits| {
            if rows.tier.is_some()
                && super::modern::draw::team::render_line(
                    buffer, rect, index, line, team, context, hits,
                )
            {
                return;
            }
            render_team_line(buffer, rect, index, &team.line(line), context, hits);
        },
    );
}

const TEAM_PANEL_TITLE: &str = " team";

impl ClientShellState {
    /// The team model when the sidebar is showing the team tree.
    fn active_team_sidebar(&self, snapshot: &ClientShellSnapshot) -> Option<&TeamSidebar> {
        TeamSidebar::active(self.team_sidebar.as_ref(), &self.config, snapshot)
            .filter(|_| self.endpoints.len() <= 1)
    }

    /// Panes in the order the sidebar lists them, for index and next/previous bindings.
    pub(super) fn sidebar_agent_pane_ids(&self, snapshot: &ClientShellSnapshot) -> Vec<String> {
        match self.active_team_sidebar(snapshot) {
            Some(team) => team.pane_order(snapshot, self.config.agent_panel_sort),
            None => ordered_agent_pane_ids(snapshot, self.config.agent_panel_sort),
        }
    }

    /// The list row to scroll to for `pane_id`; `ordinal` is its position in
    /// `sidebar_agent_pane_ids`, which is the row in the flat list.
    pub(super) fn sidebar_agent_row(
        &self,
        snapshot: &ClientShellSnapshot,
        pane_id: &str,
        ordinal: usize,
    ) -> Option<usize> {
        match self.active_team_sidebar(snapshot) {
            Some(team) => team.row_index_of(
                snapshot,
                self.config.agent_panel_sort,
                &self.collapsed_team_groups,
                pane_id,
            ),
            None => Some(ordinal),
        }
    }

    /// Toggles a work lane (modern). Presentation state only: nothing is sent.
    pub(super) fn toggle_lane(&mut self, lane: crate::ui::chrome::state::Lane) {
        self.collapsed_lanes.toggle(lane);
    }

    /// Toggles a team group header. Presentation state only: nothing is sent.
    pub(super) fn toggle_team_group(&mut self, group: usize) {
        let Some(key) = self
            .team_sidebar
            .as_ref()
            .map(|team| team.group_key(group).to_owned())
        else {
            return;
        };
        if !self.collapsed_team_groups.remove(&key) {
            self.collapsed_team_groups.insert(key);
        }
    }
}
