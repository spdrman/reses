//! `email._parseaddr._parsedate_tz`, `utils.parsedate_to_datetime` and the two ways a datetime
//! gets printed (`utils.format_datetime` for the header object, and reses.py's own strftime).

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset, Weekday};

use super::pystr;

const MONTHS: [&str; 24] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];
const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn zone(tz: &str) -> Option<i64> {
    Some(match tz {
        "UT" | "UTC" | "GMT" | "Z" => 0,
        "AST" => -400,
        "ADT" => -300,
        "EST" => -500,
        "EDT" => -400,
        "CST" => -600,
        "CDT" => -500,
        "MST" => -700,
        "MDT" => -600,
        "PST" => -800,
        "PDT" => -700,
        _ => return None,
    })
}

struct Parsed {
    yy: i64,
    mm: i64,
    dd: i64,
    thh: i64,
    tmm: i64,
    tss: i64,
    /// Offset in seconds; `None` for -0000 and for zones Python doesn't know.
    tz: Option<i64>,
}

fn parsedate_tz(data: &str) -> Option<Parsed> {
    let mut data: Vec<String> = pystr::split_ws(data).map(str::to_string).collect();
    if data.is_empty() {
        return None;
    }
    if data[0].ends_with(',') || DAYS.contains(&pystr::lower(&data[0]).as_str()) {
        data.remove(0);
    } else if let Some(i) = data[0].rfind(',') {
        data[0] = data[0][i + 1..].to_string();
    }
    if data.len() == 3 {
        let stuff: Vec<String> = data[0].split('-').map(str::to_string).collect();
        if stuff.len() == 3 {
            let mut d = stuff;
            d.extend(data[1..].iter().cloned());
            data = d;
        }
    }
    if data.len() == 4 {
        let s = data[3].clone();
        let i = s.find('+').or_else(|| s.find('-'));
        match i {
            Some(i) if i > 0 => {
                data.truncate(3);
                data.push(s[..i].to_string());
                data.push(s[i..].to_string());
            }
            _ => data.push(String::new()),
        }
    }
    if data.len() < 5 {
        return None;
    }
    data.truncate(5);
    let [dd, mm, yy, tm, tz]: [String; 5] = data.try_into().ok()?;
    let (mut dd, mut mm, mut yy, mut tm, mut tz) = (dd, mm, yy, tm, tz);
    if dd.is_empty() || mm.is_empty() || yy.is_empty() {
        return None;
    }
    mm = pystr::lower(&mm);
    if !MONTHS.contains(&mm.as_str()) {
        let lowered_dd = pystr::lower(&dd);
        dd = mm;
        mm = lowered_dd;
        if !MONTHS.contains(&mm.as_str()) {
            return None;
        }
    }
    let mut month = MONTHS.iter().position(|m| *m == mm).unwrap() as i64 + 1;
    if month > 12 {
        month -= 12;
    }
    if dd.ends_with(',') {
        dd.pop();
    }
    if let Some(i) = yy.find(':')
        && i > 0
    {
        std::mem::swap(&mut yy, &mut tm);
    }
    if yy.ends_with(',') {
        yy.pop();
        if yy.is_empty() {
            return None;
        }
    }
    if !yy.chars().next().is_some_and(pystr::is_digit) {
        std::mem::swap(&mut yy, &mut tz);
    }
    if tm.ends_with(',') {
        tm.pop();
    }
    let parts: Vec<&str> = tm.split(':').collect();
    let (thh, tmm, tss) = match parts.len() {
        2 => (parts[0].to_string(), parts[1].to_string(), "0".to_string()),
        3 => (parts[0].to_string(), parts[1].to_string(), parts[2].to_string()),
        1 if tm.contains('.') => {
            let p: Vec<&str> = tm.split('.').collect();
            match p.len() {
                2 => (p[0].to_string(), p[1].to_string(), "0".to_string()),
                3 => (p[0].to_string(), p[1].to_string(), p[2].to_string()),
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut yy = pystr::py_int(&yy)?;
    let dd = pystr::py_int(&dd)?;
    let thh = pystr::py_int(&thh)?;
    let tmm = pystr::py_int(&tmm)?;
    let tss = pystr::py_int(&tss)?;
    if yy < 100 {
        if yy > 68 {
            yy += 1900;
        } else {
            yy += 2000;
        }
    }
    let tz_up = tz.to_uppercase();
    let mut tzoffset = zone(&tz_up);
    if tzoffset.is_none() {
        tzoffset = pystr::py_int(&tz_up);
        if tzoffset == Some(0) && tz_up.starts_with('-') {
            tzoffset = None;
        }
    }
    if let Some(off) = tzoffset
        && off != 0
    {
        let (sign, off) = if off < 0 { (-1, -off) } else { (1, off) };
        tzoffset = Some(sign * ((off / 100) * 3600 + (off % 100) * 60));
    }
    Some(Parsed { yy, mm: month, dd, thh, tmm, tss, tz: tzoffset })
}

/// A Python datetime as parsedate_to_datetime builds it: aware, or naive for -0000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PyDateTime {
    dt: PrimitiveDateTime,
    offset: Option<UtcOffset>,
}

/// `email.utils.parsedate_to_datetime`; `None` wherever Python raises ValueError.
pub(super) fn parsedate_to_datetime(data: &str) -> Option<PyDateTime> {
    let p = parsedate_tz(data)?;
    if !(1..=9999).contains(&p.yy) {
        return None;
    }
    let month = Month::try_from(u8::try_from(p.mm).ok()?).ok()?;
    let date = Date::from_calendar_date(i32::try_from(p.yy).ok()?, month, u8::try_from(p.dd).ok()?)
        .ok()?;
    let time = Time::from_hms(
        u8::try_from(p.thh).ok()?,
        u8::try_from(p.tmm).ok()?,
        u8::try_from(p.tss).ok()?,
    )
    .ok()?;
    let offset = match p.tz {
        None => None,
        // datetime.timezone insists on strictly less than a day either way.
        Some(s) if s.abs() >= 86_400 => return None,
        Some(s) => Some(UtcOffset::from_whole_seconds(s as i32).ok()?),
    };
    Some(PyDateTime { dt: PrimitiveDateTime::new(date, time), offset })
}

fn weekday(w: Weekday) -> &'static str {
    match w {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
}

fn month_abbr(m: Month) -> &'static str {
    [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][usize::from(u8::from(m)) - 1]
}

/// strftime's %z: "+HHMM", with seconds only when there are any.
fn percent_z(off: UtcOffset) -> String {
    let s = off.whole_seconds();
    let sign = if s < 0 { '-' } else { '+' };
    let a = s.unsigned_abs();
    let (h, m, sec) = (a / 3600, a % 3600 / 60, a % 60);
    if sec != 0 {
        format!("{sign}{h:02}{m:02}{sec:02}")
    } else {
        format!("{sign}{h:02}{m:02}")
    }
}

impl PyDateTime {
    fn body(&self, year: String) -> String {
        let d = self.dt;
        format!(
            "{}, {:02} {} {} {:02}:{:02}:{:02}",
            weekday(d.weekday()),
            d.day(),
            month_abbr(d.month()),
            year,
            d.hour(),
            d.minute(),
            d.second()
        )
    }

    /// `email.utils.format_datetime`, which is what the Date header object turns into.
    pub(super) fn format_rfc2822(&self) -> String {
        let zone = match self.offset {
            None => "-0000".to_string(),
            Some(o) => percent_z(o),
        };
        format!("{} {zone}", self.body(format!("{:04}", self.dt.year())))
    }

    /// reses.py's `strftime("%a, %d %b %Y %H:%M:%S %z")`. On glibc %Y isn't zero padded, and
    /// %z of a naive datetime is empty.
    pub(super) fn format_reses(&self) -> String {
        let zone = self.offset.map(percent_z).unwrap_or_default();
        format!("{} {zone}", self.body(self.dt.year().to_string()))
    }

    /// For the inbox, a naive (-0000) date is read as UTC.
    pub(super) fn to_offset(self) -> OffsetDateTime {
        self.dt.assume_offset(self.offset.unwrap_or(UtcOffset::UTC))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(s: &str) -> Option<String> {
        parsedate_to_datetime(s).map(|d| d.format_rfc2822())
    }

    #[test]
    fn parses_like_python() {
        assert_eq!(fmt("Fri, 25 Sep 2026 17:01:31 -0700").unwrap(), "Fri, 25 Sep 2026 17:01:31 -0700");
        assert_eq!(fmt("9 Oct 26 07:07 EST").unwrap(), "Fri, 09 Oct 2026 07:07:00 -0500");
        assert_eq!(fmt("Fri, 9 Oct 2026 07:07:07 -0000").unwrap(), "Fri, 09 Oct 2026 07:07:07 -0000");
        assert_eq!(fmt("Fri, 9 Oct 2026 07:07:07 XYZ").unwrap(), "Fri, 09 Oct 2026 07:07:07 -0000");
        assert_eq!(fmt("Fri,9 Oct 2026 07:07:07+0100").unwrap(), "Fri, 09 Oct 2026 07:07:07 +0100");
        assert_eq!(fmt("Oct 9 2026 07.07.07 GMT").unwrap(), "Fri, 09 Oct 2026 07:07:07 +0000");
        assert_eq!(fmt("09-Oct-2026 07:07:07 +0000").unwrap(), "Fri, 09 Oct 2026 07:07:07 +0000");
        assert_eq!(fmt("Mon, 31 Feb 2026 10:00:00 +0000"), None);
        assert_eq!(fmt("Mon, 1 Feb 2026 23:59:60 +0000"), None);
        assert_eq!(fmt("Mon, 1 Feb 2026 10:00:00 +2400"), None);
        assert_eq!(fmt("garbage"), None);
    }

    #[test]
    fn reses_format_drops_naive_zone() {
        let d = parsedate_to_datetime("Fri, 9 Oct 2026 07:07:07 -0000").unwrap();
        assert_eq!(d.format_reses(), "Fri, 09 Oct 2026 07:07:07 ");
    }
}
