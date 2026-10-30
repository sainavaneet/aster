//! The modern mode bar: key names in the `focus` role, hints muted, short chip
//! names below 60 columns and the attention chip at the right edge. Key labels
//! come from `ModeLabels` (built when the keybindings change) and every piece
//! is a borrowed write, so a frame allocates nothing in any mode.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::client::shell::modern::model::tabs::AttentionSummary;
use crate::client::shell::{ClientCopyModeState, ClientShellConfig, ClientShellMode};
use crate::ui::chrome::draw::{put_number, put_right, put_segment_inline};

/// Below this width the bar uses the short chip names and hints.
const COMPACT_WIDTH: u16 = 60;
/// Cells of the `update ready` notice, leading space included.
const UPDATE_NOTICE: &str = " update ready";
/// Cells of ` COPY ` plus the search marker gap in the search prompt row.
const SEARCH_FOOTER: &str = "  enter search  esc cancel";
const SEARCH_FOOTER_MIN_WIDTH: u16 = 50;

/// What the bar shows besides colors: the mode and the state around it.
#[derive(Clone, Copy)]
pub(in crate::client::shell) struct ModeBarInput<'a> {
    pub(in crate::client::shell) mode: ClientShellMode,
    pub(in crate::client::shell) copy_mode: Option<&'a ClientCopyModeState>,
    pub(in crate::client::shell) endpoint_error: Option<&'a str>,
    pub(in crate::client::shell) update_available: bool,
}

struct Pen<'a> {
    buffer: &'a mut Buffer,
    x: u16,
    y: u16,
    end: u16,
}

impl Pen<'_> {
    fn text(&mut self, text: &str, style: Style) {
        self.x = put_segment_inline(self.buffer, self.x, self.y, self.end, text, style);
    }

    fn number(&mut self, value: u64, style: Style) {
        self.x = put_number(self.buffer, self.x, self.y, self.end, value as usize, style);
    }
}

struct Styles {
    base: Style,
    key: Style,
    chip: Style,
}

fn styles(config: &ClientShellConfig, mode: ClientShellMode) -> Styles {
    let palette = &config.palette;
    let roles = &config.chrome.roles;
    let chip_fg = match palette.panel_bg {
        Color::Reset => palette.surface_dim,
        color => color,
    };
    Styles {
        base: Style::default().fg(roles.text_muted).bg(palette.panel_bg),
        key: Style::default()
            .fg(roles.focus)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
        chip: Style::default()
            .fg(chip_fg)
            .bg(if mode == ClientShellMode::Resize {
                palette.mauve
            } else {
                palette.accent
            })
            .add_modifier(Modifier::BOLD),
    }
}

/// Alternating keys and hints: each pair is a key in the key style followed by
/// its hint in the muted one.
fn pairs(pen: &mut Pen<'_>, styles: &Styles, items: &[(&str, &str)]) {
    for (key, hint) in items {
        if !key.is_empty() {
            pen.text(key, styles.key);
        }
        pen.text(hint, styles.base);
    }
}

fn prefix_hints(pen: &mut Pen<'_>, styles: &Styles, config: &ClientShellConfig, compact: bool) {
    let labels = &config.mode_labels;
    if compact {
        pen.text(" PFX ", styles.chip);
        pen.text(" ", styles.base);
        pairs(
            pen,
            styles,
            &[
                ("esc", " cancel  "),
                (&labels.prefix, " send  "),
                (&labels.workspace_picker, " space  "),
                (&labels.help, " keys"),
            ],
        );
    } else {
        pen.text(" PREFIX ", styles.chip);
        pen.text(" ", styles.base);
        pairs(
            pen,
            styles,
            &[
                ("esc", " cancel  "),
                (&labels.prefix, " send prefix  "),
                (&labels.workspace_picker, " workspace nav  "),
                (&labels.help, " keybinds"),
            ],
        );
    }
}

fn navigate_hints(pen: &mut Pen<'_>, styles: &Styles, config: &ClientShellConfig, compact: bool) {
    let glyphs = config.chrome.glyphs;
    let help = &config.mode_labels.help;
    if compact {
        pen.text(" NAV ", styles.chip);
        pen.text(" esc back  ", styles.base);
        pairs(
            pen,
            styles,
            &[
                (glyphs.keys_vertical_short, " space  "),
                ("tab", " pane  "),
                (help, " keys"),
            ],
        );
    } else {
        pen.text(" NAVIGATE ", styles.chip);
        pen.text(" esc back  ", styles.base);
        pairs(
            pen,
            styles,
            &[
                (glyphs.keys_vertical, " workspace  "),
                ("tab", " pane  "),
                (help, " keybinds"),
            ],
        );
    }
}

fn resize_hints(pen: &mut Pen<'_>, styles: &Styles) {
    pen.text(" RESIZE ", styles.chip);
    pen.text("  ", styles.base);
    pairs(
        pen,
        styles,
        &[("h/l", " width  "), ("j/k", " height  "), ("esc", " done")],
    );
}

fn copy_hints(pen: &mut Pen<'_>, styles: &Styles, copy: &ClientCopyModeState) {
    pen.text(" COPY ", styles.chip);
    pen.text(" ", styles.base);
    pairs(
        pen,
        styles,
        &[
            ("h/j/k/l w/b/e { }", " move  "),
            ("/ ?", " search  "),
            ("n/N", " repeat"),
        ],
    );
    if let Some(current) = copy.search_current_global {
        pen.text(" ", styles.base);
        pen.number(current + 1, styles.base);
        pen.text("/", styles.base);
        pen.number(copy.search_total, styles.base);
    } else if !copy.search_query.is_empty() {
        pen.text(" 0/0", styles.base);
    }
    pen.text("  ", styles.base);
    let select = if copy.selection.is_some() {
        " selecting  "
    } else {
        " select  "
    };
    let (exit_keys, exit_label) = if copy.search_query.is_empty() && copy.selection.is_none() {
        ("q/esc", " exit")
    } else {
        ("esc", " clear  q exit")
    };
    pairs(
        pen,
        styles,
        &[
            ("v/space", select),
            ("y/enter", " copy  "),
            (exit_keys, exit_label),
        ],
    );
}

/// The copy-mode search row: the chip, the marker, the query field and a
/// footer when there is room. Returns the bar (no attention chip on this row).
fn search_row(
    buffer: &mut Buffer,
    bar: Rect,
    prompt: &crate::client::shell::ClientCopySearchPrompt,
    config: &ClientShellConfig,
    styles: &Styles,
) -> Rect {
    let palette = &config.palette;
    let marker = match prompt.direction {
        crate::api::schema::PaneCopySearchDirection::Forward => "/",
        crate::api::schema::PaneCopySearchDirection::Backward => "?",
    };
    put_segment_inline(buffer, bar.x, bar.y, bar.right(), " COPY ", styles.chip);
    let prefix = 8.min(bar.width);
    if bar.width >= 8 {
        put_segment_inline(buffer, bar.x + 7, bar.y, bar.right(), marker, styles.key);
    }
    let footer_width = if bar.width >= SEARCH_FOOTER_MIN_WIDTH {
        SEARCH_FOOTER.len() as u16
    } else {
        0
    };
    let field = Rect::new(
        bar.x + prefix,
        bar.y,
        bar.width.saturating_sub(prefix + footer_width),
        1,
    );
    let field = field.intersection(buffer.area);
    if !field.is_empty() {
        // Reuse the editor's pure viewport projection, then draw with bounded
        // modern cell symbols so an oversized grapheme cannot allocate.
        let (text, cursor) = prompt.query.viewport(field.width);
        let style = Style::default().fg(palette.text).bg(palette.panel_bg);
        for x in field.x..field.right() {
            buffer[(x, field.y)].set_symbol(" ").set_style(style);
        }
        put_segment_inline(buffer, field.x, field.y, field.right(), text, style);
        buffer[(field.x + cursor, field.y)]
            .set_style(Style::default().fg(palette.panel_bg).bg(palette.text));
    }
    if footer_width > 0 {
        put_segment_inline(
            buffer,
            bar.right() - footer_width,
            bar.y,
            bar.right(),
            SEARCH_FOOTER,
            styles.base,
        );
    }
    bar
}

/// Draws the bar on the last row of `pane_area`; `None` when the mode shows
/// no bar (terminal mode without an endpoint error, an empty area, copy mode
/// without its state).
pub(in crate::client::shell) fn draw_mode_bar(
    buffer: &mut Buffer,
    pane_area: Rect,
    input: ModeBarInput<'_>,
    config: &ClientShellConfig,
    summary: AttentionSummary,
) -> Option<Rect> {
    let ModeBarInput {
        mode,
        copy_mode,
        endpoint_error,
        update_available,
    } = input;
    if (mode == ClientShellMode::Terminal && endpoint_error.is_none()) || pane_area.is_empty() {
        return None;
    }
    let bar = Rect::new(
        pane_area.x,
        pane_area.y + pane_area.height.saturating_sub(1),
        pane_area.width,
        1,
    );
    let styles = styles(config, mode);
    let palette = &config.palette;
    for x in bar.x..bar.right() {
        buffer[(x, bar.y)].set_symbol(" ").set_style(styles.base);
    }
    let compact = bar.width < COMPACT_WIDTH;
    let mut pen = Pen {
        buffer: &mut *buffer,
        x: bar.x,
        y: bar.y,
        end: bar.right(),
    };
    if let Some(error) = endpoint_error {
        pen.text(" ERROR ", styles.chip);
        pen.text(" ", styles.base);
        pen.text(error, styles.base);
    } else {
        match mode {
            ClientShellMode::Prefix => prefix_hints(&mut pen, &styles, config, compact),
            ClientShellMode::Navigate => navigate_hints(&mut pen, &styles, config, compact),
            ClientShellMode::Resize => resize_hints(&mut pen, &styles),
            ClientShellMode::Copy => {
                let copy = copy_mode?;
                if let Some(prompt) = copy.search_prompt.as_ref() {
                    return Some(search_row(buffer, bar, prompt, config, &styles));
                }
                copy_hints(&mut pen, &styles, copy);
            }
            ClientShellMode::Terminal => {}
        }
    }
    let text_end = pen.x;
    let mut chip_limit = bar.right();
    if update_available && mode == ClientShellMode::Navigate {
        let width = (UPDATE_NOTICE.len() as u16).min(bar.width);
        let area = Rect::new(bar.right().saturating_sub(width), bar.y, width, 1);
        buffer.set_style(area, Style::default().bg(palette.panel_bg));
        put_right(
            buffer,
            area,
            area.y,
            UPDATE_NOTICE,
            Style::default()
                .fg(palette.accent)
                .bg(palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        chip_limit = area.x;
    }
    // The attention chip sits against the right edge (or the update notice)
    // and only draws when the hints leave it a free cell either side.
    let chip = summary.width();
    let start = chip_limit.saturating_sub(chip.saturating_add(1));
    if chip > 0 && start > text_end {
        super::tabs::render_attention(buffer, Rect::new(start, bar.y, chip, 1), &summary, config);
    }
    Some(bar)
}
