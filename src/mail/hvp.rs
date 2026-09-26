//! A port of the parts of CPython 3.11's `email._header_value_parser` and `email.headerregistry`
//! that decide what `msg.get(name)` returns under `policy.default`.
//!
//! Python's header objects are strings built from a parse tree: address headers are re-rendered
//! from the parsed mailboxes, MIME headers print their parameters in a canonical form, and
//! everything else is the unstructured text with encoded words decoded. Matching reses.py byte
//! for byte means building the same tree, so the functions below follow the Python ones rule for
//! rule, with the same names. What I left out is the defect bookkeeping, since no defect changes
//! what reses prints, and the header folding code, since nothing here serialises a message.
//!
//! Input strings carry raw 8-bit bytes as escape characters (see `pystr`), exactly as Python
//! carries them as surrogates.

use std::cell::RefCell;

use super::codec::{self, Errors};
use super::{date, pystr, transfer};

type TT = &'static str;

const WSP: &str = " \t";
const CFWS_LEADER: &str = " \t(";
const SPECIALS: &str = "()<>@,:;.\\\"[]";
const ATOM_ENDS: &str = "()<>@,:;.\\\"[] \t";
const DOT_ATOM_ENDS: &str = "()<>@,:;\\\"[] \t";
const PHRASE_ENDS: &str = ")<>@,:;\\[]";
const TOKEN_ENDS: &str = "()<>@,:;\\\"[]/?= \t";
const ATTRIBUTE_ENDS: &str = "()<>@,:;\\\"[]/?=*'% \t";
const EXTENDED_ATTRIBUTE_ENDS: &str = "()<>@,:;\\\"[]/?=*' \t";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermKind {
    /// ValueTerminal: its value is itself.
    Value,
    /// WhiteSpaceTerminal: its value is a single space.
    Ws,
    /// EWWhiteSpaceTerminal: the space between two encoded words, which vanishes.
    EwWs,
}

/// The TokenList subclasses whose `str()` or `value` differ from the plain list's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cls {
    Plain,
    Ws,
    Bare,
    LocalPart,
    AddrSpec,
    Mailbox,
    InvalidMailbox,
    MimeParams,
}

#[derive(Debug, Clone)]
enum Tok {
    T { s: String, tt: TT, kind: TermKind },
    L(List),
}

#[derive(Debug, Clone)]
struct List {
    cls: Cls,
    tt: TT,
    items: Vec<Tok>,
    // Parameter attributes.
    sectioned: bool,
    extended: bool,
    charset: String,
    // Section attribute.
    number: u64,
}

impl List {
    fn new(cls: Cls, tt: TT) -> List {
        List {
            cls,
            tt,
            items: Vec::new(),
            sectioned: false,
            extended: false,
            charset: "us-ascii".into(),
            number: 0,
        }
    }

    fn with(cls: Cls, tt: TT, items: Vec<Tok>) -> List {
        let mut l = List::new(cls, tt);
        l.items = items;
        l
    }

    fn push(&mut self, t: impl Into<Tok>) {
        self.items.push(t.into());
    }

    fn section_number(&self) -> u64 {
        if self.sectioned {
            match self.items.get(1) {
                Some(Tok::L(s)) => s.number,
                _ => 0,
            }
        } else {
            0
        }
    }
}

impl From<List> for Tok {
    fn from(l: List) -> Tok {
        Tok::L(l)
    }
}

fn vt(s: impl Into<String>, tt: TT) -> Tok {
    Tok::T {
        s: s.into(),
        tt,
        kind: TermKind::Value,
    }
}

fn wt(s: impl Into<String>, tt: TT) -> Tok {
    Tok::T {
        s: s.into(),
        tt,
        kind: TermKind::Ws,
    }
}

fn dot() -> Tok {
    vt(".", "dot")
}

fn quote_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

impl Tok {
    fn tt(&self) -> TT {
        match self {
            Tok::T { tt, .. } => tt,
            Tok::L(l) => l.tt,
        }
    }

    fn items(&self) -> &[Tok] {
        match self {
            Tok::T { .. } => &[],
            Tok::L(l) => &l.items,
        }
    }

    fn as_list(&self) -> Option<&List> {
        match self {
            Tok::L(l) => Some(l),
            Tok::T { .. } => None,
        }
    }

    fn as_list_mut(&mut self) -> Option<&mut List> {
        match self {
            Tok::L(l) => Some(l),
            Tok::T { .. } => None,
        }
    }

    /// `str(token)`.
    fn to_str(&self) -> String {
        match self {
            Tok::T {
                kind: TermKind::EwWs,
                ..
            } => String::new(),
            Tok::T { s, .. } => s.clone(),
            Tok::L(l) => match l.cls {
                Cls::Bare => quote_string(&join_str(&l.items)),
                Cls::MimeParams => {
                    let params: Vec<String> = mime_params(l)
                        .into_iter()
                        .map(|(name, value)| {
                            if value.is_empty() {
                                name
                            } else {
                                format!("{name}={}", quote_string(&value))
                            }
                        })
                        .collect();
                    if params.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", params.join("; "))
                    }
                }
                _ => join_str(&l.items),
            },
        }
    }

    /// `token.value`.
    fn value(&self) -> String {
        match self {
            Tok::T { s, kind, .. } => match kind {
                TermKind::Value => s.clone(),
                TermKind::Ws => " ".into(),
                TermKind::EwWs => String::new(),
            },
            Tok::L(l) => match l.cls {
                Cls::Ws => " ".into(),
                Cls::Bare => join_str(&l.items),
                Cls::LocalPart => match l.items.first() {
                    Some(q) if q.tt() == "quoted-string" => quoted_value(q),
                    Some(t) => t.value(),
                    None => String::new(),
                },
                Cls::AddrSpec => {
                    if l.items.len() < 3 {
                        l.items.first().map(Tok::value).unwrap_or_default()
                    } else {
                        format!(
                            "{}{}{}",
                            pystr::rstrip(&l.items[0].value()),
                            l.items[1].value(),
                            pystr::lstrip(&l.items[2].value())
                        )
                    }
                }
                _ => join_value(&l.items),
            },
        }
    }
}

fn join_str(items: &[Tok]) -> String {
    items.iter().map(Tok::to_str).collect()
}

fn join_value(items: &[Tok]) -> String {
    items.iter().map(Tok::value).collect()
}

/// QuotedString.quoted_value.
fn quoted_value(q: &Tok) -> String {
    q.items()
        .iter()
        .map(|x| {
            if x.tt() == "bare-quoted-string" {
                x.to_str()
            } else {
                x.value()
            }
        })
        .collect()
}

/// QuotedString.stripped_value (also `content`).
fn quoted_stripped(q: &Tok) -> Option<String> {
    q.items()
        .iter()
        .find(|x| x.tt() == "bare-quoted-string")
        .map(Tok::value)
}

// ---------------------------------------------------------------------------------------------
// Parser

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PErr {
    /// HeaderParseError.
    Parse,
    /// _InvalidEwError, a HeaderParseError subclass that get_unstructured tells apart.
    InvalidEw,
}

type R<'a, T = Tok> = Result<(T, &'a str), PErr>;

fn first(v: &str) -> Option<char> {
    v.chars().next()
}

fn starts_in(v: &str, set: &str) -> bool {
    first(v).is_some_and(|c| set.contains(c))
}

fn starts(v: &str, c: char) -> bool {
    first(v) == Some(c)
}

fn rest1(v: &str) -> &str {
    let mut it = v.chars();
    it.next();
    it.as_str()
}

/// `_wsp_splitter(value, 1)`: the text before the first run of spaces/tabs, and the rest.
fn split_wsp(v: &str) -> (&str, &str) {
    match v.find([' ', '\t']) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    }
}

fn get_ptext_to_endchars<'a>(value: &'a str, endchars: &str) -> (String, &'a str, bool) {
    let (fragment, _) = split_wsp(value);
    let mut vchars = String::new();
    let mut escape = false;
    let mut had_qp = false;
    let mut end = fragment.len();
    for (pos, c) in fragment.char_indices() {
        if c == '\\' {
            if escape {
                escape = false;
                had_qp = true;
            } else {
                escape = true;
                continue;
            }
        }
        if escape {
            escape = false;
        } else if endchars.contains(c) {
            end = pos;
            break;
        }
        vchars.push(c);
    }
    (vchars, &value[end..], had_qp)
}

fn get_fws(value: &str) -> (Tok, &str) {
    let newvalue = pystr::lstrip(value);
    (wt(&value[..value.len() - newvalue.len()], "fws"), newvalue)
}

/// `_ew.decode`: the decoded text, or an error where Python raises ValueError/KeyError.
fn ew_decode(ew_body: &str) -> Result<String, PErr> {
    let parts: Vec<&str> = ew_body.split('?').collect();
    if parts.len() != 3 {
        return Err(PErr::InvalidEw);
    }
    let (charset, _lang) = parts[0].split_once('*').unwrap_or((parts[0], ""));
    let cte = parts[1].to_lowercase();
    let raw = pystr::to_bytes(parts[2]);
    if parts[2]
        .chars()
        .any(|c| !c.is_ascii() && pystr::unescape_char(c).is_none())
    {
        return Err(PErr::InvalidEw);
    }
    let bytes = match cte.as_str() {
        "q" => transfer::decode_q(&raw),
        "b" => transfer::decode_b(&raw),
        _ => return Err(PErr::InvalidEw),
    };
    Ok(match codec::lookup(charset) {
        Some(c) => codec::decode(&bytes, c, Errors::Strict)
            .unwrap_or_else(|| codec::decode(&bytes, c, Errors::SurrogateEscape).unwrap()),
        None => codec::decode(&bytes, codec::Codec::Ascii, Errors::SurrogateEscape).unwrap(),
    })
}

thread_local! {
    /// Where every "?=" sits in the header being decoded: (address, length, offsets). Encoded
    /// words are tried at every "=?" and each try looks for the next "?=", so without this a
    /// long header full of "=?" would be searched once per try.
    static CLOSERS: RefCell<Option<(usize, usize, Vec<usize>)>> = const { RefCell::new(None) };
}

/// Indexes a header's "?=" positions for as long as it's alive.
struct CloserIndex;

impl CloserIndex {
    fn install(value: &str) -> CloserIndex {
        let positions = value.match_indices("?=").map(|(i, _)| i).collect();
        CLOSERS.with(|c| *c.borrow_mut() = Some((value.as_ptr() as usize, value.len(), positions)));
        CloserIndex
    }
}

impl Drop for CloserIndex {
    fn drop(&mut self) {
        CLOSERS.with(|c| *c.borrow_mut() = None);
    }
}

/// `s[from..].find("?=")` as an offset into `s`, from the index when `s` lies inside the
/// indexed header (the parser only ever hands it pieces of that header).
fn find_closer(s: &str, from: usize) -> Option<usize> {
    let start = s.as_ptr() as usize;
    let indexed = CLOSERS.with(|c| {
        let c = c.borrow();
        let (base, len, positions) = c.as_ref()?;
        if start < *base || start + s.len() > base + len {
            return None;
        }
        let off = start - base;
        let k = positions.partition_point(|&p| p < off + from);
        Some(
            positions
                .get(k)
                .map(|&p| p - off)
                .filter(|&p| p + 2 <= s.len()),
        )
    });
    match indexed {
        Some(found) => found,
        None => s.get(from..)?.find("?=").map(|i| i + from),
    }
}

/// How many "?" `s` has, counting no further than `limit`.
fn count_q(s: &str, limit: usize) -> usize {
    s.bytes().filter(|&b| b == b'?').take(limit).count()
}

fn get_encoded_word(value: &str) -> R<'_> {
    if !value.starts_with("=?") {
        return Err(PErr::Parse);
    }
    let body = &value[2..];
    let Some(i) = find_closer(value, 2).map(|p| p - 2) else {
        return Err(PErr::Parse);
    };
    let head = &body[..i];
    let mut questions = count_q(head, 3);
    let mut end = i; // where the encoded word's text stops, within body
    let mut rest_at = i + 2; // where the rest of the value starts, within body
    let rest = &body[rest_at..];
    let mut rc = rest.chars();
    if let (Some(a), Some(b)) = (rc.next(), rc.next())
        && a.is_ascii_hexdigit()
        && b.is_ascii_hexdigit()
        && questions < 2
    {
        // The ? after the CTE was followed by an =XX escape, so the real end is further on.
        match find_closer(value, 2 + rest_at) {
            Some(k) => {
                let k = k - 2;
                questions += 1 + count_q(&body[rest_at..k], 3);
                end = k;
                rest_at = k + 2;
            }
            None => {
                questions += 1 + count_q(rest, 3);
                end = body.len();
                rest_at = body.len();
            }
        }
    }
    // _ew.decode needs exactly charset?cte?text; checking that first means a failed try
    // costs nothing like the length of what it looked at.
    if questions != 2 {
        return Err(PErr::InvalidEw);
    }
    let rest = &body[rest_at..];
    let mut text = ew_decode(&body[..end])?;
    let mut ew = List::new(Cls::Plain, "encoded-word");
    while !text.is_empty() {
        if starts_in(&text, WSP) {
            let (t, r) = get_fws(&text);
            ew.push(t);
            text = r.to_string();
            continue;
        }
        let (chars, r) = split_wsp(&text);
        ew.push(vt(chars, "vtext"));
        text = r.to_string();
    }
    Ok((ew.into(), rest))
}

/// Lookup tables over an unstructured value so the per-token work in get_unstructured doesn't
/// rescan the rest of the header each time round.
struct Scan<'a> {
    bytes: &'a [u8],
    /// Next space or tab at or after each offset.
    ws_next: Vec<usize>,
    /// Next "?" at or after each offset.
    q_next: Vec<usize>,
    /// Offsets of every "=?" and every "?=".
    openers: Vec<usize>,
    closers: Vec<usize>,
    /// For the whitespace-delimited token ending at `.0`, the last offset where
    /// `rfc2047_matcher` could start a match inside it.
    region: Option<(usize, Option<usize>)>,
}

impl<'a> Scan<'a> {
    fn new(value: &'a str) -> Scan<'a> {
        let bytes = value.as_bytes();
        let n = bytes.len();
        let mut ws_next = vec![n; n + 1];
        let mut q_next = vec![n; n + 1];
        for i in (0..n).rev() {
            ws_next[i] = if matches!(bytes[i], b' ' | b'\t') {
                i
            } else {
                ws_next[i + 1]
            };
            q_next[i] = if bytes[i] == b'?' { i } else { q_next[i + 1] };
        }
        Scan {
            bytes,
            ws_next,
            q_next,
            openers: value.match_indices("=?").map(|(i, _)| i).collect(),
            closers: value.match_indices("?=").map(|(i, _)| i).collect(),
            region: None,
        }
    }

    /// `rfc2047_matcher.search(tok)` for the token starting at `pos`, which is
    /// `=\?[^?]*\?[qQbB]\?.*?\?=` somewhere inside it.
    fn has_encoded_word(&mut self, pos: usize) -> bool {
        let end = self.ws_next[pos];
        if self.region.is_none_or(|(e, _)| e != end) {
            let last_closer = self.closers.partition_point(|&q| q + 2 <= end);
            let last_closer = last_closer.checked_sub(1).map(|k| self.closers[k]);
            let from = self.openers.partition_point(|&p| p < pos);
            let mut best = None;
            for &p in self.openers[from..].iter().take_while(|&&p| p < end) {
                let j = self.q_next[(p + 2).min(self.bytes.len())];
                if j + 2 < end
                    && matches!(self.bytes[j + 1], b'q' | b'Q' | b'b' | b'B')
                    && self.bytes[j + 2] == b'?'
                    && last_closer.is_some_and(|q| q >= j + 3)
                {
                    best = Some(p);
                }
            }
            self.region = Some((end, best));
        }
        self.region
            .and_then(|(_, best)| best)
            .is_some_and(|p| p >= pos)
    }

    fn first_opener(&self, pos: usize) -> Option<usize> {
        let k = self.openers.partition_point(|&p| p < pos);
        self.openers.get(k).copied()
    }
}

fn get_unstructured(full: &str) -> List {
    let mut u = List::new(Cls::Plain, "unstructured");
    let mut scan = Scan::new(full);
    let mut pos = 0;
    while pos < full.len() {
        let value = &full[pos..];
        if starts_in(value, WSP) {
            let (t, r) = get_fws(value);
            u.push(t);
            pos = full.len() - r.len();
            continue;
        }
        let mut valid_ew = true;
        if value.starts_with("=?") {
            match get_encoded_word(value) {
                Err(PErr::InvalidEw) => valid_ew = false,
                Err(PErr::Parse) => {}
                Ok((tok, r)) => {
                    let n = u.items.len();
                    let mut have_ws = true;
                    if n > 0 && u.items[n - 1].tt() != "fws" {
                        have_ws = false;
                    }
                    if have_ws && n > 1 && u.items[n - 2].tt() == "encoded-word" {
                        let s = u.items[n - 1].to_str();
                        u.items[n - 1] = Tok::T {
                            s,
                            tt: "fws",
                            kind: TermKind::EwWs,
                        };
                    }
                    u.push(tok);
                    pos = full.len() - r.len();
                    continue;
                }
            }
        }
        let mut end = scan.ws_next[pos];
        if valid_ew && scan.has_encoded_word(pos) {
            end = scan.first_opener(pos).expect("the matcher found =?");
            if end == pos {
                // Python would loop forever here; take the "=" as text and move on.
                end = pos + 1;
            }
        }
        u.push(vt(&full[pos..end], "vtext"));
        pos = end;
    }
    u
}

fn get_qp_ctext(value: &str) -> (Tok, &str) {
    let (p, r, _) = get_ptext_to_endchars(value, "()");
    (wt(p, "ptext"), r)
}

fn get_qcontent(value: &str) -> (Tok, &str) {
    let (p, r, _) = get_ptext_to_endchars(value, "\"");
    (vt(p, "ptext"), r)
}

fn run_not_in<'a>(value: &'a str, ends: &str) -> (&'a str, &'a str) {
    let end = value
        .find(|c: char| ends.contains(c))
        .unwrap_or(value.len());
    (&value[..end], &value[end..])
}

fn get_atext(value: &str) -> R<'_> {
    let (a, r) = run_not_in(value, ATOM_ENDS);
    if a.is_empty() {
        return Err(PErr::Parse);
    }
    Ok((vt(a, "atext"), r))
}

fn get_bare_quoted_string(value: &str) -> R<'_> {
    if !starts(value, '"') {
        return Err(PErr::Parse);
    }
    let mut bare = List::new(Cls::Bare, "bare-quoted-string");
    let mut value = &value[1..];
    if starts(value, '"') {
        let (t, r) = get_qcontent(value);
        bare.push(t);
        value = r;
    }
    while !value.is_empty() && !starts(value, '"') {
        let tok;
        if starts_in(value, WSP) {
            (tok, value) = get_fws(value);
        } else if value.starts_with("=?") {
            let mut valid_ew = false;
            match get_encoded_word(value) {
                Ok((t, r)) => {
                    tok = t;
                    value = r;
                    valid_ew = true;
                }
                Err(_) => (tok, value) = get_qcontent(value),
            }
            let n = bare.items.len();
            if valid_ew
                && n > 1
                && bare.items[n - 1].tt() == "fws"
                && bare.items[n - 2].tt() == "encoded-word"
            {
                let s = bare.items[n - 1].to_str();
                bare.items[n - 1] = Tok::T {
                    s,
                    tt: "fws",
                    kind: TermKind::EwWs,
                };
            }
        } else {
            (tok, value) = get_qcontent(value);
        }
        bare.push(tok);
    }
    if value.is_empty() {
        return Ok((bare.into(), value));
    }
    Ok((bare.into(), &value[1..]))
}

/// `get_comment`. Python builds a Comment node per nesting level and recurses both to parse it
/// and to print it. Nothing ever looks inside a comment except to print it, so I build its
/// `str()` directly, one open level per stack entry, and return it as a single whitespace-valued
/// terminal. The text is the same, and neither the parse nor any later walk over the tree
/// recurses, however deep the parens go.
fn get_comment(value: &str) -> R<'_> {
    if !value.is_empty() && !starts(value, '(') {
        return Err(PErr::Parse);
    }
    let mut levels: Vec<String> = vec![String::from("(")];
    let mut value = rest1(value);
    let escape = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)")
    };
    loop {
        if value.is_empty() {
            // End of header inside a comment: every open level still prints its ")".
            let mut done = String::new();
            while let Some(mut level) = levels.pop() {
                level.push_str(&done);
                level.push(')');
                done = level;
            }
            return Ok((wt(done, "comment"), value));
        }
        if starts(value, ')') {
            value = &value[1..];
            let mut level = levels.pop().expect("an open comment");
            level.push(')');
            match levels.last_mut() {
                Some(parent) => parent.push_str(&level),
                None => return Ok((wt(level, "comment"), value)),
            }
            continue;
        }
        let top = levels.last_mut().expect("an open comment");
        if starts_in(value, WSP) {
            let (tok, rest) = get_fws(value);
            top.push_str(&tok.to_str());
            value = rest;
        } else if starts(value, '(') {
            levels.push(String::from("("));
            value = &value[1..];
        } else {
            let (tok, rest) = get_qp_ctext(value);
            top.push_str(&escape(&tok.to_str()));
            value = rest;
        }
    }
}

fn get_cfws(mut value: &str) -> R<'_> {
    let mut cfws = List::new(Cls::Ws, "cfws");
    while starts_in(value, CFWS_LEADER) {
        let tok;
        if starts_in(value, WSP) {
            (tok, value) = get_fws(value);
        } else {
            (tok, value) = get_comment(value)?;
        }
        cfws.push(tok);
    }
    Ok((cfws.into(), value))
}

fn get_quoted_string(mut value: &str) -> R<'_> {
    let mut q = List::new(Cls::Plain, "quoted-string");
    let mut tok;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        q.push(tok);
    }
    (tok, value) = get_bare_quoted_string(value)?;
    q.push(tok);
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        q.push(tok);
    }
    Ok((q.into(), value))
}

fn get_atom(mut value: &str) -> R<'_> {
    let mut atom = List::new(Cls::Plain, "atom");
    let mut tok;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        atom.push(tok);
    }
    if starts_in(value, ATOM_ENDS) {
        return Err(PErr::Parse);
    }
    if value.starts_with("=?") {
        match get_encoded_word(value) {
            Ok((t, r)) => (tok, value) = (t, r),
            Err(_) => (tok, value) = get_atext(value)?,
        }
    } else {
        (tok, value) = get_atext(value)?;
    }
    atom.push(tok);
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        atom.push(tok);
    }
    Ok((atom.into(), value))
}

fn get_dot_atom_text(mut value: &str) -> R<'_> {
    let mut dat = List::new(Cls::Plain, "dot-atom-text");
    if value.is_empty() || starts_in(value, ATOM_ENDS) {
        return Err(PErr::Parse);
    }
    while !value.is_empty() && !starts_in(value, ATOM_ENDS) {
        let tok;
        (tok, value) = get_atext(value)?;
        dat.push(tok);
        if starts(value, '.') {
            dat.push(dot());
            value = &value[1..];
        }
    }
    if dat.items.last().is_some_and(|t| t.tt() == "dot") {
        return Err(PErr::Parse);
    }
    Ok((dat.into(), value))
}

fn get_dot_atom(mut value: &str) -> R<'_> {
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut da = List::new(Cls::Plain, "dot-atom");
    let mut tok;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        da.push(tok);
    }
    if value.starts_with("=?") {
        match get_encoded_word(value) {
            Ok((t, r)) => (tok, value) = (t, r),
            Err(_) => (tok, value) = get_dot_atom_text(value)?,
        }
    } else {
        (tok, value) = get_dot_atom_text(value)?;
    }
    da.push(tok);
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        da.push(tok);
    }
    Ok((da.into(), value))
}

/// `token[:0] = [leader]`.
fn prepend(tok: &mut Tok, leader: Tok) {
    if let Some(l) = tok.as_list_mut() {
        l.items.insert(0, leader);
    }
}

fn get_word(mut value: &str) -> R<'_> {
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut leader = None;
    if starts_in(value, CFWS_LEADER) {
        let l;
        (l, value) = get_cfws(value)?;
        leader = Some(l);
    }
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut tok;
    if starts(value, '"') {
        (tok, value) = get_quoted_string(value)?;
    } else if starts_in(value, SPECIALS) {
        return Err(PErr::Parse);
    } else {
        (tok, value) = get_atom(value)?;
    }
    if let Some(l) = leader {
        prepend(&mut tok, l);
    }
    Ok((tok, value))
}

fn get_phrase(mut value: &str) -> R<'_, List> {
    let mut phrase = List::new(Cls::Plain, "phrase");
    if let Ok((t, r)) = get_word(value) {
        phrase.push(t);
        value = r;
    }
    while !value.is_empty() && !starts_in(value, PHRASE_ENDS) {
        if starts(value, '.') {
            phrase.push(dot());
            value = &value[1..];
        } else {
            let tok;
            match get_word(value) {
                Ok((t, r)) => (tok, value) = (t, r),
                Err(e) => {
                    if starts_in(value, CFWS_LEADER) {
                        (tok, value) = get_cfws(value)?;
                    } else {
                        return Err(e);
                    }
                }
            }
            phrase.push(tok);
        }
    }
    Ok((phrase, value))
}

fn get_local_part(mut value: &str) -> R<'_> {
    let mut lp = List::new(Cls::LocalPart, "local-part");
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut leader = None;
    if starts_in(value, CFWS_LEADER) {
        let l;
        (l, value) = get_cfws(value)?;
        leader = Some(l);
    }
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut tok = match get_dot_atom(value) {
        Ok((t, r)) => {
            value = r;
            t
        }
        Err(_) => match get_word(value) {
            Ok((t, r)) => {
                value = r;
                t
            }
            Err(e) => {
                let c = first(value).unwrap();
                if c != '\\' && PHRASE_ENDS.contains(c) {
                    return Err(e);
                }
                List::new(Cls::Plain, "").into()
            }
        },
    };
    if let Some(l) = leader {
        prepend(&mut tok, l);
    }
    lp.push(tok);
    if let Some(c) = first(value)
        && (c == '\\' || !PHRASE_ENDS.contains(c))
    {
        let joined = format!("{}{}", lp.items[0].to_str(), value);
        let (obs, rest) = get_obs_local_part(&joined)?;
        // `rest` is a suffix of `joined`, which is itself `str(local_part) + value`, and every
        // character of the old local part has been consumed by now, so it is also a suffix of
        // `value`. Map it back so the caller keeps borrowing the original header.
        let consumed = joined.len() - rest.len();
        let prefix = joined.len() - value.len();
        value = if consumed >= prefix {
            &value[consumed - prefix..]
        } else {
            value
        };
        lp.items[0] = obs.into();
    }
    Ok((lp.into(), value))
}

fn get_obs_local_part(mut value: &str) -> R<'_, List> {
    let mut obs = List::new(Cls::Plain, "obs-local-part");
    while let Some(c) = first(value) {
        if !(c == '\\' || !PHRASE_ENDS.contains(c)) {
            break;
        }
        if c == '.' {
            obs.push(dot());
            value = &value[1..];
            continue;
        } else if c == '\\' {
            obs.push(vt("\\", "misplaced-special"));
            value = &value[1..];
            continue;
        }
        let tok;
        match get_word(value) {
            Ok((t, r)) => (tok, value) = (t, r),
            Err(e) => {
                if !CFWS_LEADER.contains(c) {
                    return Err(e);
                }
                (tok, value) = get_cfws(value)?;
            }
        }
        obs.push(tok);
    }
    if obs.items.is_empty() {
        // Python indexes obs_local_part[0] here and dies with IndexError.
        return Err(PErr::Parse);
    }
    Ok((obs, value))
}

fn get_dtext(value: &str) -> (Tok, &str) {
    let (p, r, _) = get_ptext_to_endchars(value, "[]");
    (vt(p, "ptext"), r)
}

fn get_domain_literal(mut value: &str) -> R<'_> {
    let mut dl = List::new(Cls::Plain, "domain-literal");
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut tok;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        dl.push(tok);
    }
    if !starts(value, '[') {
        return Err(PErr::Parse);
    }
    value = &value[1..];
    let early_end = |dl: &mut List, value: &str| {
        if value.is_empty() {
            dl.push(vt("]", "domain-literal-end"));
            true
        } else {
            false
        }
    };
    if early_end(&mut dl, value) {
        return Ok((dl.into(), value));
    }
    dl.push(vt("[", "domain-literal-start"));
    if starts_in(value, WSP) {
        (tok, value) = get_fws(value);
        dl.push(tok);
    }
    (tok, value) = get_dtext(value);
    dl.push(tok);
    if early_end(&mut dl, value) {
        return Ok((dl.into(), value));
    }
    if starts_in(value, WSP) {
        (tok, value) = get_fws(value);
        dl.push(tok);
    }
    if early_end(&mut dl, value) {
        return Ok((dl.into(), value));
    }
    if !starts(value, ']') {
        return Err(PErr::Parse);
    }
    dl.push(vt("]", "domain-literal-end"));
    value = &value[1..];
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        dl.push(tok);
    }
    Ok((dl.into(), value))
}

fn get_domain(mut value: &str) -> R<'_> {
    let mut domain = List::new(Cls::Plain, "domain");
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut leader = None;
    if starts_in(value, CFWS_LEADER) {
        let l;
        (l, value) = get_cfws(value)?;
        leader = Some(l);
    }
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    if starts(value, '[') {
        let mut tok;
        (tok, value) = get_domain_literal(value)?;
        if let Some(l) = leader {
            prepend(&mut tok, l);
        }
        domain.push(tok);
        return Ok((domain.into(), value));
    }
    let mut tok = match get_dot_atom(value) {
        Ok((t, r)) => {
            value = r;
            t
        }
        Err(_) => {
            let (t, r) = get_atom(value)?;
            value = r;
            t
        }
    };
    if starts(value, '@') {
        return Err(PErr::Parse);
    }
    if let Some(l) = leader {
        prepend(&mut tok, l);
    }
    domain.push(tok);
    if starts(value, '.') {
        if domain.items[0].tt() == "dot-atom" {
            let inner = domain.items.remove(0);
            if let Tok::L(l) = inner {
                domain.items = l.items;
            }
        }
        while starts(value, '.') {
            domain.push(dot());
            let tok;
            (tok, value) = get_atom(&value[1..])?;
            domain.push(tok);
        }
    }
    Ok((domain.into(), value))
}

fn get_addr_spec(mut value: &str) -> R<'_> {
    let mut a = List::new(Cls::AddrSpec, "addr-spec");
    let mut tok;
    (tok, value) = get_local_part(value)?;
    a.push(tok);
    if !starts(value, '@') {
        return Ok((a.into(), value));
    }
    a.push(vt("@", "address-at-symbol"));
    (tok, value) = get_domain(&value[1..])?;
    a.push(tok);
    Ok((a.into(), value))
}

fn get_obs_route(mut value: &str) -> R<'_> {
    let mut route = List::new(Cls::Plain, "obs-route");
    let mut tok;
    while starts(value, ',') || starts_in(value, CFWS_LEADER) {
        if starts_in(value, CFWS_LEADER) {
            (tok, value) = get_cfws(value)?;
            route.push(tok);
        } else {
            route.push(vt(",", "list-separator"));
            value = &value[1..];
        }
    }
    if !starts(value, '@') {
        return Err(PErr::Parse);
    }
    route.push(vt("@", "route-component-marker"));
    (tok, value) = get_domain(&value[1..])?;
    route.push(tok);
    while starts(value, ',') {
        route.push(vt(",", "list-separator"));
        value = &value[1..];
        if value.is_empty() {
            break;
        }
        if starts_in(value, CFWS_LEADER) {
            (tok, value) = get_cfws(value)?;
            route.push(tok);
        }
        if starts(value, '@') {
            route.push(vt("@", "route-component-marker"));
            (tok, value) = get_domain(&value[1..])?;
            route.push(tok);
        }
    }
    if !starts(value, ':') {
        return Err(PErr::Parse);
    }
    route.push(vt(":", "end-of-obs-route-marker"));
    Ok((route.into(), &value[1..]))
}

fn get_angle_addr(mut value: &str) -> R<'_> {
    let mut aa = List::new(Cls::Plain, "angle-addr");
    let mut tok;
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        aa.push(tok);
    }
    if !starts(value, '<') {
        return Err(PErr::Parse);
    }
    aa.push(vt("<", "angle-addr-start"));
    value = &value[1..];
    if value.is_empty() {
        // Python indexes value[0] here and raises IndexError.
        return Err(PErr::Parse);
    }
    if starts(value, '>') {
        aa.push(vt(">", "angle-addr-end"));
        return Ok((aa.into(), &value[1..]));
    }
    match get_addr_spec(value) {
        Ok((t, r)) => (tok, value) = (t, r),
        Err(_) => {
            let (route, r) = get_obs_route(value)?;
            aa.push(route);
            (tok, value) = get_addr_spec(r)?;
        }
    }
    aa.push(tok);
    if starts(value, '>') {
        value = &value[1..];
    }
    aa.push(vt(">", "angle-addr-end"));
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        aa.push(tok);
    }
    Ok((aa.into(), value))
}

fn get_display_name(value: &str) -> R<'_, List> {
    let (phrase, value) = get_phrase(value)?;
    Ok((List::with(Cls::Plain, "display-name", phrase.items), value))
}

fn get_name_addr(mut value: &str) -> R<'_> {
    let mut na = List::new(Cls::Plain, "name-addr");
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut leader = None;
    if starts_in(value, CFWS_LEADER) {
        let l;
        (l, value) = get_cfws(value)?;
        leader = Some(l);
        if value.is_empty() {
            return Err(PErr::Parse);
        }
    }
    if !starts(value, '<') {
        if starts_in(value, PHRASE_ENDS) {
            return Err(PErr::Parse);
        }
        let mut dn;
        (dn, value) = get_display_name(value)?;
        if value.is_empty() {
            return Err(PErr::Parse);
        }
        if let Some(l) = leader.take() {
            match dn.items.first_mut() {
                Some(first @ Tok::L(_)) => prepend(first, l),
                _ => dn.items.insert(0, l),
            }
        }
        na.push(dn);
    }
    let mut tok;
    (tok, value) = get_angle_addr(value)?;
    if let Some(l) = leader {
        prepend(&mut tok, l);
    }
    na.push(tok);
    Ok((na.into(), value))
}

fn get_mailbox(value: &str) -> R<'_> {
    let mut mb = List::new(Cls::Mailbox, "mailbox");
    let (tok, value) = match get_name_addr(value) {
        Ok(x) => x,
        Err(_) => get_addr_spec(value)?,
    };
    mb.push(tok);
    Ok((mb.into(), value))
}

fn get_invalid_mailbox<'a>(mut value: &'a str, endchars: &str) -> R<'a, List> {
    let mut im = List::new(Cls::InvalidMailbox, "invalid-mailbox");
    while let Some(c) = first(value) {
        if endchars.contains(c) {
            break;
        }
        if PHRASE_ENDS.contains(c) {
            im.push(vt(c, "misplaced-special"));
            value = rest1(value);
        } else {
            let (p, r) = get_phrase(value)?;
            if r.len() == value.len() {
                // No progress; Python can't reach this, but never spin.
                im.push(vt(c, "misplaced-special"));
                value = rest1(value);
                continue;
            }
            im.push(p);
            value = r;
        }
    }
    Ok((im, value))
}

fn extend_last(list: &mut List, extra: List) {
    if let Some(Tok::L(last)) = list.items.last_mut() {
        last.items.extend(extra.items);
    }
}

fn get_mailbox_list(mut value: &str) -> R<'_> {
    let mut ml = List::new(Cls::Plain, "mailbox-list");
    while !value.is_empty() && !starts(value, ';') {
        match get_mailbox(value) {
            Ok((t, r)) => {
                ml.push(t);
                value = r;
            }
            Err(_) => {
                if starts_in(value, CFWS_LEADER) {
                    let leader;
                    (leader, value) = get_cfws(value)?;
                    if value.is_empty() || starts_in(value, ",;") {
                        ml.push(leader);
                    } else {
                        let mut t;
                        (t, value) = get_invalid_mailbox(value, ",;")?;
                        t.items.insert(0, leader);
                        ml.push(t);
                    }
                } else if starts(value, ',') {
                    // An empty element; nothing to keep.
                } else {
                    let t;
                    (t, value) = get_invalid_mailbox(value, ",;")?;
                    ml.push(t);
                }
            }
        }
        if !value.is_empty() && !starts_in(value, ",;") {
            let t;
            (t, value) = get_invalid_mailbox(value, ",;")?;
            extend_last(&mut ml, t);
        }
        if starts(value, ',') {
            ml.push(vt(",", "list-separator"));
            value = &value[1..];
        }
    }
    Ok((ml.into(), value))
}

fn get_group_list(mut value: &str) -> R<'_> {
    let mut gl = List::new(Cls::Plain, "group-list");
    if value.is_empty() {
        return Ok((gl.into(), value));
    }
    let mut leader = None;
    if starts_in(value, CFWS_LEADER) {
        let l;
        (l, value) = get_cfws(value)?;
        if value.is_empty() || starts(value, ';') {
            gl.push(l);
            return Ok((gl.into(), value));
        }
        leader = Some(l);
    }
    let mut tok;
    (tok, value) = get_mailbox_list(value)?;
    if list_all_mailboxes(tok.as_list().unwrap()).is_empty() {
        if let Some(l) = leader {
            gl.push(l);
        }
        if let Tok::L(l) = tok {
            gl.items.extend(l.items);
        }
        return Ok((gl.into(), value));
    }
    if let Some(l) = leader {
        prepend(&mut tok, l);
    }
    gl.push(tok);
    Ok((gl.into(), value))
}

fn get_group(mut value: &str) -> R<'_> {
    let mut group = List::new(Cls::Plain, "group");
    let dn;
    (dn, value) = get_display_name(value)?;
    if !starts(value, ':') {
        return Err(PErr::Parse);
    }
    group.push(dn);
    group.push(vt(":", "group-display-name-terminator"));
    value = &value[1..];
    if starts(value, ';') {
        group.push(vt(";", "group-terminator"));
        return Ok((group.into(), &value[1..]));
    }
    let mut tok;
    (tok, value) = get_group_list(value)?;
    group.push(tok);
    if !value.is_empty() && !starts(value, ';') {
        return Err(PErr::Parse);
    }
    group.push(vt(";", "group-terminator"));
    value = if value.is_empty() { value } else { &value[1..] };
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        group.push(tok);
    }
    Ok((group.into(), value))
}

fn get_address(value: &str) -> R<'_> {
    let (tok, value) = match get_group(value) {
        Ok(x) => x,
        Err(_) => get_mailbox(value)?,
    };
    Ok((List::with(Cls::Plain, "address", vec![tok]).into(), value))
}

fn get_address_list(mut value: &str) -> List {
    let mut al = List::new(Cls::Plain, "address-list");
    // Python raises out of this loop in a few impossible-looking corners (and then reses dies
    // with a traceback); I stop parsing there instead.
    let step = |value: &mut &str, al: &mut List| -> Result<(), PErr> {
        match get_address(value) {
            Ok((t, r)) => {
                al.push(t);
                *value = r;
            }
            Err(_) => {
                if starts_in(value, CFWS_LEADER) {
                    let leader;
                    (leader, *value) = get_cfws(value)?;
                    if value.is_empty() || starts(value, ',') {
                        al.push(leader);
                    } else {
                        let mut t;
                        (t, *value) = get_invalid_mailbox(value, ",")?;
                        t.items.insert(0, leader);
                        al.push(List::with(Cls::Plain, "address", vec![t.into()]));
                    }
                } else if starts(value, ',') {
                    // Empty element.
                } else {
                    let t;
                    (t, *value) = get_invalid_mailbox(value, ",")?;
                    al.push(List::with(Cls::Plain, "address", vec![t.into()]));
                }
            }
        }
        if !value.is_empty() && !starts(value, ',') {
            let t;
            (t, *value) = get_invalid_mailbox(value, ",")?;
            if let Some(Tok::L(addr)) = al.items.last_mut()
                && let Some(Tok::L(mb)) = addr.items.first_mut()
            {
                mb.items.extend(t.items);
            }
        }
        if !value.is_empty() {
            al.push(vt(",", "list-separator"));
            *value = rest1(value);
        }
        Ok(())
    };
    while !value.is_empty() {
        let before = value.len();
        if step(&mut value, &mut al).is_err() || value.len() == before {
            break;
        }
    }
    al
}

fn get_no_fold_literal(value: &str) -> R<'_> {
    let mut nfl = List::new(Cls::Plain, "no-fold-literal");
    if !starts(value, '[') {
        return Err(PErr::Parse);
    }
    nfl.push(vt("[", "no-fold-literal-start"));
    let (tok, value) = get_dtext(&value[1..]);
    nfl.push(tok);
    if !starts(value, ']') {
        return Err(PErr::Parse);
    }
    nfl.push(vt("]", "no-fold-literal-end"));
    Ok((nfl.into(), &value[1..]))
}

fn get_msg_id(mut value: &str) -> R<'_> {
    let mut msg_id = List::new(Cls::Plain, "msg-id");
    let mut tok;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        msg_id.push(tok);
    }
    if !starts(value, '<') {
        return Err(PErr::Parse);
    }
    msg_id.push(vt("<", "msg-id-start"));
    value = &value[1..];
    match get_dot_atom_text(value) {
        Ok((t, r)) => (tok, value) = (t, r),
        Err(_) => {
            let (o, r) = get_obs_local_part(value)?;
            (tok, value) = (o.into(), r);
        }
    }
    msg_id.push(tok);
    if !starts(value, '@') {
        if starts(value, '>') {
            msg_id.push(vt(">", "msg-id-end"));
            value = &value[1..];
        }
        return Ok((msg_id.into(), value));
    }
    msg_id.push(vt("@", "address-at-symbol"));
    value = &value[1..];
    match get_dot_atom_text(value) {
        Ok((t, r)) => (tok, value) = (t, r),
        Err(_) => match get_no_fold_literal(value) {
            Ok((t, r)) => (tok, value) = (t, r),
            Err(_) => (tok, value) = get_domain(value)?,
        },
    }
    msg_id.push(tok);
    if starts(value, '>') {
        value = &value[1..];
    }
    msg_id.push(vt(">", "msg-id-end"));
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        msg_id.push(tok);
    }
    Ok((msg_id.into(), value))
}

fn parse_message_id(value: &str) -> List {
    match get_msg_id(value) {
        // Anything after a valid msg-id is dropped from the tree, and so from str().
        Ok((tok, _rest)) => List::with(Cls::Plain, "message-id", vec![tok]),
        Err(_) => {
            let u = get_unstructured(value);
            List::with(Cls::Plain, "invalid-message-id", u.items)
        }
    }
}

// --- MIME headers ----------------------------------------------------------------------------

fn get_invalid_parameter(mut value: &str) -> R<'_, List> {
    let mut ip = List::new(Cls::Plain, "invalid-parameter");
    while let Some(c) = first(value) {
        if c == ';' {
            break;
        }
        if PHRASE_ENDS.contains(c) {
            ip.push(vt(c, "misplaced-special"));
            value = rest1(value);
        } else {
            let (p, r) = get_phrase(value)?;
            if r.len() == value.len() {
                ip.push(vt(c, "misplaced-special"));
                value = rest1(value);
                continue;
            }
            ip.push(p);
            value = r;
        }
    }
    Ok((ip, value))
}

fn get_token(mut value: &str) -> R<'_> {
    let mut t = List::new(Cls::Plain, "token");
    let mut tok;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        t.push(tok);
    }
    if starts_in(value, TOKEN_ENDS) {
        return Err(PErr::Parse);
    }
    let (text, r) = run_not_in(value, TOKEN_ENDS);
    if text.is_empty() {
        return Err(PErr::Parse);
    }
    t.push(vt(text, "ttext"));
    value = r;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        t.push(tok);
    }
    Ok((t.into(), value))
}

fn get_attrtext(value: &str) -> R<'_> {
    let (text, r) = run_not_in(value, ATTRIBUTE_ENDS);
    if text.is_empty() {
        return Err(PErr::Parse);
    }
    Ok((vt(text, "attrtext"), r))
}

fn get_extended_attrtext(value: &str) -> R<'_> {
    let (text, r) = run_not_in(value, EXTENDED_ATTRIBUTE_ENDS);
    if text.is_empty() {
        return Err(PErr::Parse);
    }
    Ok((vt(text, "extended-attrtext"), r))
}

fn get_attribute_with<'a>(mut value: &'a str, ends: &str, text: fn(&str) -> R<'_>) -> R<'a> {
    let mut a = List::new(Cls::Plain, "attribute");
    let mut tok;
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        a.push(tok);
    }
    if starts_in(value, ends) {
        return Err(PErr::Parse);
    }
    (tok, value) = text(value)?;
    a.push(tok);
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        a.push(tok);
    }
    Ok((a.into(), value))
}

fn get_attribute(value: &str) -> R<'_> {
    get_attribute_with(value, ATTRIBUTE_ENDS, get_attrtext)
}

fn get_extended_attribute(value: &str) -> R<'_> {
    get_attribute_with(value, EXTENDED_ATTRIBUTE_ENDS, get_extended_attrtext)
}

fn get_section(value: &str) -> R<'_, List> {
    let mut section = List::new(Cls::Plain, "section");
    if !starts(value, '*') {
        return Err(PErr::Parse);
    }
    section.push(vt("*", "section-marker"));
    let value = &value[1..];
    let (digits, rest) = value.split_at(
        value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    if digits.is_empty() {
        return Err(PErr::Parse);
    }
    section.number = digits.parse().unwrap_or(u64::MAX);
    section.push(vt(digits, "digits"));
    Ok((section, rest))
}

fn get_value(mut value: &str) -> R<'_> {
    let mut v = List::new(Cls::Plain, "value");
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut leader = None;
    if starts_in(value, CFWS_LEADER) {
        let l;
        (l, value) = get_cfws(value)?;
        leader = Some(l);
    }
    if value.is_empty() {
        return Err(PErr::Parse);
    }
    let mut tok;
    if starts(value, '"') {
        (tok, value) = get_quoted_string(value)?;
    } else {
        (tok, value) = get_extended_attribute(value)?;
    }
    if let Some(l) = leader {
        prepend(&mut tok, l);
    }
    v.push(tok);
    Ok((v.into(), value))
}

/// The tail of get_parameter, after the "=" and any quoted-string hackery. Tokens go into
/// `target`, which is either the parameter itself or the emptied bare-quoted-string.
fn parameter_tail<'b>(
    param: &mut List,
    target: &mut Vec<Tok>,
    mut value: &'b str,
    hack: bool,
) -> Result<&'b str, PErr> {
    let mut token: Option<Tok> = None;
    if !starts(value, '\'') {
        let (t, r) = get_value(value)?;
        token = Some(t);
        value = r;
    }
    if (!param.extended || param.section_number() > 0) && !starts(value, '\'') {
        target.extend(token);
        return Ok(value);
    }
    if value.is_empty() {
        target.extend(token);
        if !hack {
            return Ok(value);
        }
    } else {
        if let Some(tok) = token {
            // Python walks the value's children looking for extended-attrtext and, not finding
            // it at that depth, ends up on the last child.
            if let Some(t) = tok.items().last().cloned() {
                param.charset = t.value();
                target.push(t);
            }
        }
        if !starts(value, '\'') {
            return Err(PErr::Parse);
        }
        target.push(vt("'", "RFC2231-delimiter"));
        value = &value[1..];
        if !value.is_empty() && !starts(value, '\'') {
            let t;
            (t, value) = get_attrtext(value)?;
            target.push(t);
            if !starts(value, '\'') {
                return Err(PErr::Parse);
            }
        }
        target.push(vt("'", "RFC2231-delimiter"));
        value = if value.is_empty() { value } else { &value[1..] };
    }
    let token;
    if hack {
        let mut v = List::new(Cls::Plain, "value");
        while !value.is_empty() {
            let t;
            if starts_in(value, WSP) {
                (t, value) = get_fws(value);
            } else if starts(value, '"') {
                t = vt("\"", "DQUOTE");
                value = &value[1..];
            } else {
                (t, value) = get_qcontent(value);
            }
            v.push(t);
        }
        token = v.into();
    } else {
        (token, value) = get_value(value)?;
    }
    target.push(token);
    Ok(value)
}

fn get_parameter(mut value: &str) -> R<'_> {
    let mut param = List::new(Cls::Plain, "parameter");
    let mut tok;
    (tok, value) = get_attribute(value)?;
    param.push(tok);
    if value.is_empty() || starts(value, ';') {
        return Ok((param.into(), value));
    }
    if starts(value, '*') {
        if let Ok((s, r)) = get_section(value) {
            param.sectioned = true;
            param.push(s);
            value = r;
        }
        if value.is_empty() {
            return Err(PErr::Parse);
        }
        if starts(value, '*') {
            param.push(vt("*", "extended-parameter-marker"));
            value = &value[1..];
            param.extended = true;
        }
    }
    if !starts(value, '=') {
        return Err(PErr::Parse);
    }
    param.push(vt("=", "parameter-separator"));
    value = &value[1..];
    if starts_in(value, CFWS_LEADER) {
        (tok, value) = get_cfws(value)?;
        param.push(tok);
    }
    if param.extended && starts(value, '"') {
        let (qstring, remainder) = get_quoted_string(value)?;
        let inner = quoted_stripped(&qstring).unwrap_or_default();
        let mut semi_valid = false;
        if param.section_number() == 0 {
            if inner.starts_with('\'') {
                semi_valid = true;
            } else {
                let (_, rest) = get_attrtext(&inner)?;
                if rest.starts_with('\'') {
                    semi_valid = true;
                }
            }
        } else if let Ok((_, rest)) = get_extended_attrtext(&inner)
            && rest.is_empty()
        {
            semi_valid = true;
        }
        if semi_valid {
            let mut target = Vec::new();
            parameter_tail(&mut param, &mut target, &inner, true)?;
            let mut qstring = qstring;
            if let Some(q) = qstring.as_list_mut()
                && let Some(Tok::L(bare)) =
                    q.items.iter_mut().find(|t| t.tt() == "bare-quoted-string")
            {
                bare.items = target;
            }
            param.push(qstring);
            return Ok((param.into(), remainder));
        }
    }
    let mut target = Vec::new();
    let rest = parameter_tail(&mut param, &mut target, value, false)?;
    param.items.extend(target);
    Ok((param.into(), rest))
}

fn parse_mime_parameters(mut value: &str) -> List {
    let mut mp = List::new(Cls::MimeParams, "mime-parameters");
    while !value.is_empty() {
        let before = value.len();
        match get_parameter(value) {
            Ok((t, r)) => {
                mp.push(t);
                value = r;
            }
            Err(_) => {
                let mut leader = None;
                if starts_in(value, CFWS_LEADER) {
                    match get_cfws(value) {
                        Ok((l, r)) => {
                            leader = Some(l);
                            value = r;
                        }
                        Err(_) => return mp,
                    }
                }
                if value.is_empty() {
                    mp.items.extend(leader);
                    return mp;
                }
                if starts(value, ';') {
                    mp.items.extend(leader);
                } else {
                    let Ok((mut t, r)) = get_invalid_parameter(value) else {
                        return mp;
                    };
                    if let Some(l) = leader {
                        t.items.insert(0, l);
                    }
                    mp.push(t);
                    value = r;
                }
            }
        }
        if !value.is_empty() && !starts(value, ';') {
            let Ok((t, r)) = get_invalid_parameter(value) else {
                return mp;
            };
            extend_last(&mut mp, t);
            value = r;
        }
        if !value.is_empty() {
            mp.push(vt(";", "parameter-separator"));
            value = &value[1..];
        }
        if value.len() == before {
            break;
        }
    }
    mp
}

fn find_mime_parameters(tl: &mut List, mut value: &str) {
    while let Some(c) = first(value) {
        if c == ';' {
            break;
        }
        if PHRASE_ENDS.contains(c) {
            tl.push(vt(c, "misplaced-special"));
            value = rest1(value);
        } else {
            match get_phrase(value) {
                Ok((p, r)) if r.len() < value.len() => {
                    tl.push(p);
                    value = r;
                }
                _ => {
                    tl.push(vt(c, "misplaced-special"));
                    value = rest1(value);
                }
            }
        }
    }
    if value.is_empty() {
        return;
    }
    tl.push(vt(";", "parameter-separator"));
    tl.push(parse_mime_parameters(&value[1..]));
}

fn parse_content_type_header(value: &str) -> List {
    let mut ctype = List::new(Cls::Plain, "content-type");
    if value.is_empty() {
        return ctype;
    }
    let (tok, value) = match get_token(value) {
        Ok(x) => x,
        Err(_) => {
            find_mime_parameters(&mut ctype, value);
            return ctype;
        }
    };
    ctype.push(tok);
    if !starts(value, '/') {
        if !value.is_empty() {
            find_mime_parameters(&mut ctype, value);
        }
        return ctype;
    }
    ctype.push(vt("/", "content-type-separator"));
    let value = &value[1..];
    let (tok, value) = match get_token(value) {
        Ok(x) => x,
        Err(_) => {
            find_mime_parameters(&mut ctype, value);
            return ctype;
        }
    };
    ctype.push(tok);
    if value.is_empty() {
        return ctype;
    }
    if !starts(value, ';') {
        find_mime_parameters(&mut ctype, value);
        return ctype;
    }
    ctype.push(vt(";", "parameter-separator"));
    ctype.push(parse_mime_parameters(&value[1..]));
    ctype
}

fn parse_content_disposition_header(value: &str) -> (List, Option<String>) {
    let mut disp = List::new(Cls::Plain, "content-disposition");
    if value.is_empty() {
        return (disp, None);
    }
    let (tok, value) = match get_token(value) {
        Ok(x) => x,
        Err(_) => {
            find_mime_parameters(&mut disp, value);
            return (disp, None);
        }
    };
    let cd = pystr::lower(pystr::strip(&tok.value()));
    disp.push(tok);
    if value.is_empty() {
        return (disp, Some(cd));
    }
    if !starts(value, ';') {
        find_mime_parameters(&mut disp, value);
        return (disp, Some(cd));
    }
    disp.push(vt(";", "parameter-separator"));
    disp.push(parse_mime_parameters(&value[1..]));
    (disp, Some(cd))
}

fn parse_content_transfer_encoding_header(value: &str) -> List {
    let mut cte = List::new(Cls::Plain, "content-transfer-encoding");
    if value.is_empty() {
        return cte;
    }
    let mut value = match get_token(value) {
        Ok((tok, r)) => {
            cte.push(tok);
            r
        }
        Err(_) => value,
    };
    while let Some(c) = first(value) {
        if PHRASE_ENDS.contains(c) {
            cte.push(vt(c, "misplaced-special"));
            value = rest1(value);
        } else {
            match get_phrase(value) {
                Ok((p, r)) if r.len() < value.len() => {
                    cte.push(p);
                    value = r;
                }
                _ => {
                    cte.push(vt(c, "misplaced-special"));
                    value = rest1(value);
                }
            }
        }
    }
    cte
}

// --- Parameter values ------------------------------------------------------------------------

/// Parameter.param_value.
fn param_value(p: &List) -> String {
    for t in &p.items {
        if t.tt() == "value" {
            return value_stripped(t);
        }
        if t.tt() == "quoted-string" {
            for b in t.items() {
                if b.tt() == "bare-quoted-string" {
                    for v in b.items() {
                        if v.tt() == "value" {
                            return value_stripped(v);
                        }
                    }
                }
            }
        }
    }
    String::new()
}

/// Value.stripped_value.
fn value_stripped(v: &Tok) -> String {
    let items = v.items();
    let Some(mut tok) = items.first() else {
        return String::new();
    };
    if tok.tt() == "cfws" {
        match items.get(1) {
            Some(t) => tok = t,
            None => return v.value(),
        }
    }
    let tt = tok.tt();
    if tt.ends_with("quoted-string") {
        return quoted_stripped(tok).unwrap_or_default();
    }
    if tt.ends_with("attribute") {
        // Attribute.stripped_value: the first *attrtext child's value.
        return tok
            .items()
            .iter()
            .find(|x| x.tt().ends_with("attrtext"))
            .map(Tok::value)
            .unwrap_or_default();
    }
    v.value()
}

/// MimeParameters.params: RFC 2231 sections joined and decoded.
fn mime_params(mp: &List) -> Vec<(String, String)> {
    let mut order: Vec<String> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut groups: Vec<Vec<(u64, &List)>> = Vec::new();
    for t in &mp.items {
        let Tok::L(p) = t else { continue };
        if !p.tt.ends_with("parameter") {
            continue;
        }
        match p.items.first() {
            Some(a) if a.tt() == "attribute" => {
                let name = pystr::strip(&a.value()).to_string();
                let idx = *index.entry(name.clone()).or_insert_with(|| {
                    order.push(name);
                    groups.push(Vec::new());
                    order.len() - 1
                });
                groups[idx].push((p.section_number(), p));
            }
            _ => continue,
        }
    }
    let mut out = Vec::new();
    for (name, mut parts) in order.into_iter().zip(groups) {
        parts.sort_by_key(|(n, _)| *n);
        let first_param = parts[0].1;
        let charset = first_param.charset.clone();
        if !first_param.extended && parts.len() > 1 && parts[1].0 == 0 {
            parts.truncate(1);
        }
        let mut value = String::new();
        let mut i = 0u64;
        for (section, param) in parts {
            if section != i && !param.extended {
                continue;
            }
            i += 1;
            let mut v = param_value(param);
            if param.extended {
                if pystr::has_escapes(&v) {
                    // unquote_to_bytes can't encode the escaped bytes, so Python falls back to
                    // unquote(..., encoding='latin-1') on the ASCII runs.
                    v = unquote_latin1_keep_escapes(&v);
                } else {
                    let bytes = transfer::unquote_to_bytes(v.as_bytes());
                    let c = codec::lookup(&charset).unwrap_or(codec::Codec::Ascii);
                    v = codec::decode(&bytes, c, Errors::SurrogateEscape).unwrap();
                }
            }
            value.push_str(&v);
        }
        out.push((name, value));
    }
    out
}

fn unquote_latin1_keep_escapes(s: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let bytes = transfer::unquote_to_bytes(run.as_bytes());
        out.extend(bytes.iter().map(|&b| b as char));
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

// --- Addresses -------------------------------------------------------------------------------

/// DisplayName.display_name.
fn display_name(dn: &Tok) -> String {
    let mut res: Vec<Tok> = dn.items().to_vec();
    if res.is_empty() {
        return String::new();
    }
    if res[0].tt() == "cfws" {
        res.remove(0);
    } else if res[0].items().first().is_some_and(|t| t.tt() == "cfws") {
        let rest = res[0].items()[1..].to_vec();
        res[0] = List::with(Cls::Plain, "", rest).into();
    }
    if let Some(last) = res.last() {
        if last.tt() == "cfws" {
            res.pop();
        } else if last.items().last().is_some_and(|t| t.tt() == "cfws") {
            let items = last.items();
            let rest = items[..items.len() - 1].to_vec();
            let n = res.len();
            res[n - 1] = List::with(Cls::Plain, "", rest).into();
        }
    }
    join_value(&res)
}

/// LocalPart.local_part: whitespace dropped around dots, kept elsewhere.
fn local_part_text(lp: &Tok) -> String {
    let Some(inner) = lp.items().first() else {
        return String::new();
    };
    let mut toks: Vec<Tok> = inner.items().to_vec();
    toks.push(dot());
    let mut res: Vec<Tok> = vec![dot()];
    let mut last_is_tl = false;
    for tok in toks {
        if tok.tt() == "cfws" {
            continue;
        }
        let n = res.len();
        if last_is_tl
            && tok.tt() == "dot"
            && res[n - 1].items().last().is_some_and(|t| t.tt() == "cfws")
        {
            let items = res[n - 1].items();
            let trimmed = items[..items.len() - 1].to_vec();
            res[n - 1] = List::with(Cls::Plain, "", trimmed).into();
        }
        let is_tl = tok.as_list().is_some();
        let last_is_dot = res[res.len() - 1].tt() == "dot";
        if is_tl && last_is_dot && tok.items().first().is_some_and(|t| t.tt() == "cfws") {
            res.push(List::with(Cls::Plain, "", tok.items()[1..].to_vec()).into());
        } else {
            res.push(tok);
        }
        last_is_tl = is_tl;
    }
    let n = res.len();
    join_value(&res[1..n - 1])
}

fn addr_spec_parts(a: &Tok) -> (Option<String>, Option<String>) {
    let items = a.items();
    let local = items.first().map(local_part_text);
    let domain = if items.len() < 3 {
        None
    } else {
        Some(pystr::split_ws(&items[items.len() - 1].value()).collect::<String>())
    };
    (local, domain)
}

struct Mailbox {
    display_name: Option<String>,
    local_part: Option<String>,
    domain: Option<String>,
}

fn mailbox_parts(mb: &List) -> Mailbox {
    let none = Mailbox {
        display_name: None,
        local_part: None,
        domain: None,
    };
    if mb.cls == Cls::InvalidMailbox {
        return none;
    }
    let Some(inner) = mb.items.first() else {
        return none;
    };
    match inner.tt() {
        "name-addr" => {
            let na = inner.items();
            let display_name = if na.len() == 1 {
                None
            } else {
                Some(display_name(&na[0]))
            };
            let (local_part, domain) = na
                .last()
                .and_then(|aa| aa.items().iter().find(|x| x.tt() == "addr-spec"))
                .map(addr_spec_parts)
                .unwrap_or((None, None));
            Mailbox {
                display_name,
                local_part,
                domain,
            }
        }
        "addr-spec" => {
            let (local_part, domain) = addr_spec_parts(inner);
            Mailbox {
                display_name: None,
                local_part,
                domain,
            }
        }
        _ => none,
    }
}

fn list_all_mailboxes(l: &List) -> Vec<&List> {
    l.items
        .iter()
        .filter_map(Tok::as_list)
        .filter(|x| x.tt == "mailbox" || x.tt == "invalid-mailbox")
        .collect()
}

/// headerregistry.Address.__str__.
fn format_address(display: &str, username: &str, domain: &str) -> String {
    let mut addr_spec = if username.chars().any(|c| DOT_ATOM_ENDS.contains(c)) {
        quote_string(username)
    } else {
        username.to_string()
    };
    if !domain.is_empty() {
        addr_spec = format!("{addr_spec}@{domain}");
    } else if addr_spec.is_empty() {
        addr_spec = "<>".into();
    }
    let disp = if display.chars().any(|c| SPECIALS.contains(c)) {
        quote_string(display)
    } else {
        display.to_string()
    };
    if disp.is_empty() {
        addr_spec
    } else {
        let spec = if addr_spec == "<>" { "" } else { &addr_spec };
        format!("{disp} <{spec}>")
    }
}

/// AddressHeader.parse: the header value re-rendered from its groups and mailboxes.
fn address_header(value: &str) -> String {
    let al = get_address_list(value);
    let mut groups = Vec::new();
    for addr in al.items.iter().filter(|t| t.tt() == "address") {
        let Some(Tok::L(inner)) = addr.items().first() else {
            continue;
        };
        let (group_name, mailboxes): (Option<String>, Vec<&List>) = if inner.tt == "group" {
            let name = inner.items.first().map(display_name);
            let mbs = match inner.items.get(2) {
                Some(Tok::L(gl)) if gl.tt == "group-list" => match gl.items.first() {
                    Some(Tok::L(ml)) if ml.tt == "mailbox-list" => list_all_mailboxes(ml),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            (name, mbs)
        } else {
            (None, vec![inner])
        };
        let rendered: Vec<String> = mailboxes
            .iter()
            .map(|mb| {
                let m = mailbox_parts(mb);
                format_address(
                    &m.display_name.unwrap_or_default(),
                    &m.local_part.unwrap_or_default(),
                    &m.domain.unwrap_or_default(),
                )
            })
            .collect();
        groups.push(match group_name {
            None if rendered.len() == 1 => rendered.into_iter().next().unwrap(),
            name => {
                let disp = match name {
                    None => "None".to_string(),
                    Some(d) if d.chars().any(|c| SPECIALS.contains(c)) => quote_string(&d),
                    Some(d) => d,
                };
                let list = rendered.join(", ");
                if list.is_empty() {
                    format!("{disp}:;")
                } else {
                    format!("{disp}: {list};")
                }
            }
        });
    }
    groups.join(", ")
}

// --- The header registry ---------------------------------------------------------------------

/// What `str(msg[name])` is under policy.default for a header whose raw value (with line breaks
/// already removed) is `value`. `name_lower` picks the header class, as the registry does.
pub(super) fn decode_header(name_lower: &str, value: &str) -> String {
    let _index = CloserIndex::install(value);
    let s = match name_lower {
        "date" | "resent-date" | "orig-date" => {
            if value.is_empty() {
                String::new()
            } else {
                match date::parsedate_to_datetime(value) {
                    Some(dt) => dt.format_rfc2822(),
                    None => value.to_string(),
                }
            }
        }
        "sender" | "resent-sender" | "to" | "resent-to" | "cc" | "resent-cc" | "bcc"
        | "resent-bcc" | "from" | "resent-from" | "reply-to" => address_header(value),
        "content-type" => Tok::from(parse_content_type_header(value)).to_str(),
        "content-disposition" => Tok::from(parse_content_disposition_header(value).0).to_str(),
        "content-transfer-encoding" => {
            Tok::from(parse_content_transfer_encoding_header(value)).to_str()
        }
        "message-id" => Tok::from(parse_message_id(value)).to_str(),
        _ => Tok::from(get_unstructured(value)).to_str(),
    };
    pystr::sanitize(s)
}

/// ContentDispositionHeader.content_disposition.
pub(super) fn content_disposition(value: &str) -> Option<String> {
    let _index = CloserIndex::install(value);
    parse_content_disposition_header(value)
        .1
        .map(pystr::sanitize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(v: &str) -> String {
        decode_header("to", v)
    }

    #[test]
    fn addresses_render_like_headerregistry() {
        assert_eq!(addr("\"Alice\" <a@example.com>"), "Alice <a@example.com>");
        assert_eq!(
            addr("\"Doe, J\" <j@example.com> (c)"),
            "\"Doe, J\" <j@example.com>"
        );
        assert_eq!(
            addr("a@example.com, b@example.org"),
            "a@example.com, b@example.org"
        );
        assert_eq!(addr("undisclosed-recipients:;"), "undisclosed-recipients:;");
        assert_eq!(addr("G: a@example.com;"), "G: a@example.com;");
        assert_eq!(addr("<>"), "<>");
        assert_eq!(addr("x (comment) <x@example.com>"), "x <x@example.com>");
        assert_eq!(
            addr("=?utf-8?q?J=C3=B6rg?= <j@example.com>"),
            "Jörg <j@example.com>"
        );
    }

    #[test]
    fn unstructured_joins_adjacent_encoded_words() {
        assert_eq!(
            decode_header("subject", "=?utf-8?q?a?= =?utf-8?q?b?= c"),
            "ab c"
        );
        assert_eq!(decode_header("subject", "x=?utf-8?q?a?="), "xa");
        assert_eq!(decode_header("subject", "=?bogus?="), "=?bogus?=");
    }

    #[test]
    fn content_type_params_are_canonical() {
        assert_eq!(
            decode_header("content-type", "text/plain; charset=UTF-8; format=flowed"),
            "text/plain; charset=\"UTF-8\"; format=\"flowed\""
        );
        assert_eq!(
            decode_header(
                "content-disposition",
                "attachment; filename*=UTF-8''a%C3%A9.txt"
            ),
            "attachment; filename=\"aé.txt\""
        );
        assert_eq!(
            decode_header(
                "content-type",
                "application/x; name*0=\"ab\"; name*1=\"cd\""
            ),
            "application/x; name=\"abcd\""
        );
    }

    #[test]
    fn message_id_drops_trailing_text() {
        assert_eq!(decode_header("message-id", "<a@b> junk"), "<a@b> ");
        assert_eq!(decode_header("message-id", "not-an-id"), "not-an-id");
    }
}
