//! The Python codecs that show up in mail, with Python's error handling.
//!
//! Python looks a charset up with `codecs.lookup`, which normalises the name and then walks its
//! alias table. I support the charsets that make up nearly all mail. Anything else is unknown,
//! which is what the callers then handle (mostly by falling back to ASCII, as Python does).

use super::pystr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Codec {
    Utf8,
    Ascii,
    Latin1,
    Cp1252,
    Iso8859_15,
}

/// `encodings.normalize_encoding` followed by the alias lookup.
pub(super) fn lookup(name: &str) -> Option<Codec> {
    let lowered = name.to_lowercase();
    let mut norm = String::new();
    let mut punct = false;
    for c in lowered.chars() {
        if c.is_alphanumeric() || c == '.' {
            if punct && !norm.is_empty() {
                norm.push('_');
            }
            if c.is_ascii() {
                norm.push(c);
            }
            punct = false;
        } else {
            punct = true;
        }
    }
    let find = |n: &str| -> Option<Codec> {
        Some(match n {
            "utf_8" | "utf8" | "u8" | "utf" | "utf8_ucs2" | "utf8_ucs4" | "cp65001" => Codec::Utf8,
            "ascii" | "646" | "ansi_x3.4_1968" | "ansi_x3_4_1968" | "ansi_x3.4_1986" | "cp367"
            | "csascii" | "ibm367" | "iso646_us" | "iso_646.irv_1991" | "iso_ir_6" | "us"
            | "us_ascii" => Codec::Ascii,
            "latin_1" | "8859" | "cp819" | "csisolatin1" | "ibm819" | "iso8859" | "iso8859_1"
            | "iso_8859_1" | "iso_8859_1_1987" | "iso_ir_100" | "l1" | "latin" | "latin1" => {
                Codec::Latin1
            }
            "cp1252" | "windows_1252" | "1252" => Codec::Cp1252,
            "iso8859_15" | "iso_8859_15" | "l9" | "latin9" => Codec::Iso8859_15,
            _ => return None,
        })
    };
    find(&norm).or_else(|| find(&norm.replace('.', "_")))
}

const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20ac}'),
    None,
    Some('\u{201a}'),
    Some('\u{0192}'),
    Some('\u{201e}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02c6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017d}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201c}'),
    Some('\u{201d}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02dc}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203a}'),
    Some('\u{0153}'),
    None,
    Some('\u{017e}'),
    Some('\u{0178}'),
];

fn single_byte(codec: Codec, b: u8) -> Option<char> {
    match codec {
        Codec::Ascii => (b < 0x80).then_some(b as char),
        Codec::Latin1 => Some(b as char),
        Codec::Cp1252 => match b {
            0x80..=0x9f => CP1252_HIGH[usize::from(b - 0x80)],
            _ => Some(b as char),
        },
        Codec::Iso8859_15 => Some(match b {
            0xa4 => '\u{20ac}',
            0xa6 => '\u{0160}',
            0xa8 => '\u{0161}',
            0xb4 => '\u{017d}',
            0xb8 => '\u{017e}',
            0xbc => '\u{0152}',
            0xbd => '\u{0153}',
            0xbe => '\u{0178}',
            _ => b as char,
        }),
        Codec::Utf8 => unreachable!("utf-8 is not a single-byte codec"),
    }
}

/// What to do with bytes the codec can't decode, like Python's `errors=` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Errors {
    Strict,
    Replace,
    SurrogateEscape,
}

/// `bytes.decode(codec, errors)`. `None` only for `Errors::Strict` on undecodable input.
pub(super) fn decode(data: &[u8], codec: Codec, errors: Errors) -> Option<String> {
    let bad = |out: &mut String, b: u8| -> bool {
        match errors {
            Errors::Strict => false,
            Errors::Replace => {
                out.push('\u{fffd}');
                true
            }
            Errors::SurrogateEscape => {
                out.push(pystr::escape_byte(b));
                true
            }
        }
    };
    let mut out = String::with_capacity(data.len());
    if codec == Codec::Utf8 {
        let mut rest = data;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    return Some(out);
                }
                Err(e) => {
                    let (good, after) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(good).expect("valid prefix"));
                    let n = e.error_len().unwrap_or(after.len());
                    match errors {
                        Errors::Strict => return None,
                        // One U+FFFD per maximal invalid subsequence, as Python does.
                        Errors::Replace => out.push('\u{fffd}'),
                        Errors::SurrogateEscape => {
                            out.extend(after[..n].iter().map(|&b| pystr::escape_byte(b)))
                        }
                    }
                    rest = &after[n..];
                }
            }
        }
    }
    for &b in data {
        match single_byte(codec, b) {
            Some(c) => out.push(c),
            None => {
                if !bad(&mut out, b) {
                    return None;
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_normalise_like_python() {
        assert_eq!(lookup("UTF-8"), Some(Codec::Utf8));
        assert_eq!(lookup(" utf8 "), Some(Codec::Utf8));
        assert_eq!(lookup("US-ASCII"), Some(Codec::Ascii));
        assert_eq!(lookup("ISO-8859-1"), Some(Codec::Latin1));
        assert_eq!(lookup("iso8859-15"), Some(Codec::Iso8859_15));
        assert_eq!(lookup("Windows-1252"), Some(Codec::Cp1252));
        assert_eq!(lookup("x-unknown"), None);
    }

    #[test]
    fn replace_and_escape() {
        assert_eq!(decode(b"a\x81b", Codec::Cp1252, Errors::Replace).unwrap(), "a\u{fffd}b");
        assert_eq!(decode(b"a\x81b", Codec::Cp1252, Errors::Strict), None);
        assert_eq!(decode(b"\xe2\x82", Codec::Utf8, Errors::Replace).unwrap(), "\u{fffd}");
        let esc = decode(b"x\xff", Codec::Utf8, Errors::SurrogateEscape).unwrap();
        assert_eq!(pystr::to_bytes(&esc), b"x\xff");
    }
}
