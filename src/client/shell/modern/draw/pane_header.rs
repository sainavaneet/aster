//! The worker header painted on a pane's top border row (the client composes
//! the server's pane surface, then writes this over the border).
//!
//! Per pane and frame this reads one precomputed `PaneHeader`, plans the
//! layout from stored widths with integer arithmetic, and writes into existing
//! frame cells without allocating: a cell's symbol is cleared and refilled in
//! place (border cells already hold room for any glyph used here). Junction and
//! corner cells are never written, so shared dividers stay intact.

use ratatui::style::{Color, Modifier};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::client::shell::modern::model::pane_header::{HeaderWidths, PaneHeader};
use crate::client::shell::team_sidebar::TeamRenderContext;
#[cfg(test)]
use crate::client::shell::team_sidebar::TeamSidebar;
#[cfg(test)]
use crate::client::shell::PaneHit;
#[cfg(test)]
use crate::protocol::FrameData;
use crate::protocol::{color_to_u32, modifier_to_u16, CellData};
use crate::ui::chrome::age::{age_text, AGE_BUFFER_LEN};
use crate::ui::chrome::draw::display_width;

/// Narrowest pane that gets a header; below this its border row is left alone.
pub(in crate::client::shell) const MIN_PANE_WIDTH: u16 = 6;
/// The title keeps at least this many cells (or its own width, if shorter)
/// before the right-hand and kind/model parts may take any room.
const MIN_TITLE: u16 = 8;
/// A role name longer than this is elided even when there is room.
const ROLE_MAX: u16 = 14;
/// Border corner, tee and edge glyphs a header must never overwrite.
/// The horizontal border glyph a cleared header span shows.
#[cfg(test)]
const BORDER: &str = "─";
const STOPS: [&str; 10] = ["┬", "┴", "┼", "├", "┤", "┌", "┐", "└", "┘", "│"];

/// What fits in a header `width` cells wide. Widths are cells; `0`/`false`
/// means the part is dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct Plan {
    pub(in crate::client::shell) role: u16,
    pub(in crate::client::shell) member: u16,
    pub(in crate::client::shell) title: u16,
    pub(in crate::client::shell) kind: bool,
    pub(in crate::client::shell) model: bool,
    pub(in crate::client::shell) age: bool,
    pub(in crate::client::shell) step: bool,
}

/// Plans the header, most important first: the gutter and state glyph, the role
/// name, the member name, the title (down to `MIN_TITLE` cells), the age, the
/// kind, the model, then the step line. Whatever the optional parts do not use
/// goes back to the title.
pub(in crate::client::shell) fn plan(width: u16, hw: &HeaderWidths, age_width: u16) -> Plan {
    let mut plan = Plan::default();
    let Some(mut rest) = width.checked_sub(2) else {
        return plan;
    };
    let member_min = if hw.member > 0 { 2 } else { 0 };
    let role_room = rest.saturating_sub(member_min);
    if hw.role > 0 && role_room >= 2 {
        plan.role = hw.role.min(ROLE_MAX).min(role_room - 1);
        rest -= plan.role + 1;
    }
    if hw.member > 0 && rest >= 2 {
        plan.member = hw.member.min(rest - 1);
        rest -= plan.member + 1;
    }
    let title_min = MIN_TITLE.min(hw.title);
    let mut title_fits = false;
    if hw.title > 0 && rest >= 3 + title_min {
        title_fits = true;
        rest -= 3 + title_min;
    }
    if age_width > 0 && rest >= age_width + 2 {
        plan.age = true;
        rest -= age_width + 2;
    }
    if hw.kind > 0 && rest > hw.kind {
        plan.kind = true;
        rest -= hw.kind + 1;
    }
    if plan.kind && hw.model > 0 && rest > hw.model {
        plan.model = true;
        rest -= hw.model + 1;
    }
    if hw.step > 0 && rest > hw.step {
        plan.step = true;
        rest -= hw.step + 1;
    }
    if title_fits {
        plan.title = hw.title.min(title_min + rest);
    }
    plan
}

pub(in crate::client::shell) fn is_stop(symbol: &str) -> bool {
    STOPS.contains(&symbol)
}

/// Writes a cell in place. A symbol that does not fit the cell's existing
/// capacity is replaced by a space (or the cell is left alone if even that does
/// not fit), so painting can never allocate; the compositor gives every cell it
/// copies from the pane surface room for a whole grapheme cluster
/// (cached from the projected header text), so this only matters for frames built elsewhere.
fn set_cell(cell: &mut CellData, symbol: &str, fg: u32, bg: Option<u32>, modifier: u16) {
    let capacity = cell.symbol.capacity();
    let symbol = if symbol.len() <= capacity {
        symbol
    } else {
        " "
    };
    if symbol.len() > capacity {
        return;
    }
    cell.symbol.clear();
    cell.symbol.push_str(symbol);
    cell.fg = fg;
    if let Some(bg) = bg {
        cell.bg = bg;
    }
    cell.modifier = modifier;
}

/// The style every part of one header is written with.
#[derive(Clone, Copy)]
struct Look {
    bg: Option<Color>,
    ellipsis: &'static str,
}

/// Writes `text` into `cells[*x..limit]`, eliding with the glyph set's ellipsis
/// when it does not fit. Whole grapheme clusters retain their display width;
/// standalone zero-width clusters are skipped. Advances `*x`.
fn put(
    cells: &mut [CellData],
    x: &mut usize,
    limit: usize,
    text: &str,
    text_width: u16,
    (fg, modifier): (Color, Modifier),
    look: Look,
) {
    let available = limit.saturating_sub(*x);
    let fits = usize::from(text_width) <= available;
    // Both glyph sets guarantee a one-cell ellipsis.
    let ellipsis_width = 1;
    let fg = color_to_u32(fg);
    let bg = look.bg.map(color_to_u32);
    let modifier = modifier_to_u16(modifier);
    let budget = if fits {
        available
    } else {
        available.saturating_sub(ellipsis_width)
    };
    // Inspect only the visible prefix plus one lookahead byte. The lookahead
    // proves that the final ASCII scalar is not joined to a combining suffix.
    // A long model/title never incurs an unbounded is_ascii scan per frame.
    let checked = text.len().min(budget.saturating_add(1));
    if text.as_bytes()[..checked]
        .iter()
        .all(|byte| (b' '..=b'~').contains(byte))
    {
        let count = text.len().min(budget);
        for offset in 0..count {
            set_cell(
                &mut cells[*x + offset],
                &text[offset..offset + 1],
                fg,
                bg,
                modifier,
            );
        }
        *x += count;
        if !fits && available >= ellipsis_width {
            set_cell(&mut cells[*x], look.ellipsis, fg, bg, modifier);
            *x += ellipsis_width;
        }
        return;
    }
    let end = *x + budget;
    for grapheme in text.graphemes(true) {
        let width = if grapheme.len() == 1 && (b' '..=b'~').contains(&grapheme.as_bytes()[0]) {
            1
        } else {
            UnicodeWidthStr::width(grapheme)
        };
        if width == 0 {
            continue;
        }
        if *x + width > end {
            break;
        }
        set_cell(&mut cells[*x], grapheme, fg, bg, modifier);
        for filler in 1..width {
            set_cell(&mut cells[*x + filler], " ", fg, bg, modifier);
        }
        *x += width;
    }
    if !fits && available >= ellipsis_width {
        set_cell(&mut cells[*x], look.ellipsis, fg, bg, modifier);
        *x += ellipsis_width;
    }
}

fn space(cells: &mut [CellData], x: &mut usize, look: Look) {
    set_cell(
        &mut cells[*x],
        " ",
        color_to_u32(Color::Reset),
        look.bg.map(color_to_u32),
        modifier_to_u16(Modifier::empty()),
    );
    *x += 1;
}

/// Paints the header of every worker pane whose top border row is visible.
#[cfg(test)]
pub(in crate::client::shell) fn paint(
    frame: &mut FrameData,
    panes: &[PaneHit],
    team: &TeamSidebar,
    ctx: &TeamRenderContext<'_>,
) {
    for hit in panes {
        let framed = hit.inner_rect.y > hit.rect.y;
        if hit.popup || !framed || hit.rect.width < MIN_PANE_WIDTH {
            continue;
        }
        let Some(header) = team.modern.headers.get(&hit.pane_id) else {
            continue;
        };
        paint_one(frame, hit, header, ctx);
    }
}

#[cfg(test)]
fn paint_one(
    frame: &mut FrameData,
    hit: &PaneHit,
    header: &PaneHeader,
    ctx: &TeamRenderContext<'_>,
) {
    let width = usize::from(frame.width);
    let row = usize::from(hit.rect.y);
    let (first, last) = (
        usize::from(hit.rect.x) + 1,
        usize::from(hit.rect.right()).saturating_sub(1),
    );
    if row >= usize::from(frame.height) || last > width {
        return;
    }
    let cells = &mut frame.cells[row * width..(row + 1) * width];
    let end = (first..last)
        .find(|index| is_stop(&cells[*index].symbol))
        .unwrap_or(last);
    let prepared = prepare(header, &hit.pane_id, ctx);
    // The server may have drawn its own title here. Everything the header does
    // not overwrite must read as plain border again, so first clear the whole
    // writable span (junctions are outside it) to the border's own style.
    let (border_fg, border_bg, border_modifier) = {
        let corner = &cells[first - 1];
        (corner.fg, corner.bg, corner.modifier)
    };
    for cell in &mut cells[first..end] {
        if cell.symbol != BORDER {
            // A cell without room for the glyph still has to stop showing the
            // old text: a space (or nothing, at zero capacity) never allocates.
            let fill = if BORDER.len() <= cell.symbol.capacity() {
                BORDER
            } else if cell.symbol.capacity() > 0 {
                " "
            } else {
                ""
            };
            cell.symbol.clear();
            cell.symbol.push_str(fill);
        }
        cell.fg = border_fg;
        cell.bg = border_bg;
        cell.modifier = border_modifier;
        cell.hyperlink = None;
        cell.skip = false;
    }
    if let Some(bg) = prepared.look.bg {
        for cell in &mut cells[first..end] {
            cell.bg = color_to_u32(bg);
        }
    }

    paint_prepared(cells, first, end, header, ctx, prepared);
}

/// Frame-local styles and glyphs, shared by fused copy/clear and painting.
#[derive(Clone, Copy)]
pub(in crate::client::shell) struct PreparedHeader {
    look: Look,
    glyph: &'static str,
    glyph_color: Color,
    is_focused: bool,
    recent: bool,
}
impl PreparedHeader {
    pub(in crate::client::shell) fn background(self) -> Option<u32> {
        self.look.bg.map(color_to_u32)
    }
}
pub(in crate::client::shell) fn prepare(
    header: &PaneHeader,
    pane_id: &str,
    ctx: &TeamRenderContext<'_>,
) -> PreparedHeader {
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let palette = &ctx.config.palette;
    let is_focused = ctx.focused_pane_id == Some(pane_id);
    let (glyph, glyph_style, phase) = super::state_glyph(ctx, header.state, pane_id);
    let glyph_color = glyph_style.fg.unwrap_or(roles.state_idle);
    // A calm marker for a pane that just changed: a faint fill under the header
    // that decays with the recency window. Static under reduced motion's
    // one-shots and absent when animations are off.
    let pulse = phase.status_flash.max(phase.recency_highlight * 0.65);
    let bg = (pulse > 0.0).then(|| {
        crate::client::shell::animation::blend_color(
            palette.panel_bg,
            palette.surface0,
            pulse,
            palette,
        )
    });
    PreparedHeader {
        look: Look {
            bg,
            ellipsis: chrome.glyphs.ellipsis,
        },
        glyph,
        glyph_color,
        is_focused,
        recent: ctx.recent.is_recent(pane_id, ctx.now),
    }
}

/// The row is already reserved, cleared and filled by its existing copy pass.
pub(in crate::client::shell) fn paint_prepared(
    cells: &mut [CellData],
    first: usize,
    end: usize,
    header: &PaneHeader,
    ctx: &TeamRenderContext<'_>,
    prepared: PreparedHeader,
) {
    let span = u16::try_from(end.saturating_sub(first)).unwrap_or(0);
    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let PreparedHeader {
        look,
        glyph,
        glyph_color,
        is_focused,
        recent,
    } = prepared;
    let mut age_buffer = [0u8; AGE_BUFFER_LEN];
    let age = header
        .created_at_ms
        .map(|created| age_text(ctx.now_unix_ms.saturating_sub(created), &mut age_buffer));
    let age_width = age.map_or(0, display_width);
    let plan = plan(span, &header.widths, age_width);
    if span < 2 {
        return;
    }
    let bold = if is_focused {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    // A static mark for a pane that just changed (finished, or now needs you):
    // an underline, present whatever `ui.animations` says, until the coarse tick
    // ends it.
    let name_modifier = if recent {
        bold | Modifier::UNDERLINED
    } else {
        bold
    };
    let muted = (roles.text_muted, Modifier::empty());
    let mut x = first;
    let tint = header
        .tint
        .map_or(roles.text_muted, |index| roles.tint(index));
    put(
        cells,
        &mut x,
        end,
        chrome.glyphs.role_gutter,
        1,
        (tint, Modifier::empty()),
        look,
    );
    if plan.role > 0 {
        let role_color = if is_focused {
            roles.focus
        } else {
            roles.text_body
        };
        put(
            cells,
            &mut x,
            first + 1 + usize::from(plan.role),
            &header.role,
            header.widths.role,
            (role_color, bold),
            look,
        );
        space(cells, &mut x, look);
    }
    put(cells, &mut x, end, glyph, 1, (glyph_color, bold), look);
    if plan.member > 0 {
        space(cells, &mut x, look);
        let color = if is_focused {
            roles.focus
        } else {
            roles.text_strong
        };
        let limit = x + usize::from(plan.member);
        put(
            cells,
            &mut x,
            limit,
            &header.member,
            header.widths.member,
            (color, name_modifier),
            look,
        );
    }
    if plan.kind {
        space(cells, &mut x, look);
        let limit = x + usize::from(header.widths.kind);
        put(
            cells,
            &mut x,
            limit,
            &header.kind,
            header.widths.kind,
            muted,
            look,
        );
        if let (true, Some(model)) = (plan.model, header.model.as_deref()) {
            put(cells, &mut x, end, chrome.glyphs.separator, 1, muted, look);
            let limit = x + usize::from(header.widths.model);
            put(
                cells,
                &mut x,
                limit,
                model,
                header.widths.model,
                muted,
                look,
            );
        }
    }
    if plan.title > 0 {
        if let Some(title) = header.title.as_deref() {
            space(cells, &mut x, look);
            put(cells, &mut x, end, chrome.glyphs.separator, 1, muted, look);
            space(cells, &mut x, look);
            let limit = x + usize::from(plan.title);
            put(
                cells,
                &mut x,
                limit,
                title,
                header.widths.title,
                (roles.text_subtle, Modifier::empty()),
                look,
            );
        }
    }
    // Right-aligned: the step line, then the age, each after one space and a
    // closing space before the corner.
    let right_total = usize::from(if plan.step { header.widths.step + 1 } else { 0 })
        + usize::from(if plan.age { age_width + 2 } else { 0 });
    let mut r = end.saturating_sub(right_total);
    if r >= x {
        if plan.step {
            if let Some(step) = header.step.as_deref() {
                space(cells, &mut r, look);
                let limit = r + usize::from(header.widths.step);
                put(cells, &mut r, limit, step, header.widths.step, muted, look);
            }
        }
        if let (true, Some(age)) = (plan.age, age) {
            space(cells, &mut r, look);
            put(cells, &mut r, end, age, age_width, muted, look);
            space(cells, &mut r, look);
        }
    }
}

#[cfg(test)]
mod phase6_ascii_tests {
    use super::*;

    // Characterization of the pre-optimization grapheme loop, kept independent
    // of the production fast path so clipping cannot bless a boundary change.
    fn original_put(
        cells: &mut [CellData],
        x: &mut usize,
        limit: usize,
        text: &str,
        text_width: u16,
        (fg, modifier): (Color, Modifier),
        look: Look,
    ) {
        let available = limit.saturating_sub(*x);
        let fits = usize::from(text_width) <= available;
        let budget = if fits {
            available
        } else {
            available.saturating_sub(1)
        };
        let end = *x + budget;
        let fg = color_to_u32(fg);
        let bg = look.bg.map(color_to_u32);
        let modifier = modifier_to_u16(modifier);
        for grapheme in text.graphemes(true) {
            let width = if grapheme.len() == 1 && (b' '..=b'~').contains(&grapheme.as_bytes()[0]) {
                1
            } else {
                UnicodeWidthStr::width(grapheme)
            };
            if width == 0 {
                continue;
            }
            if *x + width > end {
                break;
            }
            set_cell(&mut cells[*x], grapheme, fg, bg, modifier);
            for filler in 1..width {
                set_cell(&mut cells[*x + filler], " ", fg, bg, modifier);
            }
            *x += width;
        }
        if !fits && available >= 1 {
            set_cell(&mut cells[*x], look.ellipsis, fg, bg, modifier);
            *x += 1;
        }
    }

    #[test]
    fn ascii_header_clipping_matches_original_grapheme_boundaries_without_allocating() {
        let texts = [
            "".to_owned(),
            "ASCII field".into(),
            "a".repeat(40_000),
            "cafe\u{301} next".into(),
            "#\u{fe0f}\u{20e3} digits".into(),
            "👩‍👩‍👧‍👦 family".into(),
            "☀\u{fe0f} sunshine".into(),
            "界 and ASCII".into(),
            "a\r\nb".into(),
            "\u{301}word".into(),
        ];
        let cells = || {
            (0..80)
                .map(|_| {
                    let mut symbol = String::with_capacity(64);
                    symbol.push('─');
                    CellData {
                        symbol,
                        fg: 0,
                        bg: 0,
                        modifier: 0,
                        skip: false,
                        hyperlink: None,
                    }
                })
                .collect::<Vec<_>>()
        };
        for text in texts {
            let width = display_width(&text);
            for available in 0..65 {
                for ellipsis in ["…", "~"] {
                    let mut expected = cells();
                    let mut actual = cells();
                    let mut old_x = 3;
                    let mut new_x = 3;
                    let look = Look {
                        bg: Some(Color::Black),
                        ellipsis,
                    };
                    original_put(
                        &mut expected,
                        &mut old_x,
                        3 + available,
                        &text,
                        width,
                        (Color::White, Modifier::BOLD),
                        look,
                    );
                    let allocations = crate::client::shell::tests::draw_allocations(|| {
                        put(
                            &mut actual,
                            &mut new_x,
                            3 + available,
                            &text,
                            width,
                            (Color::White, Modifier::BOLD),
                            look,
                        )
                    });
                    assert_eq!(allocations, 0);
                    assert_eq!(old_x, new_x, "{text:?}/{available}");
                    assert_eq!(expected, actual, "{text:?}/{available}");
                }
            }
        }
    }
}
