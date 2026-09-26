//! Decoding raw RFC 5322 messages for `reses FILE` and the inbox.
//!
//! I lean on the mail-parser crate (built with `full_encoding`, so every charset mail uses decodes)
//! for the parse, and on html2text for HTML bodies. The code here only decides what to show:
//! which part is the body, what counts as an attachment and what it's called, how headers read
//! when printed, and a handful of repairs where mail-parser is stricter or looser than the
//! standards (see `parts` and `headers`).
//!
//! The expected output for each test message is committed in tests/fixtures/mail. Those goldens
//! come from tests/mail_oracle.py, which follows the same rules on top of Python's standard email
//! package, so nothing the tests expect was produced by the code they test.
//!
//! The rules, which the oracle mirrors:
//! - The header lines are From, Reply-To, To, Cc, Bcc, Date, Subject and Message-ID, then an
//!   Attachments line when there are any, then "Message:", a blank line and the body.
//! - A part is an attachment if its disposition says so, if it has a file name, or if it's
//!   anything but plain text or HTML. A named text file is never the body.
//! - The body is the first plain text part that isn't an attachment, or the first HTML one
//!   converted to text when there's no plain one; `--html` prefers HTML the same way round.
//! - Bcc is an explicit Bcc header, or else every envelope recipient not already in To or Cc.

mod headers;
mod parts;
mod save;
mod sniff;

use std::io;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

pub use save::SaveReport;

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

/// The readable form of a message: the header lines, an Attachments line when there are
/// attachments, "Message:", a blank line, then the body. `prefer_html` picks the HTML body as
/// written over the plain one.
pub fn format_message(raw: &[u8], prefer_html: bool) -> String {
    let input = parts::prepare(raw);
    let parsed = parts::Parsed::new(&input);
    let mut lines = vec![
        format!("From: {}", parsed.addresses("From")),
        format!("Reply-To: {}", parsed.addresses("Reply-To")),
        format!("To: {}", parsed.addresses("To")),
        format!("Cc: {}", parsed.addresses("Cc")),
        format!("Bcc: {}", parsed.bcc()),
        format!("Date: {}", parsed.date_line()),
        format!("Subject: {}", parsed.subject()),
        format!("Message-ID: {}", parsed.raw_header("Message-ID")),
    ];
    // The Attachments line lists each attachment with the size it saves at.
    let files = parsed.attachments();
    if !files.is_empty() {
        let listed: Vec<String> = files
            .iter()
            .map(|a| format!("{} ({} bytes)", a.name, a.bytes.len()))
            .collect();
        lines.push(format!("Attachments: {}", listed.join(", ")));
    }
    lines.push("Message:".into());
    lines.push(String::new());
    lines.push(parsed.body(prefer_html));
    lines.join("\n") + "\n"
}

/// Whether the header block ends inside `raw`, that is, whether a blank line follows the
/// headers. A prefix fetched for the inbox list often stops before that.
fn header_block_complete(raw: &[u8]) -> bool {
    let mut at_line_start = true;
    for (i, &b) in raw.iter().enumerate() {
        if at_line_start && (b == b'\n' || b == b'\r') {
            return true;
        }
        at_line_start = b == b'\n' || (b == b'\r' && raw.get(i + 1) != Some(&b'\n'));
    }
    false
}

/// Header summary for the inbox list. Works on a prefix that stops anywhere, including inside
/// the headers.
pub fn summarize(raw: &[u8]) -> Summary {
    // A prefix that stops partway through a header line has that line cut short, so I leave it
    // out rather than show half a value.
    let mut data = raw;
    if !header_block_complete(raw) && !matches!(raw.last(), Some(b'\n' | b'\r') | None) {
        let cut = raw
            .iter()
            .rposition(|&b| b == b'\n' || b == b'\r')
            .map_or(0, |i| i + 1);
        data = &raw[..cut];
    }
    let input = parts::prepare(data);
    let parsed = parts::Parsed::new(&input);
    Summary {
        from: parsed.addresses("From"),
        to: parsed.addresses("To"),
        cc: parsed.addresses("Cc"),
        subject: parsed.subject(),
        date: parsed.date(),
        date_raw: parsed.raw_header("Date"),
        message_id: parsed.raw_header("Message-ID"),
        has_attachments: !parsed.attachments().is_empty(),
    }
}

/// True when the first bytes of an object look like a stored RFC 5322 message rather than some
/// other file. Must accept a prefix that ends mid-header. A false answer hides the object from
/// the inbox, so when in doubt this says yes.
pub fn looks_like_email(prefix: &[u8]) -> bool {
    sniff::looks_like_email(prefix)
}

/// Write every attachment into `dir`, never overwriting, and return the paths written. At most
/// `SaveReport::MAX` are written per call; `save_attachments_report` also says how many it left.
pub fn save_attachments(raw: &[u8], dir: &Path) -> io::Result<Vec<PathBuf>> {
    save_attachments_report(raw, dir).map(|r| r.saved)
}

/// `save_attachments`, reporting the attachments the per-save cap left unwritten as well as the
/// paths written.
pub fn save_attachments_report(raw: &[u8], dir: &Path) -> io::Result<SaveReport> {
    std::fs::create_dir_all(dir)?;
    let input = parts::prepare(raw);
    let parsed = parts::Parsed::new(&input);
    let items = parsed
        .attachments()
        .into_iter()
        .map(|a| (a.name, a.bytes))
        .collect();
    save::save_all(dir, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header block is complete once a blank line follows it, whatever the line endings.
    #[test]
    fn header_block_end() {
        assert!(header_block_complete(b"A: b\r\n\r\nbody"));
        assert!(header_block_complete(b"A: b\n\nbody"));
        assert!(header_block_complete(b"A: b\r\rbody"));
        assert!(!header_block_complete(b"A: b\r\nC: d"));
        assert!(!header_block_complete(b"A: b\r\n"));
    }
}
