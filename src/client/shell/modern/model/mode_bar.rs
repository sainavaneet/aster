//! Key labels of the modern mode bar, resolved when the keybindings change so
//! drawing the bar never formats or collects them.

use crate::config::LiveKeybindConfig;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct ModeLabels {
    pub(in crate::client::shell) prefix: String,
    pub(in crate::client::shell) workspace_picker: String,
    pub(in crate::client::shell) help: String,
}

impl ModeLabels {
    pub(in crate::client::shell) fn build(keybinds: &LiveKeybindConfig) -> Self {
        let rhs = |bindings: &crate::config::ActionKeybinds| {
            bindings
                .prefix_rhs_label()
                .unwrap_or_else(|| "unset".to_owned())
        };
        Self {
            prefix: keybinds.primary_prefix_label(),
            workspace_picker: rhs(&keybinds.keybinds.workspace_picker),
            help: rhs(&keybinds.keybinds.help),
        }
    }
}
