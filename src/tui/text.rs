//! Small text helpers every screen shares, so sizes, truncation and escaping look the same
//! everywhere.
//!
//! Widths are measured per grapheme with the same unicode-width rules ratatui uses to lay out
//! cells. Measuring per `char` made an emoji with a variation selector (❤️) count one column
//! while the terminal drew two, and every column after it slid over.
//!
//! Escaping is the one place anything S3 or a message hands us gets made safe for a terminal.
//! An object key can carry an escape sequence (OSC 52 writes the clipboard, CSI moves the
//! cursor and can repaint the delete confirmation), so a key, bucket, prefix or error string
//! never reaches a widget or stdout without going through here first.

use std::borrow::Cow;
use std::fmt::Write as _;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Columns a character takes on screen.
pub fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    UnicodeWidthStr::width(&*c.encode_utf8(&mut buf))
}

/// Columns a string takes on screen, grapheme by grapheme, as ratatui lays it out.
pub fn width(s: &str) -> usize {
    s.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// Exactly `cols` columns: padded with spaces, or cut with an ellipsis. It cuts between
/// graphemes, so an emoji or an accented letter never gets split in half.
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
    for g in s.graphemes(true) {
        let w = UnicodeWidthStr::width(g);
        if used + w > cols - 1 {
            break;
        }
        out.push_str(g);
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

/// Characters a terminal acts on or that change how the text around them reads: C0 and C1
/// controls, DEL, and the bidi and invisible formatting characters that can make two
/// different keys look the same.
fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200b}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// The one escaping function. Every character that needs it is written out visibly: C0,
/// DEL and C1 as `\x1b`, the rest as `\u{202e}`. `keep` names the controls to leave alone
/// (newline and tab, for a message body). With `strict`, a backslash is doubled too, so an
/// escaped key and a key that really contains the text `\x1b` can never look alike.
fn escape_with<'s>(s: &'s str, keep: &[char], strict: bool) -> Cow<'s, str> {
    let dirty = |c: char| (needs_escape(c) && !keep.contains(&c)) || (strict && c == '\\');
    if !s.chars().any(dirty) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if !dirty(c) {
            out.push(c);
        } else if c == '\\' {
            out.push_str("\\\\");
        } else if (c as u32) < 0x100 {
            let _ = write!(out, "\\x{:02x}", c as u32);
        } else {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
    }
    Cow::Owned(out)
}

/// A key, bucket, prefix, title, error or status message, made safe for one line of a
/// terminal: every control visibly escaped, backslashes doubled.
pub fn escape(s: &str) -> Cow<'_, str> {
    escape_with(s, &[], true)
}

/// Multi-line text for a terminal (a decoded message on stdout): the same escaping, keeping
/// newlines and tabs, and leaving backslashes as they were written.
pub fn escape_text(s: &str) -> Cow<'_, str> {
    escape_with(s, &['\n', '\t'], false)
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
    fn emoji_with_a_variation_selector_count_as_the_terminal_draws_them() {
        // U+2764 then U+FE0F: one grapheme, drawn two columns wide.
        let heart = "\u{2764}\u{fe0f}";
        assert_eq!(width(heart), 2);
        assert_eq!(width(&format!("a{heart}b")), 4);
        // ratatui lays the same string out in the same number of cells.
        let line = ratatui::text::Line::raw(format!("a{heart}b"));
        assert_eq!(line.width(), width(&format!("a{heart}b")));
        // Cutting never splits the grapheme or overshoots the column count.
        for cols in 1..8 {
            let cut = fit(&heart.repeat(3), cols);
            assert_eq!(width(&cut), cols, "{cols}: {cut:?}");
            assert!(
                !cut.contains("\u{2764} ") && !cut.starts_with('\u{fe0f}'),
                "{cut:?}"
            );
        }
        // A family emoji joined with ZWJ is one grapheme too.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(width(family), ratatui::text::Line::raw(family).width());
    }

    #[test]
    fn clean_flattens_control_characters() {
        assert_eq!(clean("a\r\n\tb"), "a   b");
    }

    #[test]
    fn escape_writes_every_terminal_control_out_visibly() {
        // OSC 52 would write the clipboard; CSI 2J would clear the screen.
        let osc52 = "mail/\u{1b}]52;c;aGk=\u{7}.eml";
        assert_eq!(escape(osc52), "mail/\\x1b]52;c;aGk=\\x07.eml");
        assert_eq!(escape("a\u{1b}[2Jb"), "a\\x1b[2Jb");
        // C1 CSI, DEL, and line breaks are escaped too on a single line.
        assert_eq!(escape("a\u{9b}31mb"), "a\\x9b31mb");
        assert_eq!(escape("a\u{7f}b"), "a\\x7fb");
        assert_eq!(escape("a\nb\r\tc"), "a\\x0ab\\x0d\\x09c");
        // Bidi overrides and invisible characters can make two keys read alike.
        assert_eq!(escape("invoice\u{202e}fdp.exe"), "invoice\\u{202e}fdp.exe");
        assert_eq!(escape("a\u{200b}b"), "a\\u{200b}b");
        assert_eq!(escape("a\u{feff}b"), "a\\u{feff}b");
        // Ordinary text, including emoji joined with ZWJ, comes back untouched and unallocated.
        for plain in [
            "mail/2026-09-25 Invoice.eml",
            "日本語",
            "\u{1f468}\u{200d}\u{1f469}",
        ] {
            assert!(matches!(escape(plain), Cow::Borrowed(_)), "{plain}");
        }
    }

    #[test]
    fn two_different_keys_never_escape_to_the_same_text() {
        let a = "mail/a\u{1b}b";
        let b = "mail/a\\x1bb";
        assert_ne!(escape(a), escape(b));
        assert_eq!(escape(b), "mail/a\\\\x1bb");
    }

    #[test]
    fn escape_text_keeps_the_shape_of_a_message() {
        let body = "Subject: hi \u{1b}]0;owned\u{7}\nline two\tindented\nC:\\path";
        assert_eq!(
            escape_text(body),
            "Subject: hi \\x1b]0;owned\\x07\nline two\tindented\nC:\\path"
        );
        // A bare carriage return could overwrite the line it's on.
        assert_eq!(escape_text("safe\rEVIL"), "safe\\x0dEVIL");
        assert!(matches!(escape_text("plain\ntext\n"), Cow::Borrowed(_)));
    }
}
