//! A port of `email.feedparser.FeedParser` and the `email.message.EmailMessage` methods reses.py
//! calls (get, get_all, get_param, get_filename, get_body, iter_attachments, get_content,
//! get_payload(decode=True)), under policy.default.
//!
//! Parts live in an arena and refer to each other by index, which keeps the parser's
//! `_cur`/`_last` bookkeeping the same shape as Python's.

use std::collections::VecDeque;

use super::codec::{self, Errors};
use super::{hvp, pystr, transfer};

#[derive(Debug, Clone)]
pub(super) enum Payload {
    None,
    Text(Vec<u8>),
    Parts(Vec<usize>),
}

#[derive(Debug, Clone)]
pub(super) struct Part {
    /// (name, raw value) exactly as header_source_parse stores them, bytes escaped.
    headers: Vec<(String, String)>,
    pub(super) payload: Payload,
    default_type: &'static str,
}

#[derive(Debug, Clone)]
pub(super) struct Message {
    parts: Vec<Part>,
}

pub(super) type PartId = usize;
pub(super) const ROOT: PartId = 0;

// --- Line splitting and the buffered input --------------------------------------------------

/// Splits like StringIO(newline='').readlines(): each line keeps its \r\n, \r or \n.
fn split_lines(data: &[u8]) -> VecDeque<Vec<u8>> {
    let mut lines = VecDeque::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                lines.push_back(data[start..=i].to_vec());
                start = i + 1;
            }
            b'\r' => {
                let end = if data.get(i + 1) == Some(&b'\n') { i + 1 } else { i };
                lines.push_back(data[start..=end].to_vec());
                i = end;
                start = end + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < data.len() {
        lines.push_back(data[start..].to_vec());
    }
    lines
}

#[derive(Debug, Clone)]
enum EofMatcher {
    Boundary(Vec<char>),
    BlankLine,
}

/// `boundaryre.match(line)`: `(--boundary)(--)?[ \t]*(\r\n|\r|\n)?$`, returning whether it was
/// the close delimiter and whether a line ending followed.
fn boundary_match(line: &[u8], boundary: &[char]) -> Option<(bool, bool)> {
    let text: Vec<char> = line.iter().map(|&b| pystr::escape_byte(b)).collect();
    let n = boundary.len() + 2;
    if text.len() < n || text[0] != '-' || text[1] != '-' || text[2..n] != *boundary {
        return None;
    }
    let tail_ok = |t: &[char]| -> Option<bool> {
        let mut i = 0;
        while i < t.len() && (t[i] == ' ' || t[i] == '\t') {
            i += 1;
        }
        match &t[i..] {
            [] => Some(false),
            ['\r', '\n'] | ['\r'] | ['\n'] => Some(true),
            // `$` also matches just before a final newline.
            ['\r', '\n', '\n'] | ['\r', '\r', '\n'] => None,
            _ => None,
        }
    };
    let rest = &text[n..];
    if rest.len() >= 2
        && rest[0] == '-'
        && rest[1] == '-'
        && let Some(sep) = tail_ok(&rest[2..])
    {
        return Some((true, sep));
    }
    tail_ok(rest).map(|sep| (false, sep))
}

fn is_blank(line: &[u8]) -> bool {
    matches!(line.first(), Some(b'\r' | b'\n'))
}

struct Input {
    lines: VecDeque<Vec<u8>>,
    eofstack: Vec<EofMatcher>,
}

impl Input {
    /// BufferedSubFile.readline: `None` stands for the '' that ends the current part.
    fn readline(&mut self) -> Option<Vec<u8>> {
        let line = self.lines.pop_front()?;
        for m in self.eofstack.iter().rev() {
            let hit = match m {
                EofMatcher::Boundary(b) => boundary_match(&line, b).is_some(),
                EofMatcher::BlankLine => is_blank(&line),
            };
            if hit {
                self.lines.push_front(line);
                return None;
            }
        }
        if line.is_empty() {
            return None;
        }
        Some(line)
    }

    fn unreadline(&mut self, line: Vec<u8>) {
        self.lines.push_front(line);
    }
}

/// headerRE: `^(From |[\041-\071\073-\176]*:|[\t ])`.
fn is_header_line(line: &[u8]) -> bool {
    if line.starts_with(b"From ") || matches!(line.first(), Some(b'\t' | b' ')) {
        return true;
    }
    for &b in line {
        if b == b':' {
            return true;
        }
        if !(0x21..=0x7e).contains(&b) {
            return false;
        }
    }
    false
}

fn strip_eol(line: &[u8]) -> &[u8] {
    if line.ends_with(b"\r\n") {
        &line[..line.len() - 2]
    } else if line.ends_with(b"\n") || line.ends_with(b"\r") {
        &line[..line.len() - 1]
    } else {
        line
    }
}

// --- The parser -----------------------------------------------------------------------------

struct Parser {
    input: Input,
    parts: Vec<Part>,
    stack: Vec<PartId>,
    cur: Option<PartId>,
    last: PartId,
}

impl Parser {
    fn new_message(&mut self) {
        let default_type = match self.cur {
            Some(c) if self.content_type(c) == "multipart/digest" => "message/rfc822",
            _ => "text/plain",
        };
        let id = self.parts.len();
        self.parts.push(Part { headers: Vec::new(), payload: Payload::None, default_type });
        if let Some(&parent) = self.stack.last() {
            match &mut self.parts[parent].payload {
                Payload::Parts(v) => v.push(id),
                p => *p = Payload::Parts(vec![id]),
            }
        }
        self.stack.push(id);
        self.cur = Some(id);
        self.last = id;
    }

    fn pop_message(&mut self) -> PartId {
        let id = self.stack.pop().expect("message stack");
        self.cur = self.stack.last().copied();
        id
    }

    fn content_type(&self, id: PartId) -> String {
        content_type(&self.parts[id])
    }

    fn cur(&self) -> PartId {
        self.cur.expect("current message")
    }

    fn parse_headers(&mut self, lines: Vec<Vec<u8>>) {
        let cur = self.cur();
        let mut lastvalue: Vec<Vec<u8>> = Vec::new();
        let count = lines.len();
        for (lineno, line) in lines.into_iter().enumerate() {
            if matches!(line.first(), Some(b' ' | b'\t')) {
                if lastvalue.is_empty() {
                    // A first line that's a continuation is dropped.
                    continue;
                }
                lastvalue.push(line);
                continue;
            }
            if !lastvalue.is_empty() {
                let h = source_parse(&lastvalue);
                self.parts[cur].headers.push(h);
                lastvalue.clear();
            }
            if line.starts_with(b"From ") {
                if lineno == 0 {
                    continue; // the unix-from envelope line
                } else if lineno == count - 1 {
                    self.input.unreadline(line);
                    return;
                } else {
                    continue;
                }
            }
            let i = line.iter().position(|&b| b == b':').unwrap_or(0);
            if i == 0 {
                continue;
            }
            lastvalue = vec![line];
        }
        if !lastvalue.is_empty() {
            let h = source_parse(&lastvalue);
            self.parts[cur].headers.push(h);
        }
    }

    fn set_payload(&mut self, id: PartId, lines: Vec<Vec<u8>>) {
        self.parts[id].payload = Payload::Text(lines.concat());
    }

    fn read_rest(&mut self) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        while let Some(l) = self.input.readline() {
            lines.push(l);
        }
        lines
    }

    fn parsegen(&mut self) {
        self.new_message();
        let cur = self.cur();
        let mut headers = Vec::new();
        while let Some(line) = self.input.readline() {
            if !is_header_line(&line) {
                if !is_blank(&line) {
                    self.input.unreadline(line);
                }
                break;
            }
            headers.push(line);
        }
        self.parse_headers(headers);

        let ctype = self.content_type(cur);
        if ctype == "message/delivery-status" {
            loop {
                self.input.eofstack.push(EofMatcher::BlankLine);
                self.parsegen();
                self.pop_message();
                self.input.eofstack.pop();
                let _blank = self.input.readline();
                match self.input.readline() {
                    None => break,
                    Some(l) => self.input.unreadline(l),
                }
            }
            return;
        }
        if ctype.split('/').next() == Some("message") {
            self.parsegen();
            self.pop_message();
            return;
        }
        if ctype.split('/').next() == Some("multipart") {
            let Some(boundary) = get_boundary(&self.parts[cur]) else {
                let lines = self.read_rest();
                self.set_payload(cur, lines);
                return;
            };
            let boundary: Vec<char> = boundary.chars().collect();
            let mut capturing_preamble = true;
            let mut preamble: Vec<Vec<u8>> = Vec::new();
            let mut close_boundary_seen = false;
            let mut linesep = false;
            while let Some(line) = self.input.readline() {
                let Some((is_end, sep)) = boundary_match(&line, &boundary) else {
                    preamble.push(line);
                    continue;
                };
                if is_end {
                    close_boundary_seen = true;
                    linesep = sep;
                    break;
                }
                if capturing_preamble {
                    capturing_preamble = false;
                    self.input.unreadline(line);
                    continue;
                }
                // Skip any run of further boundary lines.
                loop {
                    match self.input.readline() {
                        Some(l) if boundary_match(&l, &boundary).is_some() => {}
                        Some(l) => {
                            self.input.unreadline(l);
                            break;
                        }
                        None => {
                            self.input.unreadline(Vec::new());
                            break;
                        }
                    }
                }
                self.input.eofstack.push(EofMatcher::Boundary(boundary.clone()));
                self.parsegen();
                // The newline before a boundary belongs to the boundary.
                let last = self.last;
                if content_type(&self.parts[last]).starts_with("multipart/") {
                    // Only the epilogue would change, and nothing here reads it.
                } else if let Payload::Text(p) = &mut self.parts[last].payload {
                    let cut = p.len() - strip_eol(p).len();
                    p.truncate(p.len() - cut);
                }
                self.input.eofstack.pop();
                self.pop_message();
                self.last = self.cur();
            }
            if capturing_preamble {
                self.set_payload(cur, preamble);
                let _ = self.read_rest();
                return;
            }
            if !close_boundary_seen {
                return;
            }
            let _ = linesep;
            let _epilogue = self.read_rest();
            return;
        }
        let lines = self.read_rest();
        self.set_payload(cur, lines);
    }
}

/// policy.header_source_parse: the name up to the first colon, and the value with the first
/// line's leading blanks removed and the final line ending dropped.
fn source_parse(lines: &[Vec<u8>]) -> (String, String) {
    let first = &lines[0];
    let i = first.iter().position(|&b| b == b':').unwrap_or(first.len());
    let name = pystr::from_bytes(&first[..i]);
    let mut value: Vec<u8> = first[(i + 1).min(first.len())..].to_vec();
    let lead = value.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
    value.drain(..lead);
    for l in &lines[1..] {
        value.extend_from_slice(l);
    }
    while matches!(value.last(), Some(b'\r' | b'\n')) {
        value.pop();
    }
    (name, pystr::from_bytes(&value))
}

// --- Message API ----------------------------------------------------------------------------

fn fetch(name: &str, raw: &str) -> String {
    // header_fetch_parse drops every CR and LF, then the registry builds the header object.
    let unfolded: String = raw.chars().filter(|&c| c != '\r' && c != '\n').collect();
    hvp::decode_header(&pystr::lower(name), &unfolded)
}

impl Part {
    fn raw(&self, name: &str) -> Option<(&str, &str)> {
        let want = pystr::lower(name);
        self.headers
            .iter()
            .find(|(k, _)| pystr::lower(k) == want)
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// `msg.get(name)`, as the string the header object is.
    pub(super) fn get(&self, name: &str) -> Option<String> {
        self.raw(name).map(|(k, v)| fetch(k, v))
    }

    /// `msg.get_all(name, [])`.
    pub(super) fn get_all(&self, name: &str) -> Vec<String> {
        let want = pystr::lower(name);
        self.headers
            .iter()
            .filter(|(k, _)| pystr::lower(k) == want)
            .map(|(k, v)| fetch(k, v))
            .collect()
    }

    /// The first `name` header as written, line breaks removed but nothing decoded.
    pub(super) fn raw_value(&self, name: &str) -> Option<String> {
        self.raw(name).map(|(_, v)| v.chars().filter(|&c| c != '\r' && c != '\n').collect())
    }

    fn contains(&self, name: &str) -> bool {
        self.raw(name).is_some()
    }

    /// `msg.get_param(param, header=header)`; values are always plain strings here because
    /// the header object's str() has already folded any RFC 2231 pieces together.
    pub(super) fn get_param(&self, param: &str, header: &str) -> Option<String> {
        if !self.contains(header) {
            return None;
        }
        let params = get_params_preserve(&self.get(header)?);
        params
            .into_iter()
            .find(|(k, _)| pystr::lower(k) == pystr::lower(param))
            .map(|(_, v)| unquote(&v))
    }

    pub(super) fn is_attachment(&self) -> bool {
        match self.raw("content-disposition") {
            None => false,
            Some((_, v)) => {
                let unfolded: String = v.chars().filter(|&c| c != '\r' && c != '\n').collect();
                hvp::content_disposition(&unfolded).as_deref() == Some("attachment")
            }
        }
    }

    pub(super) fn get_filename(&self) -> Option<String> {
        let f = self
            .get_param("filename", "content-disposition")
            .or_else(|| self.get_param("name", "content-type"))?;
        Some(pystr::strip(&unquote(&f)).to_string())
    }

    /// `get_payload(decode=True)`; `None` for a multipart (payload is a list of parts).
    pub(super) fn decoded_payload(&self) -> Option<Vec<u8>> {
        let Payload::Text(bytes) = &self.payload else {
            return match self.payload {
                Payload::Parts(_) => None,
                _ => Some(Vec::new()),
            };
        };
        let cte = pystr::lower(&self.get("content-transfer-encoding").unwrap_or_default());
        Some(match cte.as_str() {
            "quoted-printable" => transfer::a2b_qp(bytes),
            "base64" => transfer::decode_base64_body(bytes),
            _ => bytes.clone(),
        })
    }

    /// The text of a text/* part: raw_data_manager.get_text_content.
    pub(super) fn text_content(&self) -> String {
        let bytes = self.decoded_payload().unwrap_or_default();
        let charset = self.get_param("charset", "content-type").unwrap_or_else(|| "ASCII".into());
        match codec::lookup(&charset) {
            Some(c) => codec::decode(&bytes, c, Errors::Replace).unwrap(),
            // Python raises LookupError here; decoding as UTF-8 is the useful fallback.
            None => String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

/// `Message.get_content_type()`.
pub(super) fn content_type(p: &Part) -> String {
    match p.get("content-type") {
        None => p.default_type.to_string(),
        Some(v) => {
            let ctype = pystr::lower(split_param(&v));
            if ctype.matches('/').count() != 1 { "text/plain".into() } else { ctype }
        }
    }
}

/// `_splitparam(value)[0]`.
fn split_param(v: &str) -> &str {
    pystr::strip(v.split_once(';').map_or(v, |(a, _)| a))
}

/// `email.utils.unquote`.
fn unquote(s: &str) -> String {
    if s.chars().count() > 1 {
        if s.starts_with('"') && s.ends_with('"') {
            return s[1..s.len() - 1].replace("\\\\", "\\").replace("\\\"", "\"");
        }
        if s.starts_with('<') && s.ends_with('>') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// `_parseparam`: split on semicolons that aren't inside quotes.
fn parse_param(s: &str) -> Vec<String> {
    let mut s: String = format!(";{s}");
    let mut plist = Vec::new();
    while s.starts_with(';') {
        s.remove(0);
        let count_quotes = |t: &str| t.matches('"').count() as i64 - t.matches("\\\"").count() as i64;
        let mut end = s.find(';');
        while let Some(e) = end {
            if e > 0 && count_quotes(&s[..e]) % 2 != 0 {
                end = s[e + 1..].find(';').map(|x| x + e + 1);
            } else {
                break;
            }
        }
        let end = end.unwrap_or(s.len());
        let mut f = s[..end].to_string();
        if let Some(i) = f.find('=') {
            f = format!("{}={}", pystr::lower(pystr::strip(&f[..i])), pystr::strip(&f[i + 1..]));
        }
        plist.push(pystr::strip(&f).to_string());
        s = s[end..].to_string();
    }
    plist
}

/// `Message._get_params_preserve` including `utils.decode_params`.
fn get_params_preserve(value: &str) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> = parse_param(value)
        .into_iter()
        .map(|p| match p.split_once('=') {
            Some((n, v)) => (pystr::strip(n).to_string(), pystr::strip(v).to_string()),
            None => (pystr::strip(&p).to_string(), String::new()),
        })
        .collect();
    // decode_params: names never carry RFC 2231 stars at this point, so every value just
    // round-trips through unquote and quote.
    for (_, v) in params.iter_mut().skip(1) {
        let u = unquote(v);
        *v = format!("\"{}\"", u.replace('\\', "\\\\").replace('"', "\\\""));
    }
    params
}

fn get_boundary(p: &Part) -> Option<String> {
    let b = p.get_param("boundary", "content-type")?;
    Some(pystr::rstrip(&unquote(&b)).to_string())
}

impl Message {
    pub(super) fn parse(data: &[u8]) -> Message {
        let mut p = Parser {
            input: Input { lines: split_lines(data), eofstack: Vec::new() },
            parts: Vec::new(),
            stack: Vec::new(),
            cur: None,
            last: 0,
        };
        p.parsegen();
        Message { parts: p.parts }
    }

    pub(super) fn part(&self, id: PartId) -> &Part {
        &self.parts[id]
    }

    fn children(&self, id: PartId) -> &[PartId] {
        match &self.parts[id].payload {
            Payload::Parts(v) => v,
            _ => &[],
        }
    }

    fn is_multipart(&self, id: PartId) -> bool {
        matches!(self.parts[id].payload, Payload::Parts(_))
    }

    fn find_body(&self, part: PartId, prefs: &[&str], out: &mut Vec<(usize, PartId)>) {
        let p = &self.parts[part];
        if p.is_attachment() {
            return;
        }
        let ctype = content_type(p);
        let (maintype, subtype) = ctype.split_once('/').unwrap_or((&ctype, ""));
        if maintype == "text" {
            if let Some(i) = prefs.iter().position(|x| *x == subtype) {
                out.push((i, part));
            }
            return;
        }
        // Python tests the root here, not the part (`self.is_multipart()`).
        if maintype != "multipart" || !self.is_multipart(ROOT) {
            return;
        }
        if subtype != "related" {
            for &sub in self.children(part) {
                self.find_body(sub, prefs, out);
            }
            return;
        }
        if let Some(i) = prefs.iter().position(|x| *x == "related") {
            out.push((i, part));
        }
        let mut candidate = None;
        if let Some(start) = p.get_param("start", "content-type").filter(|s| !s.is_empty()) {
            candidate = self
                .children(part)
                .iter()
                .copied()
                .find(|&s| self.parts[s].get("content-id").as_deref() == Some(start.as_str()));
        }
        if candidate.is_none() {
            candidate = self.children(part).first().copied();
        }
        if let Some(c) = candidate {
            self.find_body(c, prefs, out);
        }
    }

    /// `msg.get_body(preferencelist)`.
    pub(super) fn get_body(&self, prefs: &[&str]) -> Option<PartId> {
        let mut found = Vec::new();
        self.find_body(ROOT, prefs, &mut found);
        let mut best = prefs.len();
        let mut body = None;
        for (prio, part) in found {
            if prio < best {
                best = prio;
                body = Some(part);
                if prio == 0 {
                    break;
                }
            }
        }
        body
    }

    /// `msg.iter_attachments()` on the root message.
    pub(super) fn iter_attachments(&self) -> Vec<PartId> {
        let root = &self.parts[ROOT];
        let ctype = content_type(root);
        let (maintype, subtype) = ctype.split_once('/').unwrap_or((&ctype, ""));
        if maintype != "multipart" || subtype == "alternative" {
            return Vec::new();
        }
        let Payload::Parts(parts) = &root.payload else {
            return Vec::new();
        };
        if subtype == "related" {
            if let Some(start) = root.get_param("start", "content-type").filter(|s| !s.is_empty()) {
                let mut found = false;
                let mut atts = Vec::new();
                for &p in parts {
                    if self.parts[p].get("content-id").as_deref() == Some(start.as_str()) {
                        found = true;
                    } else {
                        atts.push(p);
                    }
                }
                if found {
                    return atts;
                }
            }
            return parts.iter().skip(1).copied().collect();
        }
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for &p in parts {
            let part = &self.parts[p];
            let ct = content_type(part);
            let (mt, st) = ct.split_once('/').unwrap_or((&ct, ""));
            let body_type = matches!(
                (mt, st),
                ("text", "plain") | ("text", "html") | ("multipart", "related") | ("multipart", "alternative")
            );
            if body_type && !part.is_attachment() && !seen.iter().any(|s| s == st) {
                seen.push(st.to_string());
                continue;
            }
            out.push(p);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_universally() {
        let l = split_lines(b"a\r\nb\rc\nd");
        assert_eq!(l, [b"a\r\n".to_vec(), b"b\r".to_vec(), b"c\n".to_vec(), b"d".to_vec()]);
    }

    #[test]
    fn boundary_lines() {
        let b: Vec<char> = "B".chars().collect();
        assert_eq!(boundary_match(b"--B\r\n", &b), Some((false, true)));
        assert_eq!(boundary_match(b"--B--", &b), Some((true, false)));
        assert_eq!(boundary_match(b"--B-- \t\n", &b), Some((true, true)));
        assert_eq!(boundary_match(b"--Bx\n", &b), None);
        assert_eq!(boundary_match(b"-B\n", &b), None);
    }

    #[test]
    fn params_split_outside_quotes() {
        assert_eq!(parse_param("a; b=\"x;y\"; C=z"), ["a", "b=\"x;y\"", "c=z"]);
    }
}
