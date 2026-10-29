//! The Orbit flow view: opening it, its layout cache, keyboard and mouse. It is
//! client presentation state over `ClientShellSnapshot.orbit`; nothing here runs
//! while the view is closed.

use crossterm::event::{MouseButton, MouseEventKind};

use super::modern::model::flow::{inner_of, FlowKey, FlowLayout};
use super::*;

impl ClientShellState {
    /// Opens the flow view on the task that needs you most, else the first.
    /// Inert when `ui.flow_view` is off or the session has no Orbit team.
    pub(super) fn open_orbit_flow(&mut self, outcome: &mut ClientShellInput) {
        if !self.config.flow_view || !self.config.chrome.is_modern() {
            return;
        }
        let Some(orbit) = self.snapshot.as_deref().and_then(|s| s.orbit.as_ref()) else {
            return;
        };
        let task = orbit
            .active_tasks
            .iter()
            .position(|task| task.awaiting_approval)
            .or_else(|| orbit.active_tasks.iter().position(|task| task.blocked))
            .unwrap_or(0);
        self.overlay = Some(ClientShellOverlay::OrbitFlow(ClientOrbitFlowOverlay {
            task,
            step: 0,
            default_step: true,
            layout: None,
        }));
        outcome.repaint = true;
    }

    /// Brings the open view's layout up to date for the frame's popup. A no-op
    /// when the key (snapshot, task, width) is unchanged.
    pub(super) fn refresh_flow_layout(
        overlay: &mut Option<ClientShellOverlay>,
        snapshot: &ClientShellSnapshot,
        serial: u64,
        area: Rect,
    ) {
        let Some(ClientShellOverlay::OrbitFlow(flow)) = overlay.as_mut() else {
            return;
        };
        let Some(orbit) = snapshot.orbit.as_ref() else {
            flow.layout = None;
            return;
        };
        let Some(popup) = super::render::flow_popup(area) else {
            return;
        };
        // Ordinals can change when earlier tasks finish or the projected step
        // window moves. Preserve the selected identities across that refresh.
        let mut selected_index = None;
        if let Some(old) = flow.layout.as_ref().filter(|old| old.key.serial != serial) {
            if let Some(task) = orbit
                .active_tasks
                .iter()
                .position(|task| task.task_id == old.task_id)
            {
                flow.task = task;
                selected_index = old
                    .slots
                    .get(flow.step)
                    .and_then(|slot| old.step_indices.get(slot.step))
                    .copied();
            } else {
                flow.task = flow.task.min(orbit.active_tasks.len().saturating_sub(1));
                flow.default_step = true;
            }
        }
        let key = FlowKey {
            serial,
            task: flow.task,
            width: inner_of(popup).width,
        };
        if flow.layout.as_ref().is_some_and(|layout| layout.key == key) {
            return;
        }
        let layout = FlowLayout::build(orbit, flow.task, key);
        if flow.default_step {
            flow.default_step = false;
            let task = orbit.active_tasks.get(flow.task);
            flow.step = task
                .and_then(|task| {
                    layout
                        .slots
                        .iter()
                        .position(|slot| task.steps[slot.step].state == "active")
                })
                .unwrap_or(0);
        }
        if let Some(index) = selected_index {
            flow.step = layout
                .slots
                .iter()
                .position(|slot| layout.step_indices[slot.step] == index)
                .unwrap_or(0);
        }
        flow.step = flow.step.min(layout.slots.len().saturating_sub(1));
        flow.layout = Some(layout);
    }

    /// The pane `slot`'s worker sits in, only while that pane is in the snapshot.
    /// `None` when the step has no recorded worker or the worker stopped.
    pub(super) fn flow_slot_pane(&self, slot: usize) -> Option<&str> {
        let Some(ClientShellOverlay::OrbitFlow(flow)) = self.overlay.as_ref() else {
            return None;
        };
        let snapshot = self.snapshot.as_deref()?;
        let orbit = snapshot.orbit.as_ref()?;
        let layout = flow.layout.as_ref()?;
        if layout.key.serial != self.snapshot_serial {
            return None;
        }
        let task = orbit.active_tasks.get(flow.task)?;
        let pane = match layout.slots.get(slot) {
            Some(slot) => task.steps.get(slot.step)?.pane_id.as_deref()?,
            None if layout.slots.is_empty() => layout.lone_pane.as_deref()?,
            None => return None,
        };
        snapshot
            .agents
            .iter()
            .any(|agent| agent.pane_id == pane)
            .then_some(pane)
    }

    /// Whether Enter would focus the selected step's worker.
    pub(super) fn flow_selected_pane_available(&self) -> bool {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::OrbitFlow(flow)) => self.flow_slot_pane(flow.step).is_some(),
            _ => false,
        }
    }

    fn focus_flow_slot(&mut self, slot: usize, outcome: &mut ClientShellInput) {
        let Some(pane_id) = self.flow_slot_pane(slot).map(str::to_owned) else {
            outcome.repaint = true;
            return;
        };
        self.overlay = None;
        self.mode = ClientShellMode::Terminal;
        self.push_endpoint_method(
            crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id }),
            outcome,
        );
        outcome.repaint = true;
    }

    fn flow_task_count(&self) -> usize {
        self.snapshot
            .as_deref()
            .and_then(|s| s.orbit.as_ref())
            .map_or(0, |orbit| orbit.active_tasks.len())
    }

    fn select_flow_task(&mut self, task: usize) {
        let count = self.flow_task_count();
        if let Some(ClientShellOverlay::OrbitFlow(flow)) = self.overlay.as_mut() {
            let task = task.min(count.saturating_sub(1));
            if task != flow.task {
                flow.task = task;
                flow.step = 0;
                flow.default_step = true;
                flow.layout = None;
            }
        }
    }

    fn move_flow_step(&mut self, delta: isize) {
        if let Some(ClientShellOverlay::OrbitFlow(flow)) = self.overlay.as_mut() {
            let last = flow
                .layout
                .as_ref()
                .map_or(0, |layout| layout.slots.len().saturating_sub(1));
            flow.step = flow.step.saturating_add_signed(delta).min(last);
        }
    }

    pub(super) fn handle_flow_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let task = match self.overlay.as_ref() {
            Some(ClientShellOverlay::OrbitFlow(flow)) => (flow.task, flow.step),
            _ => return,
        };
        match code {
            KeyCode::Esc | KeyCode::Char('q' | 'f') => self.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => self.move_flow_step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_flow_step(1),
            KeyCode::Home => self.move_flow_step(isize::MIN / 2),
            KeyCode::End => self.move_flow_step(isize::MAX / 2),
            KeyCode::Left | KeyCode::Char('h') => self.select_flow_task(task.0.saturating_sub(1)),
            KeyCode::BackTab => self.select_flow_task(task.0.saturating_sub(1)),
            KeyCode::Right | KeyCode::Char('l') => self.select_flow_task(task.0 + 1),
            KeyCode::Tab if modifiers.is_empty() => self.select_flow_task(task.0 + 1),
            KeyCode::Enter => self.focus_flow_slot(task.1, outcome),
            _ => {}
        }
        outcome.repaint = true;
    }

    pub(super) fn handle_flow_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) {
        let point = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_flow_step(-1),
            MouseEventKind::ScrollDown => self.move_flow_step(1),
            MouseEventKind::Down(MouseButton::Left) => {
                let step = self
                    .hits
                    .flow_steps
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, slot)| *slot);
                let task = self
                    .hits
                    .flow_tasks
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, task)| *task);
                if let Some(slot) = step {
                    if let Some(ClientShellOverlay::OrbitFlow(flow)) = self.overlay.as_mut() {
                        flow.step = slot;
                    }
                    self.focus_flow_slot(slot, outcome);
                    return;
                } else if let Some(task) = task {
                    self.select_flow_task(task);
                } else if super::contains(self.hits.overlay_cancel, point)
                    || !super::contains(self.hits.flow_popup, point)
                {
                    self.overlay = None;
                }
            }
            _ => return,
        }
        outcome.repaint = true;
    }
}
