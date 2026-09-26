//! reses.py's `html_to_text` and the `html.unescape` it ends with, done by hand so that each
//! regular expression matches what Python's `re` would.

use super::entities::HTML5;
use super::pystr;

/// Case-insensitive match of a text character against an ASCII lowercase pattern letter, with
/// the extra equivalences Python's `re.IGNORECASE` applies to str patterns.
fn ci_eq(pattern: char, c: char) -> bool {
    c.to_ascii_lowercase() == pattern
        || matches!(
            (pattern, c),
            ('s', '\u{17f}') | ('i', '\u{131}') | ('i', '\u{130}') | ('k', '\u{212a}')
        )
}

fn ci_word_at(text: &[char], at: usize, word: &str) -> bool {
    let n = word.chars().count();
    at + n <= text.len()
        && word
            .chars()
            .zip(&text[at..at + n])
            .all(|(p, &c)| ci_eq(p, c))
}

/// `re.sub(r"(?is)<(script|style).*?</\1>", "", s)`.
fn drop_script_style(text: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < text.len() {
        if text[i] == '<' {
            for word in ["script", "style"] {
                if ci_word_at(text, i + 1, word) {
                    let body = i + 1 + word.len();
                    let mut j = body;
                    while j + 1 < text.len() {
                        if text[j] == '<'
                            && text[j + 1] == '/'
                            && ci_word_at(text, j + 2, word)
                            && text.get(j + 2 + word.len()) == Some(&'>')
                        {
                            i = j + 3 + word.len();
                            continue 'outer;
                        }
                        j += 1;
                    }
                }
            }
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

/// `re.sub(r"(?i)<br\s*/?>", "\n", s)`.
fn br_to_newline(text: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == '<' && ci_word_at(text, i + 1, "br") {
            let mut j = i + 3;
            while j < text.len() && pystr::is_space(text[j]) {
                j += 1;
            }
            if j < text.len() && text[j] == '/' {
                j += 1;
            }
            if j < text.len() && text[j] == '>' {
                out.push('\n');
                i = j + 1;
                continue;
            }
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

/// `re.sub(r"(?i)</(p|div|tr|li|h[1-6])>", "\n", s)`.
fn block_ends_to_newline(text: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == '<' && text.get(i + 1) == Some(&'/') {
            let at = i + 2;
            let mut len = None;
            for word in ["p", "div", "tr", "li"] {
                if ci_word_at(text, at, word) && text.get(at + word.len()) == Some(&'>') {
                    len = Some(word.len());
                    break;
                }
            }
            if len.is_none()
                && text.get(at).is_some_and(|&c| ci_eq('h', c))
                && text.get(at + 1).is_some_and(|c| ('1'..='6').contains(c))
                && text.get(at + 2) == Some(&'>')
            {
                len = Some(2);
            }
            if let Some(n) = len {
                out.push('\n');
                i = at + n + 1;
                continue;
            }
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

/// `re.sub(r"<[^>]+>", "", s)`.
fn drop_tags(text: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == '<'
            && let Some(close) = text[i + 1..].iter().position(|&c| c == '>')
            && close > 0
        {
            i += close + 2;
            continue;
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

fn invalid_charref(n: u64) -> Option<&'static str> {
    Some(match n {
        0x00 => "\u{fffd}",
        0x0d => "\r",
        0x80 => "\u{20ac}",
        0x81 => "\u{81}",
        0x82 => "\u{201a}",
        0x83 => "\u{0192}",
        0x84 => "\u{201e}",
        0x85 => "\u{2026}",
        0x86 => "\u{2020}",
        0x87 => "\u{2021}",
        0x88 => "\u{02c6}",
        0x89 => "\u{2030}",
        0x8a => "\u{0160}",
        0x8b => "\u{2039}",
        0x8c => "\u{0152}",
        0x8d => "\u{8d}",
        0x8e => "\u{017d}",
        0x8f => "\u{8f}",
        0x90 => "\u{90}",
        0x91 => "\u{2018}",
        0x92 => "\u{2019}",
        0x93 => "\u{201c}",
        0x94 => "\u{201d}",
        0x95 => "\u{2022}",
        0x96 => "\u{2013}",
        0x97 => "\u{2014}",
        0x98 => "\u{02dc}",
        0x99 => "\u{2122}",
        0x9a => "\u{0161}",
        0x9b => "\u{203a}",
        0x9c => "\u{0153}",
        0x9d => "\u{9d}",
        0x9e => "\u{017e}",
        0x9f => "\u{0178}",
        _ => return None,
    })
}

fn invalid_codepoint(n: u64) -> bool {
    matches!(n, 0x1..=0x8 | 0xb | 0xe..=0x1f | 0x7f..=0x9f | 0xfdd0..=0xfdef)
        || (n & 0xfffe == 0xfffe && n <= 0x10ffff)
}

fn entity(name: &str) -> Option<&'static str> {
    HTML5
        .binary_search_by(|(k, _)| k.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|i| HTML5[i].1)
}

fn numeric(digits: &[char], radix: u32) -> String {
    let mut n: u64 = 0;
    for c in digits {
        n = n
            .saturating_mul(u64::from(radix))
            .saturating_add(u64::from(c.to_digit(radix).unwrap()));
    }
    if let Some(s) = invalid_charref(n) {
        return s.to_string();
    }
    if (0xd800..=0xdfff).contains(&n) || n > 0x10ffff {
        return "\u{fffd}".into();
    }
    if invalid_codepoint(n) {
        return String::new();
    }
    char::from_u32(n as u32)
        .map(String::from)
        .unwrap_or_default()
}

/// `html.unescape`.
pub(super) fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let text: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] != '&' {
            out.push(text[i]);
            i += 1;
            continue;
        }
        let at = i + 1;
        if text.get(at) == Some(&'#') {
            let dec = text[at + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if dec > 0 {
                out.push_str(&numeric(&text[at + 1..at + 1 + dec], 10));
                i = at + 1 + dec;
                if text.get(i) == Some(&';') {
                    i += 1;
                }
                continue;
            }
            if matches!(text.get(at + 1), Some('x' | 'X')) {
                let hex = text[at + 2..]
                    .iter()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .count();
                if hex > 0 {
                    out.push_str(&numeric(&text[at + 2..at + 2 + hex], 16));
                    i = at + 2 + hex;
                    if text.get(i) == Some(&';') {
                        i += 1;
                    }
                    continue;
                }
            }
            out.push('&');
            i += 1;
            continue;
        }
        let run = text[at..]
            .iter()
            .take(32)
            .take_while(|&&c| !matches!(c, '\t' | '\n' | '\u{c}' | ' ' | '<' | '&' | '#' | ';'))
            .count();
        if run == 0 {
            out.push('&');
            i += 1;
            continue;
        }
        let mut end = at + run;
        if text.get(end) == Some(&';') {
            end += 1;
        }
        let name: String = text[at..end].iter().collect();
        i = end;
        if let Some(v) = entity(&name) {
            out.push_str(v);
            continue;
        }
        let chars: Vec<char> = name.chars().collect();
        let mut done = false;
        for x in (2..chars.len()).rev() {
            let prefix: String = chars[..x].iter().collect();
            if let Some(v) = entity(&prefix) {
                out.push_str(v);
                out.extend(&chars[x..]);
                done = true;
                break;
            }
        }
        if !done {
            out.push('&');
            out.push_str(&name);
        }
    }
    out
}

/// reses.py's html_to_text.
pub(super) fn html_to_text(markup: &str) -> String {
    let text: Vec<char> = markup.chars().collect();
    let text = drop_script_style(&text);
    let text = br_to_newline(&text);
    let text = block_ends_to_newline(&text);
    let text: String = drop_tags(&text).into_iter().collect();
    let text = unescape(&text);
    // re.sub(r"\n{3,}", "\n\n", text)
    let mut out = String::with_capacity(text.len());
    let mut run = 0;
    for c in text.chars() {
        if c == '\n' {
            run += 1;
            continue;
        }
        if run > 0 {
            out.push_str(if run >= 2 { "\n\n" } else { "\n" });
            run = 0;
        }
        out.push(c);
    }
    if run > 0 {
        out.push_str(if run >= 2 { "\n\n" } else { "\n" });
    }
    pystr::strip(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_like_python() {
        assert_eq!(unescape("&amp;&lt;&gt;&quot;&#39;"), "&<>\"'");
        assert_eq!(
            unescape("&notit; &amp &ampx &bogus; & &#; &#x;"),
            "¬it; & &x &bogus; & &#; &#x;"
        );
        assert_eq!(
            unescape("&#128; &#0; &#xD800; &#1; &#99999999999;"),
            "€ \u{fffd} \u{fffd}  \u{fffd}"
        );
        assert_eq!(unescape("&#x263A;&#9786"), "☺☺");
    }

    #[test]
    fn converts_markup() {
        assert_eq!(
            html_to_text("<p>one</p><p>two &amp; three</p>\r\n"),
            "one\ntwo & three"
        );
        assert_eq!(
            html_to_text("a<SCRIPT>x</script>b<style a>y</STYLE>c"),
            "abc"
        );
        assert_eq!(html_to_text("a<br>b<BR />c<br\n/>d<bra>e"), "a\nb\nc\nde");
        assert_eq!(html_to_text("a<script>never closed"), "anever closed");
        assert_eq!(html_to_text("x\n\n\n\ny<>z"), "x\n\ny<>z");
    }
}
