//! One decoded message, fetched from S3: scroll it, flip to the HTML part, save its text or
//! its attachments, or delete it. Decoding happens on the worker (`Job::Open`).

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::inbox::{Handoff, display_from, render_confirm};
use super::jobs::{Decoded, Done, Generation, Job, JobId, Outcome};
use super::page;
use super::saved;
use super::text::{clean, escape, human_size, width as text_width};
use super::{Ctx, Session, Transition, View};
use crate::mail;
use crate::s3::S3Error;
use time::OffsetDateTime;

mod pager;
use pager::Pager;

pub struct MessageScreen {
    pub bucket: String,
    pub key: String,
    /// The account the message was opened with, whatever happens to `ctx.session` later.
    session: Option<Session>,
    subject: Option<String>,
    from: String,
    out_dir: PathBuf,
    /// Where a delete still in flight goes when this screen closes, so the inbox reports it.
    handoff: Option<Handoff>,
    started: bool,
    fetch: Option<JobId>,
    delete: Option<JobId>,
    /// Shared with the job result, never copied: a message can be tens of megabytes.
    message: Option<std::sync::Arc<Decoded>>,
    /// Bumped when this screen closes, so an Open it queued and never got is skipped.
    generation: Generation,
    html: bool,
    error: Option<String>,
    confirm: bool,
    /// Scrolls the shown text, wrapping only the lines a screen needs.
    pager: Option<Pager>,
}

impl MessageScreen {
    /// I build a message screen for `bucket/key` that saves into ~/Downloads. The fetch starts
    /// on first focus, so building one costs nothing.
    pub fn new(bucket: String, key: String) -> Self {
        Self {
            bucket,
            key,
            session: None,
            subject: None,
            from: String::new(),
            out_dir: PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads"),
            handoff: None,
            started: false,
            fetch: None,
            delete: None,
            message: None,
            generation: Generation::new(),
            html: false,
            error: None,
            confirm: false,
            pager: None,
        }
    }

    /// The subject the inbox already knows, for the delete prompt before the body arrives.
    pub fn with_subject(mut self, subject: String) -> Self {
        self.subject = Some(clean(subject.trim()));
        self
    }

    /// Where `w` and `a` write (default `~/Downloads`).
    pub fn with_out_dir(mut self, dir: PathBuf) -> Self {
        self.out_dir = dir;
        self
    }

    /// Talk to S3 through this account rather than whichever one is current.
    pub fn with_session(mut self, session: Session) -> Self {
        self.session = Some(session);
        self
    }

    /// I share the inbox's set of handed-over deletes, so a delete still in flight when I close
    /// gets reported by the inbox instead of vanishing.
    pub(super) fn with_handoff(mut self, handoff: Handoff) -> Self {
        self.handoff = Some(handoff);
        self
    }

    /// I spell out the message as an s3:// URL for the title.
    fn location(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key)
    }

    /// I return the text or HTML part, whichever is showing, or nothing while it loads.
    fn shown(&self) -> &str {
        match &self.message {
            Some(m) if self.html => &m.html,
            Some(m) => &m.text,
            None => "",
        }
    }

    /// Move the pager with `f`, handing it the text it pages over.
    fn scroll(&mut self, f: impl FnOnce(&mut Pager, &str)) {
        let text = match &self.message {
            Some(m) if self.html => m.html.as_str(),
            Some(m) => m.text.as_str(),
            None => return,
        };
        if let Some(pager) = self.pager.as_mut() {
            f(pager, text);
        }
    }

    /// Leave, handing a delete that hasn't answered yet to the inbox.
    fn close(&mut self) -> Transition {
        // An Open still queued for this screen is no longer wanted.
        self.generation.bump();
        if let (Some(id), Some(handoff)) = (self.delete.take(), &self.handoff) {
            handoff.borrow_mut().insert(id);
        }
        Transition::Pop
    }

    /// I open the HTML part in the default browser, wrapped in the re:SES reader page (see
    /// [`page`]), and leave the text part on screen. A message with no HTML part, or a browser
    /// that won't start, gets a status line saying so instead.
    fn open_html(&self, ctx: &mut Ctx) {
        let Some(message) = &self.message else {
            return;
        };
        let details = mail::details(&message.raw);
        if details.html.is_none() {
            ctx.info("This message has no HTML part.");
            return;
        }
        let location = format!("s3://{}/{}", self.bucket, self.key);
        let now = OffsetDateTime::now_utc();
        let copy = page::Copy {
            location: &location,
            opened: now.checked_to_offset(ctx.local_offset).unwrap_or(now),
        };
        let html = page::reader(&details, &copy);
        // Written first, then handed over: the browser reads the file after I've moved on.
        match page::write(&ctx.page_dir, html.as_bytes(), ".html")
            .and_then(|path| (ctx.open_file)(&path))
        {
            Ok(()) => ctx.info("Opened the HTML part in your browser."),
            Err(e) => ctx.error(format!(
                "Couldn't open the HTML part: {e}. H shows the HTML source here instead."
            )),
        }
    }

    /// I hand the message itself, byte for byte, to the default mail app as a private `.eml` copy, so
    /// its own Reply, Reply all and Forward work, with threading, quoting and attachments that a
    /// `mailto:` link can't carry.
    fn open_in_mail(&self, ctx: &mut Ctx) {
        let Some(message) = &self.message else {
            return;
        };
        match page::write(&ctx.page_dir, &message.raw, ".eml")
            .and_then(|path| (ctx.open_file)(&path))
        {
            Ok(()) => ctx.info("Opened the message in your mail app."),
            Err(e) => ctx.error(format!("Couldn't open the message in your mail app: {e}")),
        }
    }

    /// I write the part that's showing to a new .txt file named after the key, never
    /// overwriting an existing one, and say where it went.
    fn write_text(&mut self, ctx: &mut Ctx) {
        if self.message.is_none() {
            ctx.info("The message is still loading.");
            return;
        }
        let stem = file_stem(&self.key);
        match write_new(&self.out_dir, &stem, "txt", self.shown().as_bytes()) {
            Ok(path) => {
                // Marked as downloaded on macOS; a failure there costs a note, never the file.
                let note = saved::quarantine_all(std::slice::from_ref(&path))
                    .map(|n| format!("; {n}"))
                    .unwrap_or_default();
                ctx.info(format!("Wrote {}{note}", path.display()));
            }
            Err(e) => ctx.error(format!(
                "Could not write into {}: {e}",
                self.out_dir.display()
            )),
        }
    }

    /// I save every attachment into the output folder and name the files on the status line,
    /// or say the message has none.
    fn save_attachments(&mut self, ctx: &mut Ctx) {
        let Some(message) = &self.message else {
            ctx.info("The message is still loading.");
            return;
        };
        match mail::save_attachments(&message.raw, &self.out_dir) {
            Ok(paths) if paths.is_empty() => ctx.info("This message has no attachments."),
            Ok(paths) => {
                let noun = if paths.len() == 1 {
                    "attachment"
                } else {
                    "attachments"
                };
                // The names on disk, which differ from the message's when one was taken.
                let note = saved::quarantine_all(&paths)
                    .map(|n| format!("; {n}"))
                    .unwrap_or_default();
                ctx.info(format!(
                    "Saved {} {noun} to {}: {}{note}",
                    paths.len(),
                    self.out_dir.display(),
                    saved::names(&paths)
                ));
            }
            Err(e) => ctx.error(format!(
                "Could not save attachments into {}: {e}",
                self.out_dir.display()
            )),
        }
    }

    /// I turn a fetch error into a sentence: how big the message is when it's over the size
    /// cap, and the plain error otherwise.
    fn fetch_error(&self, e: &S3Error) -> String {
        if let S3Error::TooLarge { size, limit } = e {
            let how_big = match size {
                Some(size) => human_size(*size),
                None => format!("over {}", human_size(*limit)),
            };
            return format!(
                "This message is too large to open ({how_big}): {}",
                self.location()
            );
        }
        if e.is_not_found() {
            return format!("This message no longer exists: {}", self.location());
        }
        format!("Could not fetch {}: {e}", self.location())
    }
}

impl View for MessageScreen {
    /// I title the screen with the message's location, marking when the HTML part is showing.
    fn title(&self) -> String {
        let view = if self.html { " (HTML)" } else { "" };
        format!("Message {}{view}", self.location())
    }

    /// I draw the message: the error in red if the fetch failed, a loading line until it
    /// arrives, then the pager's window of wrapped lines under a short header, with the delete
    /// confirmation on top when it's open.
    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(format!(" {}", escape(err)))
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false }),
                area,
            );
        } else if self.message.is_none() {
            frame.render_widget(
                Paragraph::new(format!(" Fetching {} …", escape(&self.location()))),
                area,
            );
        } else {
            let text = match &self.message {
                Some(m) if self.html => m.html.as_str(),
                Some(m) => m.text.as_str(),
                None => "",
            };
            let rows = match self.pager.as_mut() {
                Some(pager) => pager.screen(text, area.width as usize, area.height as usize),
                None => Vec::new(),
            };
            let shown: Vec<_> = rows.into_iter().map(Line::raw).collect();
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

    /// I handle a key. An open delete confirmation takes it first and only a bare y deletes;
    /// otherwise I scroll, open the HTML in the browser or flip to its source, save text or
    /// attachments, ask to delete, or go back.
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.confirm {
            self.confirm = false;
            // Only a bare y deletes; ctrl-y and everything else cancel.
            if key.code == KeyCode::Char('y') && key.modifiers == KeyModifiers::NONE {
                let job = Job::Delete {
                    bucket: self.bucket.clone(),
                    key: self.key.clone(),
                };
                match &self.session {
                    Some(session) => self.delete = Some(ctx.submit_to(session, job, None)),
                    None => ctx.error("Not connected to an account."),
                }
            } else {
                ctx.info("Delete cancelled.");
            }
            return Transition::None;
        }
        match key.code {
            // Shift+arrows page, since a MacBook has no Page Up or Page Down key. They come first, or the
            // bare arrow arms below would take them a line at a time.
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll(|p, t| p.up(t, p.page()))
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll(|p, t| p.down(t, p.page()))
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll(|p, t| p.up(t, 1)),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(|p, t| p.down(t, 1)),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll(|p, t| p.up(t, p.page())),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(|p, t| p.down(t, p.page())),
            KeyCode::Home | KeyCode::Char('g') => self.scroll(|p, _| p.home()),
            KeyCode::End | KeyCode::Char('G') => self.scroll(|p, t| p.end(t)),
            KeyCode::Char('h') if self.message.is_some() => self.open_html(ctx),
            KeyCode::Char('H') if self.message.is_some() => {
                self.html = !self.html;
                // The other part, from its top.
                self.scroll(|p, t| p.reset(t));
                ctx.info(if self.html {
                    "Showing the HTML source."
                } else {
                    "Showing the text part."
                });
            }
            KeyCode::Char('o') if self.message.is_some() => self.open_in_mail(ctx),
            KeyCode::Char('w') => self.write_text(ctx),
            KeyCode::Char('a') => self.save_attachments(ctx),
            KeyCode::Char('d') if key.modifiers == KeyModifiers::NONE => self.confirm = true,
            KeyCode::Esc | KeyCode::Char('q') => return self.close(),
            _ => {}
        }
        Transition::None
    }

    /// Nothing here takes text, so a paste only matters as a no to an open confirmation.
    fn on_paste(&mut self, _text: &str, ctx: &mut Ctx) -> Transition {
        if self.confirm {
            self.confirm = false;
            ctx.info("Delete cancelled.");
        }
        Transition::None
    }

    /// I take in the decoded message or the fetch error, and the answer to my delete, which
    /// closes the screen when it worked and says why when it didn't.
    fn on_done(&mut self, done: &Done, ctx: &mut Ctx) -> Transition {
        if Some(done.id) == self.fetch {
            self.fetch = None;
            match &done.result {
                Ok(Outcome::Message(message)) => {
                    let subject = message.summary.subject.trim();
                    if !subject.is_empty() {
                        self.subject = Some(clean(subject));
                    }
                    self.from = display_from(&message.summary.from);
                    self.pager = Some(Pager::new(&message.text));
                    self.message = Some(std::sync::Arc::clone(message));
                }
                Ok(_) => self.error = Some(format!("Unexpected reply for {}", self.location())),
                Err(e) => self.error = Some(self.fetch_error(e)),
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

    /// I start the fetch the first time I'm shown, on my own session or ctx.session if I wasn't
    /// given one. Later focuses change nothing.
    fn on_focus(&mut self, ctx: &mut Ctx) {
        if self.started {
            return;
        }
        self.started = true;
        if self.session.is_none() {
            self.session = ctx.session.clone();
        }
        let job = Job::Open {
            bucket: self.bucket.clone(),
            key: self.key.clone(),
        };
        match &self.session {
            Some(session) => {
                self.fetch = Some(ctx.submit_to(session, job, Some(&self.generation)));
            }
            None => self.error = Some("Not connected to an account.".into()),
        }
    }

    /// I report my own session, so the header bar names the account this message came from.
    fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// I list the keys the message screen takes.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", "scroll"),
            ("⇧↑↓", "page"),
            // Short labels, so the row still fits delete and back at the demo's 106 columns.
            ("h", "html"),
            ("H", if self.html { "text" } else { "source" }),
            ("w", "save text"),
            ("a", "save attachments"),
            ("d", "delete"),
            // Last before q: at the demo's width it's the one that gives way, not delete.
            ("o", "mail app"),
            ("q", "back"),
        ]
    }
}

impl Drop for MessageScreen {
    /// However the screen goes away (closed, reset, or the app quitting), an Open it queued
    /// and never got is skipped rather than run for nobody.
    fn drop(&mut self) {
        self.generation.bump();
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

/// Wrap one source line to `width` columns, breaking after a space where there is one. Always
/// at least one row, so an empty line still takes its row.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    #[cfg(test)]
    WRAPPED_LINES.with(|c| c.set(c.get() + 1));
    let width = width.max(1);
    let mut out = Vec::new();
    let line = clean(&line.replace('\t', "    "));
    let mut cur = String::new();
    let mut used = 0;
    // Grapheme by grapheme, measured the way the terminal draws them, so an emoji with a
    // variation selector takes its two columns here too.
    for g in line.graphemes(true) {
        let w = text_width(g);
        if used + w > width && !cur.is_empty() {
            // Carry the unfinished word over when the line has a space to break at.
            let carry = match cur.rfind(' ') {
                Some(i) if i + 1 < cur.len() => cur.split_off(i + 1),
                _ => String::new(),
            };
            out.push(cur.trim_end().to_string());
            used = text_width(&carry);
            cur = carry;
        }
        cur.push_str(g);
        used += w;
    }
    out.push(cur);
    out
}

#[cfg(test)]
thread_local! {
    /// Source lines this thread has wrapped, so a test can tell on-screen work from all of it.
    static WRAPPED_LINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use ratatui::crossterm::event::KeyCode;

    use super::*;
    use crate::s3::{S3Error, Store};
    use crate::tui::inbox::InboxScreen;
    use crate::tui::inbox::fixtures::*;
    use crate::tui::testing::{self, key, screen, settle, shifted};
    use crate::tui::{App, Status};

    const KEY: &str = "mail/msg1";

    /// I build a message with a hundred numbered body lines, long enough to need scrolling.
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

    /// I build a multipart message with a text part, an HTML part and one attachment.
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

    /// I open a message screen on `store` for the fixture key, saving into `out`, and let its
    /// fetch settle. The returned guard keeps the config dir alive.
    fn open(store: Arc<dyn Store>, out: &Path) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(store));
        let view = MessageScreen::new(BUCKET.into(), KEY.into()).with_out_dir(out.to_path_buf());
        let mut app = App::with_view(ctx, Box::new(view));
        settle(&mut app);
        (app, dir)
    }

    /// I build a store holding `raw` at the fixture key.
    fn store_with(raw: &[u8]) -> Arc<Timed> {
        let s = Timed::new();
        s.put(BUCKET, KEY, raw);
        s
    }

    /// I check the decoded headers and body show up and that End, Home, the page keys and the
    /// arrows all scroll the way they should.
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

    /// I check scrolling past either end stops there, and that the last page stays full rather
    /// than scrolling into blank space.
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

    /// I check a line wider than the screen wraps so its end is still readable.
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

    /// I check Shift+↓ and Shift+↑ page the message exactly like Page Down and Page Up, and that
    /// Space and b do too, so a MacBook keyboard pages without reaching for fn.
    #[test]
    fn shift_arrows_space_and_b_page_like_the_page_keys() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        // Each run starts from the top, so the screens can be compared like for like.
        let after = |app: &mut App, keys: &[KeyEvent]| {
            app.key(key(KeyCode::Home));
            // Drawn once first, since the page size is only known after a render.
            screen(app, 80, 20);
            for k in keys {
                app.key(*k);
            }
            screen(app, 80, 20)
        };
        let paged = after(&mut app, &[key(KeyCode::PageDown)]);
        assert_ne!(
            paged,
            after(&mut app, &[key(KeyCode::Down)]),
            "a page is more than a line"
        );
        assert_eq!(after(&mut app, &[shifted(KeyCode::Down)]), paged);
        assert_eq!(after(&mut app, &[key(KeyCode::Char(' '))]), paged);
        let two_down = [key(KeyCode::PageDown), key(KeyCode::PageDown)];
        let back = after(&mut app, &[two_down[0], two_down[1], key(KeyCode::PageUp)]);
        assert_eq!(
            after(&mut app, &[two_down[0], two_down[1], shifted(KeyCode::Up)]),
            back
        );
        assert_eq!(
            after(
                &mut app,
                &[two_down[0], two_down[1], key(KeyCode::Char('b'))]
            ),
            back
        );
    }

    /// I check the hints name keys a MacBook has: Shift+arrows for paging, not Page Up/Down.
    #[test]
    fn the_hints_name_paging_keys_a_mac_has() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        let scr = screen(&mut app, 120, 20);
        assert!(scr.contains("⇧↑↓  page"), "{scr}");
        assert!(!scr.contains("pgup"), "{scr}");
    }

    /// I stand in for the browser: every page the screen asks to open is recorded, not shown.
    fn recording(app: &mut App) -> Arc<Mutex<Vec<PathBuf>>> {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&opened);
        app.ctx.open_file = Arc::new(move |path: &Path| {
            seen.lock().unwrap().push(path.to_path_buf());
            Ok(())
        });
        opened
    }

    /// I check `h` opens the HTML part in the browser as a guarded page of its own, and leaves
    /// the text part on screen.
    #[test]
    fn h_opens_the_html_part_in_the_browser() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        let opened = recording(&mut app);
        app.key(key(KeyCode::Char('h')));
        let opened = opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1, "{opened:?}");
        assert_eq!(opened[0].extension().unwrap(), "html");
        let page = fs::read_to_string(&opened[0]).unwrap();
        assert!(page.contains("<b>html</b>"), "{page}");
        assert!(page.contains("Content-Security-Policy"), "{page}");
        // Wrapped in the reader: the sender up top, and where this copy came from below.
        assert!(page.contains("alice@example.com"), "{page}");
        assert!(page.contains("About this message"), "{page}");
        assert!(page.contains(&format!("s3://{BUCKET}/{KEY}")), "{page}");
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("the plain version"), "{scr}");
        assert!(
            scr.contains("Opened the HTML part in your browser"),
            "{scr}"
        );
    }

    /// I check `o` hands the message itself, byte for byte, to the mail app as a private `.eml`
    /// file, so its own Reply, Reply all and Forward work with threading and attachments.
    #[test]
    fn o_opens_the_message_in_the_mail_app() {
        let out = tempfile::tempdir().unwrap();
        let raw = multipart();
        let (mut app, _d) = open(store_with(&raw), out.path());
        let opened = recording(&mut app);
        app.key(key(KeyCode::Char('o')));
        let opened = opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1, "{opened:?}");
        assert_eq!(opened[0].extension().unwrap(), "eml");
        assert_eq!(fs::read(&opened[0]).unwrap(), raw);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&opened[0]).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let scr = screen(&mut app, 100, 20);
        assert!(scr.contains("Opened the message in your mail app"), "{scr}");
    }

    /// I check a mail app that won't start is an error on the status line, not a silent no-op.
    #[test]
    fn a_failed_mail_app_open_is_an_error() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        app.ctx.open_file = Arc::new(|_: &Path| Err(std::io::Error::other("no mail app")));
        app.key(key(KeyCode::Char('o')));
        let scr = screen(&mut app, 100, 20);
        assert!(scr.contains("no mail app"), "{scr}");
    }

    /// I check the hints list `o`, so it's findable without the README.
    #[test]
    fn the_hints_list_the_mail_app_key() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        let scr = screen(&mut app, 160, 20);
        assert!(scr.contains("o  mail app"), "{scr}");
    }

    /// I check a message with no HTML part says so and opens nothing, rather than an empty page.
    #[test]
    fn h_without_an_html_part_says_so() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        let opened = recording(&mut app);
        app.key(key(KeyCode::Char('h')));
        assert!(opened.lock().unwrap().is_empty());
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("This message has no HTML part"), "{scr}");
    }

    /// I check a browser that can't start is an error on the status line, and that it points at
    /// `H`, the source view that works without one.
    #[test]
    fn a_failed_open_is_an_error_that_points_at_the_source_view() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        app.ctx.open_file = Arc::new(|_: &Path| Err(std::io::Error::other("no browser here")));
        app.key(key(KeyCode::Char('h')));
        let scr = screen(&mut app, 100, 20);
        assert!(scr.contains("no browser here"), "{scr}");
        assert!(scr.contains("H shows the HTML source"), "{scr}");
    }

    /// I check `H` still switches between the plain part and the HTML source in the terminal,
    /// for when there's no browser to open.
    #[test]
    fn capital_h_toggles_the_html_source() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("the plain version"), "{scr}");
        app.key(key(KeyCode::Char('H')));
        let scr = screen(&mut app, 80, 20);
        assert!(scr.contains("<b>html</b>"), "{scr}");
        assert!(!scr.contains("the plain version"), "{scr}");
        app.key(key(KeyCode::Char('H')));
        assert!(screen(&mut app, 80, 20).contains("the plain version"));
    }

    /// I check the test context itself refuses to open a browser, so a test that forgets its
    /// recorder fails instead of popping one up.
    #[test]
    fn the_test_context_never_opens_a_browser() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), None);
        assert!((ctx.open_file)(Path::new("/nowhere.html")).is_err());
    }

    /// I check `w` writes exactly the decoded text, names the file on the status line, and never
    /// overwrites an earlier save.
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

    /// I check `a` saves the attachments and says how many went into which folder.
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

    /// I check the status line names the file as it actually landed on disk, so a renamed
    /// note-1.txt isn't reported as note.txt.
    #[test]
    fn the_status_line_names_the_files_as_they_were_saved() {
        let out = tempfile::tempdir().unwrap();
        // A note.txt is already there, so the attachment lands as note-1.txt.
        std::fs::write(out.path().join("note.txt"), b"older").unwrap();
        let (mut app, _d) = open(store_with(&multipart()), out.path());
        app.key(key(KeyCode::Char('a')));
        assert!(out.path().join("note-1.txt").exists());
        let scr = screen(&mut app, 200, 20);
        let status = scr.lines().last().unwrap();
        assert!(status.contains("note-1.txt"), "{scr}");
        assert!(!status.contains("error"), "{scr}");
    }

    /// I check `a` on a message with nothing attached says so instead of doing nothing.
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

    /// I check `d` on the message asks first with subject and key, and that `n` leaves the object
    /// and me on the message.
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

    /// I check deleting from the message screen drops me back in the inbox with that row gone.
    #[test]
    fn deleting_from_the_message_returns_to_the_inbox_without_the_row() {
        let store = Timed::new();
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

    /// I check a delete S3 refuses keeps me on the message and shows the error on the status line.
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

    /// I check a message deleted since the listing says it no longer exists and names its key.
    #[test]
    fn a_missing_object_says_so() {
        let s = Timed::new();
        s.create_bucket(BUCKET);
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(s, out.path());
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("no longer exists"), "{scr}");
        assert!(scr.contains("s3://inbox-bucket/mail/msg1"), "{scr}");
    }

    /// I check `q` leaves the message screen, which quits when it's the only screen.
    #[test]
    fn q_goes_back() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&long_message()), out.path());
        app.key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    /// Answers every get the way the client does above its size cap.
    struct TooBig(Option<u64>);

    impl Store for TooBig {
        /// I have no buckets.
        fn list_buckets(&self) -> Result<Vec<crate::s3::Bucket>, S3Error> {
            Ok(Vec::new())
        }
        /// I list nothing.
        fn list(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<crate::s3::Listing, S3Error> {
            Ok(crate::s3::Listing::default())
        }
        /// I answer a peek with nothing.
        fn get_range(&self, _: &str, _: &str, _: u64, _: u64) -> Result<Vec<u8>, S3Error> {
            Ok(Vec::new())
        }
        /// I refuse every get as over the size cap, with the size S3 reported when there is one.
        fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, S3Error> {
            Err(S3Error::TooLarge {
                size: self.0,
                limit: 41 * 1024 * 1024,
            })
        }
        /// I pretend every delete worked.
        fn delete(&self, _: &str, _: &str) -> Result<(), S3Error> {
            Ok(())
        }
    }

    /// I check a message over the size cap says how big it is, or that it's over the cap when S3
    /// didn't send a length.
    #[test]
    fn a_message_over_the_size_cap_says_how_big_it_is() {
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(Arc::new(TooBig(Some(120 * 1024 * 1024))), out.path());
        let scr = screen(&mut app, 100, 10);
        assert!(scr.contains("too large to open (120.0 MiB)"), "{scr}");
        assert!(scr.contains("s3://inbox-bucket/mail/msg1"), "{scr}");
        // No Content-Length: say it's over the cap instead.
        let (mut app, _d) = open(Arc::new(TooBig(None)), out.path());
        assert!(screen(&mut app, 100, 10).contains("too large to open (over 41.0 MiB)"));
    }

    /// I check a failed delete is reported exactly once: by the message while it's open, or by
    /// the inbox when I closed the message before the answer came back.
    #[test]
    fn a_failed_delete_after_closing_the_message_is_still_reported_once() {
        let inner = Timed::new();
        inner.put(BUCKET, KEY, &long_message());
        let store = Arc::new(Failing {
            inner: inner.clone(),
            list_err: None,
            delete_err: Some(access_denied()),
        });
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(store));
        let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox())));
        settle(&mut app);

        // While the message is open, it reports its own failure and the inbox stays quiet.
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        match &app.ctx.status {
            Some(Status::Error(m)) => {
                assert!(m.contains("AccessDenied"), "{m}");
                assert!(!m.contains("after closing"), "reported twice: {m}");
            }
            other => panic!("{other:?}"),
        }

        // Close it before the answer comes back: the inbox picks the failure up.
        app.ctx.status = None;
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        app.key(key(KeyCode::Char('q')));
        assert!(app.stack.last().unwrap().title().contains("Inbox"));
        settle(&mut app);
        match &app.ctx.status {
            Some(Status::Error(m)) => {
                assert!(m.contains("after closing the message"), "{m}");
                assert!(m.contains("mail/msg1") && m.contains("AccessDenied"), "{m}");
            }
            other => panic!("the failure was lost: {other:?}"),
        }
        assert!(inner.contains(BUCKET, KEY));
        assert!(screen(&mut app, 100, 12).contains("Quarterly report"));
    }

    /// I check a delete that finishes after I closed the message still drops the row from the inbox.
    #[test]
    fn a_successful_delete_after_closing_still_drops_the_row() {
        let store = Timed::new();
        store.put(BUCKET, KEY, &long_message());
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(store.clone()));
        let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox())));
        settle(&mut app);
        app.key(key(KeyCode::Enter));
        settle(&mut app);
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        app.key(key(KeyCode::Char('q')));
        settle(&mut app);
        assert!(!store.contains(BUCKET, KEY));
        assert_eq!(app.stack.len(), 1);
        let scr = screen(&mut app, 100, 12);
        assert!(!scr.contains("Quarterly report"), "{scr}");
        assert!(scr.lines().last().unwrap().contains("Deleted"), "{scr}");
    }

    /// I switch the session's account under an open message and check its delete still goes to the
    /// account it was opened with.
    #[test]
    fn the_message_keeps_its_own_account() {
        let mine = store_with(&long_message());
        let out = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(mine.clone()));
        let view = MessageScreen::new(BUCKET.into(), KEY.into())
            .with_session(ctx.session.clone().unwrap())
            .with_out_dir(out.path().to_path_buf());
        let mut app = App::with_view(ctx, Box::new(view));
        let theirs = store_with(&long_message());
        app.ctx.session.as_mut().unwrap().store = theirs.clone();
        settle(&mut app);
        assert!(screen(&mut app, 80, 20).contains("Quarterly report"));
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        settle(&mut app);
        assert!(!mine.contains(BUCKET, KEY));
        assert!(theirs.contains(BUCKET, KEY));
    }

    /// I check the prompt flattens tabs in the subject and cuts it with an ellipsis, so a crafted
    /// subject can't push the key or the `y` line out of view.
    #[test]
    fn the_confirmation_flattens_and_cuts_a_hostile_subject() {
        let raw = email(
            "a@example.com",
            &format!("Line one\tand {}", "more words ".repeat(30)),
            "Fri, 25 Sep 2026 09:30:00 +0000",
        );
        let out = tempfile::tempdir().unwrap();
        let (mut app, _d) = open(store_with(&raw), out.path());
        app.key(key(KeyCode::Char('d')));
        for w in [100u16, 50] {
            let scr = screen(&mut app, w, 20);
            let subject = line_with(&scr, "│ Subject: Line one");
            assert!(subject.contains('…') && !subject.contains('\t'), "{scr}");
            assert!(scr.contains("s3://inbox-bucket/mail/msg1"), "{scr}");
            assert!(scr.contains("y to delete"), "{scr}");
        }
    }

    /// I find the first screen line holding `needle`, and fail with the whole screen if none does.
    fn line_with<'a>(scr: &'a str, needle: &str) -> &'a str {
        scr.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line contains {needle:?} in:\n{scr}"))
    }

    /// A decoded message of `lines` lines, each longer than one screen row at 80 columns.
    fn huge(lines: usize) -> Arc<Decoded> {
        let mut text = String::new();
        for i in 0..lines {
            text.push_str(&format!(
                "line {i:06} lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod\n"
            ));
        }
        Arc::new(Decoded::new(
            text.clone().into_bytes(),
            text.clone(),
            text,
            Default::default(),
        ))
    }

    /// A message screen that has just been handed `message` as its Open result.
    fn opened(message: &Arc<Decoded>) -> (MessageScreen, crate::tui::Ctx, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = testing::ctx(dir.path(), None);
        let mut screen = MessageScreen::new(BUCKET.into(), KEY.into());
        // As if it had already asked for the message and this is the answer.
        screen.started = true;
        screen.fetch = Some(1);
        let done = crate::tui::jobs::Done {
            id: 1,
            job: Job::Open {
                bucket: BUCKET.into(),
                key: KEY.into(),
            },
            result: Ok(Outcome::Message(Arc::clone(message))),
        };
        let _ = screen.on_done(&done, &mut ctx);
        (screen, ctx, dir)
    }

    /// I check the screen keeps the same `Arc` it was handed, so a big message is never copied on
    /// the UI thread.
    #[test]
    fn opening_a_message_shares_the_decoded_text_instead_of_copying_it() {
        let message = huge(10);
        let (screen, _ctx, _d) = opened(&message);
        let kept = screen
            .message
            .as_ref()
            .expect("the screen kept the message");
        assert!(Arc::ptr_eq(kept, &message), "the screen copied the message");
    }

    /// I count wrapped lines over open, resize and End/Home on a 100,000-line message and check only
    /// about a screenful gets wrapped each time.
    #[test]
    fn a_huge_message_only_wraps_what_is_on_screen() {
        let message = huge(100_000);
        let (screen, ctx, _d) = opened(&message);
        let mut app = App::with_view(ctx, Box::new(screen));
        WRAPPED_LINES.with(|c| c.set(0));
        // Open, resize by a column, and jump to the end and back: each only needs a screenful.
        let scr = testing::screen(&mut app, 80, 24);
        assert!(scr.contains("line 000000"), "{scr}");
        let _ = testing::screen(&mut app, 81, 24);
        app.key(key(KeyCode::End));
        let scr = testing::screen(&mut app, 81, 24);
        assert!(scr.contains("line 099999"), "{scr}");
        app.key(key(KeyCode::Home));
        assert!(testing::screen(&mut app, 81, 24).contains("line 000000"));
        let wrapped = WRAPPED_LINES.with(std::cell::Cell::get);
        assert!(
            wrapped <= 10 * 24,
            "{wrapped} of 100000 lines wrapped on the UI thread"
        );
    }

    /// I check End on a huge message puts its last line on the last body row, and that Down then
    /// does nothing and Up moves by exactly one row.
    #[test]
    fn scrolling_a_huge_message_lands_exactly_at_the_end() {
        let message = huge(5_000);
        let (screen, ctx, _d) = opened(&message);
        let mut app = App::with_view(ctx, Box::new(screen));
        let _ = testing::screen(&mut app, 80, 24);
        app.key(key(KeyCode::End));
        let scr = testing::screen(&mut app, 80, 24);
        // The last line is on the last body row, not scrolled past it.
        let body: Vec<&str> = scr.lines().skip(1).take(22).collect();
        assert!(body.last().unwrap().contains("eiusmod"), "{scr}");
        assert!(body.iter().any(|l| l.contains("line 004999")), "{scr}");
        // Further down goes nowhere; one up moves by exactly one row.
        app.key(key(KeyCode::Down));
        assert_eq!(testing::screen(&mut app, 80, 24), scr);
        app.key(key(KeyCode::Up));
        let up = testing::screen(&mut app, 80, 24);
        let up_body: Vec<&str> = up.lines().skip(1).take(22).collect();
        assert_eq!(up_body[1..], body[..21], "{up}");
    }
}
