//! One decoded message, fetched from S3: scroll it, flip to the HTML part, save its text or
//! its attachments, or delete it.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Paragraph, Wrap};

use super::inbox::{display_from, render_confirm};
use super::jobs::{Done, Job, JobId, Outcome};
use super::{Ctx, Transition, View};
use crate::mail;

pub struct MessageScreen {
    pub bucket: String,
    pub key: String,
    subject: Option<String>,
    from: String,
    out_dir: PathBuf,
    started: bool,
    fetch: Option<JobId>,
    delete: Option<JobId>,
    raw: Option<Vec<u8>>,
    text: String,
    html: bool,
    error: Option<String>,
    confirm: bool,
    /// `text` wrapped to `wrapped_for` columns; rebuilt when either changes.
    wrapped: Vec<String>,
    wrapped_for: Option<u16>,
    offset: usize,
    page: usize,
    max_offset: usize,
}

impl MessageScreen {
    pub fn new(bucket: String, key: String) -> Self {
        Self {
            bucket,
            key,
            subject: None,
            from: String::new(),
            out_dir: PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads"),
            started: false,
            fetch: None,
            delete: None,
            raw: None,
            text: String::new(),
            html: false,
            error: None,
            confirm: false,
            wrapped: Vec::new(),
            wrapped_for: None,
            offset: 0,
            page: 1,
            max_offset: 0,
        }
    }

    /// The subject the inbox already knows, for the delete prompt before the body arrives.
    pub fn with_subject(mut self, subject: String) -> Self {
        self.subject = Some(subject);
        self
    }

    /// Where `w` and `a` write (default `~/Downloads`).
    pub fn with_out_dir(mut self, dir: PathBuf) -> Self {
        self.out_dir = dir;
        self
    }

    fn location(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key)
    }

    fn set_text(&mut self) {
        if let Some(raw) = &self.raw {
            self.text = mail::format_message(raw, self.html);
            self.wrapped_for = None;
        }
    }

    fn scroll_to(&mut self, offset: usize) {
        self.offset = offset.min(self.max_offset);
    }

    fn write_text(&mut self, ctx: &mut Ctx) {
        if self.raw.is_none() {
            ctx.info("The message is still loading.");
            return;
        }
        let stem = file_stem(&self.key);
        match write_new(&self.out_dir, &stem, "txt", self.text.as_bytes()) {
            Ok(path) => ctx.info(format!("Wrote {}", path.display())),
            Err(e) => ctx.error(format!(
                "Could not write into {}: {e}",
                self.out_dir.display()
            )),
        }
    }

    fn save_attachments(&mut self, ctx: &mut Ctx) {
        let Some(raw) = &self.raw else {
            ctx.info("The message is still loading.");
            return;
        };
        match mail::save_attachments(raw, &self.out_dir) {
            Ok(paths) if paths.is_empty() => ctx.info("This message has no attachments."),
            Ok(paths) => {
                let noun = if paths.len() == 1 {
                    "attachment"
                } else {
                    "attachments"
                };
                ctx.info(format!(
                    "Saved {} {noun} to {}",
                    paths.len(),
                    self.out_dir.display()
                ));
            }
            Err(e) => ctx.error(format!(
                "Could not save attachments into {}: {e}",
                self.out_dir.display()
            )),
        }
    }
}

impl View for MessageScreen {
    fn title(&self) -> String {
        let view = if self.html { " (HTML)" } else { "" };
        format!("Message {}{view}", self.location())
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(format!(" {err}"))
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false }),
                area,
            );
        } else if self.raw.is_none() {
            frame.render_widget(
                Paragraph::new(format!(" Fetching {} …", self.location())),
                area,
            );
        } else {
            if self.wrapped_for != Some(area.width) {
                self.wrapped = wrap(&self.text, area.width as usize);
                self.wrapped_for = Some(area.width);
            }
            self.page = (area.height as usize).max(1);
            self.max_offset = self.wrapped.len().saturating_sub(self.page);
            self.offset = self.offset.min(self.max_offset);
            let shown: Vec<_> = self
                .wrapped
                .iter()
                .skip(self.offset)
                .take(self.page)
                .map(|l| ratatui::text::Line::raw(l.as_str()))
                .collect();
            frame.render_widget(Paragraph::new(shown), area);
        }

        if self.confirm {
            let subject = self
                .subject
                .clone()
                .unwrap_or_else(|| "(no subject)".into());
            render_confirm(frame, area, &subject, &self.from, &self.location());
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.confirm {
            self.confirm = false;
            if key.code == KeyCode::Char('y') {
                let job = Job::Delete {
                    bucket: self.bucket.clone(),
                    key: self.key.clone(),
                };
                match ctx.submit(job) {
                    Some(id) => self.delete = Some(id),
                    None => ctx.error("Not connected to an account."),
                }
            } else {
                ctx.info("Delete cancelled.");
            }
            return Transition::None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_to(self.offset.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_to(self.offset + 1),
            KeyCode::PageUp => self.scroll_to(self.offset.saturating_sub(self.page)),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_to(self.offset + self.page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_to(0),
            KeyCode::End | KeyCode::Char('G') => self.scroll_to(usize::MAX),
            KeyCode::Char('h') if self.raw.is_some() => {
                self.html = !self.html;
                self.set_text();
                self.offset = 0;
                ctx.info(if self.html {
                    "Showing the HTML part."
                } else {
                    "Showing the text part."
                });
            }
            KeyCode::Char('w') => self.write_text(ctx),
            KeyCode::Char('a') => self.save_attachments(ctx),
            KeyCode::Char('d') => self.confirm = true,
            KeyCode::Esc | KeyCode::Char('q') => return Transition::Pop,
            _ => {}
        }
        Transition::None
    }

    fn on_done(&mut self, done: &Done, ctx: &mut Ctx) -> Transition {
        if Some(done.id) == self.fetch {
            self.fetch = None;
            match &done.result {
                Ok(Outcome::Data(raw)) => {
                    let summary = mail::summarize(raw);
                    if !summary.subject.trim().is_empty() {
                        self.subject = Some(summary.subject.trim().to_string());
                    }
                    self.from = display_from(&summary.from);
                    self.raw = Some(raw.clone());
                    self.set_text();
                }
                Ok(_) => self.error = Some(format!("Unexpected reply for {}", self.location())),
                Err(e) if e.is_not_found() => {
                    self.error = Some(format!(
                        "This message no longer exists: {}",
                        self.location()
                    ))
                }
                Err(e) => self.error = Some(format!("Could not fetch {}: {e}", self.location())),
            }
        } else if Some(done.id) == self.delete {
            self.delete = None;
            match &done.result {
                Ok(_) => {
                    ctx.info(format!("Deleted {}", self.location()));
                    return Transition::Pop;
                }
                Err(e) => ctx.error(format!("Could not delete {}: {e}", self.location())),
            }
        }
        Transition::None
    }

    fn on_focus(&mut self, ctx: &mut Ctx) {
        if self.started {
            return;
        }
        self.started = true;
        let job = Job::Get {
            bucket: self.bucket.clone(),
            key: self.key.clone(),
        };
        match ctx.submit(job) {
            Some(id) => self.fetch = Some(id),
            None => self.error = Some("Not connected to an account.".into()),
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓ pgup pgdn", "scroll"),
            ("h", if self.html { "text" } else { "html" }),
            ("w", "save text"),
            ("a", "save attachments"),
            ("d", "delete"),
            ("q", "back"),
        ]
    }
}

/// The last path segment of a key, safe as a file name.
fn file_stem(key: &str) -> String {
    let base = key.rsplit('/').next().unwrap_or("");
    let safe: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_start_matches('.');
    if safe.is_empty() {
        "message".into()
    } else {
        safe.to_string()
    }
}

/// Write `data` to `dir/stem.ext`, or `stem-1.ext` and so on, never replacing a file.
fn write_new(dir: &Path, stem: &str, ext: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for n in 0u32.. {
        let name = if n == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(data)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("ran out of file names")
}

fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// Wrap each line to `width` columns, breaking after a space where there is one.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.replace('\t', "    ");
        let line: String = line
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut cur = String::new();
        let mut used = 0;
        for c in line.chars() {
            let w = char_width(c);
            if used + w > width && !cur.is_empty() {
                // Carry the unfinished word over when the line has a space to break at.
                let carry = match cur.rfind(' ') {
                    Some(i) if i + 1 < cur.len() => cur.split_off(i + 1),
                    _ => String::new(),
                };
                out.push(cur.trim_end().to_string());
                used = carry.chars().map(char_width).sum();
                cur = carry;
            }
            cur.push(c);
            used += w;
        }
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use ratatui::crossterm::event::KeyCode;

    use super::*;
    use crate::s3::{MemoryStore, Store};
    use crate::tui::App;
    use crate::tui::inbox::InboxScreen;
    use crate::tui::inbox::fixtures::*;
    use crate::tui::testing::{self, key, screen, settle};

    const KEY: &str = "mail/msg1";

    fn long_message() -> Vec<u8> {
        let mut raw = String::from_utf8(email(
            "Alice <alice@example.com>",
            "Quarterly report",
            "Fri, 25 Sep 2026 09:30:00 +0000",
        ))
        .unwrap();
        for i in 0..100 {
            raw.push_str(&format!("report line {i:03}\r\n"));
        }
        raw.into_bytes()
    }

    fn multipart() -> Vec<u8> {
        b"From: Alice <alice@example.com>\r\n\
To: me@example.com\r\n\
Subject: Two views\r\n\
Date: Fri, 25 Sep 2026 09:30:00 +0000\r\n\
Message-ID: <two@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/alternative; boundary=\"alt\"\r\n\
\r\n\
--alt\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
the plain version\r\n\
--alt\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>the <b>html</b> version</p>\r\n\
--alt--\r\n\
--outer\r\n\
Content-Type: text/plain; name=\"note.txt\"\r\n\
Content-Disposition: attachment; filename=\"note.txt\"\r\n\
\r\n\
attached words\r\n\
--outer--\r\n"
            .to_vec()
    }

    fn open(store: Arc<dyn Store>, out: &Path) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(store));
        let view = MessageScreen::new(BUCKET.into(), KEY.into()).with_out_dir(out.to_path_buf());
        let mut app = App::with_view(ctx, Box::new(view));
        settle(&mut app);
        (app, dir)
    }

    fn store_with(raw: &[u8]) -> Arc<MemoryStore> {
        let s = Arc::new(MemoryStore::new());
        s.put(BUCKET, KEY, raw);
        s
    }

    #[test]
    fn shows_the_decoded_message_and_scrolls() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("Subject: Quarterly report"), "{scr}");
        assert!(scr.contains("From: Alice <alice@example.com>"), "{scr}");
        assert!(!scr.contains("report line 099"), "{scr}");

        app.key(key(KeyCode::End));
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("report line 099"), "{scr}");
        assert!(!scr.contains("Subject: Quarterly report"), "{scr}");

        app.key(key(KeyCode::Home));
        assert!(screen(&mut app, 80, 20).contains("Subject: Quarterly report"));

        app.key(key(KeyCode::PageDown));
        let scr = screen(&mut app, 80, 20);
        assert!(!scr.contains("Subject: Quarterly report"), "{scr}");
        app.key(key(KeyCode::PageUp));
        assert!(screen(&mut app, 80, 20).contains("Subject: Quarterly report"));

        // Down moves one line at a time: the first line scrolls off, the second stays.
        let first = screen(&mut app, 80, 20).lines().nth(1).unwrap().to_string();
        app.key(key(KeyCode::Down));
        let scr = screen(&mut app, 80, 20);
        assert!(!scr.lines().nth(1).unwrap().contains(&first), "{scr}");
        app.key(key(KeyCode::Up));
        assert_eq!(screen(&mut app, 80, 20).lines().nth(1).unwrap(), first);
    }

    #[test]
    fn scrolling_stops_at_the_ends() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        screen(&mut app, 80, 20);
        app.key(key(KeyCode::Up));
        assert!(screen(&mut app, 80, 20).contains("Subject: Quarterly report"));
        app.key(key(KeyCode::End));
        for _ in 0..5 {
            app.key(key(KeyCode::Down));
            app.key(key(KeyCode::PageDown));
        }
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("report line 099"), "{scr}");
        // The page is still full rather than scrolled past the last line.
        assert!(scr.contains("report line 085"), "{scr}");
    }

    #[test]
    fn long_lines_wrap_to_the_width() {
        let raw = email("a@example.com", "Wide", "Fri, 25 Sep 2026 09:30:00 +0000");
        let mut raw = String::from_utf8(raw).unwrap();
        raw.push_str(&format!("{}END\r\n", "word ".repeat(40)));
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(raw.as_bytes()), out.path());
        let scr = screen(&mut app, 60, 20);
        assert!(scr.contains("END"), "{scr}");
    }

    #[test]
    fn h_toggles_the_html_view() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("the plain version"), "{scr}");
        app.key(key(KeyCode::Char('h')));
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("<b>html</b>"), "{scr}");
        assert!(!scr.contains("the plain version"), "{scr}");
        app.key(key(KeyCode::Char('h')));
        assert!(screen(&mut app, 80, 20).contains("the plain version"));
    }

    #[test]
    fn w_writes_the_decoded_text_and_says_where() {
        let out = tempfile::tempdir().unwrap();
        let raw = long_message();
        let (mut app, _d) = open(store_with(&raw), out.path());
        app.key(key(KeyCode::Char('w')));
        let written = out.path().join("msg1.txt");
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            crate::mail::format_message(&raw, false)
        );
        let scr = screen(&mut app, 200, 20);
        assert!(
            scr.lines()
                .last()
                .unwrap()
                .contains(&written.display().to_string()),
            "{scr}"
        );
        // A second write never overwrites the first.
        app.key(key(KeyCode::Char('w')));
        assert!(out.path().join("msg1-1.txt").exists());
    }

    #[test]
    fn a_saves_the_attachments_and_says_where() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        app.key(key(KeyCode::Char('a')));
        let saved = out.path().join("note.txt");
        assert_eq!(
            std::fs::read_to_string(&saved).unwrap().trim_end(),
            "attached words"
        );
        let scr = screen(&mut app, 200, 20);
        let status = scr.lines().last().unwrap();
        assert!(status.contains("1 attachment"), "{scr}");
        assert!(status.contains(&out.path().display().to_string()), "{scr}");
    }

    #[test]
    fn a_with_no_attachments_says_so() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        app.key(key(KeyCode::Char('a')));
        let scr = screen(&mut app, 120, 20);
        assert!(
            scr.lines().last().unwrap().contains("no attachments"),
            "{scr}"
        );
    }

    #[test]
    fn delete_confirms_with_subject_and_key_and_only_y_deletes() {
        let out = tempfile::tempdir().unwrap();
        let store = store_with(&long_message());
        let (mut app, _d) = open(store.clone(), out.path());
        app.key(key(KeyCode::Char('d')));
        let scr = screen(&mut app, 100, 20);
        assert!(scr.contains("Quarterly report"), "{scr}");
        assert!(scr.contains("s3://inbox-bucket/mail/msg1"), "{scr}");
        assert!(scr.contains("y to delete"), "{scr}");
        app.key(key(KeyCode::Char('n')));
        settle(&mut app);
        assert!(store.contains(BUCKET, KEY));
        assert!(app.stack.last().unwrap().title().contains("Message"));
        assert!(!screen(&mut app, 100, 20).contains("y to delete"));
    }

    #[test]
    fn deleting_from_the_message_returns_to_the_inbox_without_the_row() {
        let store = Arc::new(MemoryStore::new());
        store.put(
            BUCKET,
            "mail/keep",
            &email(
                "k@example.com",
                "Keep me",
                "Fri, 25 Sep 2026 08:00:00 +0000",
            ),
        );
        store.put(BUCKET, KEY, &long_message());
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(store.clone()));
        let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox())));
        settle(&mut app);
        // Newest first: the report (09:30) is on top.
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        assert!(app.stack.last().unwrap().title().contains("Message"));
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        assert!(!store.contains(BUCKET, KEY));
        assert_eq!(app.stack.len(), 1);
        assert!(app.stack.last().unwrap().title().contains("Inbox"));
        let scr = screen(&mut app, 100, 12);
        assert!(!scr.contains("Quarterly report"), "{scr}");
        assert!(scr.contains("Keep me"), "{scr}");
        assert!(scr.contains("1 message"), "{scr}");
    }

    #[test]
    fn a_failed_delete_stays_on_the_message_and_says_why() {
        let inner = store_with(&long_message());
        let store = Arc::new(Failing {
            inner: inner.clone(),
            list_err: None,
            delete_err: Some(access_denied()),
        });
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store, out.path());
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        assert!(inner.contains(BUCKET, KEY));
        assert!(app.stack.last().unwrap().title().contains("Message"));
        let scr = screen(&mut app, 120, 20);
        assert!(
            scr.lines().last().unwrap().contains("AccessDenied"),
            "{scr}"
        );
        assert!(
            matches!(app.ctx.status, Some(crate::tui::Status::Error(_))),
            "{:?}",
            app.ctx.status
        );
    }

    #[test]
    fn a_missing_object_says_so() {
        let s = Arc::new(MemoryStore::new());
        s.create_bucket(BUCKET);
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(s, out.path());
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("no longer exists"), "{scr}");
        assert!(scr.contains("s3://inbox-bucket/mail/msg1"), "{scr}");
    }

    #[test]
    fn q_goes_back() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        app.key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }
}
