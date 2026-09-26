//! Decoding raw RFC 5322 messages. `format_message` must produce exactly what
//! `python/reses.py` prints, byte for byte; the golden files in tests/fixtures pin that.

use std::io;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

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

/// The readable form: From, Reply-To, To, Cc, Bcc, Date, Subject, Message-ID, optional
/// Attachments line, "Message:", a blank line, then the body. See python/reses.py.
pub fn format_message(raw: &[u8], prefer_html: bool) -> String {
    let _ = (raw, prefer_html);
    String::from("reses: mail decoding is not built yet\n")
}

/// Header summary for the inbox list.
pub fn summarize(raw: &[u8]) -> Summary {
    let _ = raw;
    Summary::default()
}

/// True when the first bytes of an object look like a stored RFC 5322 message rather than
/// some other file. Must accept a prefix that ends mid-header.
pub fn looks_like_email(prefix: &[u8]) -> bool {
    let _ = prefix;
    false
}

/// Write every named attachment into `dir`, never overwriting, and return the paths written.
pub fn save_attachments(raw: &[u8], dir: &Path) -> io::Result<Vec<PathBuf>> {
    let _ = (raw, dir);
    Ok(Vec::new())
}
