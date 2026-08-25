pub(crate) fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

pub(crate) fn truncate_display(text: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_string();
    }
    let budget = max_width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

pub(super) fn pad_to_width(mut text: String, width: usize) -> String {
    let used = display_width(&text);
    if used < width {
        text.push_str(&" ".repeat(width - used));
    }
    text
}

pub(super) fn line_to_string(line: Line<'_>) -> String {
    line.spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

pub(super) fn visible_segment_range(
    text: &str,
    start: usize,
    len: usize,
) -> Option<std::ops::Range<u16>> {
    let visible_len = display_width(text);
    if start >= visible_len {
        return None;
    }
    let end = (start + len).min(visible_len);
    Some(start as u16..end as u16)
}

pub(super) fn slice_display(text: &str, start: u16, end: u16) -> String {
    let mut cell = 0_u16;
    let mut out = String::new();
    for ch in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if cell >= end {
            break;
        }
        if cell >= start && cell + width <= end {
            out.push(ch);
        }
        cell = cell.saturating_add(width);
    }
    out
}
use ratatui::text::Line;

#[cfg(test)]
mod tests;
