//! The inbox list: one row per stored message, From / Subject / Date / Size, newest first.
//!
//! The listing and every row's header peek go through the job pool, so the table fills in as
//! results arrive, in whatever order the workers finish them.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use time::{OffsetDateTime, UtcOffset};

use super::accounts::AccountsScreen;
use super::jobs::{Done, Job, JobId, Outcome};
use super::message::MessageScreen;
use super::{Ctx, Transition, View};
use crate::config::Inbox;
use crate::mail::{self, Summary};
use crate::s3::{ObjectInfo, S3Error};

/// The first header peek. Most header blocks fit; the rest get a bigger second look.
const FIRST_PEEK: u64 = 32 * 1024;
/// Stop growing the peek here and summarize whatever arrived.
const MAX_PEEK: u64 = 1024 * 1024;
/// Stubbed for the red tests.
pub(super) const MAX_PAGES: usize = 1000;
#[allow(dead_code)]
const DEFAULT_PAGE: usize = 24;

const DATE_W: usize = 10;
const SIZE_W: usize = 8;
const GAP: usize = 2;

enum Head {
    Pending,
    Mail(Summary),
    NotEmail,
    Unreadable(String),
}

struct Row {
    info: ObjectInfo,
    head: Head,
}

impl Row {
    fn summary(&self) -> Option<&Summary> {
        match &self.head {
            Head::Mail(s) => Some(s),
            _ => None,
        }
    }

    fn subject(&self) -> String {
        match self.summary() {
            Some(s) if !s.subject.trim().is_empty() => clean(&s.subject),
            _ => "(no subject)".into(),
        }
    }

    fn sort_time(&self) -> Option<OffsetDateTime> {
        self.summary()
            .and_then(|s| s.date)
            .or(self.info.last_modified)
    }
}

pub struct InboxScreen {
    pub inbox: Inbox,
    rows: Vec<Row>,
    /// Listing and peek jobs of the current load. A refresh starts a fresh set.
    jobs: HashSet<JobId>,
    /// Deletes this screen asked for, so it knows whose result to report.
    deletes: HashSet<JobId>,
    started: bool,
    listing_done: bool,
    error: Option<String>,
    /// Selection by key, so it stays on the same message while rows re-sort.
    selected: Option<String>,
    offset: usize,
    page: usize,
    filter: String,
    typing: bool,
    /// Key waiting for a `y`.
    confirm: Option<String>,
    now: Option<OffsetDateTime>,
    downloads: PathBuf,
}

impl InboxScreen {
    pub fn new(inbox: Inbox) -> Self {
        Self {
            inbox,
            rows: Vec::new(),
            jobs: HashSet::new(),
            deletes: HashSet::new(),
            started: false,
            listing_done: false,
            error: None,
            selected: None,
            offset: 0,
            page: 1,
            filter: String::new(),
            typing: false,
            confirm: None,
            now: None,
            downloads: default_downloads(),
        }
    }

    /// Fix "now" so today's-time versus older-date formatting is testable.
    pub fn with_now(mut self, now: OffsetDateTime) -> Self {
        self.now = Some(now);
        self
    }

    /// Where the message screen writes text and attachments (default `~/Downloads`).
    pub fn with_downloads_dir(mut self, dir: PathBuf) -> Self {
        self.downloads = dir;
        self
    }

    fn location(&self) -> String {
        format!("s3://{}/{}", self.inbox.bucket, self.inbox.prefix)
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.started = true;
        self.rows.clear();
        self.jobs.clear();
        self.error = None;
        self.listing_done = false;
        self.offset = 0;
        self.list_page(None, ctx);
    }

    fn list_page(&mut self, token: Option<String>, ctx: &mut Ctx) {
        let job = Job::List {
            bucket: self.inbox.bucket.clone(),
            prefix: self.inbox.prefix.clone(),
            delimiter: true,
            token,
        };
        match ctx.submit(job) {
            Some(id) => {
                self.jobs.insert(id);
            }
            None => {
                self.listing_done = true;
                self.error = Some("Not connected to an account. Press u to pick one.".into());
            }
        }
    }

    fn peek(&mut self, key: &str, bytes: u64, ctx: &mut Ctx) {
        let job = Job::Peek {
            bucket: self.inbox.bucket.clone(),
            key: key.to_string(),
            bytes,
        };
        if let Some(id) = ctx.submit(job) {
            self.jobs.insert(id);
        }
    }

    fn is_direct_child(&self, key: &str) -> bool {
        key.strip_prefix(self.inbox.prefix.as_str())
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
    }

    fn row_mut(&mut self, key: &str) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.info.key == key)
    }

    fn list_error(&self, e: &S3Error, ctx: &Ctx) -> String {
        let profile = ctx
            .session
            .as_ref()
            .map_or(self.inbox.profile.as_str(), |s| s.profile.name.as_str());
        match e {
            S3Error::Service { code, .. } if code == "NoSuchBucket" => format!(
                "The bucket {} does not exist. Press u to pick another inbox.",
                self.inbox.bucket
            ),
            S3Error::Service { status: 403, .. } => format!(
                "Access denied: profile {profile} is not allowed to list {}. ({e}) \
                 Press u to pick another account.",
                self.location()
            ),
            _ => format!("Could not list {}: {e}", self.location()),
        }
    }

    /// Rows that are shown, filtered and sorted newest first.
    fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        let mut out: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let row = &self.rows[i];
                if matches!(row.head, Head::NotEmail) {
                    return false;
                }
                if needle.is_empty() {
                    return true;
                }
                match row.summary() {
                    Some(s) => {
                        s.from.to_lowercase().contains(&needle)
                            || s.subject.to_lowercase().contains(&needle)
                    }
                    None => false,
                }
            })
            .collect();
        out.sort_by_key(|&i| {
            let r = &self.rows[i];
            (Reverse(r.sort_time()), Reverse(r.info.key.clone()))
        });
        out
    }

    fn selected_pos(&self, visible: &[usize]) -> usize {
        self.selected
            .as_ref()
            .and_then(|k| visible.iter().position(|&i| &self.rows[i].info.key == k))
            .unwrap_or(0)
    }

    fn select(&mut self, visible: &[usize], pos: usize) {
        self.selected = visible
            .get(pos.min(visible.len().saturating_sub(1)))
            .map(|&i| self.rows[i].info.key.clone());
    }

    fn current(&self) -> Option<&Row> {
        let visible = self.visible();
        visible
            .get(self.selected_pos(&visible))
            .map(|&i| &self.rows[i])
    }

    fn counts(&self) -> (usize, usize) {
        let hidden = self
            .rows
            .iter()
            .filter(|r| matches!(r.head, Head::NotEmail))
            .count();
        (self.rows.len() - hidden, hidden)
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) => self.filter.push(c),
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Enter => self.typing = false,
            KeyCode::Esc => {
                self.filter.clear();
                self.typing = false;
            }
            _ => {}
        }
    }

    fn render_table(&mut self, frame: &mut Frame, area: Rect, visible: &[usize]) {
        let now = self
            .now
            .unwrap_or_else(OffsetDateTime::now_utc)
            .to_offset(UtcOffset::UTC);
        let width = area.width as usize;
        let rest = width.saturating_sub(1 + GAP + DATE_W + GAP + SIZE_W + GAP);
        let from_w = (rest * 3 / 10).clamp(rest.min(6), 30);
        let subject_w = rest - from_w;

        let [head, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        let header = format!(
            " {}{:GAP$}{}{:GAP$}{:>DATE_W$}{:GAP$}{:>SIZE_W$}",
            fit("From", from_w),
            "",
            fit("Subject", subject_w),
            "",
            "Date",
            "",
            "Size"
        );
        frame.render_widget(
            Paragraph::new(header).style(Style::default().add_modifier(Modifier::BOLD)),
            head,
        );

        self.page = (body.height as usize).max(1);
        let sel = self.selected_pos(visible);
        if sel < self.offset {
            self.offset = sel;
        } else if sel >= self.offset + self.page {
            self.offset = sel + 1 - self.page;
        }
        self.offset = self.offset.min(visible.len().saturating_sub(self.page));

        let lines: Vec<Line> = visible
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.page)
            .map(|(pos, &i)| {
                let row = &self.rows[i];
                let size = human_size(row.info.size);
                let (from, subject, date, dim) = match &row.head {
                    Head::Mail(s) => (
                        display_from(&s.from),
                        row.subject(),
                        format_date(row.sort_time(), now),
                        false,
                    ),
                    Head::Unreadable(e) => (
                        String::new(),
                        format!("(could not read headers: {e})"),
                        String::new(),
                        true,
                    ),
                    _ => ("…".into(), "loading…".into(), String::new(), true),
                };
                let text = format!(
                    " {}{:GAP$}{}{:GAP$}{:>DATE_W$}{:GAP$}{:>SIZE_W$}",
                    fit(&from, from_w),
                    "",
                    fit(&subject, subject_w),
                    "",
                    date,
                    "",
                    size
                );
                let mut style = Style::default();
                if dim {
                    style = style.fg(Color::DarkGray);
                }
                if pos == sel {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::styled(text, style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), body);
    }
}

impl View for InboxScreen {
    fn title(&self) -> String {
        let mut t = format!("Inbox {}", self.location());
        if self.error.is_none() && self.started {
            let (messages, hidden) = self.counts();
            let noun = if messages == 1 { "message" } else { "messages" };
            t.push_str(&format!(" · {messages} {noun}"));
            if hidden > 0 {
                t.push_str(&format!(" · {hidden} not email"));
            }
            if !self.filter.is_empty() {
                t.push_str(&format!(" · {} shown", self.visible().len()));
            }
        }
        t
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        let show_filter = self.typing || !self.filter.is_empty();
        let [main, filter_area] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(show_filter)),
        ])
        .areas(area);

        let visible = self.visible();
        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(format!(" {err}"))
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false }),
                main,
            );
        } else if visible.is_empty() {
            let pending = self.rows.iter().any(|r| matches!(r.head, Head::Pending));
            let msg = if !self.listing_done || pending {
                format!(" Loading {} …", self.location())
            } else if !self.filter.is_empty() {
                format!(" Nothing matches /{}", self.filter)
            } else {
                let (_, hidden) = self.counts();
                let mut m = format!(" No messages in {}.", self.location());
                if hidden > 0 {
                    let noun = if hidden == 1 { "object" } else { "objects" };
                    m.push_str(&format!(" ({hidden} other {noun} there are not email.)"));
                }
                m
            };
            frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: false }), main);
        } else {
            self.render_table(frame, main, &visible);
        }

        if show_filter {
            let cursor = if self.typing { "_" } else { "" };
            frame.render_widget(
                Paragraph::new(format!(" /{}{cursor}", self.filter))
                    .style(Style::default().fg(Color::Yellow)),
                filter_area,
            );
        }

        if let Some(key) = &self.confirm {
            let row = self.rows.iter().find(|r| &r.info.key == key);
            let subject = row.map_or_else(|| "(no subject)".into(), Row::subject);
            let from = row
                .and_then(Row::summary)
                .map(|s| display_from(&s.from))
                .unwrap_or_default();
            render_confirm(
                frame,
                area,
                &subject,
                &from,
                &format!("s3://{}/{key}", self.inbox.bucket),
            );
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if let Some(target) = self.confirm.take() {
            if key.code == KeyCode::Char('y') {
                let job = Job::Delete {
                    bucket: self.inbox.bucket.clone(),
                    key: target,
                };
                match ctx.submit(job) {
                    Some(id) => {
                        self.deletes.insert(id);
                    }
                    None => ctx.error("Not connected to an account."),
                }
            } else {
                ctx.info("Delete cancelled.");
            }
            return Transition::None;
        }
        if self.typing {
            self.on_filter_key(key);
            return Transition::None;
        }

        let visible = self.visible();
        let pos = self.selected_pos(&visible);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(&visible, pos.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(&visible, pos + 1),
            KeyCode::PageUp => self.select(&visible, pos.saturating_sub(self.page)),
            KeyCode::PageDown => self.select(&visible, pos + self.page),
            KeyCode::Home | KeyCode::Char('g') => self.select(&visible, 0),
            KeyCode::End | KeyCode::Char('G') => self.select(&visible, usize::MAX),
            KeyCode::Enter => {
                if let Some(row) = self.current() {
                    let screen =
                        MessageScreen::new(self.inbox.bucket.clone(), row.info.key.clone())
                            .with_subject(row.subject())
                            .with_out_dir(self.downloads.clone());
                    return Transition::Push(Box::new(screen));
                }
            }
            KeyCode::Char('d') => {
                self.confirm = self.current().map(|r| r.info.key.clone());
            }
            KeyCode::Char('r') => {
                self.load(ctx);
                ctx.info(format!("Refreshing {}", self.location()));
            }
            KeyCode::Char('/') => self.typing = true,
            KeyCode::Char('u') => return Transition::Push(Box::new(AccountsScreen::new(ctx))),
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            KeyCode::Esc | KeyCode::Char('q') => return Transition::Pop,
            _ => {}
        }
        Transition::None
    }

    fn on_done(&mut self, done: &Done, ctx: &mut Ctx) -> Transition {
        // A delete from this screen or from the message screen: either way the row goes.
        if let Job::Delete { bucket, key } = &done.job {
            let mine = self.deletes.remove(&done.id);
            let location = format!("s3://{bucket}/{key}");
            match &done.result {
                Ok(_) if *bucket == self.inbox.bucket => {
                    self.rows.retain(|r| &r.info.key != key);
                    if mine {
                        ctx.info(format!("Deleted {location}"));
                    }
                }
                Ok(_) => {}
                Err(e) if mine => ctx.error(format!("Could not delete {location}: {e}")),
                Err(_) => {}
            }
            return Transition::None;
        }

        if !self.jobs.remove(&done.id) {
            return Transition::None;
        }
        match (&done.job, &done.result) {
            (Job::List { .. }, Ok(Outcome::Listing(listing))) => {
                for obj in &listing.objects {
                    if !self.is_direct_child(&obj.key) || self.row_mut(&obj.key).is_some() {
                        continue;
                    }
                    self.rows.push(Row {
                        info: obj.clone(),
                        head: Head::Pending,
                    });
                    self.peek(&obj.key, FIRST_PEEK, ctx);
                }
                match &listing.next_token {
                    Some(t) => self.list_page(Some(t.clone()), ctx),
                    None => self.listing_done = true,
                }
            }
            (Job::List { .. }, Err(e)) => {
                self.listing_done = true;
                let msg = self.list_error(e, ctx);
                if self.rows.is_empty() {
                    self.error = Some(msg);
                } else {
                    ctx.error(msg);
                }
            }
            (Job::Peek { key, bytes, .. }, Ok(Outcome::Data(data))) => {
                let grow = !header_ended(data) && data.len() as u64 >= *bytes && *bytes < MAX_PEEK;
                let head = if !mail::looks_like_email(data) {
                    Some(Head::NotEmail)
                } else if grow {
                    None
                } else {
                    Some(Head::Mail(mail::summarize(data)))
                };
                match head {
                    Some(head) => {
                        if let Some(row) = self.row_mut(key) {
                            row.head = head;
                        }
                    }
                    None => {
                        let key = key.clone();
                        self.peek(&key, (*bytes * 4).min(MAX_PEEK), ctx);
                    }
                }
            }
            (Job::Peek { key, .. }, Err(e)) => {
                let msg = e.to_string();
                if let Some(row) = self.row_mut(key) {
                    row.head = Head::Unreadable(msg);
                }
            }
            _ => {}
        }
        Transition::None
    }

    fn on_focus(&mut self, ctx: &mut Ctx) {
        if !self.started {
            self.load(ctx);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.typing {
            return vec![("enter", "done"), ("esc", "clear filter")];
        }
        vec![
            ("↑↓", "move"),
            ("enter", "open"),
            ("d", "delete"),
            ("/", "filter"),
            ("r", "refresh"),
            ("u", "accounts"),
            ("q", "quit"),
        ]
    }
}

/// The delete confirmation both screens show: what is going, from where, and which key does it.
pub(super) fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    subject: &str,
    from: &str,
    location: &str,
) {
    let mut lines = vec![
        Line::styled(
            "Delete this message from S3?",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(format!("Subject: {subject}")),
    ];
    if !from.is_empty() {
        lines.push(Line::raw(format!("From:    {from}")));
    }
    lines.push(Line::raw(format!("Object:  {location}")));
    lines.push(Line::raw(""));
    lines.push(Line::raw("Press y to delete, any other key to cancel."));

    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (inner_w + 4).min(area.width);
    let text_w = w.saturating_sub(4).max(1);
    // Wrapped height, so a long key still fits inside the box.
    let text_h: u16 = lines
        .iter()
        .map(|l| (l.width() as u16).div_ceil(text_w).max(1))
        .sum();
    let h = (text_h + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        rect,
    );
}

fn default_downloads() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads")
}

/// True once the blank line that ends the header block is in the bytes we have.
fn header_ended(data: &[u8]) -> bool {
    data.windows(2).any(|w| w == b"\n\n") || data.windows(4).any(|w| w == b"\r\n\r\n")
}

/// The display name when there is one, else the address.
pub(super) fn display_from(from: &str) -> String {
    let from = clean(from);
    let from = from.trim();
    if let Some(open) = from.find('<') {
        let name = from[..open].trim().trim_matches('"').trim();
        if !name.is_empty() {
            return name.to_string();
        }
        let addr = from[open + 1..].split('>').next().unwrap_or("").trim();
        return addr.to_string();
    }
    from.to_string()
}

/// Header text on one line: control characters (folded headers, tabs) become spaces.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Today's messages as a time, this year's as "Sep 20", older ones as a full date.
fn format_date(date: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    let Some(d) = date else {
        return String::new();
    };
    let d = d.to_offset(UtcOffset::UTC);
    if d.date() == now.date() {
        format!("{:02}:{:02}", d.hour(), d.minute())
    } else if d.year() == now.year() {
        let month = d.month().to_string();
        format!("{} {}", &month[..3], d.day())
    } else {
        format!("{}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
    }
}

fn human_size(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    for unit in ["KB", "MB", "GB"] {
        if v < 1000.0 {
            return format!("{v:.1} {unit}");
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// Exactly `width` columns: padded, or cut with an ellipsis.
fn fit(s: &str, width: usize) -> String {
    let total: usize = s.chars().map(char_width).sum();
    if total <= width {
        return format!("{s}{}", " ".repeat(width - total));
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > width - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    used += 1;
    out.push_str(&" ".repeat(width - used));
    out
}

/// Message builders and a store that fails on demand, shared with the message screen's tests.
#[cfg(test)]
pub(super) mod fixtures {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use time::OffsetDateTime;

    use crate::config::Inbox;
    use crate::s3::{Bucket, Listing, MemoryStore, S3Error, Store};

    pub const BUCKET: &str = "inbox-bucket";
    pub const PREFIX: &str = "mail/";

    /// A MemoryStore that lists each object with the time S3 received it (MemoryStore itself
    /// stamps everything with the epoch), and counts header peeks.
    pub struct Timed {
        inner: MemoryStore,
        received: Mutex<HashMap<String, OffsetDateTime>>,
        peeks: AtomicUsize,
    }

    impl Timed {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: MemoryStore::new(),
                received: Mutex::new(HashMap::new()),
                peeks: AtomicUsize::new(0),
            })
        }

        /// Received when its Date header says, the ordinary case for mail SES delivers.
        pub fn put(&self, bucket: &str, key: &str, data: &[u8]) {
            let at = crate::mail::summarize(data)
                .date
                .unwrap_or(OffsetDateTime::UNIX_EPOCH);
            self.put_received(bucket, key, data, at);
        }

        pub fn put_received(&self, bucket: &str, key: &str, data: &[u8], at: OffsetDateTime) {
            self.inner.put(bucket, key, data);
            self.received.lock().unwrap().insert(key.to_string(), at);
        }

        pub fn create_bucket(&self, bucket: &str) {
            self.inner.create_bucket(bucket);
        }

        pub fn contains(&self, bucket: &str, key: &str) -> bool {
            self.inner.contains(bucket, key)
        }

        /// Ranged gets so far, which is what a header peek does.
        pub fn peeks(&self) -> usize {
            self.peeks.load(Ordering::SeqCst)
        }
    }

    impl Store for Timed {
        fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
            self.inner.list_buckets()
        }
        fn list(
            &self,
            bucket: &str,
            prefix: &str,
            delimiter: Option<&str>,
            token: Option<&str>,
        ) -> Result<Listing, S3Error> {
            let mut listing = self.inner.list(bucket, prefix, delimiter, token)?;
            let received = self.received.lock().unwrap();
            for obj in &mut listing.objects {
                if let Some(at) = received.get(&obj.key) {
                    obj.last_modified = Some(*at);
                }
            }
            Ok(listing)
        }
        fn get_range(
            &self,
            bucket: &str,
            key: &str,
            start: u64,
            end: u64,
        ) -> Result<Vec<u8>, S3Error> {
            self.peeks.fetch_add(1, Ordering::SeqCst);
            self.inner.get_range(bucket, key, start, end)
        }
        fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
            self.inner.get(bucket, key)
        }
        fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
            self.inner.delete(bucket, key)
        }
    }

    /// Always answers a listing with the same page and a continuation token from `next`.
    pub struct Endless {
        pub next: fn(usize) -> String,
        pub calls: AtomicUsize,
    }

    impl Store for Endless {
        fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
            Ok(Vec::new())
        }
        fn list(
            &self,
            _: &str,
            prefix: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Listing, S3Error> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Listing {
                prefixes: Vec::new(),
                objects: vec![crate::s3::ObjectInfo {
                    key: format!("{prefix}obj{n}"),
                    size: 10,
                    last_modified: None,
                }],
                next_token: Some((self.next)(n)),
            })
        }
        fn get_range(&self, _: &str, _: &str, _: u64, _: u64) -> Result<Vec<u8>, S3Error> {
            Ok(b"not mail".to_vec())
        }
        fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, S3Error> {
            Ok(b"not mail".to_vec())
        }
        fn delete(&self, _: &str, _: &str) -> Result<(), S3Error> {
            Ok(())
        }
    }

    pub fn inbox() -> Inbox {
        Inbox {
            profile: "test".into(),
            bucket: BUCKET.into(),
            prefix: PREFIX.into(),
            region: Some("us-east-1".into()),
        }
    }

    /// A plain-text message as SES stores it.
    pub fn email(from: &str, subject: &str, date: &str) -> Vec<u8> {
        format!(
            "Return-Path: <bounce@example.com>\r\n\
             From: {from}\r\n\
             To: me@example.com\r\n\
             Subject: {subject}\r\n\
             Date: {date}\r\n\
             Message-ID: <{subject}@example.com>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             body of {subject}\r\n"
        )
        .into_bytes()
    }

    pub fn access_denied() -> S3Error {
        S3Error::Service {
            status: 403,
            code: "AccessDenied".into(),
            message: "Access Denied".into(),
        }
    }

    /// Delegates to a MemoryStore, except for the calls told to fail.
    pub struct Failing {
        pub inner: Arc<Timed>,
        pub list_err: Option<S3Error>,
        pub delete_err: Option<S3Error>,
    }

    impl Store for Failing {
        fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
            self.inner.list_buckets()
        }
        fn list(
            &self,
            bucket: &str,
            prefix: &str,
            delimiter: Option<&str>,
            token: Option<&str>,
        ) -> Result<Listing, S3Error> {
            match &self.list_err {
                Some(e) => Err(e.clone()),
                None => self.inner.list(bucket, prefix, delimiter, token),
            }
        }
        fn get_range(
            &self,
            bucket: &str,
            key: &str,
            start: u64,
            end: u64,
        ) -> Result<Vec<u8>, S3Error> {
            self.inner.get_range(bucket, key, start, end)
        }
        fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
            self.inner.get(bucket, key)
        }
        fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
            match &self.delete_err {
                Some(e) => Err(e.clone()),
                None => self.inner.delete(bucket, key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ratatui::crossterm::event::KeyCode;
    use time::macros::datetime;

    use super::fixtures::*;
    use super::*;
    use crate::s3::Store;
    use crate::tui::App;
    use crate::tui::jobs::Job;
    use crate::tui::testing::{self, chars, key, screen, settle};

    const NOW: time::OffsetDateTime = datetime!(2026-09-25 15:00 UTC);

    fn app_with(store: Arc<dyn Store>) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(store));
        let view = InboxScreen::new(inbox())
            .with_now(NOW)
            .with_downloads_dir(dir.path().join("downloads"));
        let mut app = App::with_view(ctx, Box::new(view));
        settle(&mut app);
        (app, dir)
    }

    /// Three messages: today, earlier this month, and last year, stored oldest key first.
    fn three() -> Arc<Timed> {
        let s = Timed::new();
        s.put(
            BUCKET,
            "mail/aaa",
            &email(
                "Carol <carol@example.com>",
                "Old news",
                "Fri, 03 Jan 2025 08:00:00 +0000",
            ),
        );
        s.put(
            BUCKET,
            "mail/bbb",
            &email(
                "\"Alice Example\" <alice@example.com>",
                "Lunch today",
                "Fri, 25 Sep 2026 09:30:00 +0000",
            ),
        );
        s.put(
            BUCKET,
            "mail/ccc",
            &email(
                "bob@example.com",
                "Invoice for September",
                "Sun, 20 Sep 2026 12:00:00 +0000",
            ),
        );
        s
    }

    fn line_with<'a>(scr: &'a str, needle: &str) -> &'a str {
        scr.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line contains {needle:?} in:\n{scr}"))
    }

    fn top_title(app: &App) -> String {
        app.stack.last().unwrap().title()
    }

    #[test]
    fn columns_render_like_a_mail_client_at_full_width() {
        let (mut app, _d) = app_with(three());
        let scr = screen(&mut app, 100, 12);
        let head = line_with(&scr, "Subject");
        for col in ["From", "Subject", "Date", "Size"] {
            assert!(head.contains(col), "missing column {col}:\n{scr}");
        }
        // Display name rather than the address, today's message as a time.
        let alice = line_with(&scr, "Lunch today");
        assert!(alice.contains("Alice Example"), "{scr}");
        assert!(!alice.contains("alice@example.com"), "{scr}");
        // Only the name: no quotes and no start of the address, however wide the cell is.
        assert!(!alice.contains('"') && !alice.contains('<'), "{scr}");
        assert!(alice.contains("09:30"), "{scr}");
        // No display name falls back to the address; older messages show a date.
        let bob = line_with(&scr, "Invoice for September");
        assert!(bob.contains("bob@example.com"), "{scr}");
        assert!(bob.contains("Sep 20"), "{scr}");
        assert!(line_with(&scr, "Old news").contains("2025-01-03"), "{scr}");
        assert!(scr.contains("3 messages"), "{scr}");
    }

    #[test]
    fn size_column_is_human_readable() {
        let s = Timed::new();
        let mut raw = email("a@example.com", "Sized", "Fri, 25 Sep 2026 09:30:00 +0000");
        raw.resize(2048, b'x');
        s.put(BUCKET, "mail/sized", &raw);
        let small = email("a@example.com", "Tiny", "Fri, 25 Sep 2026 09:31:00 +0000");
        let tiny_len = small.len();
        s.put(BUCKET, "mail/tiny", &small);
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 10);
        assert!(line_with(&scr, "Sized").contains("2.0 KiB"), "{scr}");
        assert!(
            line_with(&scr, "Tiny").contains(&format!("{tiny_len} B")),
            "{scr}"
        );
    }

    #[test]
    fn columns_truncate_cleanly_at_narrow_width() {
        let s = Timed::new();
        s.put(
            BUCKET,
            "mail/long",
            &email(
                "Someone With A Very Long Display Name <long@example.com>",
                "A subject line that is much too long to fit in a narrow terminal window",
                "Fri, 25 Sep 2026 09:30:00 +0000",
            ),
        );
        let (mut app, _d) = app_with(s);
        for width in [80u16, 60, 50] {
            let scr = screen(&mut app, width, 8);
            let row = line_with(&scr, "A subject");
            // Both long cells are cut with an ellipsis, and Date and Size still fit on the row.
            assert!(row.matches('…').count() >= 2, "width {width}:\n{scr}");
            assert!(row.contains("09:30"), "width {width}:\n{scr}");
            assert!(row.contains(" B"), "width {width}:\n{scr}");
            assert!(
                !row.contains("narrow terminal window"),
                "width {width}:\n{scr}"
            );
            let head = line_with(&scr, "Subject");
            assert!(head.contains("Date") && head.contains("Size"), "{scr}");
            // Subject gets more of the room than From.
            assert!(
                head.find("Subject").unwrap() < width as usize / 2,
                "width {width}:\n{scr}"
            );
        }
        // Below the minimum width Date and Size give their room to Subject and From.
        for width in [49u16, 30] {
            let scr = screen(&mut app, width, 8);
            let row = line_with(&scr, "A subject");
            assert!(!row.contains("09:30"), "width {width}:\n{scr}");
            assert!(!line_with(&scr, "Subject").contains("Size"), "{scr}");
            assert!(row.contains('…'), "width {width}:\n{scr}");
        }
    }

    #[test]
    fn newest_message_comes_first() {
        let (mut app, _d) = app_with(three());
        let scr = screen(&mut app, 100, 12);
        let pos = |s: &str| scr.find(s).unwrap_or_else(|| panic!("{s} missing:\n{scr}"));
        assert!(pos("Lunch today") < pos("Invoice for September"), "{scr}");
        assert!(pos("Invoice for September") < pos("Old news"), "{scr}");
    }

    #[test]
    fn non_email_objects_are_hidden_and_counted() {
        let s = three();
        s.put(
            BUCKET,
            "mail/AMAZON_SES_SETUP_NOTIFICATION",
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
        );
        s.put(BUCKET, "mail/notes.bin", &[0u8, 159, 146, 150, 0, 1, 2, 3]);
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 12);
        assert!(!scr.contains("AMAZON_SES"), "{scr}");
        assert!(!scr.contains("notes.bin"), "{scr}");
        assert!(scr.contains("3 messages"), "{scr}");
        assert!(scr.contains("2 not email"), "{scr}");
    }

    #[test]
    fn lists_every_page_but_only_direct_children() {
        // MemoryStore pages three keys at a time, so this needs several pages.
        let s = Timed::new();
        for i in 0..8 {
            s.put(
                BUCKET,
                &format!("mail/m{i}"),
                &email(
                    "a@example.com",
                    &format!("Message number {i}"),
                    &format!("1{i} Sep 2026 09:00:00 +0000"),
                ),
            );
        }
        s.put(
            BUCKET,
            "mail/sub/deeper",
            &email("a@example.com", "Deeper", "Fri, 25 Sep 2026 09:00:00 +0000"),
        );
        s.put(
            BUCKET,
            "other/elsewhere",
            &email(
                "a@example.com",
                "Elsewhere",
                "Fri, 25 Sep 2026 09:00:00 +0000",
            ),
        );
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 20);
        for i in 0..8 {
            assert!(scr.contains(&format!("Message number {i}")), "{scr}");
        }
        assert!(!scr.contains("Deeper"), "{scr}");
        assert!(!scr.contains("Elsewhere"), "{scr}");
        assert!(scr.contains("8 messages"), "{scr}");
    }

    #[test]
    fn rows_show_a_placeholder_until_their_headers_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(three()));
        let view = InboxScreen::new(inbox()).with_now(NOW);
        let mut app = App::with_view(ctx, Box::new(view));
        // One pump handles the listing; the header peeks it queued have not run yet.
        app.pump();
        let scr = screen(&mut app, 100, 12);
        assert!(!scr.contains("Lunch today"), "{scr}");
        assert_eq!(scr.matches("loading").count(), 3, "{scr}");
        settle(&mut app);
        let scr = screen(&mut app, 100, 12);
        assert!(scr.contains("Lunch today"), "{scr}");
        assert!(!scr.contains("loading"), "{scr}");
    }

    #[test]
    fn header_peeks_can_finish_in_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(three()));
        let view = InboxScreen::new(inbox()).with_now(NOW);
        let mut app = App::with_view(ctx, Box::new(view));
        // Hand results over the way the pool would: listings as they come, peeks held back
        // and then delivered newest submission first.
        let mut held = Vec::new();
        loop {
            let batch = app.ctx.jobs.poll();
            if batch.is_empty() {
                break;
            }
            for done in batch {
                if matches!(done.job, Job::PeekHead { .. }) {
                    held.push(done);
                } else {
                    deliver(&mut app, &done);
                }
            }
        }
        assert_eq!(held.len(), 3);
        held.reverse();
        for done in &held {
            deliver(&mut app, done);
        }
        settle(&mut app);
        let scr = screen(&mut app, 100, 12);
        let pos = |s: &str| scr.find(s).unwrap_or_else(|| panic!("{s} missing:\n{scr}"));
        assert!(pos("Lunch today") < pos("Invoice for September"), "{scr}");
        assert!(pos("Invoice for September") < pos("Old news"), "{scr}");
    }

    fn deliver(app: &mut App, done: &crate::tui::jobs::Done) {
        let last = app.stack.len() - 1;
        let mut top = crate::tui::Transition::None;
        for (i, v) in app.stack.iter_mut().enumerate() {
            let t = v.on_done(done, &mut app.ctx);
            if i == last {
                top = t;
            }
        }
        app.apply(top);
    }

    #[test]
    fn a_header_block_longer_than_the_first_peek_is_fetched_in_full() {
        let s = Timed::new();
        let mut raw = String::new();
        // 40 KiB of Received headers before the ones the list needs.
        for i in 0..400 {
            raw.push_str(&format!(
                "Received: from relay{i}.example.com by mx.example.com with SMTP id {i:0>80}\r\n"
            ));
        }
        raw.push_str(
            &String::from_utf8(email(
                "Dana <dana@example.com>",
                "Found after a long header block",
                "Fri, 25 Sep 2026 10:00:00 +0000",
            ))
            .unwrap(),
        );
        assert!(raw.len() > 40 * 1024);
        s.put(BUCKET, "mail/long-headers", raw.as_bytes());
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 8);
        assert!(scr.contains("Found after a long header block"), "{scr}");
        assert!(scr.contains("Dana"), "{scr}");
    }

    #[test]
    fn results_for_jobs_it_did_not_submit_are_ignored() {
        let (mut app, _d) = app_with(three());
        let other = Timed::new();
        other.put(
            BUCKET,
            "mail/zzz",
            &email(
                "x@example.com",
                "Not mine",
                "Fri, 25 Sep 2026 11:00:00 +0000",
            ),
        );
        // Somebody else's listing of the same folder, from another store.
        app.ctx.session.as_mut().unwrap().store = other;
        app.ctx.submit(Job::List {
            bucket: BUCKET.into(),
            prefix: PREFIX.into(),
            delimiter: true,
            token: None,
        });
        settle(&mut app);
        let scr = screen(&mut app, 100, 12);
        assert!(!scr.contains("Not mine"), "{scr}");
        assert!(scr.contains("3 messages"), "{scr}");
    }

    #[test]
    fn enter_opens_the_selected_message() {
        let (mut app, _d) = app_with(three());
        // Newest first, so the first row is "Lunch today"; move down to the invoice.
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        assert!(top_title(&app).contains("Message"), "{}", top_title(&app));
        let scr = screen(&mut app, 100, 20);
        assert!(scr.contains("body of Invoice for September"), "{scr}");
        app.key(key(KeyCode::Esc));
        assert!(top_title(&app).contains("Inbox"));
        // Coming back does not reload or lose the rows.
        settle(&mut app);
        assert!(screen(&mut app, 100, 12).contains("3 messages"));
    }

    #[test]
    fn delete_asks_for_confirmation_naming_subject_and_key() {
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::Char('d')));
        let scr = screen(&mut app, 100, 16);
        assert!(scr.contains("Lunch today"), "{scr}");
        assert!(scr.contains("s3://inbox-bucket/mail/bbb"), "{scr}");
        assert!(scr.contains("y to delete"), "{scr}");
    }

    #[test]
    fn any_key_but_y_cancels_the_delete() {
        let store = three();
        let (mut app, _d) = app_with(store.clone());
        for cancel in [
            KeyCode::Char('n'),
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Char('Y'),
        ] {
            app.key(key(KeyCode::Char('d')));
            app.key(key(cancel));
            settle(&mut app);
            assert!(store.contains(BUCKET, "mail/bbb"), "{cancel:?} deleted it");
            let scr = screen(&mut app, 100, 12);
            assert!(scr.contains("Lunch today"), "{cancel:?}:\n{scr}");
            assert!(
                !scr.contains("s3://inbox-bucket/mail/bbb"),
                "{cancel:?}:\n{scr}"
            );
        }
    }

    #[test]
    fn y_deletes_the_object_and_the_row() {
        let store = three();
        let (mut app, _d) = app_with(store.clone());
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        assert!(!store.contains(BUCKET, "mail/bbb"));
        assert!(store.contains(BUCKET, "mail/ccc"));
        let scr = screen(&mut app, 100, 12);
        assert!(!scr.contains("Lunch today"), "{scr}");
        assert!(scr.contains("Invoice for September"), "{scr}");
        assert!(scr.contains("2 messages"), "{scr}");
        assert!(line_with(&scr, "Deleted").contains("mail/bbb"), "{scr}");
    }

    #[test]
    fn a_failed_delete_keeps_the_row_and_says_why() {
        let inner = three();
        let store = Arc::new(Failing {
            inner: inner.clone(),
            list_err: None,
            delete_err: Some(access_denied()),
        });
        let (mut app, _d) = app_with(store);
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        assert!(inner.contains(BUCKET, "mail/bbb"));
        let scr = screen(&mut app, 100, 12);
        assert!(scr.contains("Lunch today"), "{scr}");
        assert!(scr.contains("3 messages"), "{scr}");
        let status = scr.lines().last().unwrap();
        assert!(status.contains("mail/bbb"), "{scr}");
        assert!(status.contains("AccessDenied"), "{scr}");
        assert!(
            matches!(app.ctx.status, Some(crate::tui::Status::Error(_))),
            "{:?}",
            app.ctx.status
        );
    }

    #[test]
    fn a_folder_marker_object_is_not_a_row() {
        // The S3 console creates a zero-byte object named after the folder itself.
        let s = three();
        s.put(BUCKET, "mail/", b"");
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 12);
        assert!(scr.contains("3 messages"), "{scr}");
        assert!(!scr.contains("loading"), "{scr}");
        assert!(!scr.contains("could not read"), "{scr}");
        // Not an object in the folder at all, so it is not counted as a non-email one either.
        assert!(!scr.contains("not email"), "{scr}");
    }

    #[test]
    fn slash_filters_on_from_and_subject() {
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::Char('/')));
        chars(&mut app, "alice");
        app.key(key(KeyCode::Enter));
        let scr = screen(&mut app, 100, 12);
        assert!(scr.contains("Lunch today"), "{scr}");
        assert!(!scr.contains("Invoice"), "{scr}");
        assert!(!scr.contains("Old news"), "{scr}");
        // Subject matches too, case-insensitively.
        app.key(key(KeyCode::Char('/')));
        for _ in 0..5 {
            app.key(key(KeyCode::Backspace));
        }
        chars(&mut app, "INVOICE");
        app.key(key(KeyCode::Enter));
        let scr = screen(&mut app, 100, 12);
        assert!(scr.contains("Invoice for September"), "{scr}");
        assert!(!scr.contains("Lunch today"), "{scr}");
        // Enter on the filtered list opens the filtered row.
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        assert!(screen(&mut app, 100, 20).contains("body of Invoice for September"));
        app.key(key(KeyCode::Esc));
        // Esc in the filter prompt clears it.
        app.key(key(KeyCode::Char('/')));
        app.key(key(KeyCode::Esc));
        let scr = screen(&mut app, 100, 12);
        assert!(
            scr.contains("Lunch today") && scr.contains("Old news"),
            "{scr}"
        );
        // Esc on the list with a filter applied clears it rather than leaving the inbox.
        app.key(key(KeyCode::Char('/')));
        chars(&mut app, "carol");
        app.key(key(KeyCode::Enter));
        assert!(!screen(&mut app, 100, 12).contains("Lunch today"));
        app.key(key(KeyCode::Esc));
        assert!(!app.quit);
        let scr = screen(&mut app, 100, 12);
        assert!(
            scr.contains("Lunch today") && scr.contains("Old news"),
            "{scr}"
        );
    }

    #[test]
    fn r_refreshes_the_listing() {
        let store = three();
        let (mut app, _d) = app_with(store.clone());
        store.put(
            BUCKET,
            "mail/ddd",
            &email(
                "eve@example.com",
                "Arrived later",
                "Fri, 25 Sep 2026 14:00:00 +0000",
            ),
        );
        assert!(!screen(&mut app, 100, 12).contains("Arrived later"));
        app.key(key(KeyCode::Char('r')));
        settle(&mut app);
        let scr = screen(&mut app, 100, 12);
        assert!(scr.contains("Arrived later"), "{scr}");
        assert!(scr.contains("4 messages"), "{scr}");
    }

    #[test]
    fn u_goes_to_the_accounts_screen() {
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::Char('u')));
        assert!(top_title(&app).contains("Accounts"), "{}", top_title(&app));
    }

    #[test]
    fn an_empty_inbox_says_so() {
        let s = Timed::new();
        s.create_bucket(BUCKET);
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 10);
        assert!(
            scr.contains("No messages in s3://inbox-bucket/mail/"),
            "{scr}"
        );
        assert!(!scr.contains("Subject"), "{scr}");
    }

    #[test]
    fn a_folder_with_only_non_email_says_so() {
        let s = Timed::new();
        s.put(BUCKET, "mail/image.png", b"\x89PNG\r\n\x1a\n\x00\x00");
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 10);
        assert!(
            scr.contains("No messages in s3://inbox-bucket/mail/"),
            "{scr}"
        );
        assert!(scr.contains("1 not email"), "{scr}");
    }

    #[test]
    fn a_missing_bucket_says_so() {
        let s = Timed::new();
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("bucket inbox-bucket does not exist"), "{scr}");
        assert!(!scr.contains("Subject"), "{scr}");
    }

    #[test]
    fn access_denied_says_so() {
        let store = Arc::new(Failing {
            inner: three(),
            list_err: Some(access_denied()),
            delete_err: None,
        });
        let (mut app, _d) = app_with(store);
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("Access denied"), "{scr}");
        assert!(scr.contains("s3://inbox-bucket/mail/"), "{scr}");
        assert!(scr.contains("profile test"), "{scr}");
        assert!(!scr.contains("Subject"), "{scr}");
    }

    #[test]
    fn no_connected_account_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), None);
        let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox())));
        settle(&mut app);
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("Not connected"), "{scr}");
    }

    #[test]
    fn selection_scrolls_with_a_long_list() {
        let s = Timed::new();
        for i in 0..30 {
            s.put(
                BUCKET,
                &format!("mail/m{i:02}"),
                &email(
                    "a@example.com",
                    &format!("Numbered {i:02}"),
                    &format!("25 Sep 2026 10:{i:02}:00 +0000"),
                ),
            );
        }
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 80, 10);
        assert!(
            scr.contains("Numbered 29") && !scr.contains("Numbered 00"),
            "{scr}"
        );
        app.key(key(KeyCode::End));
        let scr = screen(&mut app, 80, 10);
        assert!(
            scr.contains("Numbered 00") && !scr.contains("Numbered 29"),
            "{scr}"
        );
        app.key(key(KeyCode::Home));
        assert!(screen(&mut app, 80, 10).contains("Numbered 29"));
    }

    fn row_with<'a>(scr: &'a str, needle: &str) -> Option<&'a str> {
        scr.lines().find(|l| l.contains(needle))
    }

    #[test]
    fn esc_at_the_root_inbox_does_not_quit_but_q_does() {
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::Esc));
        assert!(!app.quit);
        assert!(screen(&mut app, 100, 12).contains("Lunch today"));
        app.key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn the_inbox_keeps_its_own_account_after_another_one_connects() {
        let mine = three();
        let (mut app, _d) = app_with(mine.clone());
        // Somewhere else (the accounts screen behind `u`) connects a different account
        // that happens to have an object under the same key.
        let theirs = Timed::new();
        theirs.put(
            BUCKET,
            "mail/bbb",
            &email("x@example.com", "Theirs", "Fri, 25 Sep 2026 09:30:00 +0000"),
        );
        app.ctx.session.as_mut().unwrap().store = theirs.clone();

        app.key(key(KeyCode::Char('r')));
        settle(&mut app);
        let scr = screen(&mut app, 100, 12);
        assert!(
            scr.contains("Lunch today") && !scr.contains("Theirs"),
            "{scr}"
        );

        app.key(key(KeyCode::Enter));
        settle(&mut app);
        assert!(screen(&mut app, 100, 20).contains("body of Lunch today"));
        app.key(key(KeyCode::Char('q')));

        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        assert!(!mine.contains(BUCKET, "mail/bbb"));
        assert!(theirs.contains(BUCKET, "mail/bbb"));
    }

    #[test]
    fn dates_show_in_the_local_offset() {
        let s = Timed::new();
        // 02:30 UTC on the 25th is still the evening of the 24th four hours west.
        s.put(
            BUCKET,
            "mail/late",
            &email(
                "a@example.com",
                "Late last night",
                "Fri, 25 Sep 2026 02:30:00 +0000",
            ),
        );
        s.put(
            BUCKET,
            "mail/morning",
            &email(
                "a@example.com",
                "This morning",
                "Fri, 25 Sep 2026 13:15:00 +0000",
            ),
        );
        let (mut app, _d) = app_with(s);
        app.ctx.local_offset = time::UtcOffset::from_hms(-4, 0, 0).unwrap();
        let scr = screen(&mut app, 100, 10);
        assert!(
            line_with(&scr, "Late last night").contains("Sep 24"),
            "{scr}"
        );
        assert!(line_with(&scr, "This morning").contains("09:15"), "{scr}");
    }

    #[test]
    fn a_forged_future_date_cannot_pin_a_message_to_the_top() {
        let s = Timed::new();
        s.put_received(
            BUCKET,
            "mail/forged",
            &email(
                "spam@example.com",
                "Forged date",
                "Tue, 01 Jan 2030 00:00:00 +0000",
            ),
            datetime!(2026-09-01 08:00 UTC),
        );
        s.put(
            BUCKET,
            "mail/real",
            &email(
                "a@example.com",
                "Honest mail",
                "Sun, 20 Sep 2026 12:00:00 +0000",
            ),
        );
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 10);
        let pos = |s: &str| scr.find(s).unwrap_or_else(|| panic!("{s} missing:\n{scr}"));
        assert!(pos("Honest mail") < pos("Forged date"), "{scr}");
        // The Date column still shows what the sender wrote.
        assert!(
            line_with(&scr, "Forged date").contains("2030-01-01"),
            "{scr}"
        );
    }

    #[test]
    fn only_rows_on_screen_and_a_page_ahead_get_peeked() {
        let s = Timed::new();
        for i in 0..200 {
            s.put_received(
                BUCKET,
                &format!("mail/m{i:03}"),
                &email(
                    "a@example.com",
                    &format!("Number {i:03}"),
                    "25 Sep 2026 10:00:00 +0000",
                ),
                OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(i),
            );
        }
        let (mut app, _d) = app_with(s.clone());
        let before = s.peeks();
        assert!(before > 0 && before <= 2 * DEFAULT_PAGE, "{before} peeks");
        assert!(screen(&mut app, 80, 12).contains("Number 199"));
        // The first render says the page is 9 rows; jumping to the end peeks around there.
        settle(&mut app);
        app.key(key(KeyCode::End));
        settle(&mut app);
        let scr = screen(&mut app, 80, 12);
        assert!(
            scr.contains("Number 000") && !scr.contains("loading"),
            "{scr}"
        );
        assert!(s.peeks() < 100, "{} peeks for 200 rows", s.peeks());
        // Everything is still listed, peeked or not.
        assert!(scr.contains("200 messages"), "{scr}");
    }

    #[test]
    fn after_a_delete_the_next_row_takes_the_selection() {
        let (mut app, _d) = app_with(three());
        // Rows: Lunch today, Invoice for September, Old news. Delete the middle one.
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        assert!(screen(&mut app, 100, 20).contains("body of Old news"));
        app.key(key(KeyCode::Char('q')));
        // Deleting the last row selects the one above it.
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        assert!(screen(&mut app, 100, 20).contains("body of Lunch today"));
    }

    #[test]
    fn a_repeated_continuation_token_stops_the_listing() {
        let store = Arc::new(Endless {
            next: |_| "same".into(),
            calls: AtomicUsize::new(0),
        });
        let (mut app, _d) = app_with(store.clone());
        assert_eq!(store.calls.load(Ordering::SeqCst), 2);
        let scr = screen(&mut app, 120, 10);
        assert!(
            scr.lines()
                .last()
                .unwrap()
                .contains("same continuation token"),
            "{scr}"
        );
    }

    #[test]
    fn the_listing_stops_at_the_page_cap() {
        let store = Arc::new(Endless {
            next: |n| format!("token-{n}"),
            calls: AtomicUsize::new(0),
        });
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(store.clone()));
        let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox()).with_now(NOW)));
        // More pages than `settle` allows pumps for.
        for _ in 0..10 * MAX_PAGES {
            if app.pump() == 0 {
                break;
            }
        }
        assert_eq!(store.calls.load(Ordering::SeqCst), MAX_PAGES);
        let scr = screen(&mut app, 120, 10);
        assert!(
            scr.lines()
                .last()
                .unwrap()
                .contains(&format!("after {MAX_PAGES} pages")),
            "{scr}"
        );
    }

    #[test]
    fn the_confirmation_always_shows_the_key_and_the_y_line() {
        let s = Timed::new();
        let subject = format!("URGENT {}", "account suspended verify now ".repeat(12));
        s.put(
            BUCKET,
            "mail/bbb",
            &email(
                "\"Very Long Sender Name Indeed\" <x@example.com>",
                &subject,
                "Fri, 25 Sep 2026 09:30:00 +0000",
            ),
        );
        let (mut app, _d) = app_with(s);
        app.key(key(KeyCode::Char('d')));
        for (w, h) in [(100u16, 20u16), (60, 20), (44, 20), (44, 9), (60, 8)] {
            let scr = screen(&mut app, w, h);
            assert!(
                scr.contains("s3://inbox-bucket/mail/bbb"),
                "{w}x{h}:\n{scr}"
            );
            assert!(scr.contains("y to delete"), "{w}x{h}:\n{scr}");
            let subject_line = line_with(&scr, "Subject: URGENT");
            assert!(subject_line.contains('…'), "{w}x{h}:\n{scr}");
        }
        // A screen too small for any of it still renders rather than panicking.
        for (w, h) in [(10u16, 5u16), (3, 3), (1, 1)] {
            screen(&mut app, w, h);
        }
        assert!(row_with(&screen(&mut app, 100, 20), "y to delete").is_some());
    }
}
