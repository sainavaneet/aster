//! Glyph tables for the modern style. Every unicode glyph is one cell wide
//! under `unicode-width`; `ASCII` swaps each for a one-cell ASCII fallback
//! (see section 4.4 of docs/design/aster-visual-redesign.md).

use super::state::StateKind;
use crate::config::UiGlyphsConfig;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GlyphSet {
    pub(crate) working: &'static str,
    pub(crate) idle: &'static str,
    pub(crate) blocked: &'static str,
    pub(crate) approval: &'static str,
    pub(crate) done: &'static str,
    pub(crate) failed: &'static str,
    pub(crate) queued: &'static str,
    pub(crate) unknown: &'static str,
    pub(crate) group_open: &'static str,
    pub(crate) group_closed: &'static str,
    pub(crate) role_gutter: &'static str,
    pub(crate) node_done: &'static str,
    pub(crate) node_active: &'static str,
    pub(crate) node_pending: &'static str,
    pub(crate) node_failed: &'static str,
    pub(crate) bar_full: &'static str,
    pub(crate) bar_empty: &'static str,
    pub(crate) handoff: &'static str,
    pub(crate) parallel: &'static str,
    pub(crate) integrate: &'static str,
    pub(crate) separator: &'static str,
    pub(crate) ellipsis: &'static str,
    pub(crate) chip_on: &'static str,
    pub(crate) chip_off: &'static str,
    /// Tree pieces of the flow view: vertical, branch, last branch, fork.
    pub(crate) tree_vertical: &'static str,
    pub(crate) tree_mid: &'static str,
    pub(crate) tree_end: &'static str,
    pub(crate) tree_fork: &'static str,
    /// Working spinner frames; the first frame is the static working glyph's
    /// stand-in while animations are off.
    pub(crate) spinner: &'static [&'static str],
}

pub(crate) const UNICODE: GlyphSet = GlyphSet {
    working: "◐",
    idle: "○",
    blocked: "▲",
    approval: "⚑",
    done: "✓",
    failed: "✗",
    queued: "◌",
    unknown: "·",
    group_open: "▾",
    group_closed: "▸",
    role_gutter: "▌",
    node_done: "●",
    node_active: "◐",
    node_pending: "○",
    node_failed: "✗",
    bar_full: "━",
    bar_empty: "─",
    handoff: "▶",
    parallel: "‖",
    integrate: "⊕",
    separator: "·",
    ellipsis: "…",
    chip_on: "▪",
    chip_off: "▫",
    tree_vertical: "│",
    tree_mid: "├",
    tree_end: "╰",
    tree_fork: "┬",
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"],
};

pub(crate) const ASCII: GlyphSet = GlyphSet {
    working: "*",
    idle: "o",
    blocked: "!",
    approval: "?",
    done: "+",
    failed: "x",
    queued: ".",
    unknown: "-",
    group_open: "v",
    group_closed: ">",
    role_gutter: "|",
    node_done: "#",
    node_active: "*",
    node_pending: ".",
    node_failed: "x",
    bar_full: "=",
    bar_empty: "-",
    handoff: ">",
    parallel: "|",
    integrate: "+",
    separator: ".",
    ellipsis: "~",
    chip_on: "#",
    chip_off: ".",
    tree_vertical: "|",
    tree_mid: "+",
    tree_end: "\\",
    tree_fork: "+",
    spinner: &["|", "/", "-", "\\"],
};

impl GlyphSet {
    pub(crate) fn for_config(config: UiGlyphsConfig) -> &'static GlyphSet {
        match config {
            UiGlyphsConfig::Unicode => &UNICODE,
            UiGlyphsConfig::Ascii => &ASCII,
        }
    }

    /// The glyph of a state. Exited and unknown share the neutral dot.
    pub(crate) fn state(&self, state: StateKind) -> &'static str {
        match state {
            StateKind::Working => self.working,
            StateKind::Idle => self.idle,
            StateKind::Blocked => self.blocked,
            StateKind::Approval => self.approval,
            StateKind::Done => self.done,
            StateKind::Failed => self.failed,
            StateKind::Queued => self.queued,
            StateKind::Exited | StateKind::Unknown => self.unknown,
        }
    }

    /// The working glyph for a spinner phase, or the static glyph when the
    /// spinner is not cycling (animations reduced or off).
    pub(crate) fn working_frame(&self, phase: Option<u8>) -> &'static str {
        match phase {
            Some(index) if !self.spinner.is_empty() => {
                self.spinner[usize::from(index) % self.spinner.len()]
            }
            _ => self.working,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn all(set: &GlyphSet) -> Vec<&'static str> {
        let mut glyphs = vec![
            set.working,
            set.idle,
            set.blocked,
            set.approval,
            set.done,
            set.failed,
            set.queued,
            set.unknown,
            set.group_open,
            set.group_closed,
            set.role_gutter,
            set.node_done,
            set.node_active,
            set.node_pending,
            set.node_failed,
            set.bar_full,
            set.bar_empty,
            set.handoff,
            set.parallel,
            set.integrate,
            set.separator,
            set.ellipsis,
            set.chip_on,
            set.chip_off,
            set.tree_vertical,
            set.tree_mid,
            set.tree_end,
            set.tree_fork,
        ];
        glyphs.extend(set.spinner);
        glyphs
    }

    #[test]
    fn every_glyph_is_one_cell_wide() {
        for set in [&UNICODE, &ASCII] {
            for glyph in all(set) {
                assert_eq!(UnicodeWidthStr::width(glyph), 1, "{glyph:?}");
            }
        }
    }

    #[test]
    fn the_ascii_set_is_ascii_and_states_stay_distinct() {
        for glyph in all(&ASCII) {
            assert!(glyph.is_ascii(), "{glyph:?}");
        }
        for set in [&UNICODE, &ASCII] {
            let shapes = [
                StateKind::Working,
                StateKind::Idle,
                StateKind::Blocked,
                StateKind::Approval,
                StateKind::Done,
                StateKind::Failed,
                StateKind::Queued,
                StateKind::Unknown,
            ]
            .map(|state| set.state(state));
            for (index, shape) in shapes.iter().enumerate() {
                assert!(!shapes[index + 1..].contains(shape), "{shape:?} repeats");
            }
        }
    }

    #[test]
    fn the_spinner_cycles_only_when_a_phase_is_given() {
        assert_eq!(UNICODE.working_frame(None), "◐");
        assert_eq!(UNICODE.working_frame(Some(1)), "⠙");
        assert_eq!(ASCII.working_frame(Some(5)), "/");
    }
}
