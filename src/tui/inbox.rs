//! The inbox list: one row per stored message, From / Subject / Date / Size.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use time::OffsetDateTime;

use super::{Ctx, Transition, View};
use crate::config::Inbox;

pub struct InboxScreen {
    pub inbox: Inbox,
}

impl InboxScreen {
    pub fn new(inbox: Inbox) -> Self {
        Self { inbox }
    }

    pub fn with_now(self, now: OffsetDateTime) -> Self {
        let _ = now;
        self
    }

    pub fn with_downloads_dir(self, dir: PathBuf) -> Self {
        let _ = dir;
        self
    }
}

impl View for InboxScreen {
    fn title(&self) -> String {
        "Inbox".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        frame.render_widget(Paragraph::new("Inbox: not built yet"), area);
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

/// Message builders and a store that fails on demand, shared with the message screen's tests.
#[cfg(test)]
pub(super) mod fixtures {
    use std::sync::Arc;

    use crate::config::Inbox;
    use crate::s3::{Bucket, Listing, MemoryStore, S3Error, Store};

    pub const BUCKET: &str = "inbox-bucket";
    pub const PREFIX: &str = "mail/";

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
        pub inner: Arc<MemoryStore>,
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

    use ratatui::crossterm::event::KeyCode;
    use time::macros::datetime;

    use super::fixtures::*;
    use super::*;
    use crate::s3::{MemoryStore, Store};
    use crate::tui::jobs::Job;
    use crate::tui::testing::{self, chars, key, screen, settle};
    use crate::tui::{App, View};

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
    fn three() -> Arc<MemoryStore> {
        let s = Arc::new(MemoryStore::new());
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
        let s = Arc::new(MemoryStore::new());
        let mut raw = email("a@example.com", "Sized", "Fri, 25 Sep 2026 09:30:00 +0000");
        raw.resize(2048, b'x');
        s.put(BUCKET, "mail/sized", &raw);
        let mut small = email("a@example.com", "Tiny", "Fri, 25 Sep 2026 09:31:00 +0000");
        let tiny_len = small.len();
        s.put(BUCKET, "mail/tiny", &small);
        let (mut app, _d) = app_with(s);
        let scr = screen(&mut app, 100, 10);
        assert!(line_with(&scr, "Sized").contains("2.0 KB"), "{scr}");
        assert!(
            line_with(&scr, "Tiny").contains(&format!("{tiny_len} B")),
            "{scr}"
        );
    }

    #[test]
    fn columns_truncate_cleanly_at_narrow_width() {
        let s = Arc::new(MemoryStore::new());
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
        for width in [60u16, 45] {
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
        let s = Arc::new(MemoryStore::new());
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
                if matches!(done.job, Job::Peek { .. }) {
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
        let s = Arc::new(MemoryStore::new());
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
        let other = Arc::new(MemoryStore::new());
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
        let s = Arc::new(MemoryStore::new());
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
        let s = Arc::new(MemoryStore::new());
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
        let s = Arc::new(MemoryStore::new());
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
        let s = Arc::new(MemoryStore::new());
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
}
