//! The inbox list: one row per stored message, From / Subject / Date / Size, newest first.
//!
//! Rows are ordered by when S3 received the object, which is known from the listing alone, so
//! the order never jumps and a forged Date header can't pin a message to the top. Headers are
//! peeked (and decoded on the worker) only for the rows on screen plus a page ahead.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use time::{OffsetDateTime, UtcOffset};

use super::accounts::AccountsScreen;
use super::jobs::{Done, Generation, Job, JobId, Outcome};
use super::message::MessageScreen;
use super::text::{SIZE_WIDTH, clean, escape, fit, human_size, width};
use super::{Ctx, Session, Transition, View};
use crate::config::Inbox;
use crate::mail::Summary;
use crate::s3::{ObjectInfo, S3Error};

/// A folder bigger than this many listing pages (a million keys at S3's 1000 a page) stops
/// there rather than listing forever.
pub(super) const MAX_PAGES: usize = 1000;
/// Rows peeked before the first render says how tall the screen is.
const DEFAULT_PAGE: usize = 24;
/// Header peeks a filter keeps in flight while it checks rows nobody has scrolled to.
const FILTER_BATCH: usize = 32;

const DATE_W: usize = 10;
const GAP: usize = 2;
/// Below this width the Date and Size columns go, leaving the room to Subject and From.
const NARROW: usize = 50;

/// Delete jobs a closed message screen left in flight, so the inbox can still report them.
pub(super) type Handoff = Rc<RefCell<HashSet<JobId>>>;

enum Head {
    /// Not peeked yet, with the id of the peek asked for, if one is.
    Pending(Option<JobId>),
    Mail(Summary),
    NotEmail,
    Unreadable(String),
}

struct Row {
    info: ObjectInfo,
    head: Head,
}

impl Row {
    /// I return the decoded headers once the peek has landed and turned out to be mail.
    fn summary(&self) -> Option<&Summary> {
        match &self.head {
            Head::Mail(s) => Some(s),
            _ => None,
        }
    }

    /// I give the subject flattened to one line, or a placeholder when it's missing or blank.
    fn subject(&self) -> String {
        match self.summary() {
            Some(s) if !s.subject.trim().is_empty() => clean(s.subject.trim()),
            _ => "(no subject)".into(),
        }
    }
}

/// Newest first, then by key, so equal times still have one fixed order.
fn newer(a: &Row, b: &Row) -> std::cmp::Ordering {
    b.info
        .last_modified
        .cmp(&a.info.last_modified)
        .then_with(|| b.info.key.cmp(&a.info.key))
}

pub struct InboxScreen {
    pub inbox: Inbox,
    /// The account this inbox was opened with. Switching accounts on the accounts screen
    /// doesn't change what this screen lists or deletes.
    session: Option<Session>,
    /// Bumped by a refresh: the listing still queued for the last load gets skipped.
    generation: Generation,
    /// Bumped when the rows on screen change: peeks queued for rows scrolled past get skipped,
    /// so the ones on screen don't wait behind them.
    window_gen: Generation,
    /// The first row of the window peeks were last asked for.
    window_top: Option<usize>,
    /// One slot per listed object, in listing order. A slot never moves, so everything else
    /// refers to rows by slot; a deleted row's slot becomes None.
    rows: Vec<Option<Row>>,
    index: HashMap<String, usize>,
    /// Every live slot, newest first. Pages merge into it; nothing re-sorts it.
    order: Vec<usize>,
    /// The slots shown: `order` less what isn't email or doesn't match the filter. Rebuilt
    /// from `order` (no sorting) only when `dirty`.
    view: Vec<usize>,
    dirty: bool,
    not_email: usize,
    /// Rows whose headers have been read (or failed to be), for the filter's "n of m checked".
    checked: usize,
    /// Peeks a filter asked for, beyond the window. Kept apart so scrolling doesn't cancel them.
    filter_peeks: HashSet<JobId>,
    /// Listing and peek jobs of the current load.
    jobs: HashSet<JobId>,
    /// Deletes this screen asked for, so it knows whose result to report.
    deletes: HashSet<JobId>,
    handoff: Handoff,
    /// Keys deleted while the current load's listing may already have been taken: a page of
    /// that listing can still carry them, so it mustn't bring them back.
    deleted: HashSet<String>,
    started: bool,
    listing_done: bool,
    pages: usize,
    last_token: Option<String>,
    error: Option<String>,
    /// Selection by slot, so it stays on the same message while rows come and go.
    selected: Option<usize>,
    /// Where `selected` sits in `view`, kept with it so nothing has to search for it.
    sel_pos: usize,
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
    /// I build an inbox for the saved folder with nothing listed yet. It loads on first focus
    /// and takes ctx.session then, which it keeps from that point on.
    pub fn new(inbox: Inbox) -> Self {
        Self {
            inbox,
            session: None,
            generation: Generation::new(),
            window_gen: Generation::new(),
            window_top: None,
            rows: Vec::new(),
            index: HashMap::new(),
            order: Vec::new(),
            view: Vec::new(),
            dirty: false,
            not_email: 0,
            checked: 0,
            filter_peeks: HashSet::new(),
            jobs: HashSet::new(),
            deletes: HashSet::new(),
            handoff: Handoff::default(),
            deleted: HashSet::new(),
            started: false,
            listing_done: false,
            pages: 0,
            last_token: None,
            error: None,
            selected: None,
            sel_pos: 0,
            offset: 0,
            page: DEFAULT_PAGE,
            filter: String::new(),
            typing: false,
            confirm: None,
            now: None,
            downloads: default_downloads(),
        }
    }

    /// Fix "now" so today's-time versus older-date formatting is testable.
    #[cfg(test)]
    pub fn with_now(mut self, now: OffsetDateTime) -> Self {
        self.now = Some(now);
        self
    }

    /// Where the message screen writes text and attachments (default `~/Downloads`).
    #[cfg(test)]
    pub fn with_downloads_dir(mut self, dir: PathBuf) -> Self {
        self.downloads = dir;
        self
    }

    /// I spell out the inbox folder as an s3:// URL for the title and messages.
    fn location(&self) -> String {
        format!("s3://{}/{}", self.inbox.bucket, self.inbox.prefix)
    }

    /// Start over: forget every row and list the folder again.
    fn load(&mut self, ctx: &mut Ctx) {
        self.started = true;
        // Anything still queued from the last load is skipped by the workers.
        self.generation.bump();
        self.window_gen.bump();
        self.window_top = None;
        self.rows.clear();
        self.index.clear();
        self.order.clear();
        self.view.clear();
        self.not_email = 0;
        self.checked = 0;
        self.filter_peeks.clear();
        self.dirty = true;
        self.jobs.clear();
        // A listing started from here on can't see anything deleted before now.
        self.deleted.clear();
        self.error = None;
        self.listing_done = false;
        self.pages = 0;
        self.last_token = None;
        self.offset = 0;
        self.list_page(None, ctx);
    }

    /// I queue a job on this inbox's own session, stamped with `generation`. None when no
    /// account is connected.
    fn submit(&mut self, job: Job, generation: &Generation, ctx: &mut Ctx) -> Option<JobId> {
        let session = self.session.as_ref()?;
        Some(ctx.submit_to(session, job, Some(generation)))
    }

    /// I submit one listing page of the inbox folder, following `token` when there is one, and
    /// put an error on screen if there's no session to list with.
    fn list_page(&mut self, token: Option<String>, ctx: &mut Ctx) {
        let job = Job::List {
            bucket: self.inbox.bucket.clone(),
            prefix: self.inbox.prefix.clone(),
            delimiter: true,
            token,
        };
        let generation = self.generation.clone();
        match self.submit(job, &generation, ctx) {
            Some(id) => {
                self.jobs.insert(id);
            }
            None => {
                self.listing_done = true;
                self.error = Some("Not connected to an account. Press u to pick one.".into());
            }
        }
    }

    /// The first row of the window: where the next render will scroll to, so a jump to End
    /// peeks the end, not everything on the way.
    fn window_start(&self) -> usize {
        let sel = self.sel_pos;
        if sel < self.offset {
            sel
        } else if sel >= self.offset + self.page {
            sel + 1 - self.page
        } else {
            self.offset
        }
    }

    /// Queue header peeks for the rows on screen (or about to be) and a page beyond. Only once
    /// the listing is complete: S3 lists in key order, not received order, so until then the
    /// top of the list keeps changing and peeking it would end up peeking everything.
    ///
    /// When the window moved since the last call, I bump the window generation first: peeks
    /// still queued for rows scrolled past get skipped, and the rows now on screen are asked
    /// for again so they go to the front of what's left.
    fn request_window(&mut self, ctx: &mut Ctx) {
        if !self.listing_done {
            return;
        }
        self.refresh_view();
        let top = self.window_start();
        let moved = self.window_top != Some(top);
        if moved {
            self.window_gen.bump();
            self.window_top = Some(top);
        }
        let end = (top + 2 * self.page).min(self.view.len());
        let wanted: Vec<usize> = self.view[top.min(end)..end]
            .iter()
            .copied()
            .filter(|&slot| match self.rows[slot].as_ref().map(|r| &r.head) {
                Some(Head::Pending(None)) => true,
                Some(Head::Pending(Some(_))) => moved,
                _ => false,
            })
            .collect();
        let window_gen = self.window_gen.clone();
        for slot in wanted {
            let Some(key) = self.rows[slot].as_ref().map(|r| r.info.key.clone()) else {
                continue;
            };
            let job = Job::PeekHead {
                bucket: self.inbox.bucket.clone(),
                key,
            };
            if let Some(id) = self.submit(job, &window_gen, ctx) {
                self.jobs.insert(id);
                if let Some(row) = self.rows[slot].as_mut() {
                    row.head = Head::Pending(Some(id));
                }
            }
        }
        self.request_filter_batch(ctx);
    }

    /// While a filter is on, rows nobody has peeked can't match it yet, so I peek them too, a
    /// bounded batch at a time, newest first. These use the load's generation rather than the
    /// window's, so scrolling doesn't cancel them; a refresh still does.
    fn request_filter_batch(&mut self, ctx: &mut Ctx) {
        if self.filter.is_empty() || self.checked == self.index.len() {
            return;
        }
        let room = FILTER_BATCH.saturating_sub(self.filter_peeks.len());
        let wanted: Vec<usize> = self
            .order
            .iter()
            .copied()
            .filter(|&slot| {
                matches!(
                    self.rows[slot].as_ref().map(|r| &r.head),
                    Some(Head::Pending(None))
                )
            })
            .take(room)
            .collect();
        let generation = self.generation.clone();
        for slot in wanted {
            let Some(key) = self.rows[slot].as_ref().map(|r| r.info.key.clone()) else {
                continue;
            };
            let job = Job::PeekHead {
                bucket: self.inbox.bucket.clone(),
                key,
            };
            if let Some(id) = self.submit(job, &generation, ctx) {
                self.jobs.insert(id);
                self.filter_peeks.insert(id);
                if let Some(row) = self.rows[slot].as_mut() {
                    row.head = Head::Pending(Some(id));
                }
            }
        }
    }

    /// I keep only keys sitting directly in the inbox folder, not in a subfolder below it.
    fn is_direct_child(&self, key: &str) -> bool {
        key.strip_prefix(self.inbox.prefix.as_str())
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
    }

    /// I turn a listing error into a sentence the user can act on, naming the bucket or the
    /// profile, with a hint about the key that gets them out of it.
    fn list_error(&self, e: &S3Error) -> String {
        let profile = self
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

    /// Add one listing page's objects: sort just the page, then merge it into the order, so a
    /// long listing costs about one sort of each row rather than a re-sort per page.
    fn add_page(&mut self, objects: &[ObjectInfo]) {
        let mut fresh = Vec::new();
        for obj in objects {
            if !self.is_direct_child(&obj.key)
                || self.index.contains_key(&obj.key)
                || self.deleted.contains(&obj.key)
            {
                continue;
            }
            let slot = self.rows.len();
            self.rows.push(Some(Row {
                info: obj.clone(),
                head: Head::Pending(None),
            }));
            self.index.insert(obj.key.clone(), slot);
            fresh.push(slot);
        }
        if fresh.is_empty() {
            return;
        }
        #[cfg(test)]
        SORTED_ROWS.with(|c| c.set(c.get() + fresh.len()));
        let rows = &self.rows;
        let row = |slot: usize| rows[slot].as_ref().expect("a slot in the order is live");
        fresh.sort_by(|&a, &b| newer(row(a), row(b)));
        // A linear merge of two sorted runs.
        let mut merged = Vec::with_capacity(self.order.len() + fresh.len());
        let (mut i, mut j) = (0, 0);
        while i < self.order.len() && j < fresh.len() {
            if newer(row(fresh[j]), row(self.order[i])).is_lt() {
                merged.push(fresh[j]);
                j += 1;
            } else {
                merged.push(self.order[i]);
                i += 1;
            }
        }
        merged.extend_from_slice(&self.order[i..]);
        merged.extend_from_slice(&fresh[j..]);
        self.order = merged;
        self.dirty = true;
    }

    /// Rebuild the shown slots from the order if anything they depend on changed. A pass over
    /// the order, no sorting and no copies of keys.
    fn refresh_view(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let needle = self.filter.to_lowercase();
        let rows = &self.rows;
        self.view = self
            .order
            .iter()
            .copied()
            .filter(|&slot| match rows[slot].as_ref().map(|r| &r.head) {
                Some(Head::NotEmail) | None => false,
                Some(Head::Mail(s)) => {
                    needle.is_empty()
                        || s.from.to_lowercase().contains(&needle)
                        || s.subject.to_lowercase().contains(&needle)
                }
                Some(_) => needle.is_empty(),
            })
            .collect();
        self.sel_pos = self
            .selected
            .and_then(|slot| self.view.iter().position(|&v| v == slot))
            .unwrap_or(0);
    }

    /// I put the cursor at `pos` in the visible rows, clamped, and remember which row that is.
    fn select(&mut self, pos: usize) {
        self.sel_pos = pos.min(self.view.len().saturating_sub(1));
        self.selected = self.view.get(self.sel_pos).copied();
    }

    /// I return the row under the cursor, if its slot still holds one.
    fn current(&self) -> Option<&Row> {
        self.view
            .get(self.sel_pos)
            .and_then(|&slot| self.rows[slot].as_ref())
    }

    /// I drop a deleted message's row and keep the cursor on the row that slides into its place.
    fn remove_row(&mut self, key: &str) {
        self.refresh_view();
        let Some(slot) = self.index.remove(key) else {
            return;
        };
        // Keep the cursor where it was: on the row that slides up into the gap.
        if self.selected == Some(slot)
            && let Some(pos) = self.view.iter().position(|&v| v == slot)
        {
            self.selected = self
                .view
                .get(pos + 1)
                .or_else(|| pos.checked_sub(1).and_then(|p| self.view.get(p)))
                .copied();
        }
        if let Some(row) = self.rows[slot].take() {
            match row.head {
                Head::NotEmail => {
                    self.not_email -= 1;
                    self.checked -= 1;
                }
                Head::Pending(_) => {}
                _ => self.checked -= 1,
            }
        }
        self.order.retain(|&s| s != slot);
        self.dirty = true;
    }

    /// I type into the filter: characters, backspace, enter to keep it and esc to clear it.
    /// Ctrl and alt chords are ignored.
    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            // A ctrl or alt chord is a command, not text: ctrl-a shouldn't type an a.
            KeyCode::Char(_)
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                return;
            }
            KeyCode::Char(c) => self.filter.push(c),
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Enter => self.typing = false,
            KeyCode::Esc => {
                self.filter.clear();
                self.typing = false;
            }
            _ => return,
        }
        self.dirty = true;
    }

    /// I draw the message table. I size the columns to the width (Date and Size go when it's
    /// narrow), cut every cell by display width, and show dates in the local offset.
    fn render_table(&mut self, frame: &mut Frame, area: Rect, offset_hint: UtcOffset) {
        let now_utc = self.now.unwrap_or_else(OffsetDateTime::now_utc);
        // At the very edge of the calendar the local offset has no room: stay in UTC.
        let now = now_utc.checked_to_offset(offset_hint).unwrap_or(now_utc);
        let cols = area.width as usize;
        let wide = cols >= NARROW;
        let fixed = if wide {
            1 + GAP + GAP + DATE_W + GAP + SIZE_WIDTH
        } else {
            1 + GAP
        };
        let rest = cols.saturating_sub(fixed);
        // Subject gets the room first: From takes a quarter (at least 8, at most 24 columns).
        let from_w = (rest / 4).max(rest.min(8)).min(24);
        let subject_w = rest - from_w;
        let line = |from: &str, subject: &str, date: &str, size: &str| {
            let mut s = format!(
                " {}{:GAP$}{}",
                fit(from, from_w),
                "",
                fit(subject, subject_w)
            );
            if wide {
                s.push_str(&format!(
                    "{:GAP$}{:>DATE_W$}{:GAP$}{:>SIZE_WIDTH$}",
                    "", date, "", size
                ));
            }
            s
        };

        let [head, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        frame.render_widget(
            Paragraph::new(line("From", "Subject", "Date", "Size"))
                .style(Style::default().add_modifier(Modifier::BOLD)),
            head,
        );

        self.page = (body.height as usize).max(1);
        let sel = self.sel_pos;
        if sel < self.offset {
            self.offset = sel;
        } else if sel >= self.offset + self.page {
            self.offset = sel + 1 - self.page;
        }
        self.offset = self.offset.min(self.view.len().saturating_sub(self.page));

        let lines: Vec<Line> = self
            .view
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.page)
            .filter_map(|(pos, &slot)| Some((pos, self.rows[slot].as_ref()?)))
            .map(|(pos, row)| {
                let size = human_size(row.info.size);
                let (text, dim) = match &row.head {
                    Head::Mail(s) => {
                        // Show the Date the sender wrote, falling back to when S3 got it.
                        let date = format_date(s.date.or(row.info.last_modified), now);
                        (
                            line(&display_from(&s.from), &row.subject(), &date, &size),
                            false,
                        )
                    }
                    Head::Unreadable(e) => (
                        line("", &format!("(could not read headers: {e})"), "", &size),
                        true,
                    ),
                    _ => (line("…", "loading…", "", &size), true),
                };
                let mut style = Style::default();
                // DIM rather than a fixed grey: dark grey vanished on Solarized Dark.
                if dim {
                    style = style.add_modifier(Modifier::DIM);
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
    /// I title the inbox with its location and, once it has loaded, how many messages it holds
    /// and how many objects weren't email.
    fn title(&self) -> String {
        let mut t = format!("Inbox {}", self.location());
        if self.error.is_none() && self.started {
            let messages = self.index.len() - self.not_email;
            let noun = if messages == 1 { "message" } else { "messages" };
            t.push_str(&format!(" · {messages} {noun}"));
            if self.not_email > 0 {
                t.push_str(&format!(" · {} not email", self.not_email));
            }
        }
        t
    }

    /// I draw the inbox: an error, an empty message or the table, the filter line when it's in
    /// use, and the delete confirmation on top when one is open.
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.refresh_view();
        let show_filter = self.typing || !self.filter.is_empty();
        let [main, filter_area] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(show_filter)),
        ])
        .areas(area);

        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(format!(" {}", escape(err)))
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false }),
                main,
            );
        } else if self.view.is_empty() {
            let pending = self
                .rows
                .iter()
                .flatten()
                .any(|r| matches!(r.head, Head::Pending(_)));
            let location = escape(&self.location()).into_owned();
            let filter = escape(&self.filter).into_owned();
            let total = self.index.len();
            let msg = if !self.filter.is_empty() && self.listing_done {
                if self.checked < total {
                    format!(
                        " Nothing matches /{filter} yet · {} of {total} checked",
                        self.checked
                    )
                } else {
                    format!(" Nothing matches /{filter}")
                }
            } else if !self.listing_done || pending {
                format!(" Loading {location} …")
            } else {
                let mut m = format!(" No messages in {location}.");
                if self.not_email > 0 {
                    let noun = if self.not_email == 1 {
                        "object"
                    } else {
                        "objects"
                    };
                    m.push_str(&format!(
                        " ({} other {noun} there are not email.)",
                        self.not_email
                    ));
                }
                m
            };
            frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: false }), main);
        } else {
            self.render_table(frame, main, ctx.local_offset);
        }

        if show_filter {
            let cursor = if self.typing { "_" } else { "" };
            let total = self.index.len();
            let progress = if self.checked < total {
                format!(" · {} of {total} checked", self.checked)
            } else {
                String::new()
            };
            // Bold in the terminal's own colour: yellow was 1.7:1 on a light theme.
            frame.render_widget(
                Paragraph::new(format!(
                    " /{}{cursor}   {} shown{progress}",
                    escape(&self.filter),
                    self.view.len()
                ))
                .style(Style::default().add_modifier(Modifier::BOLD)),
                filter_area,
            );
        }

        if let Some(key) = &self.confirm {
            let row = self
                .index
                .get(key)
                .and_then(|&slot| self.rows[slot].as_ref());
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

    /// I handle the key, then ask for the rows now on screen to be peeked unless the key moved
    /// me to another screen.
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        let t = self.handle_key(key, ctx);
        if matches!(t, Transition::None) {
            self.request_window(ctx);
        }
        t
    }

    /// A paste goes into the filter while it's being typed, flattened to one line. Anywhere
    /// else it's ignored, and an open delete confirmation takes it as a no.
    fn on_paste(&mut self, text: &str, ctx: &mut Ctx) -> Transition {
        if self.confirm.take().is_some() {
            ctx.info("Delete cancelled.");
        } else if self.typing {
            self.filter.extend(text.chars().filter(|c| !c.is_control()));
            self.dirty = true;
            self.request_window(ctx);
        }
        Transition::None
    }

    /// I take in a finished job: a delete (mine or one the message screen handed over), a
    /// listing page, or a header peek. Results for jobs I never submitted, or from an older
    /// generation, are dropped.
    fn on_done(&mut self, done: &Done, ctx: &mut Ctx) -> Transition {
        // A delete from this screen or from the message screen: either way the row goes.
        if let Job::Delete { bucket, key } = &done.job {
            let mine = self.deletes.remove(&done.id);
            let handed_over = self.handoff.borrow_mut().remove(&done.id);
            let location = format!("s3://{bucket}/{key}");
            match &done.result {
                Ok(_) if *bucket == self.inbox.bucket => {
                    self.remove_row(key);
                    // A listing in flight may have been taken before this: keep it from
                    // bringing the row back.
                    self.deleted.insert(key.clone());
                    if mine || handed_over {
                        ctx.info(format!("Deleted {location}"));
                    }
                }
                Ok(_) => {}
                Err(e) if mine => ctx.error(format!("Could not delete {location}: {e}")),
                Err(e) if handed_over => ctx.error(format!(
                    "Could not delete {location} (after closing the message): {e}"
                )),
                Err(_) => {}
            }
            return Transition::None;
        }

        if !self.jobs.remove(&done.id) {
            return Transition::None;
        }
        match (&done.job, &done.result) {
            (Job::PeekHead { key, .. }, Ok(Outcome::Skipped)) => {
                self.filter_peeks.remove(&done.id);
                // Skipped because the window moved on: ask again if it comes back into view,
                // unless a newer peek for it is already queued.
                if let Some(row) = self.index.get(key).and_then(|&s| self.rows[s].as_mut())
                    && matches!(row.head, Head::Pending(Some(id)) if id == done.id)
                {
                    row.head = Head::Pending(None);
                }
            }
            (_, Ok(Outcome::Skipped)) => {}
            (Job::List { .. }, Ok(Outcome::Listing(listing))) => {
                self.pages += 1;
                self.add_page(&listing.objects);
                match &listing.next_token {
                    Some(t) if self.last_token.as_ref() == Some(t) => {
                        self.listing_done = true;
                        ctx.error(format!(
                            "Stopped listing {}: S3 sent the same continuation token twice.",
                            self.location()
                        ));
                    }
                    Some(_) if self.pages >= MAX_PAGES => {
                        self.listing_done = true;
                        ctx.error(format!(
                            "Stopped listing {} after {MAX_PAGES} pages.",
                            self.location()
                        ));
                    }
                    Some(t) => {
                        self.last_token = Some(t.clone());
                        self.list_page(Some(t.clone()), ctx);
                    }
                    None => self.listing_done = true,
                }
            }
            (Job::List { .. }, Err(e)) => {
                self.listing_done = true;
                let msg = self.list_error(e);
                if self.index.is_empty() {
                    self.error = Some(msg);
                } else {
                    ctx.error(msg);
                }
            }
            (Job::PeekHead { key, .. }, result) => {
                self.filter_peeks.remove(&done.id);
                let head = match result {
                    Ok(Outcome::Head(h)) => match &h.summary {
                        Some(s) if h.is_email => Head::Mail(s.clone()),
                        _ => Head::NotEmail,
                    },
                    Ok(_) => return Transition::None,
                    Err(e) => Head::Unreadable(e.to_string()),
                };
                if let Some(row) = self.index.get(key).and_then(|&s| self.rows[s].as_mut()) {
                    if matches!(head, Head::NotEmail) && !matches!(row.head, Head::NotEmail) {
                        self.not_email += 1;
                    }
                    if matches!(row.head, Head::Pending(_)) {
                        self.checked += 1;
                    }
                    row.head = head;
                    self.dirty = true;
                }
            }
            _ => {}
        }
        // New peeks come from the tick, once per pump, not from each result.
        Transition::None
    }

    /// I pick up ctx.session the first time I'm shown and start the first load.
    fn on_focus(&mut self, ctx: &mut Ctx) {
        if self.session.is_none() {
            self.session = ctx.session.clone();
        }
        if !self.started {
            self.load(ctx);
        }
    }

    /// I keep the rows on screen peeked on every tick, so a resize fills in without a key press.
    fn on_tick(&mut self, ctx: &mut Ctx) {
        if self.started && self.error.is_none() {
            self.request_window(ctx);
        }
    }

    /// I report my own session, so the header bar names the account this inbox reads.
    fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// I show the keys the list takes, or just the filter keys while one is being typed.
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

impl InboxScreen {
    /// I handle a key. An open delete confirmation takes it first (only a bare y deletes); then
    /// the filter while it's being typed; then movement, open, delete, filter, refresh and the
    /// accounts screen.
    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if let Some(target) = self.confirm.take() {
            // Only a bare y: ctrl-y, or a y that came in as part of something else, cancels.
            if key.code == KeyCode::Char('y') && key.modifiers == KeyModifiers::NONE {
                let job = Job::Delete {
                    bucket: self.inbox.bucket.clone(),
                    key: target,
                };
                // Deliberately not stamped: a refresh never cancels a delete.
                match &self.session {
                    Some(session) => {
                        let id = ctx.submit_to(session, job, None);
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

        self.refresh_view();
        let pos = self.sel_pos;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(pos.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(pos + 1),
            KeyCode::PageUp => self.select(pos.saturating_sub(self.page)),
            KeyCode::PageDown => self.select(pos + self.page),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            KeyCode::Enter => {
                if let Some(row) = self.current() {
                    let mut screen =
                        MessageScreen::new(self.inbox.bucket.clone(), row.info.key.clone())
                            .with_subject(row.subject())
                            .with_out_dir(self.downloads.clone())
                            .with_handoff(Rc::clone(&self.handoff));
                    if let Some(session) = &self.session {
                        screen = screen.with_session(session.clone());
                    }
                    return Transition::Push(Box::new(screen));
                }
            }
            KeyCode::Char('d') if key.modifiers == KeyModifiers::NONE => {
                self.confirm = self.current().map(|r| r.info.key.clone());
            }
            KeyCode::Char('r') => {
                self.load(ctx);
                ctx.info(format!("Refreshing {}", self.location()));
            }
            KeyCode::Char('/') => self.typing = true,
            KeyCode::Char('u') => return Transition::Push(Box::new(AccountsScreen::new(ctx))),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.dirty = true;
            }
            // The inbox is the root screen, so Esc staying put keeps a stray press from quitting.
            KeyCode::Char('q') => return Transition::Pop,
            _ => {}
        }
        Transition::None
    }
}

/// The delete confirmation both screens show.
///
/// The prompt comes first, so however short the screen, "Press y" is the line that survives.
/// Then the full `s3://` location, escaped so a key's control characters show as text and two
/// different keys can't look alike, and cut into rows by display width myself rather than
/// word-wrapped: word wrap took more rows than I'd counted and pushed the last one out of the
/// box. The sender-controlled subject and From get one row each and are what go when there
/// isn't room.
pub(super) fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    subject: &str,
    from: &str,
    location: &str,
) {
    let prompt = "Press y to delete, any other key to cancel.";
    let object = format!("Object:  {}", escape(location));
    let aw = area.width as usize;
    let ah = area.height as usize;
    // As wide as the location needs, within the screen: 2 columns of border and 2 of padding.
    let widest = width(&object).max(width(prompt)).max(40);
    let inner = widest.min(aw.saturating_sub(4)).max(1);
    let prompt_rows = chunk(prompt, inner);
    let object_rows = chunk(&object, inner);

    // The prompt and the location always; the subject, From and a spacer only if they fit.
    let mut spare = ah
        .saturating_sub(2)
        .saturating_sub(prompt_rows.len() + object_rows.len());
    let mut take = |wanted: bool| {
        let yes = wanted && spare > 0;
        if yes {
            spare -= 1;
        }
        yes
    };
    let subject_line = take(true);
    let from_line = take(!from.is_empty());
    let gap = take(true);

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = prompt_rows
        .into_iter()
        .map(|r| Line::styled(r, bold))
        .collect();
    if gap {
        lines.push(Line::raw(""));
    }
    if subject_line {
        lines.push(Line::raw(fit(
            &format!("Subject: {}", clean(subject)),
            inner,
        )));
    }
    if from_line {
        lines.push(Line::raw(fit(&format!("From:    {}", clean(from)), inner)));
    }
    lines.extend(object_rows.into_iter().map(Line::raw));

    let w = (inner + 4).min(aw);
    let h = (lines.len() + 2).min(ah);
    let rect = Rect {
        x: area.x.saturating_add(((aw - w) / 2) as u16),
        y: area.y.saturating_add(((ah - h) / 2) as u16),
        width: w as u16,
        height: h as u16,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(" Delete this message from S3? ")
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        rect,
    );
}

/// `s` cut into rows of at most `cols` columns, between graphemes, so the row count is exact.
fn chunk(s: &str, cols: usize) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    let cols = cols.max(1);
    let mut rows = vec![String::new()];
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = width(g);
        if used + w > cols && used > 0 {
            rows.push(String::new());
            used = 0;
        }
        if let Some(row) = rows.last_mut() {
            row.push_str(g);
        }
        used += w;
    }
    rows
}

/// I default saved attachments and text to ~/Downloads.
fn default_downloads() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads")
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

/// In `now`'s offset: today's messages as a time, this year's as "Sep 20", older ones as a date.
///
/// A date too close to the edge of the calendar to move to that offset (year 9999 and a zone
/// ahead of it, say) stays in the offset it was written in rather than crashing the render.
fn format_date(date: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    let Some(d) = date else {
        return String::new();
    };
    let d = d.checked_to_offset(now.offset()).unwrap_or(d);
    if d.date() == now.date() {
        format!("{:02}:{:02}", d.hour(), d.minute())
    } else if d.year() == now.year() {
        let month = d.month().to_string();
        format!("{} {}", &month[..3], d.day())
    } else {
        format!("{}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
    }
}

#[cfg(test)]
mod pool_tests;

#[cfg(test)]
thread_local! {
    /// Rows that went through a sort, so a test can tell linear work from quadratic.
    static SORTED_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many rows this thread has sorted since the last reset.
#[cfg(test)]
fn sorted_rows() -> usize {
    SORTED_ROWS.with(std::cell::Cell::get)
}

/// I zero the per-thread sort counter a test reads to check how often rows get sorted.
#[cfg(test)]
fn sorted_rows_reset() {
    SORTED_ROWS.with(|c| c.set(0));
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
        /// I build an empty store that records when each object was received.
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

        /// I store an object and record `at` as its received time.
        pub fn put_received(&self, bucket: &str, key: &str, data: &[u8], at: OffsetDateTime) {
            self.inner.put(bucket, key, data);
            self.received.lock().unwrap().insert(key.to_string(), at);
        }

        /// I create an empty bucket in the wrapped store.
        pub fn create_bucket(&self, bucket: &str) {
            self.inner.create_bucket(bucket);
        }

        /// I say whether the wrapped store still holds `bucket/key`.
        pub fn contains(&self, bucket: &str, key: &str) -> bool {
            self.inner.contains(bucket, key)
        }

        /// Ranged gets so far, which is what a header peek does.
        pub fn peeks(&self) -> usize {
            self.peeks.load(Ordering::SeqCst)
        }
    }

    impl Store for Timed {
        /// I pass this straight through to the wrapped store.
        fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
            self.inner.list_buckets()
        }
        /// I list through the wrapped store and stamp each object with its recorded received time.
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
        /// I count the ranged get as a header peek, then pass it through.
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
        /// I pass this straight through to the wrapped store.
        fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
            self.inner.get(bucket, key)
        }
        /// I pass this straight through to the wrapped store.
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
        /// I have no buckets to list.
        fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
            Ok(Vec::new())
        }
        /// I hand back one new object per call with a token from `next`, so the listing never ends
        /// on its own.
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
        /// I answer every peek with bytes that are not mail.
        fn get_range(&self, _: &str, _: &str, _: u64, _: u64) -> Result<Vec<u8>, S3Error> {
            Ok(b"not mail".to_vec())
        }
        /// I answer every get with bytes that are not mail.
        fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, S3Error> {
            Ok(b"not mail".to_vec())
        }
        /// I pretend every delete worked.
        fn delete(&self, _: &str, _: &str) -> Result<(), S3Error> {
            Ok(())
        }
    }

    /// I give the inbox config every fixture points at.
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

    /// I build the AccessDenied error S3 sends for a missing permission.
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
        /// I pass this straight through to the wrapped store.
        fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
            self.inner.list_buckets()
        }
        /// I fail the listing with `list_err` when one is set, and list normally otherwise.
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
        /// I pass this straight through to the wrapped store.
        fn get_range(
            &self,
            bucket: &str,
            key: &str,
            start: u64,
            end: u64,
        ) -> Result<Vec<u8>, S3Error> {
            self.inner.get_range(bucket, key, start, end)
        }
        /// I pass this straight through to the wrapped store.
        fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
            self.inner.get(bucket, key)
        }
        /// I fail the delete with `delete_err` when one is set, and delete normally otherwise.
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

    /// I build an app showing an inbox on `store`, with a fixed clock and a downloads folder
    /// in a temp dir that lives as long as the returned guard.
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

    /// I find the first screen line holding `needle`, and fail with the whole screen if none does.
    fn line_with<'a>(scr: &'a str, needle: &str) -> &'a str {
        scr.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line contains {needle:?} in:\n{scr}"))
    }

    /// I read the title of the screen on top of the stack.
    fn top_title(app: &App) -> String {
        app.stack.last().unwrap().title()
    }

    /// I check the header row and cells read like a mail client: a display name over the address,
    /// a time for today's mail and a date for older mail, plus the message count.
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

    /// I check sizes show as KiB once they're big enough and as plain bytes below that.
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

    /// I check long From and Subject cells get cut with an ellipsis so Date and Size still fit,
    /// and that below the minimum width those two columns give their room away.
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

    /// I check the list is sorted newest first, since that's where new mail should show up.
    #[test]
    fn newest_message_comes_first() {
        let (mut app, _d) = app_with(three());
        let scr = screen(&mut app, 100, 12);
        let pos = |s: &str| scr.find(s).unwrap_or_else(|| panic!("{s} missing:\n{scr}"));
        assert!(pos("Lunch today") < pos("Invoice for September"), "{scr}");
        assert!(pos("Invoice for September") < pos("Old news"), "{scr}");
    }

    /// I check objects that aren't mail (the SES setup PNG, a binary) stay off the list but still
    /// show up in the "not email" count, so nothing vanishes silently.
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

    /// I shuffle received times across listing pages, so only a real merge of each new page
    /// gets the whole list newest first.
    #[test]
    fn later_pages_merge_into_the_newest_first_order() {
        // MemoryStore pages three keys at a time, in key order; the received times are
        // shuffled across pages, so only a correct merge gets the order right.
        let s = Timed::new();
        let minutes = [7, 2, 9, 4, 11, 1, 8, 5, 10, 3, 6, 0];
        for (i, m) in minutes.iter().enumerate() {
            s.put_received(
                BUCKET,
                &format!("mail/k{i:02}"),
                &email(
                    "a@example.com",
                    &format!("Minute {m:02}"),
                    "25 Sep 2026 10:00:00 +0000",
                ),
                time::OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(*m),
            );
        }
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 20);
        let shown: Vec<usize> = scr
            .lines()
            .filter_map(|l| l.split("Minute ").nth(1))
            .map(|rest| rest[..2].parse().unwrap())
            .collect();
        assert_eq!(shown, (0..12).rev().collect::<Vec<_>>(), "{scr}");
    }

    /// I check every page of the listing arrives, and that keys in subfolders or other prefixes
    /// never leak into the inbox.
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

    /// I check a row says "loading" until its header peek lands, so the list can show up before
    /// every message has been read.
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

    /// I deliver the header peeks in reverse and check the order still comes out right, because
    /// a pool doesn't promise they finish in the order I asked.
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
            // Peeks are asked for on the tick, as the run loop would.
            if let Some(top) = app.stack.last_mut() {
                top.on_tick(&mut app.ctx);
            }
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

    /// I hand a finished job to every screen on the stack, the way the job pump does, and apply
    /// the top screen's transition.
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

    /// I bury From and Subject under 40 KiB of Received lines and check I still find them, so a
    /// chatty relay chain can't blank a row.
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

    /// I check a listing the inbox never asked for can't add rows, even for the same folder.
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

    /// I check Enter opens the selected row, and that coming back keeps the rows without a reload.
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

    /// I check `d` asks first and names both the subject and the full S3 key, so you know
    /// exactly what's about to go.
    #[test]
    fn delete_asks_for_confirmation_naming_subject_and_key() {
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::Char('d')));
        let scr = screen(&mut app, 100, 16);
        assert!(scr.contains("Lunch today"), "{scr}");
        assert!(scr.contains("s3://inbox-bucket/mail/bbb"), "{scr}");
        assert!(scr.contains("y to delete"), "{scr}");
    }

    /// I check every key but a lowercase `y` cancels the delete, including Enter and a capital Y,
    /// since a delete can't be undone.
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

    /// I check `y` removes the object from the bucket and the row from the list, and says so.
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

    /// I check a delete S3 refuses leaves the row in place and puts the key and the error on the
    /// status line as an error.
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

    /// I check the zero-byte folder marker the S3 console creates isn't shown or counted as a message.
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

    /// I walk through the `/` filter: it matches From and Subject case-insensitively, Enter opens
    /// a filtered row, and Esc clears the filter rather than leaving the inbox.
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

    /// I check `r` lists the folder again and picks up mail that arrived after the first listing.
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

    /// I check `u` takes me to the accounts screen.
    #[test]
    fn u_goes_to_the_accounts_screen() {
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::Char('u')));
        assert!(top_title(&app).contains("Accounts"), "{}", top_title(&app));
    }

    /// I check an empty folder says there are no messages instead of drawing an empty table.
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

    /// I check a folder holding only non-email objects still says there are no messages, and how
    /// many objects it skipped.
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

    /// I check a bucket that doesn't exist gets named in plain words rather than an empty table.
    #[test]
    fn a_missing_bucket_says_so() {
        let s = Timed::new();
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("bucket inbox-bucket does not exist"), "{scr}");
        assert!(!scr.contains("Subject"), "{scr}");
    }

    /// I check an access-denied listing says so and names the folder and profile, so you know which
    /// credentials to go and fix.
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

    /// I check the inbox says "Not connected" when there's no session, rather than failing quietly.
    #[test]
    fn no_connected_account_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), None);
        let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox())));
        settle(&mut app);
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("Not connected"), "{scr}");
    }

    /// I check End and Home scroll a list longer than the screen, so the selection never goes off-screen.
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

    /// I find the screen line holding `needle`, if any.
    fn row_with<'a>(scr: &'a str, needle: &str) -> Option<&'a str> {
        scr.lines().find(|l| l.contains(needle))
    }

    /// I check Esc on the root inbox doesn't quit and only `q` does, so a stray Esc can't close the app.
    #[test]
    fn esc_at_the_root_inbox_does_not_quit_but_q_does() {
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::Esc));
        assert!(!app.quit);
        assert!(screen(&mut app, 100, 12).contains("Lunch today"));
        app.key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    /// I connect a second account behind the inbox's back and check refresh, open and delete
    /// all still go to the inbox's own account, never the new one.
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

    /// I check dates show in the terminal's local offset, which can push late UTC mail onto the day before.
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

    /// I check I sort on when S3 received the object, not the Date header, so spam dated 2030
    /// can't sit at the top forever.
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

    /// I check only the rows on screen and a page ahead get their headers peeked, so a huge folder
    /// doesn't cost one request per message up front.
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

    /// I check a taller screen gets its extra rows peeked on the next pump, without waiting for a key.
    #[test]
    fn a_taller_screen_gets_its_rows_peeked_without_a_key_press() {
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
        assert!(s.peeks() <= 2 * DEFAULT_PAGE);
        // A render at 80 rows, then only the job pump: the extra rows get peeked.
        screen(&mut app, 100, 80);
        settle(&mut app);
        let scr = screen(&mut app, 100, 80);
        assert!(!scr.contains("loading"), "{scr}");
        assert!(s.peeks() > 2 * DEFAULT_PAGE, "{} peeks", s.peeks());
    }

    /// I check the row below a deleted one takes the selection, or the one above when I delete the
    /// last row, rather than jumping back to the top.
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
        // Deleting the last row selects the one above it, not the top.
        let (mut app, _d) = app_with(three());
        app.key(key(KeyCode::End));
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        assert!(screen(&mut app, 100, 20).contains("body of Invoice for September"));
    }

    /// I check a store that hands back the same continuation token twice stops the listing and
    /// says why, instead of looping forever.
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

    /// I check a listing that never ends stops at the page cap and says so on the status line.
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

    /// I check the delete prompt keeps the key and the `y` line visible at every size with a
    /// hostile long subject, and that tiny screens render without panicking.
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
