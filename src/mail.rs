//! Decoding raw RFC 5322 messages. `format_message` must produce exactly what
//! `python/reses.py` prints, byte for byte; the golden files in tests/fixtures pin that.
//!
//! reses.py leans on Python's `email` package with `policy.default`, and a lot of what it prints
//! comes from that package's quirks: how address headers are re-rendered, which Date strings
//! survive, how encoded words are joined, how the parser trims the newline before a boundary.
//! So rather than approximate it with a different parser, the submodules port the parts of
//! CPython 3.11 that reses.py actually reaches, function by function.

mod codec;
mod date;
mod entities;
mod feed;
mod html;
mod hvp;
mod parseaddr;
mod pystr;
mod transfer;

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use feed::{Message, PartId, ROOT};

/// The headers the inbox list shows. Built from a header-only prefix of the object as well as
/// from a whole message, so every field has to cope with a truncated body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    /// Parsed Date header, if it parses.
    pub date: Option<OffsetDateTime>,
    /// The Date header as written.
    pub date_raw: String,
    pub message_id: String,
    pub has_attachments: bool,
}

const HEADER_ORDER: [&str; 8] = [
    "From",
    "Reply-To",
    "To",
    "Cc",
    "Bcc",
    "Date",
    "Subject",
    "Message-ID",
];

/// Addresses from every `name` header, as reses.py's `addresses()` collects them.
fn addresses(msg: &Message, name: &str) -> Vec<String> {
    parseaddr::getaddresses(&msg.part(ROOT).get_all(name))
        .into_iter()
        .map(|(_, a)| a)
        .filter(|a| !a.is_empty())
        .collect()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `RECEIVED_FOR.findall(value)` for `\bfor\s+<?([^\s<>;]+@[^\s<>;]+)>?` (case-insensitive).
fn received_for(value: &str) -> Vec<String> {
    let text: Vec<char> = value.chars().collect();
    let excluded = |c: char| pystr::is_space(c) || matches!(c, '<' | '>' | ';');
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= text.len() {
        let is_for = text[i..i + 3]
            .iter()
            .zip("for".chars())
            .all(|(&c, p)| c.to_ascii_lowercase() == p);
        if !is_for || (i > 0 && is_word(text[i - 1])) {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        let ws = text[j..]
            .iter()
            .take_while(|&&c| pystr::is_space(c))
            .count();
        if ws == 0 {
            i += 1;
            continue;
        }
        j += ws;
        if text.get(j) == Some(&'<') {
            j += 1;
        }
        let run = text[j..].iter().take_while(|&&c| !excluded(c)).count();
        let addr = &text[j..j + run];
        // The run needs an "@" with at least one character on each side.
        if run >= 3 && addr[1..run - 1].contains(&'@') {
            found.push(addr.iter().collect());
            j += run;
            if text.get(j) == Some(&'>') {
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    found
}

/// Recipients the message was delivered to, from the transport headers.
fn envelope_recipients(msg: &Message) -> Vec<String> {
    let mut found = Vec::new();
    for name in ["Delivered-To", "X-Original-To", "Envelope-To"] {
        found.extend(addresses(msg, name));
    }
    for received in msg.part(ROOT).get_all("Received") {
        found.extend(received_for(&received));
    }
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for addr in found {
        let key = pystr::lower(&addr);
        if !seen.contains(&key) {
            seen.push(key);
            out.push(addr);
        }
    }
    out
}

/// An explicit Bcc header if present, otherwise any envelope recipient not in To/Cc.
fn bcc(msg: &Message) -> String {
    if let Some(explicit) = msg.part(ROOT).get("Bcc")
        && !explicit.is_empty()
    {
        return explicit;
    }
    let mut visible: Vec<String> = addresses(msg, "To");
    visible.extend(addresses(msg, "Cc"));
    let visible: Vec<String> = visible.iter().map(|a| pystr::lower(a)).collect();
    envelope_recipients(msg)
        .into_iter()
        .filter(|a| !visible.contains(&pystr::lower(a)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn attachments(msg: &Message) -> Vec<(PartId, String)> {
    msg.iter_attachments()
        .into_iter()
        .filter_map(|p| {
            msg.part(p)
                .get_filename()
                .filter(|f| !f.is_empty())
                .map(|f| (p, f))
        })
        .collect()
}

fn body(msg: &Message, prefer_html: bool) -> String {
    let prefs: &[&str] = if prefer_html {
        &["html", "plain"]
    } else {
        &["plain", "html"]
    };
    let Some(id) = msg.get_body(prefs) else {
        return String::new();
    };
    let part = msg.part(id);
    let mut content = part.text_content();
    if feed::content_type(part).split('/').nth(1) == Some("html") && !prefer_html {
        content = html::html_to_text(&content);
    }
    pystr::strip(&content.replace("\r\n", "\n")).to_string()
}

/// The readable form: From, Reply-To, To, Cc, Bcc, Date, Subject, Message-ID, optional
/// Attachments line, "Message:", a blank line, then the body. See python/reses.py.
pub fn format_message(raw: &[u8], prefer_html: bool) -> String {
    let msg = Message::parse(raw);
    let root = msg.part(ROOT);
    let mut lines: Vec<String> = HEADER_ORDER
        .iter()
        .map(|&name| {
            let mut value = match name {
                "Bcc" => bcc(&msg),
                _ => root.get(name).unwrap_or_default(),
            };
            if name == "Date"
                && !value.is_empty()
                && let Some(dt) = date::parsedate_to_datetime(&value)
            {
                value = dt.format_reses();
            }
            format!("{name}: {}", pystr::strip(&value))
        })
        .collect();
    let files = attachments(&msg);
    if !files.is_empty() {
        let listed: Vec<String> = files
            .iter()
            .map(|(p, name)| {
                let size = msg.part(*p).decoded_payload().map_or(0, |b| b.len());
                format!("{name} ({size} bytes)")
            })
            .collect();
        lines.push(format!("Attachments: {}", listed.join(", ")));
    }
    lines.push("Message:".into());
    lines.push(String::new());
    lines.push(body(&msg, prefer_html));
    lines.join("\n") + "\n"
}

/// Where the header block ends, if it ends inside `raw`.
fn header_block_complete(raw: &[u8]) -> bool {
    let mut at_line_start = true;
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if at_line_start && (b == b'\n' || b == b'\r') {
            return true;
        }
        at_line_start = b == b'\n' || (b == b'\r' && raw.get(i + 1) != Some(&b'\n'));
        i += 1;
    }
    false
}

/// Header summary for the inbox list.
pub fn summarize(raw: &[u8]) -> Summary {
    let mut data = raw;
    if !header_block_complete(raw) && !matches!(raw.last(), Some(b'\n' | b'\r') | None) {
        // The prefix stops partway through a header line, so that line is cut short. Leave
        // it out rather than show half a value.
        let cut = raw
            .iter()
            .rposition(|&b| b == b'\n' || b == b'\r')
            .map_or(0, |i| i + 1);
        data = &raw[..cut];
    }
    let msg = Message::parse(data);
    let root = msg.part(ROOT);
    let get = |name: &str| pystr::strip(&root.get(name).unwrap_or_default()).to_string();
    let date_raw = root
        .raw_value("date")
        .map(|v| pystr::strip(&pystr::sanitize(v)).to_string())
        .unwrap_or_default();
    Summary {
        from: get("From"),
        to: get("To"),
        cc: get("Cc"),
        subject: get("Subject"),
        date: date::parsedate_to_datetime(&date_raw).map(date::PyDateTime::to_offset),
        date_raw,
        message_id: get("Message-ID"),
        has_attachments: !attachments(&msg).is_empty(),
    }
}

/// Headers that turn up in stored mail; at least one has to be present.
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
    "mime-version",
    "content-type",
    "dkim-signature",
    "authentication-results",
    "received-spf",
    "arc-seal",
    "x-ses-receipt",
    "x-received",
];

/// True when the first bytes of an object look like a stored RFC 5322 message rather than
/// some other file. Must accept a prefix that ends mid-header.
pub fn looks_like_email(prefix: &[u8]) -> bool {
    let mut names: Vec<String> = Vec::new();
    let mut subject = String::new();
    let mut in_subject = false;
    let mut offset = 0;
    let mut first = true;
    while offset < prefix.len() {
        let rest = &prefix[offset..];
        let end = rest.iter().position(|&b| b == b'\n').map(|i| i + 1);
        let line = &rest[..end.unwrap_or(rest.len())];
        offset += line.len();
        let complete = end.is_some();
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text.is_empty() {
            // The blank line that ends the headers.
            break;
        }
        if first && text.starts_with(b"From ") {
            first = false;
            continue;
        }
        first = false;
        if matches!(text[0], b' ' | b'\t') {
            if names.is_empty() {
                return false;
            }
            if in_subject {
                subject.push_str(&String::from_utf8_lossy(text));
            }
            continue;
        }
        match text.iter().position(|&b| b == b':') {
            Some(0) => return false,
            Some(i) => {
                if !text[..i].iter().all(|b| (0x21..=0x7e).contains(b)) {
                    return false;
                }
                let name = String::from_utf8_lossy(&text[..i]).to_ascii_lowercase();
                in_subject = name == "subject";
                if in_subject {
                    subject = String::from_utf8_lossy(&text[i + 1..]).into_owned();
                }
                names.push(name);
            }
            None => {
                // Only a header name cut off by the end of the prefix may lack its colon.
                if complete || !text.iter().all(|b| (0x21..=0x7e).contains(b)) {
                    return false;
                }
            }
        }
    }
    if !names.iter().any(|n| MAIL_HEADERS.contains(&n.as_str())) {
        return false;
    }
    // SES writes this object into the bucket when a receipt rule is set up. It is shaped like a
    // message but it isn't mail anyone sent.
    let is_setup_notice = subject
        .trim()
        .eq_ignore_ascii_case("Amazon SES Setup Notification")
        && !names.iter().any(|n| n == "received");
    !is_setup_notice
}

/// The last path component of an attachment name, so a name can't point outside `dir`.
fn safe_file_name(name: &str) -> String {
    let base = name
        .split(['/', '\\'])
        .rfind(|c| !c.is_empty() && *c != ".")
        .unwrap_or("");
    if base.is_empty() || base == ".." {
        return "attachment".into();
    }
    base.replace('\0', "_")
}

/// pathlib's stem and suffix.
fn stem_suffix(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Write every named attachment into `dir`, never overwriting, and return the paths written.
pub fn save_attachments(raw: &[u8], dir: &Path) -> io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let msg = Message::parse(raw);
    let mut saved = Vec::new();
    for (part, filename) in attachments(&msg) {
        let name = safe_file_name(&filename);
        let (stem, suffix) = stem_suffix(&name);
        let payload = msg.part(part).decoded_payload().unwrap_or_default();
        let mut n = 0;
        loop {
            let candidate = if n == 0 {
                name.clone()
            } else {
                format!("{stem}-{n}{suffix}")
            };
            let target = dir.join(candidate);
            // create_new refuses anything already there, dangling symlinks included, so an
            // existing file is never replaced even if it appears between two checks.
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                Ok(mut f) => {
                    f.write_all(&payload)?;
                    saved.push(target);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e),
            }
        }
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_for_matches_the_python_regex() {
        assert_eq!(
            received_for("by x with SMTP id y\tfor <a@example.com>; Fri"),
            ["a@example.com"]
        );
        assert_eq!(
            received_for("for a@example.com; x FOR  B@example.org"),
            ["a@example.com", "B@example.org"]
        );
        assert_eq!(received_for("before a@example.com"), Vec::<String>::new());
        assert_eq!(received_for("for @x for x@ for a@b"), ["a@b"]);
    }

    #[test]
    fn file_names() {
        assert_eq!(safe_file_name("../../etc/evil.bin"), "evil.bin");
        assert_eq!(safe_file_name("C:\\x\\y.doc"), "y.doc");
        assert_eq!(safe_file_name("dir/"), "dir");
        assert_eq!(safe_file_name(".."), "attachment");
        assert_eq!(stem_suffix("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(stem_suffix(".profile"), (".profile", ""));
        assert_eq!(stem_suffix("trailing."), ("trailing.", ""));
    }
}
