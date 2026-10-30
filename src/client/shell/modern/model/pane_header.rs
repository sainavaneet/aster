//! Worker pane headers: one precomputed record per pane that hosts an Orbit
//! worker, built with the team model when the `orbit` projection changes. The
//! header text and state never depend on geometry or focus; the painter picks
//! what fits the pane's width each frame from the widths stored here.

use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

use super::lanes::strip;
use super::team::{ModernMember, ModernRole, Open};
use crate::protocol::OrbitShellView;
use crate::ui::chrome::draw::display_width;
use crate::ui::chrome::state::StateKind;

/// Display widths of each part, so the painter plans a header without
/// measuring text per frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct HeaderWidths {
    pub(in crate::client::shell) role: u16,
    pub(in crate::client::shell) member: u16,
    pub(in crate::client::shell) kind: u16,
    pub(in crate::client::shell) model: u16,
    pub(in crate::client::shell) title: u16,
    pub(in crate::client::shell) step: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct PaneHeader {
    /// Role index for the gutter tint; `None` for the lead.
    pub(in crate::client::shell) tint: Option<usize>,
    pub(in crate::client::shell) role: String,
    pub(in crate::client::shell) member: String,
    pub(in crate::client::shell) kind: String,
    pub(in crate::client::shell) model: Option<String>,
    /// The worker's open task, whatever its state.
    pub(in crate::client::shell) title: Option<String>,
    /// The approval-aware state, the same one the sidebar shows.
    pub(in crate::client::shell) state: StateKind,
    /// When the open task was created; its age is read against the frame clock.
    pub(in crate::client::shell) created_at_ms: Option<u64>,
    /// `step 2/3`, `parallel 1/2` or `integrating` for a relayed task.
    pub(in crate::client::shell) step: Option<String>,
    pub(in crate::client::shell) widths: HeaderWidths,
    /// Bytes the largest grapheme cluster of this header needs in a cell. The
    /// compositor reserves at least this in the pane's header row, widening
    /// its normal UTF-8-scalar reservation only there, so painting never grows
    /// a string.
    pub(in crate::client::shell) cell_bytes: usize,
}

fn width(text: &str) -> u16 {
    display_width(text)
}

fn header(
    tint: Option<usize>,
    role: &str,
    member: &str,
    extra: &ModernMember,
    open: Option<&Open<'_>>,
) -> PaneHeader {
    let title = open.map(|open| open.task.title.clone());
    let step = open
        .and_then(|open| strip(open.task))
        .map(|strip| strip.line);
    let widths = HeaderWidths {
        role: width(role),
        member: width(member),
        kind: width(&extra.kind),
        model: extra.model.as_deref().map_or(0, width),
        title: title.as_deref().map_or(0, width),
        step: step.as_deref().map_or(0, width),
    };
    let cell_bytes = [
        role,
        member,
        extra.kind.as_str(),
        extra.model.as_deref().unwrap_or_default(),
        title.as_deref().unwrap_or_default(),
        step.as_deref().unwrap_or_default(),
    ]
    .into_iter()
    .flat_map(|text| text.graphemes(true))
    .map(str::len)
    .max()
    .unwrap_or(0)
    // The border glyph and the state and separator glyphs are three bytes.
    .max(3);
    PaneHeader {
        cell_bytes,
        tint,
        role: role.to_owned(),
        member: member.to_owned(),
        kind: extra.kind.clone(),
        model: extra.model.clone(),
        title,
        state: extra.state,
        created_at_ms: open.map(|open| open.task.created_at_ms),
        step,
        widths,
    }
}

/// Headers by pane id for the lead, the roles' members and scratch workers.
pub(super) fn build(
    orbit: &OrbitShellView,
    lead: &ModernMember,
    roles: &[ModernRole],
    open: &HashMap<&str, Open<'_>>,
) -> HashMap<String, PaneHeader> {
    let mut headers = HashMap::new();
    if let Some(pane_id) = orbit.lead.pane_id.as_deref() {
        headers.insert(
            pane_id.to_owned(),
            header(None, "", &orbit.lead.name, lead, None),
        );
    }
    let groups = orbit
        .roles
        .iter()
        .map(|role| (role.name.as_str(), &role.members))
        .chain((!orbit.scratch.is_empty()).then_some(("scratch", &orbit.scratch)));
    for (index, (name, members)) in groups.enumerate() {
        for (position, member) in members.iter().enumerate() {
            let Some(pane_id) = member.pane_id.as_deref() else {
                continue;
            };
            let task = member
                .terminal_id
                .as_deref()
                .and_then(|terminal| open.get(terminal));
            headers.insert(
                pane_id.to_owned(),
                header(
                    Some(index),
                    name,
                    &member.name,
                    &roles[index].members[position],
                    task,
                ),
            );
        }
    }
    headers
}

/// A reservation span in surface-local coordinates, prepared on state change.
#[derive(Debug, Clone, Copy)]
pub(in crate::client::shell) struct HeaderCopy {
    pub(in crate::client::shell) pane: usize,
    pub(in crate::client::shell) row: u16,
    pub(in crate::client::shell) from: u16,
    pub(in crate::client::shell) to: u16,
    pub(in crate::client::shell) bytes: usize,
}

/// Output revisions and cell contents do not change header reservation geometry.
pub(in crate::client::shell) fn same_geometry(
    a: &crate::protocol::PaneSurfaceFrame,
    b: &crate::protocol::PaneSurfaceFrame,
) -> bool {
    a.frame.width == b.frame.width
        && a.frame.height == b.frame.height
        && a.panes.len() == b.panes.len()
        && a.panes.iter().zip(&b.panes).all(|(a, b)| {
            a.pane_id == b.pane_id && a.rect == b.rect && a.inner_rect == b.inner_rect
        })
}

pub(in crate::client::shell) fn copy_spans(
    surface: &crate::protocol::PaneSurfaceFrame,
    headers: &HashMap<String, PaneHeader>,
) -> Vec<HeaderCopy> {
    let mut spans: Vec<_> = surface
        .panes
        .iter()
        .enumerate()
        .filter_map(|(index, pane)| {
            if pane.inner_rect.y <= pane.rect.y
                || pane.rect.width < super::super::draw::pane_header::MIN_PANE_WIDTH
            {
                return None;
            }
            let header = headers.get(&pane.pane_id)?;
            Some(HeaderCopy {
                pane: index,
                row: pane.rect.y,
                from: pane.rect.x.saturating_add(1),
                to: pane
                    .rect
                    .x
                    .saturating_add(pane.rect.width)
                    .saturating_sub(1),
                bytes: header.cell_bytes,
            })
        })
        .collect();
    spans.sort_unstable_by_key(|span| (span.row, span.from));
    spans
}

#[cfg(test)]
mod tests {
    use super::super::team::ModernTeam;
    use super::*;
    use crate::protocol::{OrbitShellActiveTask, OrbitShellLead, OrbitShellMember, OrbitShellRole};

    fn member(name: &str, pane: Option<&str>, state: &str) -> OrbitShellMember {
        OrbitShellMember {
            name: name.into(),
            kind: "claude".into(),
            terminal_id: Some(format!("term_{name}")),
            pane_id: pane.map(str::to_owned),
            state: state.into(),
            model: Some("opus".into()),
        }
    }

    fn orbit() -> OrbitShellView {
        let task = |worker: &str, approval: bool| OrbitShellActiveTask {
            task_id: "t".into(),
            worker_terminal_id: worker.into(),
            role: None,
            title: "fix the parser".into(),
            state: "working".into(),
            created_at_ms: 5,
            blocked: approval,
            awaiting_approval: approval,
            steps: Vec::new(),
            step_total: None,
            parallel: None,
            steps_omitted: None,
        };
        OrbitShellView {
            lead: OrbitShellLead {
                name: "Polaris".into(),
                terminal_id: "term_lead".into(),
                pane_id: Some("pane_0".into()),
                state: "idle".into(),
            },
            roles: vec![OrbitShellRole {
                name: "builders".into(),
                pinned: false,
                members: vec![
                    member("ada", Some("pane_1"), "working"),
                    member("bo", None, "idle"),
                ],
            }],
            scratch: vec![member("probe", Some("pane_2"), "idle")],
            active_tasks: vec![task("term_ada", true)],
            recent_tasks: Vec::new(),
            server_now_ms: None,
        }
    }

    #[test]
    fn workers_with_panes_get_headers_with_the_approval_aware_state() {
        let team = ModernTeam::build(&orbit());
        let ada = &team.headers["pane_1"];
        assert_eq!(ada.role, "builders");
        assert_eq!(ada.member, "ada");
        assert_eq!(ada.model.as_deref(), Some("opus"));
        assert_eq!(ada.title.as_deref(), Some("fix the parser"));
        assert_eq!(ada.state, StateKind::Approval);
        assert_eq!(ada.created_at_ms, Some(5));
        assert_eq!(ada.widths.member, 3);
        assert_eq!(ada.tint, Some(0));
        let probe = &team.headers["pane_2"];
        assert_eq!((probe.role.as_str(), probe.tint), ("scratch", Some(1)));
        assert!(probe.title.is_none() && probe.step.is_none());
        let lead = &team.headers["pane_0"];
        assert_eq!((lead.tint, lead.member.as_str()), (None, "Polaris"));
        assert_eq!(
            team.headers.len(),
            3,
            "a member without a pane has no header"
        );
    }
}
