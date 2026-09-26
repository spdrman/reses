//! Header values as reses prints them: address lists, the subject, the Date line, the Bcc line.
//!
//! mail-parser decodes the values (encoded words, RFC 2231, 8-bit UTF-8). What's left here is
//! presentation and two things mail-parser gets wrong for my purposes. It reads any time zone
//! name outside the eight RFC 5322 defines as UTC and takes only its first three letters, so
//! "10:00 CEST" came out two hours off (N33); I read the zone from the header as written. And
//! its header lookup returns the last occurrence where I want the first, so I look up headers
//! myself.

use std::collections::HashSet;

use mail_parser::parsers::MessageStream;
use mail_parser::{Address, DateTime, Header, HeaderForm, HeaderName, HeaderValue, Message};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// Characters that make a display name need quoting when it's printed back.
const NAME_SPECIALS: &str = ",;:<>@\"()[]\\";

/// The first header called `name`.
fn first<'a, 'x>(msg: &'a Message<'x>, name: &str) -> Option<&'a Header<'x>> {
    let wanted = HeaderName::from(name);
    msg.headers().iter().find(|h| h.name == wanted)
}

/// A header value as written, with the line breaks of its folding removed and the ends trimmed.
/// `raw` is the input mail-parser read, which its offsets point into.
fn text_at(raw: &[u8], header: &Header<'_>) -> String {
    let start = (header.offset_start as usize).min(raw.len());
    let end = (header.offset_end as usize).clamp(start, raw.len());
    String::from_utf8_lossy(&raw[start..end])
        .replace(['\r', '\n'], "")
        .trim()
        .to_string()
}

/// The first `name` header as written, or "".
pub(super) fn raw_value(msg: &Message<'_>, raw: &[u8], name: &str) -> String {
    first(msg, name).map_or_else(String::new, |h| text_at(raw, h))
}

/// A display name, quoted when it holds a character that would otherwise read as syntax.
fn quote_name(name: &str) -> String {
    if name.chars().any(|c| NAME_SPECIALS.contains(c)) {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        name.to_string()
    }
}

/// One mailbox: "Name <address>", or whichever of the two it has.
fn mailbox(name: Option<&str>, address: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty());
    let address = address.map(str::trim).filter(|a| !a.is_empty());
    match (name, address) {
        (Some(n), Some(a)) => format!("{} <{a}>", quote_name(n)),
        (Some(n), None) => quote_name(n),
        (None, Some(a)) => a.to_string(),
        (None, None) => String::new(),
    }
}

/// An address list printed back: mailboxes joined by ", ", and a group as "Name: members;".
fn render(address: &Address<'_>) -> String {
    let mut out = Vec::new();
    match address {
        Address::List(list) => {
            out.extend(list.iter().map(|a| mailbox(a.name(), a.address())));
        }
        // A group with no name is just its members; a named one keeps its syntax.
        Address::Group(groups) => {
            for group in groups {
                let members: Vec<String> = group
                    .addresses
                    .iter()
                    .map(|a| mailbox(a.name(), a.address()))
                    .filter(|m| !m.is_empty())
                    .collect();
                match group.name.as_deref() {
                    None => out.extend(members),
                    Some(name) if members.is_empty() => out.push(format!("{}:;", quote_name(name))),
                    Some(name) => out.push(format!("{}: {};", quote_name(name), members.join(", "))),
                }
            }
        }
    }
    out.retain(|m| !m.is_empty());
    out.join(", ")
}

/// The first `name` header's addresses, printed back.
pub(super) fn addresses(msg: &Message<'_>, name: &str) -> String {
    first(msg, name)
        .and_then(|h| h.value.as_address())
        .map_or_else(String::new, render)
}

pub(super) fn subject(msg: &Message<'_>) -> String {
    first(msg, "Subject")
        .and_then(|h| h.value.as_text())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Every address in every `name` header, parsed as an address list whatever the header is.
fn all_addresses(msg: &Message<'_>, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for value in msg.header_as(name, HeaderForm::Addresses) {
        if let HeaderValue::Address(list) = value {
            out.extend(
                list.iter()
                    .filter_map(|a| a.address())
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_string),
            );
        }
    }
    out
}

/// An explicit Bcc header if it has anything in it, otherwise every envelope recipient that
/// isn't already in To or Cc: Delivered-To, X-Original-To, Envelope-To and the "for" of each
/// Received, deduplicated without regard to case, in the order they appear.
pub(super) fn bcc(msg: &Message<'_>) -> String {
    let explicit = addresses(msg, "Bcc");
    if !explicit.is_empty() {
        return explicit;
    }
    // Collect the envelope recipients in header order.
    let mut found = Vec::new();
    for name in ["Delivered-To", "X-Original-To", "Envelope-To"] {
        found.extend(all_addresses(msg, name));
    }
    for header in msg.headers() {
        if header.name == HeaderName::Received
            && let Some(received) = header.value.as_received()
            && let Some(to) = &received.for_
        {
            found.push(to.trim_matches(['<', '>', ' ']).to_string());
        }
    }
    // Keep the ones nobody can see in To or Cc, each once.
    let visible: HashSet<String> = all_addresses(msg, "To")
        .into_iter()
        .chain(all_addresses(msg, "Cc"))
        .map(|a| a.to_lowercase())
        .collect();
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|a| !a.is_empty() && !visible.contains(&a.to_lowercase()))
        .filter(|a| seen.insert(a.to_lowercase()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the zone at the end of a Date value says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zone {
    /// Minutes east of UTC.
    Known(i32),
    /// "-0000", or a name nobody can pin down: RFC 5322 says both mean the zone is unknown.
    Unknown,
    /// A number no zone can have, which makes the whole date invalid.
    Invalid,
}

/// Zone names RFC 5322 4.3 defines, then common ones it doesn't. RFC 5322 treats an unknown
/// name as "-0000"; the second list is the out-of-band knowledge it allows for.
fn named_zone(name: &str) -> Option<i32> {
    Some(match name {
        "UT" | "GMT" | "Z" | "UTC" | "WET" => 0,
        "EST" => -300,
        "EDT" => -240,
        "CST" => -360,
        "CDT" => -300,
        "MST" => -420,
        "MDT" => -360,
        "PST" => -480,
        "PDT" => -420,
        "AKDT" => -480,
        "WEST" | "BST" | "CET" => 60,
        "CEST" | "EET" => 120,
        "EEST" | "MSK" => 180,
        "HKT" | "SGT" | "AWST" => 480,
        "JST" | "KST" => 540,
        "ACST" => 570,
        "AEST" => 600,
        "AEDT" => 660,
        "NZST" => 720,
        "NZDT" => 780,
        "HST" => -600,
        "AKST" => -540,
        _ => return None,
    })
}

/// The zone of a Date value: its last word once comments are taken out.
fn zone(raw: &str) -> Zone {
    // Comments go first, each from "(" to the next ")". One that never closes isn't a comment.
    let mut text = String::new();
    let mut comment: Option<String> = None;
    for c in raw.chars() {
        match (&mut comment, c) {
            (None, '(') => comment = Some(String::from("(")),
            (Some(_), ')') => {
                comment = None;
                text.push(' ');
            }
            (Some(open), c) => open.push(c),
            (None, c) => text.push(c),
        }
    }
    text.extend(comment);
    let Some(token) = text.split_whitespace().last() else {
        return Zone::Unknown;
    };
    // A signed number has to be four digits, hours and minutes within a day.
    if let Some(digits) = token.strip_prefix(['+', '-'])
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        if digits.len() != 4 {
            return Zone::Invalid;
        }
        let hours: i32 = digits[..2].parse().unwrap_or(99);
        let minutes: i32 = digits[2..].parse().unwrap_or(99);
        if hours > 23 || minutes > 59 {
            return Zone::Invalid;
        }
        if token == "-0000" {
            return Zone::Unknown;
        }
        let sign = if token.starts_with('-') { -1 } else { 1 };
        return Zone::Known(sign * (hours * 60 + minutes));
    }
    named_zone(&token.to_ascii_uppercase()).map_or(Zone::Unknown, Zone::Known)
}

/// A Date value whose time has no seconds, parsed again with ":00" added. RFC 5322 makes the
/// seconds optional, but mail-parser's date parser counts fields and loses its place without
/// them when the zone is a name ("9 Oct 26 07:07 EST").
fn with_seconds(written: &str) -> Option<DateTime> {
    let words: Vec<&str> = written.split_whitespace().collect();
    // The time is the one word shaped like H:MM or HH:MM.
    let at = words.iter().position(|w| {
        let b = w.as_bytes();
        (4..=5).contains(&b.len())
            && b[b.len() - 3] == b':'
            && b.iter().enumerate().all(|(i, c)| i == b.len() - 3 || c.is_ascii_digit())
    })?;
    let mut fixed: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    fixed[at].push_str(":00");
    let text = fixed.join(" ");
    MessageStream::new(text.as_bytes()).parse_date().into_datetime()
}

/// The Date header as a calendar date and time with its zone, or `None` when it isn't a real
/// date. Returns the value as written too.
fn parse_date(msg: &Message<'_>, raw: &[u8]) -> (String, Option<(PrimitiveDateTime, Zone)>) {
    let Some(header) = first(msg, "Date") else {
        return (String::new(), None);
    };
    let written = text_at(raw, header);
    let zone = zone(&written);
    // mail-parser reads the fields; I check they make a real date and take the zone from above.
    let fields = header
        .value
        .as_datetime()
        .cloned()
        .or_else(|| with_seconds(&written));
    let when = fields.and_then(|d| {
        let month = Month::try_from(d.month).ok()?;
        let date = Date::from_calendar_date(i32::from(d.year), month, d.day).ok()?;
        let time = Time::from_hms(d.hour, d.minute, d.second).ok()?;
        Some(PrimitiveDateTime::new(date, time))
    });
    match (when, zone) {
        (Some(when), z) if z != Zone::Invalid => (written, Some((when, z))),
        _ => (written, None),
    }
}

/// The Date line: "Tue, 22 Sep 2026 10:00:00 +0200", "-0000" for an unknown zone, or the value
/// as written when it isn't a real date.
pub(super) fn date_line(msg: &Message<'_>, raw: &[u8]) -> String {
    let (written, parsed) = parse_date(msg, raw);
    let Some((when, zone)) = parsed else {
        return written;
    };
    let stamp = match zone {
        Zone::Known(m) => {
            let sign = if m < 0 { '-' } else { '+' };
            format!("{sign}{:02}{:02}", m.abs() / 60, m.abs() % 60)
        }
        _ => "-0000".to_string(),
    };
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} {stamp}",
        DAYS[when.weekday().number_days_from_monday() as usize],
        when.day(),
        MONTHS[u8::from(when.month()) as usize - 1],
        when.year(),
        when.hour(),
        when.minute(),
        when.second()
    )
}

/// The Date header as a point in time. The inbox sorts on this, so an unknown zone reads as UTC.
pub(super) fn date(msg: &Message<'_>, raw: &[u8]) -> Option<OffsetDateTime> {
    let (_, parsed) = parse_date(msg, raw);
    let (when, zone) = parsed?;
    let offset = match zone {
        Zone::Known(m) => UtcOffset::from_whole_seconds(m * 60).ok()?,
        _ => UtcOffset::UTC,
    };
    Some(when.assume_offset(offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zones as RFC 5322 and the extra table read them.
    #[test]
    fn zones() {
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 +0200"), Zone::Known(120));
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 -0530"), Zone::Known(-330));
        assert_eq!(zone("22 Sep 2026 10:00:00 CEST"), Zone::Known(120));
        assert_eq!(zone("22 Sep 2026 10:00:00 cest"), Zone::Known(120));
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 +0200 (CEST)"), Zone::Known(120));
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 -0000"), Zone::Unknown);
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 GMT+2"), Zone::Unknown);
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 XYZ"), Zone::Unknown);
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 +2400"), Zone::Invalid);
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 +0060"), Zone::Invalid);
        assert_eq!(zone("Tue, 22 Sep 2026 10:00:00 +999999999"), Zone::Invalid);
        assert_eq!(zone(""), Zone::Unknown);
    }

    /// A time without seconds still makes a date, whatever the zone looks like.
    #[test]
    fn dates_without_seconds() {
        let d = with_seconds("9 Oct 26 07:07 EST").unwrap();
        assert_eq!((d.year, d.month, d.day, d.hour, d.minute, d.second), (2026, 10, 9, 7, 7, 0));
        assert!(with_seconds("no time here").is_none());
    }

    /// Names are quoted only when they'd otherwise read as syntax.
    #[test]
    fn names() {
        assert_eq!(mailbox(Some("Ann Lee"), Some("a@example.com")), "Ann Lee <a@example.com>");
        assert_eq!(mailbox(Some("Lee, Ann"), Some("a@example.com")), "\"Lee, Ann\" <a@example.com>");
        assert_eq!(mailbox(Some("say \"hi\""), None), "\"say \\\"hi\\\"\"");
        assert_eq!(mailbox(None, Some("a@example.com")), "a@example.com");
    }
}
