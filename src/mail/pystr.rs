//! Python string behaviour the decoder depends on.
//!
//! Python's email package reads bytes as ASCII with `surrogateescape`, so a raw byte 0x80-0xFF
//! travels through header parsing as a lone surrogate and only turns into text at the very end
//! (`_sanitize`). Rust strings can't hold surrogates, so I carry each such byte as a code point
//! in the last 128 slots of plane 16 (U+10FF80..U+10FFFF). Nothing in real mail uses those, and
//! they behave like surrogates do in the parser: not whitespace, not ASCII, not a special.

const ESC_BASE: u32 = 0x10FF00;

/// A raw byte smuggled through a string, the way Python's surrogateescape does it.
pub(super) fn escape_byte(b: u8) -> char {
    if b < 0x80 {
        b as char
    } else {
        char::from_u32(ESC_BASE + u32::from(b)).expect("plane 16 code point")
    }
}

pub(super) fn unescape_char(c: char) -> Option<u8> {
    let n = c as u32;
    if (ESC_BASE + 0x80..=ESC_BASE + 0xFF).contains(&n) {
        Some((n - ESC_BASE) as u8)
    } else {
        None
    }
}

/// `bytes.decode('ascii', 'surrogateescape')`.
pub(super) fn from_bytes(data: &[u8]) -> String {
    data.iter().map(|&b| escape_byte(b)).collect()
}

/// `str.encode('utf-8', 'surrogateescape')`.
pub(super) fn to_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match unescape_char(c) {
            Some(b) => out.push(b),
            None => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    out
}

pub(super) fn has_escapes(s: &str) -> bool {
    s.chars().any(|c| unescape_char(c).is_some())
}

/// `email.utils._sanitize`: escaped bytes are re-read as UTF-8, bad sequences become U+FFFD.
pub(super) fn sanitize(s: String) -> String {
    if has_escapes(&s) {
        String::from_utf8_lossy(&to_bytes(&s)).into_owned()
    } else {
        s
    }
}

/// `str.isspace()`. Python counts the four ASCII separators (0x1C-0x1F) that Rust doesn't.
pub(super) fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub(super) fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

pub(super) fn lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

pub(super) fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `str.split()` with no arguments.
pub(super) fn split_ws(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|p| !p.is_empty())
}

/// `int(s)` for a base-10 string that has already been split on whitespace: an optional sign,
/// then ASCII digits with single underscores allowed between them. Out-of-range values are
/// reported as `None`, which callers treat the same way as Python's later range errors.
pub(super) fn py_int(s: &str) -> Option<i64> {
    let (neg, digits) = match s.as_bytes().first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    let bytes = digits.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || !bytes[bytes.len() - 1].is_ascii_digit() {
        return None;
    }
    let mut n: i64 = 0;
    let mut prev_underscore = false;
    for &b in bytes {
        if b == b'_' {
            if prev_underscore {
                return None;
            }
            prev_underscore = true;
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        prev_underscore = false;
        // Anything this large fails Python's datetime range checks anyway.
        n = n.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    Some(if neg { -n } else { n })
}

/// Python `str.isdigit()` on one character. The fields this runs on are ASCII plus escaped
/// bytes, so ASCII digits are the only digits that can show up.
pub(super) fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

/// `str.lower()`.
pub(super) fn lower(s: &str) -> String {
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_bytes_round_trip() {
        let raw = b"a\xffb\xc3\xa9";
        let s = from_bytes(raw);
        assert_eq!(s.chars().count(), 5);
        assert!(has_escapes(&s));
        assert_eq!(to_bytes(&s), raw);
        assert_eq!(sanitize(s), "a\u{fffd}bé");
    }

    #[test]
    fn python_int_rules() {
        assert_eq!(py_int("0530"), Some(530));
        assert_eq!(py_int("-0700"), Some(-700));
        assert_eq!(py_int("1_0"), Some(10));
        assert_eq!(py_int("1__0"), None);
        assert_eq!(py_int("_1"), None);
        assert_eq!(py_int(""), None);
        assert_eq!(py_int("+"), None);
        assert_eq!(py_int("12a"), None);
    }

    #[test]
    fn python_whitespace() {
        assert_eq!(strip("\u{1c} x \u{a0}"), "x");
        assert_eq!(split_ws(" a\tb  c ").collect::<Vec<_>>(), ["a", "b", "c"]);
    }
}
