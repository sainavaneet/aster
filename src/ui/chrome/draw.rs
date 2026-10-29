//! Allocation-free cell writers shared by classic and modern chrome. They
//! write borrowed text and stack-formatted numbers into a buffer and never
//! allocate, lock, or read the clock.

use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use unicode_width::UnicodeWidthStr;

pub(crate) fn display_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}

/// Writes at most `width` cells of `text` at `(x, y)`; clipped to the buffer.
pub(crate) fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 || y >= buffer.area.bottom() || x >= buffer.area.right() {
        return;
    }
    buffer.set_stringn(x, y, text, width as usize, style);
}

/// Writes `text` clipped at column `right`; returns the next free column.
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
        return put_segment(buffer, x, y, right, text, style);
    }
    let ellipsis_width = display_width(ellipsis).min(available);
    let next = put_segment(
        buffer,
        x,
        y,
        right.saturating_sub(ellipsis_width),
        text,
        style,
    );
    put_segment(buffer, next, y, right, ellipsis, style)
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
        Ok(text) => put_segment(buffer, x, y, right, text, style),
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
        x = put_segment(buffer, x, y, right, text, style);
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
