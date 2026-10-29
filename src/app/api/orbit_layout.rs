//! Orbit layout tools: resize and zoom panes inside the bound Orbit tab through
//! the existing pane handlers, refusing the lead pane and preserving the user's
//! workspace, tab, and pane focus.
use super::responses::encode_error;
use crate::api::schema::{
    OrbitPaneResizeParams, OrbitPaneZoomParams, PaneDirection, PaneResizeParams, PaneZoomMode,
    PaneZoomParams,
};
use crate::app::App;
use crate::layout::PaneId;

const DEFAULT_RESIZE_CELLS: u16 = 10;
const MAX_RESIZE_CELLS: u16 = 200;
/// Used when no client has reported a terminal size yet.
const FALLBACK_COLUMNS: u16 = 160;
const FALLBACK_ROWS: u16 = 48;

/// The user's focus, restored after a pane handler that focuses its target.
struct FocusSnapshot {
    active: Option<usize>,
    selected: usize,
    active_tabs: Vec<usize>,
    previous_pane_focus: Option<crate::app::state::PaneFocusTarget>,
    mode: crate::app::Mode,
}

/// A resize in cells as the fraction of the tab's width or height that
/// `pane.resize` takes.
fn resize_fraction(cells: u16, direction: PaneDirection, area: ratatui::layout::Rect) -> f32 {
    let extent = match direction {
        PaneDirection::Left | PaneDirection::Right => match area.width {
            0 => FALLBACK_COLUMNS,
            width => width,
        },
        PaneDirection::Up | PaneDirection::Down => match area.height {
            0 => FALLBACK_ROWS,
            height => height,
        },
    };
    f32::from(cells) / f32::from(extent)
}

impl App {
    fn focus_snapshot(&self) -> FocusSnapshot {
        FocusSnapshot {
            active: self.state.active,
            selected: self.state.selected,
            active_tabs: self
                .state
                .workspaces
                .iter()
                .map(|workspace| workspace.active_tab)
                .collect(),
            previous_pane_focus: self.state.previous_pane_focus.clone(),
            mode: self.state.mode,
        }
    }

    fn restore_focus(&mut self, snapshot: FocusSnapshot) {
        let changed = self.state.active != snapshot.active
            || self.state.selected != snapshot.selected
            || self.state.mode != snapshot.mode;
        self.state.active = snapshot.active;
        self.state.selected = snapshot.selected;
        for (workspace, tab) in self.state.workspaces.iter_mut().zip(snapshot.active_tabs) {
            workspace.active_tab = tab;
        }
        self.state.previous_pane_focus = snapshot.previous_pane_focus;
        self.state.mode = snapshot.mode;
        if changed {
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
    }

    /// An Orbit pane other than the lead's, as (workspace, pane, public id).
    fn orbit_layout_target(
        &mut self,
        id: &str,
        pane_id: &str,
    ) -> Result<(usize, PaneId, String), String> {
        let target = self
            .resolve_orbit_pane(Some(pane_id))
            .map_err(|(code, message)| encode_error(id.into(), code, message))?;
        let lead = self.state.workspaces[target.0]
            .terminal_id(target.1)
            .is_some_and(|terminal_id| self.orbit_is_lead_terminal(terminal_id.as_str()));
        if lead {
            return Err(encode_error(
                id.into(),
                "orbit_lead_protected",
                "The Polaris lead pane is not an Orbit layout target",
            ));
        }
        Ok(target)
    }

    pub(super) fn handle_orbit_pane_resize(
        &mut self,
        id: String,
        params: OrbitPaneResizeParams,
    ) -> String {
        let cells = params.amount.unwrap_or(DEFAULT_RESIZE_CELLS);
        if !(1..=MAX_RESIZE_CELLS).contains(&cells) {
            return encode_error(
                id,
                "invalid_params",
                "amount must be between 1 and 200 cells",
            );
        }
        let pane_id = match self.orbit_layout_target(&id, &params.pane_id) {
            Ok((_, _, pane_id)) => pane_id,
            Err(response) => return response,
        };
        let amount = resize_fraction(cells, params.direction, self.state.view.terminal_area);
        // pane.resize moves the split nearest the pane and restores layout focus.
        self.handle_pane_resize(
            id,
            PaneResizeParams {
                pane_id: Some(pane_id),
                direction: params.direction,
                amount: Some(amount),
            },
        )
    }

    /// Zooms or unzooms an Orbit pane without moving the user's focus. A zoomed
    /// tab shows its focused pane, so the target stays focused in the Orbit tab
    /// while zoomed; the tab's previously focused pane is remembered and focused
    /// again on unzoom, and clients get one layout update with that final focus.
    pub(super) fn handle_orbit_pane_zoom(
        &mut self,
        id: String,
        params: OrbitPaneZoomParams,
    ) -> String {
        let (ws, pane_id) = match self.orbit_layout_target(&id, &params.pane_id) {
            Ok((ws, _, pane_id)) => (ws, pane_id),
            Err(response) => return response,
        };
        let snapshot = self.focus_snapshot();
        let tab = self.resolve_orbit().ok().map(|(_, tab)| tab);
        let before = tab.map(|tab| {
            let tab = &self.state.workspaces[ws].tabs[tab];
            (tab.layout.focused(), tab.zoomed)
        });
        let mode = if params.zoomed {
            PaneZoomMode::On
        } else {
            PaneZoomMode::Off
        };
        // pane.zoom focuses its pane: a zoomed tab shows its focused pane.
        let response = self.handle_pane_zoom(
            id,
            PaneZoomParams {
                pane_id: Some(pane_id),
                mode,
            },
        );
        self.restore_focus(snapshot);
        let (Some(tab), Some((focused, was_zoomed))) = (tab, before) else {
            return response;
        };
        if params.zoomed {
            if !was_zoomed {
                self.orbit_zoom_return = Some(focused);
            }
            return response;
        }
        let restore = self
            .orbit_zoom_return
            .take()
            .filter(|pane| self.state.workspaces[ws].tabs[tab].panes.contains_key(pane))
            .unwrap_or(focused);
        self.state.workspaces[ws].tabs[tab]
            .layout
            .focus_pane(restore);
        self.publish_orbit_layout(ws, tab, response)
    }

    /// Emits the tab's final layout and puts it, with its focused pane, in the
    /// `pane.zoom` response.
    fn publish_orbit_layout(&mut self, ws: usize, tab: usize, response: String) -> String {
        let Some(layout) = self.pane_layout_snapshot(ws, tab) else {
            return response;
        };
        self.emit_layout_updated_snapshot(layout.clone());
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&response) else {
            return response;
        };
        if let Some(zoom) = value["result"]["zoom"].as_object_mut() {
            zoom.insert(
                "focused_pane_id".into(),
                serde_json::json!(layout.focused_pane_id),
            );
            zoom.insert("layout".into(), serde_json::json!(layout));
        }
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::super::orbit_lifecycle::tests::{orbit_app, orbit_panes};
    use super::*;
    use serde_json::Value;

    fn call(app: &mut App, method: &str, params: Value) -> Value {
        let request: crate::api::schema::Request = serde_json::from_value(
            serde_json::json!({"id": "test", "method": method, "params": params}),
        )
        .unwrap();
        serde_json::from_str(&app.handle_api_request(request)).unwrap()
    }

    fn focus(app: &App) -> (Option<usize>, Vec<usize>, Option<PaneId>) {
        (
            app.state.active,
            app.state
                .workspaces
                .iter()
                .map(|ws| ws.active_tab)
                .collect(),
            app.state.workspaces[0].tabs[0].layout.focused().into(),
        )
    }

    #[test]
    fn resize_moves_the_orbit_split_without_changing_focus() {
        let mut app = orbit_app();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 200, 50);
        let before = focus(&app);
        let orbit_focus = app.state.workspaces[0].tabs[1].layout.focused();
        let (_, pane_id, _) = orbit_panes(&mut app)[0].clone();
        let response = call(
            &mut app,
            "orbit.pane_resize",
            serde_json::json!({"pane_id": pane_id, "direction": "right", "amount": 20}),
        );
        assert_eq!(response["result"]["resize"]["changed"], true, "{response}");
        assert_eq!(focus(&app), before);
        assert_eq!(
            app.state.workspaces[0].tabs[1].layout.focused(),
            orbit_focus
        );
        for amount in [0, 201] {
            let response = call(
                &mut app,
                "orbit.pane_resize",
                serde_json::json!({"pane_id": pane_id, "direction": "left", "amount": amount}),
            );
            assert_eq!(response["error"]["code"], "invalid_params", "{response}");
        }
    }

    #[test]
    fn resize_fraction_scales_cells_by_the_tab_extent() {
        let area = ratatui::layout::Rect::new(0, 0, 200, 50);
        assert!((resize_fraction(10, PaneDirection::Left, area) - 0.05).abs() < f32::EPSILON);
        assert!((resize_fraction(10, PaneDirection::Down, area) - 0.2).abs() < f32::EPSILON);
        let empty = ratatui::layout::Rect::default();
        assert!(resize_fraction(16, PaneDirection::Right, empty) > 0.0);
    }

    #[test]
    fn zoom_toggles_orbit_without_moving_user_focus() {
        let mut app = orbit_app();
        let before = focus(&app);
        let (pane, pane_id, _) = orbit_panes(&mut app)[1].clone();
        let response = call(
            &mut app,
            "orbit.pane_zoom",
            serde_json::json!({"pane_id": pane_id, "zoomed": true}),
        );
        assert_eq!(response["result"]["zoom"]["zoomed"], true, "{response}");
        assert!(app.state.workspaces[0].tabs[1].zoomed);
        assert_eq!(app.state.workspaces[0].tabs[1].layout.focused(), pane);
        assert_eq!(focus(&app), before);
        let response = call(
            &mut app,
            "orbit.pane_zoom",
            serde_json::json!({"pane_id": pane_id, "zoomed": false}),
        );
        assert_eq!(response["result"]["zoom"]["zoomed"], false, "{response}");
        assert!(!app.state.workspaces[0].tabs[1].zoomed);
        assert_eq!(focus(&app), before);
    }

    fn last_layout_focus(app: &App) -> Option<String> {
        app.event_hub
            .events_after(0)
            .into_iter()
            .rev()
            .find_map(|(_, event)| match event.data {
                crate::api::schema::EventData::LayoutUpdated { layout } => {
                    Some(layout.focused_pane_id)
                }
                _ => None,
            })
    }

    #[test]
    fn zoom_round_trip_restores_the_orbit_tabs_focused_pane_and_reports_it() {
        let mut app = orbit_app();
        let panes = orbit_panes(&mut app);
        let (pane_a, id_a, _) = panes[0].clone();
        let (pane_b, id_b, _) = panes[1].clone();
        app.state.workspaces[0].tabs[1].layout.focus_pane(pane_b);
        let before = focus(&app);
        let response = call(
            &mut app,
            "orbit.pane_zoom",
            serde_json::json!({"pane_id": id_a, "zoomed": true}),
        );
        assert_eq!(response["result"]["zoom"]["zoomed"], true, "{response}");
        // While zoomed the Orbit tab shows, and so focuses, the zoomed pane.
        assert_eq!(app.state.workspaces[0].tabs[1].layout.focused(), pane_a);
        let response = call(
            &mut app,
            "orbit.pane_zoom",
            serde_json::json!({"pane_id": id_a, "zoomed": false}),
        );
        assert_eq!(response["result"]["zoom"]["zoomed"], false, "{response}");
        assert_eq!(app.state.workspaces[0].tabs[1].layout.focused(), pane_b);
        assert_eq!(
            response["result"]["zoom"]["focused_pane_id"],
            serde_json::json!(id_b)
        );
        assert_eq!(last_layout_focus(&app), Some(id_b));
        assert_eq!(focus(&app), before);
    }

    #[test]
    fn layout_tools_refuse_the_lead_and_panes_outside_orbit() {
        let mut app = orbit_app();
        let lead = app
            .public_pane_id(0, app.state.workspaces[0].tabs[0].root_pane)
            .unwrap();
        let outside = app
            .public_pane_id(1, app.state.workspaces[1].tabs[0].root_pane)
            .unwrap();
        let (_, orbit_root, orbit_terminal) = orbit_panes(&mut app)[0].clone();
        for (method, params) in [
            (
                "orbit.pane_resize",
                serde_json::json!({"direction": "left"}),
            ),
            ("orbit.pane_zoom", serde_json::json!({"zoomed": true})),
            ("orbit.pane_close", serde_json::json!({})),
        ] {
            for (pane, code) in [
                (&lead, "orbit_scope_violation"),
                (&outside, "orbit_scope_violation"),
            ] {
                let mut params = params.clone();
                params["pane_id"] = serde_json::json!(pane);
                let response = call(&mut app, method, params);
                assert_eq!(response["error"]["code"], code, "{method}: {response}");
            }
        }
        // A lead moved into Orbit is still protected.
        app.bind_orbit_lead(orbit_terminal);
        let response = call(
            &mut app,
            "orbit.pane_zoom",
            serde_json::json!({"pane_id": orbit_root, "zoomed": true}),
        );
        assert_eq!(
            response["error"]["code"], "orbit_lead_protected",
            "{response}"
        );
    }
}
