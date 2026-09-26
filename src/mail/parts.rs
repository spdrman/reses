//! The parsed message and the decisions about its parts: which part is the body, which parts are
//! attachments and what they're called, and what bytes each one holds.
//!
//! mail-parser does the parsing. On top of it I walk the part tree myself, depth first with my
//! own stack (so nesting depth never reaches the call stack), because its own body and
//! attachment lists treat an inline image as body text and a named text file as the body, which
//! is what the panel found (N30). I also redo the transfer decoding of a part in two cases where
//! mail-parser gives up: a Content-Transfer-Encoding with a comment or odd spacing, which RFC
//! 2045 allows, and a base64 part cut off by the end of the message. Both use mail-parser's own
//! decoders, so a repaired part decodes exactly as any other.

use std::borrow::Cow;

use mail_parser::decoders::base64::base64_decode;
use mail_parser::decoders::charsets::map::charset_decoder;
use mail_parser::parsers::MessageStream;
use mail_parser::{Encoding, Message, MessageParser, MessagePart, MimeHeaders, PartType};

use super::headers;

/// One attachment: the name it's listed and saved under, and its decoded bytes.
pub(super) struct Attachment {
    pub(super) name: String,
    pub(super) bytes: Vec<u8>,
}

/// A message as mail-parser reads it, with the raw bytes its offsets refer to.
pub(super) struct Parsed<'x> {
    raw: &'x [u8],
    msg: Option<Message<'x>>,
}

/// Extensions for the names I make up for attachments that don't carry one.
fn extension(ctype: &str) -> &'static str {
    match ctype {
        "message/rfc822" | "message/global" => "eml",
        "message/delivery-status" | "text/plain" => "txt",
        "text/html" => "html",
        "text/calendar" => "ics",
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "application/zip" => "zip",
        _ => "bin",
    }
}

/// Charset labels mail clients use that mail-parser's table doesn't know, and the name it does.
const CHARSET_ALIASES: &[(&str, &str)] = &[("cp932", "shift_jis"), ("cp949", "euc-kr")];

/// The charset name to hand mail-parser's decoders.
fn charset_label(name: &str) -> &str {
    let name = name.trim();
    CHARSET_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, known)| known)
}

/// Case-insensitive search for `needle` in `hay`.
fn find_ignore_case(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// Encoded words in `headers` with a charset label mail-parser doesn't know renamed to one it
/// does, or `None` when there are none.
fn rename_charsets(headers: &[u8]) -> Option<Vec<u8>> {
    let mut out = headers.to_vec();
    let mut changed = false;
    for (alias, known) in CHARSET_ALIASES {
        let from = format!("=?{alias}?");
        let to = format!("=?{known}?");
        while let Some(at) = find_ignore_case(&out, from.as_bytes()) {
            out.splice(at..at + from.len(), to.bytes());
            changed = true;
        }
    }
    changed.then_some(out)
}

/// The input as the parser should see it. Some tools save a message with a UTF-8 byte order mark
/// or blank lines before the first header, and a few old ones end lines with a bare CR; each of
/// those hides every header from mail-parser, so I undo them. A prefix that stops between the CR
/// and LF of a line gets its LF back. And encoded words in the header blocks whose charset label
/// mail-parser doesn't know (cp932 is the common one) get a label it does, since it decodes those
/// itself and I can't reach in.
pub(super) fn prepare(raw: &[u8]) -> Cow<'_, [u8]> {
    // A byte order mark and leading blank lines go, one line ending at a time.
    let mut rest = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    loop {
        if let Some(r) = rest.strip_prefix(b"\r\n") {
            rest = r;
        } else if let Some(r) = rest
            .strip_prefix(b"\n")
            .or_else(|| rest.strip_prefix(b"\r"))
        {
            rest = r;
        } else {
            break;
        }
    }
    let mut data = Cow::Borrowed(rest);
    // A file with CRs and no LF at all uses bare CR as its line ending.
    if !data.contains(&b'\n') && data.contains(&b'\r') {
        let mut fixed = Vec::with_capacity(data.len() + data.len() / 16);
        for &b in data.iter() {
            fixed.push(b);
            if b == b'\r' {
                fixed.push(b'\n');
            }
        }
        data = Cow::Owned(fixed);
    } else if data.ends_with(b"\r") {
        data.to_mut().push(b'\n');
    }
    // Only header blocks get charset labels renamed, so no body or attachment changes.
    if CHARSET_ALIASES
        .iter()
        .any(|(alias, _)| find_ignore_case(&data, format!("=?{alias}?").as_bytes()).is_some())
        && let Some(renamed) = rename_in_headers(&data)
    {
        data = Cow::Owned(renamed);
    }
    data
}

/// `data` with `rename_charsets` applied to every header block of the message and its parts,
/// or `None` when nothing changed.
fn rename_in_headers(data: &[u8]) -> Option<Vec<u8>> {
    let msg = MessageParser::new().parse(data)?;
    let mut blocks: Vec<(usize, usize)> = msg
        .parts
        .iter()
        .map(|p| (p.offset_header as usize, p.offset_body as usize))
        .filter(|(start, end)| start < end && *end <= data.len())
        .collect();
    drop_flat(msg);
    blocks.sort_unstable();
    // Rebuild the message block by block, renaming inside headers only.
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0;
    let mut changed = false;
    for (start, end) in blocks {
        if start < at {
            continue;
        }
        out.extend_from_slice(&data[at..start]);
        match rename_charsets(&data[start..end]) {
            Some(renamed) => {
                out.extend_from_slice(&renamed);
                changed = true;
            }
            None => out.extend_from_slice(&data[start..end]),
        }
        at = end;
    }
    out.extend_from_slice(&data[at..]);
    changed.then_some(out)
}

/// Drop a parsed message without recursing. mail-parser nests each forwarded message inside
/// the part that carries it, with no depth limit, so dropping 50k nested forwards the ordinary
/// way would recurse 50k deep. Taking each nested message out before its parent goes keeps the
/// drop flat.
fn drop_flat(msg: Message<'_>) {
    let mut todo = vec![msg];
    while let Some(mut msg) = todo.pop() {
        for part in &mut msg.parts {
            if matches!(part.body, PartType::Message(_))
                && let PartType::Message(inner) =
                    std::mem::replace(&mut part.body, PartType::Multipart(Vec::new()))
            {
                todo.push(inner);
            }
        }
    }
}

/// A part's content type as "type/subtype" in lowercase. A part without one is text/plain,
/// except inside a digest, where mail-parser has already read it as a message.
fn content_type(part: &MessagePart<'_>) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
            // RFC 2045 5.2: a content type that doesn't parse means text/plain.
            None => "text/plain".into(),
        },
        None if matches!(part.body, PartType::Message(_)) => "message/rfc822".into(),
        None => "text/plain".into(),
    }
}

/// The part's file name, if it has a usable one.
fn file_name(part: &MessagePart<'_>) -> Option<String> {
    part.attachment_name()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// A part is an attachment if its disposition says so, if it has a file name, or if it's
/// anything other than plain text or HTML.
fn is_attachment(part: &MessagePart<'_>) -> bool {
    let disposed = part
        .content_disposition()
        .is_some_and(|d| d.ctype().eq_ignore_ascii_case("attachment"));
    disposed
        || file_name(part).is_some()
        || !matches!(content_type(part).as_str(), "text/plain" | "text/html")
}

/// The transfer encoding a part declares, reduced to its mechanism: comments dropped, blanks
/// trimmed, lowercase. RFC 2045 makes the header a token with the usual comments allowed around it.
fn mechanism(part: &MessagePart<'_>) -> Option<String> {
    let declared = part.content_transfer_encoding()?;
    let mut out = String::new();
    let mut depth = 0usize;
    // Everything inside parentheses is a comment, nested ones included.
    for c in declared.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    Some(out.trim().to_ascii_lowercase())
}

/// Base64 decoded with mail-parser's MIME decoder, reading to the end of `data`. That decoder
/// gives up at the first character outside the base64 alphabet, and RFC 2045 6.8 says to ignore
/// those, so when it does I drop them and decode the rest.
fn decode_base64(data: &[u8]) -> Option<Vec<u8>> {
    let (end, bytes) = MessageStream::new(data).decode_base64_mime(b"");
    if end != usize::MAX {
        return Some(bytes.into_owned());
    }
    let alphabet: Vec<u8> = data
        .iter()
        .copied()
        .filter(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        .collect();
    base64_decode(&alphabet)
}

/// Quoted-printable decoded with mail-parser's MIME decoder, which gives up on a malformed escape
/// or on a line starting with "--" (it takes that for a boundary). Then I decode the way RFC 2045
/// 6.7 asks of a robust decoder: blanks at a line end are dropped, "=" at a line end is a soft
/// break, "=XX" is a byte in either case, and any other "=" stays as it is.
fn decode_quoted_printable(data: &[u8]) -> Option<Vec<u8>> {
    let (end, bytes) = MessageStream::new(data).decode_quoted_printable_mime(b"");
    if end != usize::MAX {
        return Some(bytes.into_owned());
    }
    let mut out = Vec::with_capacity(data.len());
    for line in data.split_inclusive(|&b| b == b'\n') {
        // Split off the line ending, then the blanks a transport may have added before it.
        let (text, ending) = match line {
            [text @ .., b'\r', b'\n'] => (text, &b"\r\n"[..]),
            [text @ .., b'\n'] => (text, &b"\n"[..]),
            text => (text, &b""[..]),
        };
        let text = text.trim_ascii_end();
        let (text, soft) = match text.strip_suffix(b"=") {
            Some(t) => (t, true),
            None => (text, false),
        };
        // Escapes inside the line.
        let mut i = 0;
        while i < text.len() {
            let byte = (text[i] == b'=')
                .then(|| text.get(i + 1..i + 3))
                .flatten()
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match byte {
                Some(b) => {
                    out.push(b);
                    i += 3;
                }
                None => {
                    out.push(text[i]);
                    i += 1;
                }
            }
        }
        if !soft {
            out.extend_from_slice(ending);
        }
    }
    Some(out)
}

/// Text in Windows-1252, which is what 8-bit text with no charset almost always is when it
/// isn't UTF-8.
fn windows_1252(data: &[u8]) -> String {
    match charset_decoder(b"windows-1252") {
        Some(decode) => decode(data),
        None => String::from_utf8_lossy(data).into_owned(),
    }
}

impl<'x> Parsed<'x> {
    /// Parse `raw`, which `prepare` has already tidied. Nothing here fails: a message mail-parser
    /// finds no headers in is kept as raw text.
    pub(super) fn new(raw: &'x [u8]) -> Parsed<'x> {
        Parsed {
            raw,
            msg: MessageParser::new().parse(raw),
        }
    }

    /// The first header called `name`, as written, or "".
    pub(super) fn raw_header(&self, name: &str) -> String {
        self.msg
            .as_ref()
            .map_or_else(String::new, |m| headers::raw_value(m, self.raw, name))
    }

    /// The first `name` header's addresses, printed back.
    pub(super) fn addresses(&self, name: &str) -> String {
        self.msg
            .as_ref()
            .map_or_else(String::new, |m| headers::addresses(m, name))
    }

    /// The Subject, decoded.
    pub(super) fn subject(&self) -> String {
        self.msg.as_ref().map_or_else(String::new, headers::subject)
    }

    /// The Bcc line: an explicit Bcc, or the envelope recipients nobody else can see.
    pub(super) fn bcc(&self) -> String {
        self.msg.as_ref().map_or_else(String::new, headers::bcc)
    }

    /// The Date line: normalised when the date is real, as written when it isn't.
    pub(super) fn date_line(&self) -> String {
        self.msg
            .as_ref()
            .map_or_else(String::new, |m| headers::date_line(m, self.raw))
    }

    /// The Date header as a point in time, reading an unknown zone as UTC.
    pub(super) fn date(&self) -> Option<time::OffsetDateTime> {
        self.msg.as_ref().and_then(|m| headers::date(m, self.raw))
    }

    /// Every part that isn't a multipart container, depth first, in the order they appear. A
    /// forwarded message is one leaf: its own parts belong to it, not to this message.
    fn leaves(&self) -> Vec<&MessagePart<'x>> {
        let Some(msg) = &self.msg else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut todo = vec![0u32];
        // Children go on the stack in reverse, so they come off in order.
        while let Some(id) = todo.pop() {
            let Some(part) = msg.part(id) else { continue };
            match &part.body {
                PartType::Multipart(children) => todo.extend(children.iter().rev()),
                _ => out.push(part),
            }
        }
        out
    }

    /// A part's body after transfer decoding and before any charset decoding: the bytes an
    /// attachment saves as.
    fn bytes(&self, part: &MessagePart<'x>) -> Vec<u8> {
        // What mail-parser decoded itself I take as is. A forwarded message is its original bytes.
        match &part.body {
            PartType::Message(m) => return m.raw_message().to_vec(),
            PartType::Multipart(_) => return Vec::new(),
            PartType::Binary(b) | PartType::InlineBinary(b) if part.encoding != Encoding::None => {
                return b.to_vec();
            }
            _ => {}
        }
        // Everything else comes from the raw body, decoded by the mechanism the part declares.
        let start = (part.offset_body as usize).min(self.raw.len());
        let end = (part.offset_end as usize).clamp(start, self.raw.len());
        let data = &self.raw[start..end];
        let decoded = match mechanism(part).as_deref() {
            Some("base64") => decode_base64(data),
            Some("quoted-printable") => decode_quoted_printable(data),
            _ => None,
        };
        decoded.unwrap_or_else(|| data.to_vec())
    }

    /// A text part's body as a string: the declared charset, or with none declared, UTF-8 if the
    /// bytes are valid UTF-8 and Windows-1252 if not (N32: 8-bit text with no charset is common,
    /// and turning it into replacement characters loses it).
    fn text(&self, part: &MessagePart<'x>) -> String {
        let data = self.bytes(part);
        let charset = part.content_type().and_then(|ct| ct.attribute("charset"));
        match charset {
            Some(cs) => match charset_decoder(charset_label(cs).as_bytes()) {
                Some(decode) => decode(&data),
                None => String::from_utf8_lossy(&data).into_owned(),
            },
            None => match String::from_utf8(data) {
                Ok(s) => s,
                Err(e) => windows_1252(e.as_bytes()),
            },
        }
    }

    /// The attachments, in order. A part without a name gets attachment-N and an extension from
    /// its type, N counting attachments from 1.
    pub(super) fn attachments(&self) -> Vec<Attachment> {
        let mut out: Vec<Attachment> = Vec::new();
        for part in self.leaves() {
            if !is_attachment(part) {
                continue;
            }
            let name = file_name(part).unwrap_or_else(|| {
                format!(
                    "attachment-{}.{}",
                    out.len() + 1,
                    extension(&content_type(part))
                )
            });
            out.push(Attachment {
                name,
                bytes: self.bytes(part),
            });
        }
        out
    }

    /// The body shown under "Message:". Plain text wins unless `prefer_html`; with no plain part
    /// the HTML one is converted to text, and with `prefer_html` and no HTML part the plain one
    /// is shown.
    pub(super) fn body(&self, prefer_html: bool) -> String {
        // With no headers at all, mail-parser keeps nothing, so the whole input is the body.
        if self.msg.is_none() {
            let text = match std::str::from_utf8(self.raw) {
                Ok(s) => Cow::Borrowed(s),
                Err(_) => Cow::Owned(windows_1252(self.raw)),
            };
            return text.replace("\r\n", "\n").trim().to_string();
        }
        // The first plain and first HTML part that aren't attachments are the candidates.
        let candidates: Vec<&MessagePart<'x>> = self
            .leaves()
            .into_iter()
            .filter(|p| !is_attachment(p))
            .collect();
        let first = |ctype: &str| {
            candidates
                .iter()
                .copied()
                .find(|p| content_type(p) == ctype)
        };
        let (chosen, convert) = if prefer_html {
            (first("text/html").or_else(|| first("text/plain")), false)
        } else {
            match first("text/plain") {
                Some(p) => (Some(p), false),
                None => (first("text/html"), true),
            }
        };
        let Some(part) = chosen else {
            return String::new();
        };
        let mut text = self.text(part);
        if convert {
            text = html_to_text(&text);
        }
        text.replace("\r\n", "\n").trim().to_string()
    }
}

impl Drop for Parsed<'_> {
    /// Hand the parsed message to `drop_flat`, so a deep chain of forwards can't exhaust the stack.
    fn drop(&mut self) {
        if let Some(msg) = self.msg.take() {
            drop_flat(msg);
        }
    }
}

/// Deepest element nesting I hand html2text. Its layout work grows with the square of the depth
/// (4,000 nested divs took most of a second, 50,000 would take minutes), and no real message
/// nests anywhere near this.
const MAX_HTML_DEPTH: usize = 512;

/// Elements that never have content, so they never nest.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// `html` with every tag deeper than `MAX_HTML_DEPTH` taken out and all the text kept, or as it
/// is when it never nests that deep. I read tags loosely: comments and the bodies of script and
/// style are skipped so a "<" inside them doesn't count, and an unclosed tag simply stays open.
fn flatten_deep_html(html: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut deepest = 0usize;
    let mut at = 0;
    let mut flattening = false;
    // Two passes at most: the first only measures, the second rewrites.
    for pass in 0..2 {
        while let Some(off) = html[at..].find('<') {
            let lt = at + off;
            if flattening {
                out.push_str(&html[at..lt]);
            }
            // A comment runs to "-->".
            if html[lt..].starts_with("<!--") {
                let end = html[lt..].find("-->").map_or(html.len(), |e| lt + e + 3);
                if flattening && depth <= MAX_HTML_DEPTH {
                    out.push_str(&html[lt..end]);
                }
                at = end;
                continue;
            }
            let Some(gt) = html[lt..].find('>').map(|g| lt + g) else {
                at = lt;
                break;
            };
            let tag = &html[lt..=gt];
            let closing = tag.starts_with("</");
            let name: String = tag[if closing { 2 } else { 1 }..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            let opens = !closing
                && !name.is_empty()
                && !tag.ends_with("/>")
                && !VOID_ELEMENTS.contains(&name.as_str());
            if opens {
                depth += 1;
                deepest = deepest.max(depth);
            }
            if flattening && depth <= MAX_HTML_DEPTH {
                out.push_str(tag);
            }
            if closing && !name.is_empty() {
                depth = depth.saturating_sub(1);
            }
            at = gt + 1;
            // Script and style bodies are raw text up to their own closing tag.
            if opens && (name == "script" || name == "style") {
                let close = format!("</{name}");
                let end = html[at..]
                    .to_ascii_lowercase()
                    .find(&close)
                    .map_or(html.len(), |e| at + e);
                if flattening && depth <= MAX_HTML_DEPTH {
                    out.push_str(&html[at..end]);
                }
                at = end;
            }
        }
        if pass == 0 {
            if deepest <= MAX_HTML_DEPTH {
                return Cow::Borrowed(html);
            }
            flattening = true;
            out.reserve(html.len());
            depth = 0;
            at = 0;
        }
    }
    out.push_str(&html[at..]);
    Cow::Owned(out)
}

/// HTML turned into readable text by html2text, links listed at the end. The width is only there
/// because html2text wraps; I make it wide enough that mail paragraphs stay on one line.
fn html_to_text(html: &str) -> String {
    let html = flatten_deep_html(html);
    html2text::config::plain()
        .string_from_read(html.as_bytes(), 10_000)
        .unwrap_or_else(|_| html.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mechanism ignores what RFC 2045 lets surround the token.
    #[test]
    fn mechanisms() {
        let raw = b"Content-Type: text/plain\r\nContent-Transfer-Encoding: BASE64 (encoded (twice))\r\n\r\naGk=\r\n";
        let parsed = Parsed::new(raw);
        let part = parsed.leaves()[0];
        assert_eq!(mechanism(part).as_deref(), Some("base64"));
        assert_eq!(parsed.bytes(part), b"hi");
    }

    /// A byte order mark and blank lines in front don't hide the headers, a bare-CR file gets
    /// line endings the parser knows, and unknown charset labels are renamed in headers only.
    #[test]
    fn preparing_the_input() {
        assert_eq!(&*prepare(b"\xef\xbb\xbfFrom: a"), b"From: a");
        assert_eq!(&*prepare(b"\r\n\n\rFrom: a"), b"From: a");
        assert_eq!(&*prepare(b"From: a\r\n"), b"From: a\r\n");
        assert_eq!(
            &*prepare(b"From: a\rTo: b\r\rx\r"),
            b"From: a\r\nTo: b\r\n\r\nx\r\n"
        );
        assert_eq!(&*prepare(b"From: a\r\nTo: b\r"), b"From: a\r\nTo: b\r\n");
        let raw = b"Subject: =?CP932?B?gqA=?=\r\n\r\nbody keeps =?cp932?B?gqA=?=\r\n";
        assert_eq!(
            &*prepare(raw),
            &b"Subject: =?shift_jis?B?gqA=?=\r\n\r\nbody keeps =?cp932?B?gqA=?=\r\n"[..]
        );
    }

    /// 8-bit text with no charset is read as UTF-8 when it is, and Windows-1252 when it isn't.
    #[test]
    fn text_without_a_charset() {
        let utf8 = Parsed::new("Subject: x\r\n\r\ncafé\r\n".as_bytes());
        assert_eq!(utf8.body(false), "café");
        let latin = Parsed::new(b"Subject: x\r\n\r\ncaf\xe9 \x93q\x94\r\n");
        assert_eq!(latin.body(false), "café “q”");
    }

    /// Nesting past the limit loses its tags but keeps its text, and shallow HTML is untouched.
    #[test]
    fn deep_html_is_flattened() {
        let shallow = "<div><p>a</p></div>";
        assert!(matches!(flatten_deep_html(shallow), Cow::Borrowed(_)));
        let deep = format!("{}text{}", "<div>".repeat(2_000), "</div>".repeat(2_000));
        let flat = flatten_deep_html(&deep);
        assert_eq!(flat.matches("<div>").count(), MAX_HTML_DEPTH);
        assert_eq!(flat.matches("</div>").count(), MAX_HTML_DEPTH);
        assert!(flat.contains("text"));
        let script = format!("<script>{}</script>{}", "<div>".repeat(600), "<b>x</b>");
        assert!(matches!(flatten_deep_html(&script), Cow::Borrowed(_)));
    }

    /// Made-up names count attachments and take their extension from the type.
    #[test]
    fn made_up_names() {
        assert_eq!(extension("message/rfc822"), "eml");
        assert_eq!(extension("application/x-unknown"), "bin");
    }
}
