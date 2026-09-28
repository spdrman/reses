//! Browse buckets and folders, find stored email, save a folder as the inbox.
//!
//! SES drops raw mail into S3 wherever its receipt rule says, and people rarely remember the
//! exact prefix, so this screen lets them walk the account like a file manager until they find
//! it. I list one level at a time with a delimiter and peek the first few KB of each object on
//! screen to mark which ones look like email. `Ctrl+F` on Linux and `Cmd+F` on macOS run a flat
//! listing of everything below a folder and group the email it finds by folder. `i`
//! saves the current (or found) folder as the inbox and opens it. Every listing is paged with
//! a guard against a server that never stops handing out tokens, and every job is stamped with
//! a generation so work for a folder I've left gets skipped instead of run.

use std::collections::{BTreeMap, HashMap, VecDeque};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::inbox::InboxScreen;
use super::jobs::{Done, Generation, Job, JobId, Outcome};
use super::text::{SIZE_WIDTH, escape, fit, human_size, width};
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
#[cfg(target_os = "macos")]
const SEARCH_KEY: &str = "cmd-f";
#[cfg(not(target_os = "macos"))]
const SEARCH_KEY: &str = "ctrl-f";

#[cfg(target_os = "macos")]
fn search_modifier() -> KeyModifiers {
    KeyModifiers::SUPER
}

#[cfg(not(target_os = "macos"))]
fn search_modifier() -> KeyModifiers {
    KeyModifiers::CONTROL
}

/// Guards a run of continuation tokens against a server that never stops handing them out.
struct Paging {
    pages: usize,
    last: Option<String>,
    max: usize,
}

impl Paging {
    /// I start a fresh run with no pages seen and `max` as the cap.
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
    /// Where the first listing goes: the bucket list, or a folder when the inbox's Esc opens me (#67).
    start: Location,
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
    /// I build a browser at the bucket list with nothing loaded yet. It fetches on first focus,
    /// and borrows ctx.session then if it wasn't given one.
    pub fn new() -> Self {
        Self {
            started: false,
            start: Location::Buckets,
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

    /// I move to `location`: bump the generation so peeks for the old place are skipped, clear
    /// everything I knew about it, and submit the first listing for the new one.
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

    /// I go up a level, from a folder to its parent or from a bucket root to the bucket list,
    /// and remember where I came from so it's selected once the listing shows it.
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

    /// I open the selected bucket or folder. Objects don't open from here; the inbox does that.
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

    /// I return the folder prefix I'm in, or an empty one on the bucket list.
    fn prefix(&self) -> &str {
        match &self.location {
            Location::Folder { prefix, .. } => prefix,
            Location::Buckets => "",
        }
    }

    /// I give the name a row shows, with the current prefix taken off folders and objects.
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

    /// I rebuild the filtered rows from buckets, folders and objects, then put the cursor back on
    /// the row I came out of, or keep it on the same row while more pages land.
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

    /// I keep the selection inside the rows and scroll the window just enough to show it.
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

    /// I move the selection by `delta` rows, clamped at both ends.
    fn move_by(&mut self, delta: isize) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(n - 1);
        self.clamp();
    }

    /// I save this folder as the inbox in the config, and reset the stack to an inbox on this
    /// browser's session. If the config won't save I put the old inbox back and stay here.
    fn save_inbox(&mut self, bucket: String, prefix: String, ctx: &mut Ctx) -> Transition {
        let Some(session) = self.session.clone() else {
            ctx.error("no account is connected");
            return Transition::None;
        };
        // The bucket's own region when a redirect taught the client one, so the next start
        // goes straight there; otherwise the region we have been talking to it in.
        let region = session
            .store
            .bucket_region(&bucket)
            .unwrap_or_else(|| session.region.clone());
        let inbox = Inbox {
            profile: session.profile.name.clone(),
            bucket,
            prefix,
            region: Some(region),
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

    /// I start a search below the current folder. On the bucket list there's nothing to search,
    /// so I say so instead.
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

    /// I handle a key while the filter is being typed. I return false for keys the filter
    /// doesn't want, so the caller can treat them as navigation.
    fn filter_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            // A ctrl or alt chord is a command, not text for the filter.
            KeyCode::Char(_)
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                return true;
            }
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

    /// I handle a key in the folder view: the filter first when it's being typed, then movement,
    /// opening, going up, reload, search and saving the inbox. Afterwards I peek whatever is
    /// now on screen.
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
            // Shift+arrows page, since a MacBook has no Page Up or Page Down key. They come first, or
            // the bare arrow arms would move a row at a time.
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => self.move_by(page),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => self.move_by(-page),
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
            KeyCode::Char('f') if key.modifiers.contains(search_modifier()) => {
                self.start_search(ctx)
            }
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

    /// I handle a key while search results are up: move, stop, close, jump to a folder, or save
    /// one as the inbox.
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
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                search.move_by(search.visible.max(1) as isize)
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                search.move_by(-(search.visible.max(1) as isize))
            }
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

    /// I take in a finished listing. Buckets replace the list; a folder page is appended and I
    /// follow its continuation token until the pager says stop; an error goes on screen and the
    /// status line. Then I rebuild the rows and peek what became visible.
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

    /// I spell out where I am for the header and for error messages.
    fn path(&self) -> String {
        match &self.location {
            Location::Buckets => "buckets".into(),
            Location::Folder { bucket, prefix } => format!("{bucket}/{prefix}"),
        }
    }

    /// I build the counts line: buckets, or folders, objects and emails, noting how many
    /// objects are still unchecked and whether a listing is still loading.
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

    /// I draw the folder view: the path and counts on top, then either a message saying why
    /// there are no rows or the visible window of rows with sizes and email marks.
    fn render_folder(&mut self, frame: &mut Frame, area: Rect) {
        let [head, body] =
            Layout::vertical([Constraint::Length(HEADER_LINES), Constraint::Min(1)]).areas(area);
        // DIM and BOLD rather than grey, yellow, green or blue, which vanished on one theme or
        // another (dark grey was 1.0:1 on Solarized Dark).
        let dim = Style::default().add_modifier(Modifier::DIM);
        let path = match &self.location {
            Location::Buckets => Span::styled(
                " All buckets",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Location::Folder { .. } => Span::styled(
                format!(" {}", escape(&self.path())),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        };
        let mut second = vec![Span::styled(format!(" {}", self.summary()), dim)];
        if self.editing_filter || !self.filter.is_empty() {
            second.push(Span::styled(
                format!(
                    "   /{}{}",
                    escape(&self.filter),
                    if self.editing_filter { "_" } else { "" }
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(
            Paragraph::new(vec![Line::from(path), Line::from(second)]),
            head,
        );

        self.visible = body.height as usize;
        self.clamp();

        if self.rows.is_empty() {
            let (msg, style) = if let Some(e) = &self.list_error {
                (format!(" {}", escape(e)), Style::default().fg(Color::Red))
            } else if self.loading {
                (" Loading...".to_string(), dim)
            } else if !self.filter.is_empty() {
                (
                    format!(
                        " nothing matches /{} (esc clears the filter)",
                        escape(&self.filter)
                    ),
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
            .map(|&r| width(&escape(self.row_name(r))))
            .max()
            .unwrap_or(0)
            .min(cols.saturating_sub(22).max(10));
        let items: Vec<ListItem> = window
            .iter()
            .map(|&row| {
                // Cut and padded by display width, so wide characters keep the columns lined up.
                let name = fit(&escape(self.row_name(row)), name_w);
                match row {
                    Row::Bucket(_) => ListItem::new(Line::styled(
                        name,
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Row::Folder(_) => ListItem::new(Line::styled(
                        name,
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Row::Object(i) => {
                        let obj = &self.objects[i];
                        let (mark, style) = match obj.mark {
                            Mark::Email => ("email", Style::default().add_modifier(Modifier::BOLD)),
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

    /// A browser on `session` that opens in `bucket` at `prefix`: where the inbox's mail is, when
    /// Esc leaves the inbox. Going up from there climbs to the bucket list as usual (#67).
    pub fn at_folder(session: Session, bucket: String, prefix: String) -> Self {
        Self {
            start: Location::Folder { bucket, prefix },
            ..Self::with_session(session)
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
    /// I start at the bucket list, like `new`.
    fn default() -> Self {
        Self::new()
    }
}

impl View for BrowserScreen {
    /// I use one title for every level of the browser.
    fn title(&self) -> String {
        "Browse S3".into()
    }

    /// I draw the search results while a search is open and the folder view otherwise.
    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        match self.search.as_mut() {
            Some(search) => search.render(frame, area),
            None => self.render_folder(frame, area),
        }
    }

    /// The header bar names this browser's own account, which may not be ctx.session's.
    fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// I route a key to the search while one is open and to the folder view otherwise.
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.search.is_some() {
            self.search_key(key, ctx)
        } else {
            self.folder_key(key, ctx)
        }
    }

    /// A paste goes into the filter while it's being typed, flattened to one line.
    fn on_paste(&mut self, text: &str, _ctx: &mut Ctx) -> Transition {
        if self.search.is_none() && self.editing_filter {
            self.filter.extend(text.chars().filter(|c| !c.is_control()));
            self.selected = 0;
            self.offset = 0;
            self.rebuild_rows();
        }
        Transition::None
    }

    /// I match a finished job to what asked for it: the listing, a peek for this folder (which
    /// sets the object's mark), or the open search. Anything else is stale and I drop it.
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

    /// I pick up ctx.session if I don't have one yet and start at the bucket list the first
    /// time I'm shown.
    fn on_focus(&mut self, ctx: &mut Ctx) {
        if self.session.is_none() {
            self.session = ctx.session.clone();
        }
        if !self.started {
            self.started = true;
            self.go(self.start.clone(), ctx);
        }
    }

    /// I show the keys for the mode I'm in: search results, filter typing, the bucket list or
    /// a folder.
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
            // Paging comes last here: at 80 columns, with `?` pinned, it's the one that gives way,
            // not saving the folder as the inbox.
            Location::Folder { .. } => vec![
                ("enter", "open"),
                ("bksp", "up"),
                ("/", "filter"),
                (SEARCH_KEY, "search"),
                ("i", "inbox"),
                ("esc", "up"),
                ("⇧↑↓", "page"),
            ],
        }
    }

    /// Every key the browser takes, for the `?` overlay.
    fn help(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓  j k", "move a row"),
            ("⇧↑↓  pgup pgdn", "move a page"),
            ("g G  home end", "first or last row"),
            ("enter  →  l", "open the bucket or folder"),
            ("bksp  ←  h", "up a folder"),
            ("/", "filter the rows, esc clears it"),
            (SEARCH_KEY, "search every folder below this one for email"),
            ("i", "save this folder as the inbox and open it"),
            ("r", "reload"),
            (
                "esc",
                "up a level; from the bucket list, back to the accounts",
            ),
        ]
    }

    /// While the filter prompt is open, a `?` is part of the filter.
    fn taking_text(&self) -> bool {
        self.editing_filter
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
    /// I set up a search of everything under `bucket/prefix`, paged no further than `max_pages`.
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

    /// I say whether the search still has work to do: not stopped, and not yet finished
    /// listing and checking.
    fn running(&self) -> bool {
        !self.stopped && !(self.listing_done && self.queue.is_empty() && self.in_flight.is_empty())
    }

    /// I submit the next page of the flat listing (no delimiter, so every object below the
    /// prefix comes back).
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

    /// I record why the search failed, put it on the status line, and stop.
    fn fail(&mut self, msg: String, ctx: &mut Ctx) {
        self.error = Some(msg.clone());
        ctx.error(msg);
        self.stop();
    }

    /// I stop the search: bump the generation so queued peeks are skipped, and forget
    /// everything still waiting.
    fn stop(&mut self) {
        self.generation.bump();
        self.stopped = true;
        self.list_job = None;
        self.queue.clear();
        self.in_flight.clear();
    }

    /// I top up the peeks in flight to `SEARCH_IN_FLIGHT`, so the queue drains without flooding
    /// the pool and a stop leaves little behind.
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

    /// I take in a finished search job. A listing page queues its objects for peeking and
    /// follows the next token; a peek that looks like email counts toward its folder. Then I
    /// refill the in-flight peeks.
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

    /// I return the folder under the cursor in the search results.
    fn selected_folder(&self) -> Option<String> {
        self.folders.keys().nth(self.selected).cloned()
    }

    /// I move the search cursor by `delta`, clamped to the folders found.
    fn move_by(&mut self, delta: isize) {
        let n = self.folders.len();
        if n == 0 {
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(n - 1);
    }

    /// I draw the search: what's being searched, how far it has got, why it stopped early if
    /// it did, and then the folders holding email with their counts.
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let [head, body] = Layout::vertical([
            Constraint::Length(HEADER_LINES + u16::from(self.note.is_some())),
            Constraint::Min(1),
        ])
        .areas(area);
        // DIM and BOLD rather than grey, yellow, green or blue, which vanished on one theme or
        // another (dark grey was 1.0:1 on Solarized Dark).
        let dim = Style::default().add_modifier(Modifier::DIM);
        let state = if let Some(e) = &self.error {
            Span::styled(
                format!("failed: {}", escape(e)),
                Style::default().fg(Color::Red),
            )
        } else if self.stopped {
            Span::styled("stopped", Style::default().add_modifier(Modifier::BOLD))
        } else if self.running() {
            Span::raw("searching...")
        } else if self.note.is_some() {
            Span::styled(
                "done, but only partly (see below)".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("done")
        };
        let listed = if self.listing_done {
            format!("{}", self.listed)
        } else {
            format!("{}+", self.listed)
        };
        frame.render_widget(
            Paragraph::new(
                vec![
                    Line::from(Span::styled(
                        format!(
                            " Email under {}",
                            escape(&format!("{}/{}", self.bucket, self.prefix))
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
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
                .chain(self.note.iter().map(|n| {
                    Line::styled(
                        format!(" {}", escape(n)),
                        Style::default().add_modifier(Modifier::BOLD),
                    )
                }))
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
            .map(|(f, n)| (escape(&format!("{}/{f}", self.bucket)).into_owned(), *n))
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
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(plural(*n, "email")),
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

/// I compare two rows by kind and index, since `Row` doesn't derive `PartialEq`.
fn same_row(a: Row, b: Row) -> bool {
    matches!(
        (a, b),
        (Row::Bucket(x), Row::Bucket(y)) | (Row::Folder(x), Row::Folder(y)) | (Row::Object(x), Row::Object(y))
            if x == y
    )
}

/// I spell a count with its noun, adding an s unless there's exactly one.
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
