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
use crate::client::shell::team_sidebar::{TeamRenderContext, TeamSidebar};
use crate::client::shell::PaneHit;
use crate::protocol::{color_to_u32, modifier_to_u16, CellData, FrameData};
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

fn is_stop(symbol: &str) -> bool {
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
    let span = u16::try_from(end.saturating_sub(first)).unwrap_or(0);

    let chrome = &ctx.config.chrome;
    let roles = &chrome.roles;
    let palette = &ctx.config.palette;
    let is_focused = ctx.focused_pane_id == Some(hit.pane_id.as_str());
    let (glyph, glyph_style, phase) = super::state_glyph(ctx, header.state, &hit.pane_id);
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
    let look = Look {
        bg,
        ellipsis: chrome.glyphs.ellipsis,
    };
    if let Some(bg) = bg {
        for cell in &mut cells[first..end] {
            cell.bg = color_to_u32(bg);
        }
    }

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
            (color, bold),
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
