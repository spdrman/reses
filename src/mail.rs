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

/// One mailbox from an address header, split the way a reader shows it: either part can be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mailbox {
    pub name: String,
    pub address: String,
}

/// One result from an Authentication-Results header, such as `dkim=pass header.d=example.com`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The method, lower case: spf, dkim, dmarc.
    pub method: String,
    /// Its result, lower case: pass, fail, softfail, neutral, none and so on.
    pub result: String,
    /// The identity or domain it was checked for, when the header names one.
    pub detail: String,
}

/// Everything the HTML reader shows around a message's HTML part: the headers a mail reader
/// leads with, and what the footer says about delivery and the message itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    pub subject: String,
    pub from: Vec<Mailbox>,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub reply_to: Vec<Mailbox>,
    /// The Bcc line, as the text view prints it (an explicit Bcc, or the envelope recipients).
    pub bcc: String,
    /// The Date header as a point in time, when it parses.
    pub date: Option<OffsetDateTime>,
    /// The Date header as written.
    pub date_raw: String,
    /// When the receiving server took it: the date at the end of the first Received header.
    pub received: String,
    pub message_id: String,
    /// Each attachment's name and decoded size in bytes, in order.
    pub attachments: Vec<(String, usize)>,
    /// The HTML part as written, when there is one.
    pub html: Option<String>,
    /// Whether the message has a plain text part as well.
    pub has_text: bool,
    /// The first Authentication-Results header's checks, in order.
    pub checks: Vec<Check>,
    /// SES's X-SES-Spam-Verdict and X-SES-Virus-Verdict, as written, or "".
    pub spam_verdict: String,
    pub virus_verdict: String,
    /// The whole message's size in bytes, as stored.
    pub size: usize,
}

/// The details the HTML reader shows for `raw`.
pub fn details(raw: &[u8]) -> Details {
    let input = parts::prepare(raw);
    let parsed = parts::Parsed::new(&input);
    let boxes = |name: &str| {
        parsed
            .mailboxes(name)
            .into_iter()
            .map(|(name, address)| Mailbox { name, address })
            .collect()
    };
    // The receiving server puts its own Received line on top, with the time after its last `;`.
    let received = parsed.raw_header("Received");
    Details {
        subject: parsed.subject(),
        from: boxes("From"),
        to: boxes("To"),
        cc: boxes("Cc"),
        reply_to: boxes("Reply-To"),
        bcc: parsed.bcc(),
        date: parsed.date(),
        date_raw: parsed.raw_header("Date"),
        received: received
            .rsplit_once(';')
            .map_or_else(String::new, |(_, when)| when.trim().to_string()),
        message_id: parsed.raw_header("Message-ID"),
        attachments: parsed
            .attachments()
            .into_iter()
            .map(|a| (a.name, a.bytes.len()))
            .collect(),
        html: parsed.html_part(),
        has_text: parsed.has_text_part(),
        checks: checks(&parsed.raw_header("Authentication-Results")),
        spam_verdict: parsed.raw_header("X-SES-Spam-Verdict"),
        virus_verdict: parsed.raw_header("X-SES-Virus-Verdict"),
        size: raw.len(),
    }
}

/// A check's `prop=value` pairs as the header gave them, keys lower case.
type Props = Vec<(String, String)>;

/// The methods an Authentication-Results check can start with (RFC 8601 and its registry).
const AUTH_METHODS: &[&str] = &[
    "spf",
    "dkim",
    "dmarc",
    "arc",
    "iprev",
    "auth",
    "bimi",
    "dkim-atps",
    "smime",
    "vbr",
];

/// The checks in an Authentication-Results value, `authserv-id; method=result prop=value ...;`.
///
/// Comments go first, since one can hold a `;`. SES then separates a check's own properties
/// with `;` as well (`spf=pass client-ip=...; envelope-from=...;`), so a segment that doesn't
/// start with a method belongs to the check before it. Each check's detail is the identity it
/// was run on: the envelope sender for SPF, the signing domain for DKIM, the From domain for
/// DMARC.
fn checks(value: &str) -> Vec<Check> {
    let mut plain = String::with_capacity(value.len());
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    // Group each check with its properties, however they were separated. The detail comes after.
    let mut found: Vec<(Check, Props)> = Vec::new();
    for segment in plain.split(';').skip(1) {
        for word in segment.split_whitespace() {
            let Some((key, val)) = word.split_once('=') else {
                continue;
            };
            let key = key.to_ascii_lowercase();
            if AUTH_METHODS.contains(&key.as_str()) {
                let check = Check {
                    method: key,
                    result: val.to_ascii_lowercase(),
                    detail: String::new(),
                };
                found.push((check, Vec::new()));
            } else if let Some((_, props)) = found.last_mut() {
                props.push((key, val.to_string()));
            }
        }
    }
    // The identity each method was run on, in the order I prefer the properties.
    found
        .into_iter()
        .map(|(mut check, props)| {
            let wanted: &[&str] = match check.method.as_str() {
                "spf" => &["smtp.mailfrom", "envelope-from", "smtp.helo", "helo"],
                "dkim" => &["header.d", "header.i"],
                "dmarc" => &["header.from"],
                _ => &[],
            };
            check.detail = wanted
                .iter()
                .find_map(|k| props.iter().find(|(p, _)| p == k).map(|(_, v)| v.as_str()))
                .map_or_else(String::new, |v| v.trim_start_matches('@').to_string());
            check
        })
        .collect()
}

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
    // The fixed header lines, always all eight and always in this order, empty or not.
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
    // Then the body under its own line, with the blank line the oracle prints between them.
    lines.push("Message:".into());
    lines.push(String::new());
    lines.push(parsed.body(prefer_html));
    lines.join("\n") + "\n"
}

/// The message's HTML body as written, for a browser to render, or None when it has no HTML part.
pub fn html_part(raw: &[u8]) -> Option<String> {
    let input = parts::prepare(raw);
    parts::Parsed::new(&input).html_part()
}

/// Header summary for the inbox list. Works on a prefix that stops anywhere, including inside
/// the headers.
pub fn summarize(raw: &[u8]) -> Summary {
    // A prefix that stops partway through a header line has that line cut short, so I leave it
    // out rather than show half a value.
    let mut data = raw;
    if !parts::header_block_complete(raw) && !matches!(raw.last(), Some(b'\n' | b'\r') | None) {
        let cut = raw
            .iter()
            .rposition(|&b| b == b'\n' || b == b'\r')
            .map_or(0, |i| i + 1);
        data = &raw[..cut];
    }
    // What's left parses like any whole message; the fields just come out empty where it stops.
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
    // The attachments come out in message order, as the Attachments line lists them.
    let input = parts::prepare(raw);
    let parsed = parts::Parsed::new(&input);
    let items = parsed
        .attachments()
        .into_iter()
        .map(|a| (a.name, a.bytes))
        .collect();
    save::save_all(dir, items)
}
