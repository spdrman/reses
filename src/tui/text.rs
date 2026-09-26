//! Small text helpers every screen shares, so sizes and truncation look the same everywhere.

use ratatui::text::Span;

/// Columns a character takes on screen.
pub fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// Columns a string takes on screen.
pub fn width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Exactly `cols` columns: padded with spaces, or cut with an ellipsis.
pub fn fit(s: &str, cols: usize) -> String {
    let total = width(s);
    if total <= cols {
        return format!("{s}{}", " ".repeat(cols - total));
    }
    if cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > cols - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    used += 1;
    out.push_str(&" ".repeat(cols - used));
    out
}

/// Header text on one line: control characters (folded headers, tabs) become spaces.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Stub for the red tests: nothing is escaped yet.
pub fn escape(s: &str) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Borrowed(s)
}

/// Stub for the red tests: nothing is escaped yet.
pub fn escape_text(s: &str) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Borrowed(s)
}

/// Widest `human_size` output, for right-aligned columns.
pub const SIZE_WIDTH: usize = 9;

/// Bytes in binary units, never wider than `SIZE_WIDTH`: "512 B", "2.0 KiB", "999.9 KiB",
/// "1.0 MiB".
pub fn human_size(n: u64) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    for unit in ["KiB", "MiB", "GiB", "TiB", "PiB"] {
        // Round first, so 999.95 moves up a unit rather than printing as "1000.0".
        if (v * 10.0).round() / 10.0 < 1000.0 {
            return format!("{v:.1} {unit}");
        }
        v /= 1024.0;
    }
    format!("{v:.1} EiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units_and_stay_narrow() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 KiB");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        for n in [
            999,
            1_023_900,
            1_023_999,
            1_048_575,
            1_048_523_571,
            u64::MAX,
        ] {
            let s = human_size(n);
            assert!(s.len() <= SIZE_WIDTH, "{n} -> {s:?}");
            assert!(!s.starts_with("1000"), "{n} -> {s:?}");
        }
    }

    #[test]
    fn fit_pads_or_cuts_to_the_exact_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 0), "");
        // Wide characters count double and never overshoot.
        assert_eq!(width(&fit("日本語テキスト", 5)), 5);
        assert_eq!(width(&fit("日本語", 6)), 6);
    }

    #[test]
    fn clean_flattens_control_characters() {
        assert_eq!(clean("a\r\n\tb"), "a   b");
    }
}
