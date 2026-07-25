//! Terminal display measurements. Internal text offsets may stay in bytes,
//! but every visible width must go through the Unicode cells of the terminal.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub(crate) fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

pub(crate) fn truncate(s: &str, max_width: usize) -> String {
    if width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let ellipsis = "…";
    let target = max_width.saturating_sub(width(ellipsis));
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = char_width(ch);
        if used.saturating_add(w) > target {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push_str(ellipsis);
    out
}

pub(crate) fn pad_right(mut s: String, target_width: usize) -> String {
    let w = width(&s);
    if target_width > w {
        s.push_str(&" ".repeat(target_width - w));
    }
    s
}
