//! Telling a stored message from any other object in the bucket, from the first bytes alone.
//!
//! An object this says no to vanishes from the inbox, so it leans towards yes (N31). The header
//! syntax is read tolerantly: a byte order mark or blank lines in front, blanks before a colon
//! (RFC 5322's obsolete syntax), and one or two lines that aren't headers at all all pass. What
//! it asks for instead is evidence of mail: two distinct mail headers, one of them a From with an
//! address in it or a header only a mail server adds. A prefix that stops before the headers end
//! gets the benefit of the doubt with one mail header, as long as every line so far is a header.
//! The SES setup notice is shaped like mail but isn't anyone's message, so it's turned away by its
//! subject, decoded first so an encoded subject can't slip past.

use std::collections::HashSet;

use mail_parser::MessageParser;

/// Headers that turn up in stored mail.
const MAIL_HEADERS: &[&str] = &[
    "return-path",
    "received",
    "delivered-to",
    "x-original-to",
    "envelope-to",
    "from",
    "sender",
    "reply-to",
    "to",
    "cc",
    "bcc",
    "subject",
    "date",
    "message-id",
    "in-reply-to",
    "references",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    "dkim-signature",
    "authentication-results",
    "received-spf",
    "arc-seal",
    "x-ses-receipt",
    "x-received",
    "resent-from",
    "resent-to",
    "resent-date",
    "list-id",
    "x-mailer",
    "user-agent",
];

/// Headers only a mail server adds on delivery, which on their own mark an object as mail.
const SERVER_HEADERS: &[&str] = &["received", "return-path", "delivered-to", "x-original-to"];

/// Lines that aren't headers, tolerated inside a header block before the object counts as
/// something else.
const MAX_STRAY_LINES: usize = 2;

/// Split into lines on CRLF, LF or a bare CR, reporting whether the last line was terminated.
fn lines(data: &[u8]) -> Vec<(&[u8], bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                out.push((&data[start..i], true));
                start = i + 1;
            }
            b'\r' => {
                out.push((&data[start..i], true));
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < data.len() {
        out.push((&data[start..], false));
    }
    out
}

/// A header's name, if `line` is one: printable ASCII up to the colon, allowing blanks just
/// before it.
fn header_name(line: &[u8]) -> Option<String> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name = line[..colon].trim_ascii_end();
    let valid = !name.is_empty() && name.iter().all(|b| (0x21..=0x7e).contains(b));
    valid.then(|| String::from_utf8_lossy(name).to_ascii_lowercase())
}

pub(super) fn looks_like_email(prefix: &[u8]) -> bool {
    // A byte order mark and blank lines in front don't count against it.
    let data = prefix.strip_prefix(b"\xef\xbb\xbf").unwrap_or(prefix);
    let start = data
        .iter()
        .position(|&b| b != b'\r' && b != b'\n')
        .unwrap_or(data.len());
    let data = &data[start..];

    // Read the header block line by line, keeping the names and counting what isn't a header.
    let mut names: HashSet<String> = HashSet::new();
    let mut from_has_address = false;
    let mut stray = 0;
    let mut complete = false;
    let mut header_end = data.len();
    let all = lines(data);
    for (n, &(line, terminated)) in all.iter().enumerate() {
        if line.is_empty() {
            complete = true;
            header_end = line.as_ptr() as usize - data.as_ptr() as usize;
            break;
        }
        // An mbox "From " envelope line may come first.
        if n == 0 && line.starts_with(b"From ") {
            continue;
        }
        // Control characters mean a binary file, whatever else is there.
        if line.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f) {
            return false;
        }
        // A continuation line belongs to the header before it.
        if matches!(line[0], b' ' | b'\t') {
            if names.is_empty() && stray == 0 {
                return false;
            }
            continue;
        }
        match header_name(line) {
            Some(name) => {
                if name == "from" && line.contains(&b'@') {
                    from_has_address = true;
                }
                names.insert(name);
            }
            // A header name cut off by the end of the prefix hasn't reached its colon yet.
            None if !terminated => {}
            None => stray += 1,
        }
        if stray > MAX_STRAY_LINES {
            return false;
        }
    }

    // Weigh the evidence: two mail headers and an anchor for a whole header block, one mail
    // header for a clean prefix that stops inside it.
    let known = names.iter().filter(|n| MAIL_HEADERS.contains(&n.as_str())).count();
    let anchored = from_has_address || names.iter().any(|n| SERVER_HEADERS.contains(&n.as_str()));
    let plausible = if complete || stray > 0 {
        known >= 2 && anchored
    } else {
        known >= 1
    };
    if !plausible {
        return false;
    }

    // The SES setup notice has no Received header and a subject that gives it away.
    if !names.contains("received") {
        let subject = MessageParser::new()
            .parse_headers(&data[..header_end])
            .and_then(|m| m.subject().map(str::to_string))
            .unwrap_or_default();
        if subject.trim().eq_ignore_ascii_case("Amazon SES Setup Notification") {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Line splitting knows every ending, and whether the prefix stopped mid-line.
    #[test]
    fn line_endings() {
        assert_eq!(
            lines(b"a\r\nb\rc\nd"),
            [(&b"a"[..], true), (&b"b"[..], true), (&b"c"[..], true), (&b"d"[..], false)]
        );
    }

    #[test]
    fn header_names() {
        assert_eq!(header_name(b"Subject : x").as_deref(), Some("subject"));
        assert_eq!(header_name(b"X-Broken-Mailer v1.0"), None);
        assert_eq!(header_name(b"two words: x"), None);
        assert_eq!(header_name(b": x"), None);
    }
}
