//! Browse buckets and folders, find stored email, save a folder as the inbox.

use std::collections::{BTreeMap, HashMap, VecDeque};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::inbox::InboxScreen;
use super::jobs::{Done, Generation, Job, JobId, Outcome};
use super::text::{SIZE_WIDTH, fit, human_size, width};
use super::{Ctx, Session, Transition, View};
use crate::config::Inbox;
use crate::mail;
use crate::s3::S3Error;

/// How much of each object gets fetched to decide whether it is email.
const PEEK_BYTES: u64 = 4096;
/// Search peeks in flight at once, so stopping a search does not leave a long queue behind.
const SEARCH_IN_FLIGHT: usize = 16;
/// Lines above the list: the path and the counts.
const HEADER_LINES: u16 = 2;
/// Most pages one listing or search follows (10 million keys at S3's 1000 a page).
const MAX_PAGES: usize = 10_000;

/// Guards a run of continuation tokens against a server that never stops handing them out.
struct Paging {
    pages: usize,
    last: Option<String>,
    max: usize,
}

impl Paging {
    fn new(max: usize) -> Self {
        Self {
            pages: 0,
            last: None,
            max,
        }
    }

    /// Count a page that just arrived. Ok(Some) is the token to follow, Ok(None) means that
    /// was the last page, and Err says why following it would never end.
    fn next(&mut self, token: Option<&String>, what: &str) -> Result<Option<String>, String> {
        self.pages += 1;
        let Some(token) = token else {
            return Ok(None);
        };
        if self.last.as_ref() == Some(token) {
            return Err(format!(
                "S3 sent the same continuation token twice, so I stopped {what} after {} pages",
                self.pages
            ));
        }
        if self.pages >= self.max {
            return Err(format!("stopped {what} after {} pages", self.pages));
        }
        self.last = Some(token.clone());
        Ok(Some(token.clone()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Location {
    Buckets,
    Folder { bucket: String, prefix: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Unchecked,
    Checking,
    Email,
    NotEmail,
    Failed,
}

struct Object {
    key: String,
    size: u64,
    mark: Mark,
}

/// A row: buckets and folders first, then objects, like a file manager.
#[derive(Clone, Copy)]
enum Row {
    Bucket(usize),
    Folder(usize),
    Object(usize),
}

pub struct BrowserScreen {
    started: bool,
    location: Location,
    buckets: Vec<String>,
    folders: Vec<String>,
    objects: Vec<Object>,
    /// Rows matching the filter, in display order.
    rows: Vec<Row>,
    selected: usize,
    offset: usize,
    /// List height at the last render.
    visible: usize,
    loading: bool,
    list_job: Option<JobId>,
    list_error: Option<String>,
    paging: Paging,
    max_pages: usize,
    /// Peek jobs for the current folder, by object index.
    peeks: HashMap<JobId, usize>,
    filter: String,
    editing_filter: bool,
    /// After going up, select the folder or bucket we came out of once it shows up.
    reselect: Option<String>,
    search: Option<Search>,
    /// The account this browser talks to, fixed when it opens, so connecting somewhere else
    /// later cannot change it under this screen.
    session: Option<Session>,
    /// Bumped on every navigation, so peeks still queued for the folder we left are skipped.
    generation: Generation,
}

impl BrowserScreen {
    pub fn new() -> Self {
        Self {
            started: false,
            location: Location::Buckets,
            buckets: Vec::new(),
            folders: Vec::new(),
            objects: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            offset: 0,
            visible: 20,
            loading: false,
            list_job: None,
            list_error: None,
            paging: Paging::new(MAX_PAGES),
            max_pages: MAX_PAGES,
            peeks: HashMap::new(),
            filter: String::new(),
            editing_filter: false,
            reselect: None,
            search: None,
            session: None,
            generation: Generation::new(),
        }
    }

    fn go(&mut self, location: Location, ctx: &mut Ctx) {
        self.generation.bump();
        self.location = location;
        self.buckets.clear();
        self.folders.clear();
        self.objects.clear();
        self.rows.clear();
        self.peeks.clear();
        self.selected = 0;
        self.offset = 0;
        self.filter.clear();
        self.editing_filter = false;
        self.list_error = None;
        self.loading = true;
        self.paging = Paging::new(self.max_pages);
        let job = match &self.location {
            Location::Buckets => Job::ListBuckets,
            Location::Folder { bucket, prefix } => Job::List {
                bucket: bucket.clone(),
                prefix: prefix.clone(),
                delimiter: true,
                token: None,
            },
        };
        self.list_job = submit(ctx, self.session.as_ref(), &self.generation, job);
        if self.list_job.is_none() {
            self.loading = false;
            ctx.error("no account is connected");
        }
    }

    fn up(&mut self, ctx: &mut Ctx) {
        let Location::Folder { bucket, prefix } = self.location.clone() else {
            return;
        };
        if prefix.is_empty() {
            self.go(Location::Buckets, ctx);
            self.reselect = Some(bucket);
        } else {
            let trimmed = &prefix[..prefix.len() - 1];
            let parent = match trimmed.rfind('/') {
                Some(i) => trimmed[..=i].to_string(),
                None => String::new(),
            };
            self.go(
                Location::Folder {
                    bucket,
                    prefix: parent,
                },
                ctx,
            );
            self.reselect = Some(prefix);
        }
    }

    fn open_selected(&mut self, ctx: &mut Ctx) {
        let Some(&row) = self.rows.get(self.selected) else {
            return;
        };
        match (row, &self.location) {
            (Row::Bucket(i), _) => {
                let bucket = self.buckets[i].clone();
                self.go(
                    Location::Folder {
                        bucket,
                        prefix: String::new(),
                    },
                    ctx,
                );
            }
            (Row::Folder(i), Location::Folder { bucket, .. }) => {
                let location = Location::Folder {
                    bucket: bucket.clone(),
                    prefix: self.folders[i].clone(),
                };
                self.go(location, ctx);
            }
            _ => {}
        }
    }

    fn prefix(&self) -> &str {
        match &self.location {
            Location::Folder { prefix, .. } => prefix,
            Location::Buckets => "",
        }
    }

    fn row_name(&self, row: Row) -> &str {
        let prefix = self.prefix();
        match row {
            Row::Bucket(i) => &self.buckets[i],
            Row::Folder(i) => self.folders[i]
                .strip_prefix(prefix)
                .unwrap_or(&self.folders[i]),
            Row::Object(i) => {
                let key = &self.objects[i].key;
                key.strip_prefix(prefix).unwrap_or(key)
            }
        }
    }

    fn rebuild_rows(&mut self) {
        let keep = self.rows.get(self.selected).copied();
        let all = (0..self.buckets.len())
            .map(Row::Bucket)
            .chain((0..self.folders.len()).map(Row::Folder))
            .chain((0..self.objects.len()).map(Row::Object));
        let needle = self.filter.to_lowercase();
        self.rows = all
            .filter(|&r| needle.is_empty() || self.row_name(r).to_lowercase().contains(&needle))
            .collect();
        if let Some(name) = self.reselect.clone()
            && let Some(i) = self.rows.iter().position(|&r| match r {
                Row::Bucket(i) => self.buckets[i] == name,
                Row::Folder(i) => self.folders[i] == name,
                Row::Object(_) => false,
            })
        {
            self.selected = i;
            self.reselect = None;
        } else if let Some(keep) = keep {
            // Keep the cursor on the same row while later pages arrive.
            self.selected = self
                .rows
                .iter()
                .position(|&r| same_row(r, keep))
                .unwrap_or(0);
        }
        self.clamp();
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        let visible = self.visible.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
        self.offset = self.offset.min(self.rows.len().saturating_sub(1));
    }

    /// Peek every object on screen that has not been peeked yet.
    fn peek_visible(&mut self, ctx: &mut Ctx) {
        let Location::Folder { bucket, .. } = &self.location else {
            return;
        };
        if self.search.is_some() {
            return;
        }
        let end = (self.offset + self.visible).min(self.rows.len());
        for &row in &self.rows[self.offset.min(end)..end] {
            let Row::Object(i) = row else { continue };
            let obj = &mut self.objects[i];
            if obj.mark != Mark::Unchecked {
                continue;
            }
            if obj.size == 0 {
                obj.mark = Mark::NotEmail;
                continue;
            }
            let job = Job::Peek {
                bucket: bucket.clone(),
                key: obj.key.clone(),
                bytes: PEEK_BYTES,
            };
            match submit(ctx, self.session.as_ref(), &self.generation, job) {
                Some(id) => {
                    obj.mark = Mark::Checking;
                    self.peeks.insert(id, i);
                }
                None => return,
            }
        }
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(n - 1);
        self.clamp();
    }

    fn save_inbox(&mut self, bucket: String, prefix: String, ctx: &mut Ctx) -> Transition {
        let Some(session) = self.session.clone() else {
            ctx.error("no account is connected");
            return Transition::None;
        };
        let inbox = Inbox {
            profile: session.profile.name.clone(),
            bucket,
            prefix,
            region: Some(session.region.clone()),
        };
        let previous = ctx.config.inbox.replace(inbox.clone());
        if !ctx.save_config() {
            ctx.config.inbox = previous;
            return Transition::None;
        }
        ctx.info(format!(
            "saved {}/{} as the inbox",
            inbox.bucket, inbox.prefix
        ));
        // The inbox reads ctx.session, and the reset leaves no other screen that could be
        // relying on the old one.
        ctx.session = Some(session);
        Transition::Reset(Box::new(InboxScreen::new(inbox)))
    }

    fn start_search(&mut self, ctx: &mut Ctx) {
        let Location::Folder { bucket, prefix } = &self.location else {
            ctx.error("open a bucket first, then press s to search it");
            return;
        };
        let mut search = Search::new(
            bucket.clone(),
            prefix.clone(),
            self.max_pages,
            self.session.clone(),
        );
        search.list(None, ctx);
        self.search = Some(search);
    }

    fn filter_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) => self.filter.push(c),
            KeyCode::Backspace => {
                if self.filter.pop().is_none() {
                    self.editing_filter = false;
                }
            }
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Esc => {
                self.filter.clear();
                self.editing_filter = false;
            }
            _ => return false,
        }
        self.selected = 0;
        self.offset = 0;
        self.rebuild_rows();
        true
    }

    fn folder_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.editing_filter && self.filter_key(key) {
            self.peek_visible(ctx);
            return Transition::None;
        }
        let page = self.visible.max(1) as isize;
        match key.code {
            KeyCode::Char('q') => return Transition::Pop,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Esc if self.location == Location::Buckets => return Transition::Pop,
            KeyCode::Esc => self.up(ctx),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_selected(ctx),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.up(ctx),
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Char('r') => {
                let here = self.location.clone();
                self.go(here, ctx);
            }
            KeyCode::Char('s') => self.start_search(ctx),
            KeyCode::Char('i') => match self.location.clone() {
                Location::Folder { bucket, prefix } => {
                    return self.save_inbox(bucket, prefix, ctx);
                }
                Location::Buckets => {
                    ctx.error("open a bucket first, then press i in the folder that holds the mail")
                }
            },
            _ => {}
        }
        self.peek_visible(ctx);
        Transition::None
    }

    fn search_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        let Some(search) = self.search.as_mut() else {
            return Transition::None;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                search.stop();
                self.search = None;
                self.peek_visible(ctx);
            }
            KeyCode::Char('x') => search.stop(),
            KeyCode::Down | KeyCode::Char('j') => search.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => search.move_by(-1),
            KeyCode::PageDown => search.move_by(search.visible.max(1) as isize),
            KeyCode::PageUp => search.move_by(-(search.visible.max(1) as isize)),
            KeyCode::Home | KeyCode::Char('g') => search.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => search.move_by(isize::MAX / 2),
            KeyCode::Enter => {
                if let Some(folder) = search.selected_folder() {
                    let bucket = search.bucket.clone();
                    search.stop();
                    self.search = None;
                    self.go(
                        Location::Folder {
                            bucket,
                            prefix: folder,
                        },
                        ctx,
                    );
                }
            }
            KeyCode::Char('i') => {
                if let Some(folder) = search.selected_folder() {
                    let bucket = search.bucket.clone();
                    return self.save_inbox(bucket, folder, ctx);
                }
            }
            _ => {}
        }
        Transition::None
    }

    fn listing_done(&mut self, result: &Result<Outcome, S3Error>, ctx: &mut Ctx) {
        match result {
            Ok(Outcome::Buckets(buckets)) => {
                self.buckets = buckets.iter().map(|b| b.name.clone()).collect();
                self.loading = false;
                self.list_job = None;
            }
            Ok(Outcome::Listing(listing)) => {
                let prefix = self.prefix().to_string();
                self.folders.extend(listing.prefixes.iter().cloned());
                self.objects.extend(
                    listing
                        .objects
                        .iter()
                        // A zero-byte "folder marker" object named like the folder itself.
                        .filter(|o| o.key != prefix)
                        .map(|o| Object {
                            key: o.key.clone(),
                            size: o.size,
                            mark: Mark::Unchecked,
                        }),
                );
                self.list_job = None;
                let what = format!("listing {}", self.path());
                match (
                    self.paging.next(listing.next_token.as_ref(), &what),
                    &self.location,
                ) {
                    (Ok(Some(token)), Location::Folder { bucket, prefix }) => {
                        self.list_job = submit(
                            ctx,
                            self.session.as_ref(),
                            &self.generation,
                            Job::List {
                                bucket: bucket.clone(),
                                prefix: prefix.clone(),
                                delimiter: true,
                                token: Some(token),
                            },
                        );
                        self.loading = self.list_job.is_some();
                    }
                    (Err(msg), _) => {
                        self.loading = false;
                        ctx.error(msg);
                    }
                    _ => self.loading = false,
                }
            }
            Ok(_) => {
                self.loading = false;
                self.list_job = None;
            }
            Err(e) => {
                self.loading = false;
                self.list_job = None;
                let msg = format!("could not list {}: {e}", self.path());
                self.list_error = Some(msg.clone());
                ctx.error(msg);
            }
        }
        self.rebuild_rows();
        self.peek_visible(ctx);
    }

    fn path(&self) -> String {
        match &self.location {
            Location::Buckets => "buckets".into(),
            Location::Folder { bucket, prefix } => format!("{bucket}/{prefix}"),
        }
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        match &self.location {
            Location::Buckets => parts.push(plural(self.buckets.len(), "bucket")),
            Location::Folder { .. } => {
                parts.push(plural(self.folders.len(), "folder"));
                parts.push(plural(self.objects.len(), "object"));
                let emails = self
                    .objects
                    .iter()
                    .filter(|o| o.mark == Mark::Email)
                    .count();
                let checked = self
                    .objects
                    .iter()
                    .filter(|o| matches!(o.mark, Mark::Email | Mark::NotEmail | Mark::Failed))
                    .count();
                if checked < self.objects.len() {
                    parts.push(format!(
                        "{} so far ({checked} of {} checked)",
                        plural(emails, "email"),
                        self.objects.len()
                    ));
                } else {
                    parts.push(plural(emails, "email"));
                }
            }
        }
        if self.loading {
            parts.push("loading...".into());
        }
        parts.join(" · ")
    }

    fn render_folder(&mut self, frame: &mut Frame, area: Rect, account: Option<String>) {
        let [head, body] =
            Layout::vertical([Constraint::Length(HEADER_LINES), Constraint::Min(1)]).areas(area);
        let dim = Style::default().fg(Color::DarkGray);
        let path = match &self.location {
            Location::Buckets => Span::styled(
                " All buckets",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Location::Folder { .. } => Span::styled(
                format!(" {}", self.path()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        };
        let mut first = vec![path];
        first.extend(account.map(|a| Span::styled(a, dim)));
        let mut second = vec![Span::styled(format!(" {}", self.summary()), dim)];
        if self.editing_filter || !self.filter.is_empty() {
            second.push(Span::styled(
                format!(
                    "   /{}{}",
                    self.filter,
                    if self.editing_filter { "_" } else { "" }
                ),
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(
            Paragraph::new(vec![Line::from(first), Line::from(second)]),
            head,
        );

        self.visible = body.height as usize;
        self.clamp();

        if self.rows.is_empty() {
            let (msg, style) = if let Some(e) = &self.list_error {
                (format!(" {e}"), Style::default().fg(Color::Red))
            } else if self.loading {
                (" Loading...".to_string(), dim)
            } else if !self.filter.is_empty() {
                (
                    format!(" nothing matches /{} (esc clears the filter)", self.filter),
                    dim,
                )
            } else if self.location == Location::Buckets {
                (" This account has no buckets.".to_string(), dim)
            } else {
                (" This folder is empty.".to_string(), dim)
            };
            frame.render_widget(
                Paragraph::new(msg).style(style).wrap(Wrap { trim: false }),
                body,
            );
            return;
        }

        let end = (self.offset + self.visible).min(self.rows.len());
        let window = &self.rows[self.offset..end];
        let cols = body.width as usize;
        // Name column: as wide as the longest visible name, leaving room for size and mark.
        let name_w = window
            .iter()
            .map(|&r| width(self.row_name(r)))
            .max()
            .unwrap_or(0)
            .min(cols.saturating_sub(22).max(10));
        let items: Vec<ListItem> = window
            .iter()
            .map(|&row| {
                // Cut and padded by display width, so wide characters keep the columns lined up.
                let name = fit(self.row_name(row), name_w);
                match row {
                    Row::Bucket(_) => {
                        ListItem::new(Line::styled(name, Style::default().fg(Color::Cyan)))
                    }
                    Row::Folder(_) => ListItem::new(Line::styled(
                        name,
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Row::Object(i) => {
                        let obj = &self.objects[i];
                        let (mark, style) = match obj.mark {
                            Mark::Email => ("email", Style::default().fg(Color::Green)),
                            Mark::Checking | Mark::Unchecked => ("...", dim),
                            Mark::Failed => ("?", Style::default().fg(Color::Red)),
                            Mark::NotEmail => ("", dim),
                        };
                        ListItem::new(Line::from(vec![
                            Span::raw(format!("{name}  ")),
                            Span::styled(format!("{:>SIZE_WIDTH$}  ", human_size(obj.size)), dim),
                            Span::styled(mark, style),
                        ]))
                    }
                }
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected - self.offset));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_spacing(ratatui::widgets::HighlightSpacing::Always)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            body,
            &mut state,
        );
    }
}

impl BrowserScreen {
    /// A browser that talks to `session`, whatever `ctx.session` says later.
    pub fn with_session(session: Session) -> Self {
        Self {
            session: Some(session),
            ..Self::new()
        }
    }
}

impl BrowserScreen {
    /// Lower the page cap so a test can reach it.
    #[cfg(test)]
    pub(crate) fn with_max_pages(mut self, n: usize) -> Self {
        self.max_pages = n;
        self
    }
}

impl Default for BrowserScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl View for BrowserScreen {
    fn title(&self) -> String {
        "Browse S3".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        // The header bar names ctx.session. When this browser is on another account (opened
        // from a pushed accounts screen), say which one here.
        let account = self
            .session
            .as_ref()
            .filter(|mine| {
                ctx.session.as_ref().is_none_or(|shared| {
                    shared.profile.name != mine.profile.name || shared.region != mine.region
                })
            })
            .map(|s| format!("   as {} ({})", s.profile.name, s.region));
        match self.search.as_mut() {
            Some(search) => search.render(frame, area, account),
            None => self.render_folder(frame, area, account),
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.search.is_some() {
            self.search_key(key, ctx)
        } else {
            self.folder_key(key, ctx)
        }
    }

    fn on_done(&mut self, done: &Done, ctx: &mut Ctx) -> Transition {
        if Some(done.id) == self.list_job {
            self.listing_done(&done.result, ctx);
        } else if let Some(i) = self.peeks.remove(&done.id) {
            if let Some(obj) = self.objects.get_mut(i) {
                obj.mark = match &done.result {
                    Ok(Outcome::Data(bytes)) if mail::looks_like_email(bytes) => Mark::Email,
                    // Never ran; leave it for the next time it is on screen.
                    Ok(Outcome::Skipped) => Mark::Unchecked,
                    Ok(_) => Mark::NotEmail,
                    Err(_) => Mark::Failed,
                };
            }
        } else if let Some(search) = self.search.as_mut() {
            search.on_done(done, ctx);
        }
        Transition::None
    }

    fn on_focus(&mut self, ctx: &mut Ctx) {
        if self.session.is_none() {
            self.session = ctx.session.clone();
        }
        if !self.started {
            self.started = true;
            self.go(Location::Buckets, ctx);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if let Some(search) = &self.search {
            let mut h = vec![("enter", "go there"), ("i", "save inbox")];
            if search.running() {
                h.push(("x", "stop"));
            }
            h.push(("esc", "close"));
            return h;
        }
        if self.editing_filter {
            return vec![("enter", "keep filter"), ("esc", "clear filter")];
        }
        match self.location {
            Location::Buckets => vec![
                ("enter", "open"),
                ("/", "filter"),
                ("r", "reload"),
                ("esc", "back"),
            ],
            Location::Folder { .. } => vec![
                ("enter", "open"),
                ("bksp", "up"),
                ("/", "filter"),
                ("s", "search"),
                ("i", "inbox"),
                ("esc", "up"),
            ],
        }
    }
}

// ---- search: every object below a folder, grouped by the folder holding it ----

struct Search {
    bucket: String,
    prefix: String,
    list_job: Option<JobId>,
    listing_done: bool,
    queue: VecDeque<String>,
    in_flight: HashMap<JobId, String>,
    listed: usize,
    checked: usize,
    emails: usize,
    folders: BTreeMap<String, usize>,
    stopped: bool,
    error: Option<String>,
    /// Why the listing ended early, when it did.
    note: Option<String>,
    paging: Paging,
    session: Option<Session>,
    /// Bumped on stop, so the peeks still queued are skipped rather than run.
    generation: Generation,
    selected: usize,
    offset: usize,
    visible: usize,
}

impl Search {
    fn new(bucket: String, prefix: String, max_pages: usize, session: Option<Session>) -> Self {
        Self {
            session,
            generation: Generation::new(),
            note: None,
            paging: Paging::new(max_pages),
            bucket,
            prefix,
            list_job: None,
            listing_done: false,
            queue: VecDeque::new(),
            in_flight: HashMap::new(),
            listed: 0,
            checked: 0,
            emails: 0,
            folders: BTreeMap::new(),
            stopped: false,
            error: None,
            selected: 0,
            offset: 0,
            visible: 20,
        }
    }

    fn running(&self) -> bool {
        !self.stopped && !(self.listing_done && self.queue.is_empty() && self.in_flight.is_empty())
    }

    fn list(&mut self, token: Option<String>, ctx: &mut Ctx) {
        let job = Job::List {
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            delimiter: false,
            token,
        };
        self.list_job = submit(ctx, self.session.as_ref(), &self.generation, job);
        if self.list_job.is_none() {
            self.fail("no account is connected".into(), ctx);
        }
    }

    fn fail(&mut self, msg: String, ctx: &mut Ctx) {
        self.error = Some(msg.clone());
        ctx.error(msg);
        self.stop();
    }

    fn stop(&mut self) {
        self.generation.bump();
        self.stopped = true;
        self.list_job = None;
        self.queue.clear();
        self.in_flight.clear();
    }

    fn fill(&mut self, ctx: &mut Ctx) {
        while self.in_flight.len() < SEARCH_IN_FLIGHT
            && let Some(key) = self.queue.pop_front()
        {
            let job = Job::Peek {
                bucket: self.bucket.clone(),
                key: key.clone(),
                bytes: PEEK_BYTES,
            };
            match submit(ctx, self.session.as_ref(), &self.generation, job) {
                Some(id) => {
                    self.in_flight.insert(id, key);
                }
                None => {
                    self.fail("no account is connected".into(), ctx);
                    return;
                }
            }
        }
    }

    fn on_done(&mut self, done: &Done, ctx: &mut Ctx) {
        if self.stopped {
            return;
        }
        if Some(done.id) == self.list_job {
            self.list_job = None;
            match &done.result {
                Ok(Outcome::Listing(listing)) => {
                    self.listed += listing.objects.len();
                    for o in &listing.objects {
                        if o.size == 0 {
                            self.checked += 1;
                        } else {
                            self.queue.push_back(o.key.clone());
                        }
                    }
                    let what = format!("the search of {}/{}", self.bucket, self.prefix);
                    match self.paging.next(listing.next_token.as_ref(), &what) {
                        Ok(Some(t)) => self.list(Some(t), ctx),
                        Ok(None) => self.listing_done = true,
                        Err(msg) => {
                            // Keep checking what did arrive, and say why it is not everything.
                            self.listing_done = true;
                            ctx.error(msg.clone());
                            self.note = Some(msg);
                        }
                    }
                }
                Ok(_) => self.listing_done = true,
                Err(e) => {
                    let msg = format!("search of {}/{} failed: {e}", self.bucket, self.prefix);
                    self.fail(msg, ctx);
                    return;
                }
            }
        } else if let Some(key) = self.in_flight.remove(&done.id) {
            self.checked += 1;
            if let Ok(Outcome::Data(bytes)) = &done.result
                && mail::looks_like_email(bytes)
            {
                self.emails += 1;
                let folder = match key.rfind('/') {
                    Some(i) => key[..=i].to_string(),
                    None => String::new(),
                };
                *self.folders.entry(folder).or_insert(0) += 1;
            }
        } else {
            return;
        }
        self.fill(ctx);
    }

    fn selected_folder(&self) -> Option<String> {
        self.folders.keys().nth(self.selected).cloned()
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.folders.len();
        if n == 0 {
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(n - 1);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, account: Option<String>) {
        let [head, body] = Layout::vertical([
            Constraint::Length(HEADER_LINES + u16::from(self.note.is_some())),
            Constraint::Min(1),
        ])
        .areas(area);
        let dim = Style::default().fg(Color::DarkGray);
        let state = if let Some(e) = &self.error {
            Span::styled(format!("failed: {e}"), Style::default().fg(Color::Red))
        } else if self.stopped {
            Span::styled("stopped", Style::default().fg(Color::Yellow))
        } else if self.running() {
            Span::styled("searching...", Style::default().fg(Color::Cyan))
        } else if self.note.is_some() {
            Span::styled(
                "done, but only partly (see below)".to_string(),
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled("done", Style::default().fg(Color::Green))
        };
        let listed = if self.listing_done {
            format!("{}", self.listed)
        } else {
            format!("{}+", self.listed)
        };
        frame.render_widget(
            Paragraph::new(
                vec![
                    Line::from(
                        [Span::styled(
                            format!(" Email under {}/{}", self.bucket, self.prefix),
                            Style::default().add_modifier(Modifier::BOLD),
                        )]
                        .into_iter()
                        .chain(account.map(|a| Span::styled(a, dim)))
                        .collect::<Vec<_>>(),
                    ),
                    Line::from(vec![
                        Span::styled(
                            format!(
                                " checked {} of {listed} objects · {} in {} · ",
                                self.checked,
                                plural(self.emails, "email"),
                                plural(self.folders.len(), "folder"),
                            ),
                            dim,
                        ),
                        state,
                    ]),
                ]
                .into_iter()
                .chain(
                    self.note
                        .iter()
                        .map(|n| Line::styled(format!(" {n}"), Style::default().fg(Color::Yellow))),
                )
                .collect::<Vec<_>>(),
            ),
            head,
        );

        if self.folders.is_empty() {
            let msg = if self.running() {
                " No email found yet..."
            } else {
                " No email found under this folder."
            };
            frame.render_widget(Paragraph::new(msg).style(dim), body);
            return;
        }

        self.visible = body.height as usize;
        let visible = self.visible.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
        let cols = body.width as usize;
        let rows: Vec<(String, usize)> = self
            .folders
            .iter()
            .skip(self.offset)
            .take(visible)
            .map(|(f, n)| (format!("{}/{f}", self.bucket), *n))
            .collect();
        let name_w = rows
            .iter()
            .map(|(f, _)| width(f))
            .max()
            .unwrap_or(0)
            .min(cols.saturating_sub(16).max(10));
        let items: Vec<ListItem> = rows
            .iter()
            .map(|(f, n)| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{}  ", fit(f, name_w)),
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(plural(*n, "email"), Style::default().fg(Color::Green)),
                ]))
            })
            .collect();
        let mut list_state = ListState::default().with_selected(Some(self.selected - self.offset));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_spacing(ratatui::widgets::HighlightSpacing::Always)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            body,
            &mut list_state,
        );
    }
}

fn same_row(a: Row, b: Row) -> bool {
    matches!(
        (a, b),
        (Row::Bucket(x), Row::Bucket(y)) | (Row::Folder(x), Row::Folder(y)) | (Row::Object(x), Row::Object(y))
            if x == y
    )
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// Queue a job on a session the screen holds, stamped with its generation. None when the
/// screen has no session to talk to.
fn submit(
    ctx: &mut Ctx,
    session: Option<&Session>,
    generation: &Generation,
    job: Job,
) -> Option<JobId> {
    Some(ctx.submit_to(session?, job, Some(generation)))
}

#[cfg(test)]
mod tests;
