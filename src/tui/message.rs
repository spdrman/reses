//! One decoded message, fetched from S3.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::{Ctx, Transition, View};

pub struct MessageScreen {
    pub bucket: String,
    pub key: String,
}

impl MessageScreen {
    pub fn new(bucket: String, key: String) -> Self {
        Self { bucket, key }
    }

    pub fn with_out_dir(self, dir: PathBuf) -> Self {
        let _ = dir;
        self
    }
}

impl View for MessageScreen {
    fn title(&self) -> String {
        "Message".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        frame.render_widget(Paragraph::new("Message: not built yet"), area);
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &mut Ctx) -> Transition {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Transition::Pop,
            _ => Transition::None,
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("q", "back")]
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use ratatui::crossterm::event::KeyCode;

    use super::*;
    use crate::s3::{MemoryStore, Store};
    use crate::tui::inbox::InboxScreen;
    use crate::tui::inbox::fixtures::*;
    use crate::tui::testing::{self, key, screen, settle};
    use crate::tui::{App, View};

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
