//! Cell writers shared by classic and modern chrome. Modern inline writers
//! bound symbols to cell storage; classic writers retain ratatui representation.
//! Neither path locks or reads the clock.

use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Bytes a cell holds inline; a longer symbol would make the cell allocate.
const INLINE_SYMBOL_BYTES: usize = std::mem::size_of::<String>();

/// The symbol written for `grapheme`: itself when it fits a cell inline, else
/// its first scalar, so painting never allocates (a long ZWJ sequence degrades
/// to its first emoji instead of a heap-backed cell).
fn inline_symbol(grapheme: &str) -> &str {
    if grapheme.len() <= INLINE_SYMBOL_BYTES {
        return grapheme;
    }
    let wanted = display_width(grapheme);
    for (start, scalar) in grapheme.char_indices() {
        let end = start + scalar.len_utf8();
        if end > INLINE_SYMBOL_BYTES {
            break;
        }
        let prefix = &grapheme[..end];
        if display_width(prefix) == wanted {
            return prefix;
        }
    }
    grapheme
        .chars()
        .next()
        .map_or("", |first| &grapheme[..first.len_utf8()])
}

pub(crate) fn display_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}

/// Classic writer: preserve ratatui's full grapheme representation exactly.
pub(crate) fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 || y >= buffer.area.bottom() || x >= buffer.area.right() {
        return;
    }
    buffer.set_stringn(x, y, text, width as usize, style);
}

/// Classic segment writer; modern surfaces use `put_segment_inline`.
pub(crate) fn put_segment(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    text: &str,
    style: Style,
) -> u16 {
    let width = display_width(text).min(right.saturating_sub(x));
    put_text(buffer, x, y, width, text, style);
    x.saturating_add(width)
}

/// Writes at most `width` cells of `text` at `(x, y)`; clipped to the buffer.
fn put_text_inline(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
) -> u16 {
    if width == 0 || y >= buffer.area.bottom() || x >= buffer.area.right() {
        return x;
    }
    let right = (u32::from(x) + u32::from(width)).min(u32::from(buffer.area.right())) as u16;
    let mut x = x;
    for grapheme in text.graphemes(true) {
        if grapheme.contains(char::is_control) {
            continue;
        }
        let symbol = inline_symbol(grapheme);
        let cells = display_width(symbol);
        if cells == 0 {
            continue;
        }
        if cells > right.saturating_sub(x) {
            break;
        }
        buffer[(x, y)].set_symbol(symbol).set_style(style);
        for covered in x + 1..x + cells {
            buffer[(covered, y)].reset();
        }
        x += cells;
    }
    x
}

/// Writes `text` clipped at column `right`; returns the next free column.
pub(crate) fn put_segment_inline(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    text: &str,
    style: Style,
) -> u16 {
    put_text_inline(buffer, x, y, right.saturating_sub(x), text, style)
}

/// Writes `text` clipped at column `right`; when it does not fit, the last
/// cell is `ellipsis` instead of a hard cut. Returns the next free column.
pub(crate) fn put_elided(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    text: &str,
    ellipsis: &str,
    style: Style,
) -> u16 {
    let available = right.saturating_sub(x);
    if display_width(text) <= available || available == 0 {
        return put_segment_inline(buffer, x, y, right, text, style);
    }
    let ellipsis_width = display_width(ellipsis).min(available);
    let next = put_segment_inline(
        buffer,
        x,
        y,
        right.saturating_sub(ellipsis_width),
        text,
        style,
    );
    put_segment_inline(buffer, next, y, right, ellipsis, style)
}

/// Writes `value` in decimal without allocating; returns the next column.
pub(crate) fn put_number(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    mut value: usize,
    style: Style,
) -> u16 {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    match std::str::from_utf8(&digits[start..]) {
        Ok(text) => put_segment_inline(buffer, x, y, right, text, style),
        Err(_) => x,
    }
}

/// Writes `text` flush against the right edge of `area` on row `y`.
pub(crate) fn put_right(buffer: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let width = display_width(text).min(area.width);
    put_text(
        buffer,
        area.right().saturating_sub(width),
        y,
        width,
        text,
        style,
    );
}

/// Draws a bar of `total` cells, the first `filled` in `full`, the rest in
/// `empty`; returns the next free column.
pub(crate) fn put_bar(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    (filled, total): (u8, u8),
    (full, empty): ((&str, Style), (&str, Style)),
) -> u16 {
    let mut x = x;
    for cell in 0..total {
        let (text, style) = if cell < filled { full } else { empty };
        x = put_segment_inline(buffer, x, y, right, text, style);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_number_writes_decimal_and_clips() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let next = put_number(&mut buffer, 0, 0, 6, 1204, Style::default());
        assert_eq!(next, 4);
        let next = put_number(&mut buffer, 4, 0, 6, 99999, Style::default());
        assert_eq!(next, 6);
        let row = (0..6)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect::<String>();
        assert_eq!(row, "120499");
    }

    #[test]
    fn put_elided_marks_a_cut_and_leaves_a_fit_alone() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let style = Style::default();
        let next = put_elided(&mut buffer, 0, 0, 5, "abcdefgh", "~", style);
        assert_eq!(next, 5);
        let row = (0..5)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect::<String>();
        assert_eq!(row, "abcd~");
        let next = put_elided(&mut buffer, 0, 0, 8, "abc", "~", style);
        assert_eq!(next, 3);
    }

    #[test]
    fn put_bar_fills_and_empties_cells() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let style = Style::default();
        let next = put_bar(&mut buffer, 0, 0, 4, (2, 3), (("━", style), ("─", style)));
        assert_eq!(next, 3);
        let row = (0..3)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect::<String>();
        assert_eq!(row, "━━─");
    }
}

#[cfg(test)]
mod phase6_tests {
    use super::*;
    #[test]
    fn oversized_variation_graphemes_emit_the_width_the_writer_advances() {
        let cluster = format!("☀\u{fe0f}{}", "\u{301}".repeat(12));
        assert!(cluster.len() > INLINE_SYMBOL_BYTES);
        let mut b = Buffer::empty(Rect::new(0, 0, 8, 1));
        let end = put_segment_inline(&mut b, 0, 0, 8, &cluster, Style::default());
        assert_eq!(display_width(b[(0, 0)].symbol()), end);
        assert_eq!(end, 2);
    }
}
