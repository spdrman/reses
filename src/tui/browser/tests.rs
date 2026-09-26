use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::KeyCode;

use super::BrowserScreen;
use crate::aws_profile::Profile;
use crate::config::{AppConfig, Inbox};
use crate::s3::{Bucket, Listing, MemoryStore, ObjectInfo, S3Error, Store};
use crate::tui::testing::{self, chars, key, screen, settle};
use crate::tui::{App, Session, Status};

const EMAIL: &[u8] = b"Return-Path: <sender@example.com>\r\n\
Received: from mail.example.com by inbound-smtp.us-east-1.amazonaws.com\r\n\
From: Sender <sender@example.com>\r\n\
To: me@example.org\r\n\
Subject: Hello there\r\n\
Date: Mon, 1 Sep 2025 10:00:00 +0000\r\n\
Message-ID: <abc@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hi.\r\n";

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";
const TEXT: &[u8] = b"just some notes, nothing to see here\n";

/// A MemoryStore that records every call, and can be told to fail listings.
struct Spy {
    inner: MemoryStore,
    peeks: Mutex<Vec<(String, u64, u64)>>,
    lists: Mutex<Vec<(String, Option<String>)>>,
    fail_list: AtomicBool,
    /// How listings of the "loop" bucket answer: 0 normally, 1 the same token forever,
    /// 2 a fresh token forever.
    endless: AtomicUsize,
    /// While true, every peek waits inside the store, so a one-thread pool backs up behind it.
    hold: Mutex<bool>,
    released: Condvar,
    /// What `bucket_region` answers, as if the client had followed a region redirect.
    learned_region: Mutex<Option<String>>,
}

impl Spy {
    fn new(inner: MemoryStore) -> Arc<Self> {
        Arc::new(Self {
            inner,
            peeks: Mutex::new(Vec::new()),
            lists: Mutex::new(Vec::new()),
            fail_list: AtomicBool::new(false),
            endless: AtomicUsize::new(0),
            hold: Mutex::new(false),
            released: Condvar::new(),
            learned_region: Mutex::new(None),
        })
    }

    fn peek_count(&self) -> usize {
        self.peeks.lock().unwrap().len()
    }

    fn peeks_per_key(&self) -> HashMap<String, usize> {
        let mut m = HashMap::new();
        for (k, _, _) in self.peeks.lock().unwrap().iter() {
            *m.entry(k.clone()).or_insert(0) += 1;
        }
        m
    }
}

impl Store for Spy {
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
        self.lists
            .lock()
            .unwrap()
            .push((prefix.to_string(), delimiter.map(str::to_string)));
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(S3Error::Service {
                status: 403,
                code: "AccessDenied".into(),
                message: "Access Denied".into(),
            });
        }
        let mode = self.endless.load(Ordering::SeqCst);
        if bucket == "loop" && mode != 0 {
            let n = self.lists.lock().unwrap().len();
            return Ok(Listing {
                prefixes: Vec::new(),
                objects: vec![ObjectInfo {
                    key: format!("{prefix}obj-{n:05}"),
                    size: TEXT.len() as u64,
                    last_modified: None,
                }],
                next_token: Some(if mode == 1 {
                    "same".into()
                } else {
                    format!("t{n}")
                }),
            });
        }
        self.inner.list(bucket, prefix, delimiter, token)
    }
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        self.peeks
            .lock()
            .unwrap()
            .push((key.to_string(), start, end));
        let mut held = self.hold.lock().unwrap();
        while *held {
            held = self.released.wait(held).unwrap();
        }
        drop(held);
        self.inner.get_range(bucket, key, start, end)
    }
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        self.inner.get(bucket, key)
    }
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        self.inner.delete(bucket, key)
    }
    fn bucket_region(&self, bucket: &str) -> Option<String> {
        let _ = bucket;
        self.learned_region.lock().unwrap().clone()
    }
}

/// Two buckets. `mail` holds an SES-like layout: an inbound folder with email and some other
/// files, a nested folder of email, and a folder with no email at all.
fn mail_store() -> MemoryStore {
    let s = MemoryStore::new();
    s.create_bucket("archive");
    s.put("mail", "AMAZON_SES_SETUP_NOTIFICATION", TEXT);
    s.put("mail", "inbound/msg-one", EMAIL);
    s.put("mail", "inbound/msg-two", EMAIL);
    s.put("mail", "inbound/logo.png", PNG);
    s.put("mail", "inbound/notes.txt", TEXT);
    s.put("mail", "inbound/2025/sep/msg-three", EMAIL);
    s.put("mail", "inbound/2025/sep/msg-four", EMAIL);
    s.put("mail", "inbound/2025/sep/msg-five", EMAIL);
    s.put("mail", "pictures/cat.png", PNG);
    s.put("mail", "pictures/dog.png", PNG);
    s
}

fn app_on(dir: &Path, spy: &Arc<Spy>) -> App {
    let ctx = testing::ctx(dir, Some(Arc::clone(spy) as Arc<dyn Store>));
    let mut app = App::with_view(ctx, Box::new(BrowserScreen::new()));
    settle(&mut app);
    app
}

fn press(app: &mut App, code: KeyCode) {
    app.key(key(code));
    settle(app);
}

/// Move the selection to the row showing `name` and press Enter.
fn open(app: &mut App, name: &str) {
    press(app, KeyCode::Home);
    for _ in 0..200 {
        let s = screen(app, 100, 40);
        if selected_row(&s).contains(name) {
            press(app, KeyCode::Enter);
            return;
        }
        press(app, KeyCode::Down);
    }
    panic!("never selected {name}");
}

/// The highlighted row carries a "> " marker.
fn selected_row(s: &str) -> String {
    s.lines()
        .find(|l| l.trim_start().starts_with("> "))
        .unwrap_or_else(|| panic!("no selected row in:\n{s}"))
        .to_string()
}

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
}

fn status_error(app: &App) -> String {
    match &app.ctx.status {
        Some(Status::Error(m)) => m.clone(),
        other => panic!("expected an error on the status line, got {other:?}"),
    }
}

// ---- buckets and folders ----

#[test]
fn starts_at_the_bucket_list() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    let s = screen(&mut app, 80, 12);
    assert!(s.contains("Browse S3"), "{s}");
    assert!(s.contains("archive"), "{s}");
    assert!(s.contains("mail"), "{s}");
    assert!(selected_row(&s).contains("archive"), "{s}");
    let footer = s.lines().last().unwrap();
    for hint in ["enter", "open", "filter"] {
        assert!(footer.contains(hint), "missing {hint}: {footer}");
    }
    // Search and saving the inbox only make sense inside a bucket.
    open(&mut app, "mail");
    let s = screen(&mut app, 80, 12);
    let footer = s.lines().last().unwrap();
    for hint in ["enter", "open", "up", "filter", "search", "inbox"] {
        assert!(footer.contains(hint), "missing {hint}: {footer}");
    }
}

#[test]
fn enter_opens_a_bucket_with_its_folders_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("mail/"), "path shown: {s}");
    assert!(s.contains("inbound/"), "{s}");
    assert!(s.contains("pictures/"), "{s}");
    assert!(s.contains("AMAZON_SES_SETUP_NOTIFICATION"), "{s}");
    let lists = spy.lists.lock().unwrap().clone();
    assert!(
        lists.contains(&(String::new(), Some("/".into()))),
        "folders use the delimiter: {lists:?}"
    );
}

#[test]
fn every_page_of_a_listing_is_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let s = MemoryStore::new().with_page_size(2);
    for i in 0..9 {
        s.put("bk", &format!("f/obj-{i:02}"), TEXT);
    }
    let spy = Spy::new(s);
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "bk");
    open(&mut app, "f/");
    let s = screen(&mut app, 100, 30);
    for i in 0..9 {
        assert!(
            s.contains(&format!("obj-{i:02}")),
            "obj-{i:02} missing:\n{s}"
        );
    }
    assert!(s.contains("9 objects"), "{s}");
    let f_pages = spy
        .lists
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "f/")
        .count();
    assert_eq!(f_pages, 5, "9 objects at 2 a page");
}

#[test]
fn enter_goes_into_a_folder_and_backspace_goes_up() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("mail/inbound/"), "{s}");
    assert!(s.contains("2025/"), "{s}");
    assert!(s.contains("msg-one"), "{s}");
    assert!(!s.contains("pictures/"), "{s}");

    open(&mut app, "2025/");
    open(&mut app, "sep/");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("mail/inbound/2025/sep/"), "{s}");
    assert!(s.contains("msg-three"), "{s}");

    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("mail/inbound/"), "{s}");
    assert!(!s.contains("mail/inbound/2025"), "{s}");
    assert!(s.contains("msg-one"), "{s}");

    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("archive"), "back at the bucket list: {s}");
    assert!(!s.contains("inbound"), "{s}");
}

#[test]
fn esc_goes_up_one_level_and_pops_only_at_the_bucket_list() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    open(&mut app, "2025/");
    press(&mut app, KeyCode::Esc);
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("mail/inbound/"), "{s}");
    assert!(!s.contains("mail/inbound/2025"), "{s}");
    assert!(
        selected_row(&s).contains("2025/"),
        "back on the folder it came out of: {s}"
    );
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert!(!app.quit, "still at the bucket list");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("archive"), "{s}");
    assert!(selected_row(&s).contains("mail"), "{s}");
    press(&mut app, KeyCode::Esc);
    assert!(
        app.quit,
        "the browser was the only view, so going back from the bucket list quits"
    );
}

#[test]
fn an_empty_folder_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "archive");
    let s = screen(&mut app, 100, 12);
    assert!(s.contains("empty"), "{s}");
}

#[test]
fn a_listing_error_shows_on_the_status_line() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    spy.fail_list.store(true, Ordering::SeqCst);
    open(&mut app, "mail");
    assert!(
        status_error(&app).contains("AccessDenied"),
        "{:?}",
        app.ctx.status
    );
    let s = screen(&mut app, 100, 12);
    assert!(s.contains("AccessDenied"), "{s}");
}

#[test]
fn a_huge_folder_peeks_only_what_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let s = MemoryStore::new().with_page_size(1000);
    for i in 0..5000 {
        s.put("bk", &format!("obj-{i:05}"), TEXT);
    }
    let spy = Spy::new(s);
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "bk");
    let s = screen(&mut app, 100, 30);
    assert!(s.contains("5000 objects"), "{s}");
    assert!(s.contains("obj-00000"), "{s}");
    assert!(!s.contains("obj-04999"), "{s}");
    let first = spy.peek_count();
    assert!(first > 0 && first <= 60, "peeked {first} of 5000");

    press(&mut app, KeyCode::End);
    let s = screen(&mut app, 100, 30);
    assert!(selected_row(&s).contains("obj-04999"), "{s}");
    assert!(
        spy.peek_count() <= first + 60,
        "peeked {}",
        spy.peek_count()
    );

    press(&mut app, KeyCode::PageUp);
    let s = screen(&mut app, 100, 30);
    assert!(!selected_row(&s).contains("obj-04999"), "{s}");
}

// ---- email marks ----

#[test]
fn visible_objects_are_peeked_once_and_email_is_marked() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    // Moving around and re-rendering must not peek anything again.
    for _ in 0..3 {
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Up);
        let _ = screen(&mut app, 100, 20);
    }
    let s = screen(&mut app, 100, 20);
    assert!(line_with(&s, "msg-one").contains("email"), "{s}");
    assert!(line_with(&s, "msg-two").contains("email"), "{s}");
    assert!(!line_with(&s, "logo.png").contains("email"), "{s}");
    assert!(!line_with(&s, "notes.txt").contains("email"), "{s}");
    assert!(s.contains("2 emails"), "{s}");

    let per_key = spy.peeks_per_key();
    for k in [
        "inbound/msg-one",
        "inbound/msg-two",
        "inbound/logo.png",
        "inbound/notes.txt",
    ] {
        assert_eq!(per_key.get(k), Some(&1), "{k}: {per_key:?}");
    }
    assert!(
        !per_key.keys().any(|k| k.starts_with("inbound/2025")),
        "folders are not peeked: {per_key:?}"
    );
    for (_, start, end) in spy.peeks.lock().unwrap().iter() {
        assert_eq!((*start, *end), (0, 4095), "a 4 KiB peek");
    }
}

#[test]
fn coming_back_to_a_folder_does_not_mix_up_marks() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "pictures/");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("0 emails"), "{s}");
    press(&mut app, KeyCode::Backspace);
    open(&mut app, "inbound/");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("2 emails"), "{s}");
}

// ---- filter ----

#[test]
fn slash_filters_the_listing_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    press(&mut app, KeyCode::Char('/'));
    chars(&mut app, "MSG");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("msg-one") && s.contains("msg-two"), "{s}");
    assert!(!s.contains("logo.png") && !s.contains("notes.txt"), "{s}");
    assert!(!s.contains("2025/"), "{s}");
    assert!(s.contains("/MSG"), "the filter text shows: {s}");

    // Enter keeps the filter and gives the keys back to the list.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    let s = screen(&mut app, 100, 20);
    assert!(selected_row(&s).contains("msg-two"), "{s}");
    assert!(!s.contains("logo.png"), "{s}");

    // Esc clears it.
    press(&mut app, KeyCode::Esc);
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("logo.png"), "{s}");
    assert_eq!(
        app.stack.len(),
        1,
        "esc cleared the filter, it did not go back"
    );
}

#[test]
fn a_filter_that_matches_nothing_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    press(&mut app, KeyCode::Char('/'));
    chars(&mut app, "zzzz");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("nothing matches"), "{s}");
}

// ---- search ----

#[test]
fn s_searches_down_from_the_folder_and_lists_folders_holding_email() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    press(&mut app, KeyCode::Char('s'));
    let s = screen(&mut app, 100, 20);
    assert!(
        line_with(&s, "inbound/2025/sep/").contains("3 emails"),
        "{s}"
    );
    let inbound = s
        .lines()
        .find(|l| l.contains("inbound/") && !l.contains("2025"))
        .unwrap_or_else(|| panic!("{s}"));
    assert!(inbound.contains("2 emails"), "{s}");
    assert!(!s.contains("pictures/"), "no email there: {s}");
    assert!(s.contains("done"), "{s}");
    assert!(
        s.contains("10 objects"),
        "progress counts what it checked: {s}"
    );
    let lists = spy.lists.lock().unwrap().clone();
    assert!(
        lists.contains(&(String::new(), None)),
        "search lists without a delimiter: {lists:?}"
    );
    let footer = s.lines().last().unwrap();
    assert!(footer.contains("go there"), "{footer}");
    assert!(!footer.contains("stop"), "nothing left to stop: {footer}");
}

#[test]
fn search_from_a_folder_stays_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    // Only count what the search itself peeks, not the rows the folder views showed.
    spy.peeks.lock().unwrap().clear();
    press(&mut app, KeyCode::Char('s'));
    let per_key = spy.peeks_per_key();
    assert_eq!(per_key.len(), 7, "everything under inbound/: {per_key:?}");
    assert!(
        !per_key.keys().any(|k| k.starts_with("pictures/")),
        "{per_key:?}"
    );
    assert!(!per_key.contains_key("AMAZON_SES_SETUP_NOTIFICATION"));
}

#[test]
fn enter_on_a_search_result_jumps_to_that_folder() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    press(&mut app, KeyCode::Char('s'));
    open(&mut app, "inbound/2025/sep/");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("mail/inbound/2025/sep/"), "{s}");
    assert!(line_with(&s, "msg-three").contains("email"), "{s}");
    assert!(!s.contains("done"), "the search view is gone: {s}");
}

#[test]
fn a_search_can_be_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let s = MemoryStore::new().with_page_size(50);
    for i in 0..400 {
        s.put("bk", &format!("deep/f{:02}/msg-{i:03}", i % 20), EMAIL);
    }
    let spy = Spy::new(s);
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "bk");
    // Start the search and let one round of work finish, then stop it.
    app.key(key(KeyCode::Char('s')));
    app.pump();
    app.pump();
    let s = screen(&mut app, 100, 30);
    assert!(s.contains("searching"), "{s}");
    assert!(s.lines().last().unwrap().contains("stop"), "{s}");
    app.key(key(KeyCode::Char('x')));
    let stopped_at = spy.peek_count();
    settle(&mut app);
    assert_eq!(spy.peek_count(), stopped_at, "no new peeks after stopping");
    assert!(stopped_at < 400, "it stopped early: {stopped_at}");
    let s = screen(&mut app, 100, 30);
    assert!(s.contains("stopped"), "{s}");
    // Esc leaves the results and goes back to the folder.
    press(&mut app, KeyCode::Esc);
    let s = screen(&mut app, 100, 30);
    assert!(s.contains("deep/"), "{s}");
    assert!(!s.contains("stopped"), "{s}");
    assert_eq!(app.stack.len(), 1);
}

#[test]
fn search_needs_a_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    press(&mut app, KeyCode::Char('s'));
    assert!(
        status_error(&app).contains("bucket"),
        "{:?}",
        app.ctx.status
    );
}

// ---- save as inbox ----

#[test]
fn i_saves_the_folder_as_the_inbox_and_opens_it() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    press(&mut app, KeyCode::Char('i'));
    let want = Inbox {
        profile: "test".into(),
        bucket: "mail".into(),
        prefix: "inbound/".into(),
        region: Some("us-east-1".into()),
    };
    assert_eq!(app.ctx.config.inbox.as_ref(), Some(&want));
    let saved = AppConfig::load(&dir.path().join("config.toml")).unwrap();
    assert_eq!(saved.inbox, Some(want));
    assert_eq!(app.stack.len(), 1, "the stack was reset");
    // The inbox screen adds the folder and counts after "Inbox", so only the start is fixed.
    let title = app.stack[0].title();
    assert!(title.starts_with("Inbox"), "top view is {title:?}");
}

#[test]
fn i_at_the_bucket_root_saves_an_empty_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    press(&mut app, KeyCode::Char('i'));
    let saved = AppConfig::load(&dir.path().join("config.toml")).unwrap();
    let inbox = saved.inbox.expect("saved");
    assert_eq!((inbox.bucket.as_str(), inbox.prefix.as_str()), ("mail", ""));
}

#[test]
fn i_on_the_bucket_list_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = app_on(dir.path(), &spy);
    press(&mut app, KeyCode::Char('i'));
    assert!(
        status_error(&app).contains("bucket"),
        "{:?}",
        app.ctx.status
    );
    assert_eq!(app.ctx.config.inbox, None);
    assert!(!dir.path().join("config.toml").exists());
    assert_eq!(app.stack[0].title(), "Browse S3");
}

// ---- runaway paging ----

fn loop_app(dir: &Path, spy: &Arc<Spy>, mode: usize, max_pages: usize) -> App {
    spy.inner.create_bucket("loop");
    let ctx = testing::ctx(dir, Some(Arc::clone(spy) as Arc<dyn Store>));
    let screen = BrowserScreen::new().with_max_pages(max_pages);
    let mut app = App::with_view(ctx, Box::new(screen));
    settle(&mut app);
    spy.endless.store(mode, Ordering::SeqCst);
    app
}

fn loop_lists(spy: &Spy, delimiter: Option<&str>) -> usize {
    spy.lists
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, d)| d.as_deref() == delimiter)
        .count()
}

#[test]
fn a_repeated_continuation_token_stops_the_listing() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = loop_app(dir.path(), &spy, 1, 50);
    open(&mut app, "loop");
    assert_eq!(loop_lists(&spy, Some("/")), 2, "the repeat is not followed");
    assert!(
        status_error(&app).contains("continuation token"),
        "{:?}",
        app.ctx.status
    );
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("2 objects"), "what did arrive stays: {s}");
    assert!(!s.contains("loading"), "{s}");
}

#[test]
fn a_listing_stops_at_the_page_cap() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = loop_app(dir.path(), &spy, 2, 7);
    open(&mut app, "loop");
    assert_eq!(loop_lists(&spy, Some("/")), 7);
    assert!(
        status_error(&app).contains("7 pages"),
        "{:?}",
        app.ctx.status
    );
    let s = screen(&mut app, 100, 20);
    assert!(!s.contains("loading"), "{s}");
}

#[test]
fn search_stops_on_a_repeated_token() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = loop_app(dir.path(), &spy, 1, 50);
    spy.endless.store(0, Ordering::SeqCst);
    open(&mut app, "loop");
    spy.endless.store(1, Ordering::SeqCst);
    press(&mut app, KeyCode::Char('s'));
    assert_eq!(loop_lists(&spy, None), 2, "the repeat is not followed");
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("continuation token"), "{s}");
    assert!(!s.contains("searching"), "{s}");
}

#[test]
fn search_stops_at_the_page_cap() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = loop_app(dir.path(), &spy, 2, 7);
    spy.endless.store(0, Ordering::SeqCst);
    open(&mut app, "loop");
    spy.endless.store(2, Ordering::SeqCst);
    press(&mut app, KeyCode::Char('s'));
    assert_eq!(loop_lists(&spy, None), 7);
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("7 pages"), "{s}");
    assert!(!s.contains("searching"), "{s}");
}

// ---- display width ----

/// Column (in terminal cells) where `needle` starts on `line`. A wide character renders as
/// its own symbol plus a blank cell, so every char of the rendered line is one cell.
fn cell_col(line: &str, needle: &str) -> usize {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not on {line:?}"));
    line[..byte].chars().count()
}

#[test]
fn wide_names_are_cut_by_display_width_and_keep_the_columns_lined_up() {
    let dir = tempfile::tempdir().unwrap();
    let s = MemoryStore::new();
    let wide = "受信メール保存フォルダの中にある長い名前のファイルです";
    s.put("bk", "a-plain-ascii-name", TEXT);
    s.put("bk", wide, TEXT);
    let spy = Spy::new(s);
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "bk");
    let width = 50;
    let screen_text = screen(&mut app, width, 12);
    let ascii = line_with(&screen_text, "a-plain-ascii");
    let cjk = line_with(&screen_text, "受");
    assert!(
        cjk.contains("37 B"),
        "the size fell off the row:\n{screen_text}"
    );
    assert_eq!(
        cell_col(ascii, "37 B"),
        cell_col(cjk, "37 B"),
        "sizes line up:\n{screen_text}"
    );
    assert!(cjk.chars().count() <= width as usize, "{screen_text}");
}

#[test]
fn wide_folder_names_in_search_results_keep_their_counts_on_screen() {
    let dir = tempfile::tempdir().unwrap();
    let s = MemoryStore::new();
    let wide = "受信メール保存フォルダの中にある長い名前のフォルダです/";
    s.put("bk", &format!("{wide}m1"), EMAIL);
    s.put("bk", "plain/m1", EMAIL);
    let spy = Spy::new(s);
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "bk");
    press(&mut app, KeyCode::Char('s'));
    let screen_text = screen(&mut app, 50, 12);
    let cjk = line_with(&screen_text, "受");
    assert!(
        cjk.contains("1 email"),
        "the count fell off the row:\n{screen_text}"
    );
    let plain = line_with(&screen_text, "bk/plain/");
    assert_eq!(
        cell_col(plain, "1 email"),
        cell_col(cjk, "1 email"),
        "{screen_text}"
    );
}

// ---- the browser's own session and generations ----

fn session_on(name: &str, store: Arc<dyn Store>) -> Session {
    Session {
        profile: Profile {
            name: name.into(),
            access_key_id: "AKIAFAKEFAKE00000005".into(),
            secret_access_key: "fakeSecret".into(),
            session_token: None,
            region: Some("eu-west-1".into()),
        },
        region: "eu-west-1".into(),
        store,
    }
}

#[test]
fn the_browser_keeps_talking_to_the_session_it_was_opened_with() {
    let dir = tempfile::tempdir().unwrap();
    let mine = Spy::new(mail_store());
    let theirs = MemoryStore::new();
    theirs.create_bucket("somebody-elses-bucket");
    let theirs: Arc<dyn Store> = Arc::new(theirs);
    let ctx = testing::ctx(dir.path(), Some(Arc::clone(&theirs)));
    let screen_view = BrowserScreen::with_session(session_on("mine", mine.clone()));
    let mut app = App::with_view(ctx, Box::new(screen_view));
    settle(&mut app);
    let s = screen(&mut app, 100, 12);
    assert!(s.contains("archive"), "{s}");
    assert!(!s.contains("somebody-elses-bucket"), "{s}");
    assert!(s.contains("mine"), "the browser names its own account: {s}");

    // Whatever happens to the shared session afterwards, the browser stays on its own.
    app.ctx.session = Some(session_on("other", Arc::clone(&theirs)));
    open(&mut app, "mail");
    let s = screen(&mut app, 100, 12);
    assert!(s.contains("inbound/"), "{s}");
    assert!(!mine.lists.lock().unwrap().is_empty());
}

#[test]
fn saving_the_inbox_hands_the_browsers_session_to_the_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let mine = Spy::new(mail_store());
    let theirs: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let ctx = testing::ctx(dir.path(), Some(theirs));
    let screen_view = BrowserScreen::with_session(session_on("mine", mine.clone()));
    let mut app = App::with_view(ctx, Box::new(screen_view));
    settle(&mut app);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    press(&mut app, KeyCode::Char('i'));
    let inbox = app.ctx.config.inbox.clone().expect("saved");
    assert_eq!(inbox.profile, "mine");
    assert_eq!(inbox.region.as_deref(), Some("eu-west-1"));
    let session = app.ctx.session.as_ref().expect("a session for the inbox");
    assert_eq!(session.profile.name, "mine");
    assert!(
        app.stack[0].title().starts_with("Inbox"),
        "{}",
        app.stack[0].title()
    );
}

/// A browser on a real one-thread pool, so a stale stamp can be seen being skipped.
fn pooled(dir: &Path, spy: &Arc<Spy>) -> App {
    let mut ctx = crate::tui::Ctx::new(
        AppConfig::default(),
        dir.join("config.toml"),
        dir.join("credentials"),
        crate::tui::jobs::Jobs::pool(1),
    );
    ctx.session = Some(session_on("pooled", Arc::clone(spy) as Arc<dyn Store>));
    App::with_view(ctx, Box::new(BrowserScreen::new()))
}

/// Pump until `done` holds, for at most a few seconds.
fn pump_until(app: &mut App, what: &str, mut done: impl FnMut(&mut App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn release(spy: &Spy) {
    *spy.hold.lock().unwrap() = false;
    spy.released.notify_all();
}

#[test]
fn leaving_a_folder_drops_its_queued_peeks() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    let mut app = pooled(dir.path(), &spy);
    pump_until(&mut app, "the bucket list", |a| {
        screen(a, 100, 20).contains("archive")
    });
    open_pooled(&mut app, "mail");
    pump_until(&mut app, "the root peek", |_| spy.peek_count() == 1);
    spy.peeks.lock().unwrap().clear();
    *spy.hold.lock().unwrap() = true;
    open_pooled(&mut app, "inbound/");
    // One peek is inside the store; the other three wait in the queue behind it.
    pump_until(&mut app, "the first peek", |_| spy.peek_count() == 1);
    press_pooled(&mut app, KeyCode::Backspace);
    release(&spy);
    pump_until(&mut app, "the folder above", |a| {
        screen(a, 100, 20).contains("pictures/")
    });
    std::thread::sleep(Duration::from_millis(100));
    app.pump();
    let inbound = spy
        .peeks
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, _, _)| k.starts_with("inbound/"))
        .count();
    assert_eq!(inbound, 1, "the queued peeks were skipped");
}

#[test]
fn stopping_a_search_drops_its_queued_peeks() {
    let dir = tempfile::tempdir().unwrap();
    let s = MemoryStore::new();
    for i in 0..40 {
        s.put("bk", &format!("deep/msg-{i:02}"), EMAIL);
    }
    let spy = Spy::new(s);
    let mut app = pooled(dir.path(), &spy);
    pump_until(&mut app, "the bucket list", |a| {
        screen(a, 100, 20).contains("bk")
    });
    open_pooled(&mut app, "bk");
    // The folder view peeks nothing: the only row is a folder.
    *spy.hold.lock().unwrap() = true;
    press_pooled(&mut app, KeyCode::Char('s'));
    pump_until(&mut app, "the first search peek", |_| spy.peek_count() == 1);
    press_pooled(&mut app, KeyCode::Char('x'));
    release(&spy);
    std::thread::sleep(Duration::from_millis(500));
    app.pump();
    assert_eq!(spy.peek_count(), 1, "the queued search peeks were skipped");
    assert!(screen(&mut app, 100, 20).contains("stopped"));
}

fn press_pooled(app: &mut App, code: KeyCode) {
    app.key(key(code));
    app.pump();
}

/// `open` for the pooled app: select the row, press Enter, wait for the listing.
fn open_pooled(app: &mut App, name: &str) {
    for _ in 0..50 {
        let s = screen(app, 100, 20);
        if selected_row(&s).contains(name) {
            press_pooled(app, KeyCode::Enter);
            let path = name.to_string();
            pump_until(app, "the listing", |a| {
                let s = screen(a, 100, 20);
                s.contains(&path) && !s.contains("loading")
            });
            return;
        }
        press_pooled(app, KeyCode::Down);
    }
    panic!("never selected {name}");
}

#[test]
fn a_wide_name_that_fits_is_shown_whole() {
    let dir = tempfile::tempdir().unwrap();
    let s = MemoryStore::new();
    // Ten characters, twenty columns: the column must be sized in columns to hold it.
    let wide = "受信メール保存フォルダ";
    s.put("bk", "a", TEXT);
    s.put("bk", wide, EMAIL);
    s.put("bk", &format!("{wide}/m1"), EMAIL);
    let spy = Spy::new(s);
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "bk");
    let spaced: String = wide.chars().map(|c| format!("{c} ")).collect();
    let spaced = spaced.trim_end();
    let s = screen(&mut app, 100, 12);
    assert!(s.contains(spaced), "cut although it fits:\n{s}");
    press(&mut app, KeyCode::Char('s'));
    let s = screen(&mut app, 100, 12);
    assert!(s.contains(spaced), "cut in the search results:\n{s}");
}

#[test]
fn the_inbox_gets_the_buckets_own_region_once_the_client_has_learned_it() {
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::new(mail_store());
    *spy.learned_region.lock().unwrap() = Some("ap-south-1".into());
    let mut app = app_on(dir.path(), &spy);
    open(&mut app, "mail");
    open(&mut app, "inbound/");
    press(&mut app, KeyCode::Char('i'));
    let saved = AppConfig::load(&dir.path().join("config.toml")).unwrap();
    let inbox = saved.inbox.expect("saved");
    assert_eq!(
        inbox.region.as_deref(),
        Some("ap-south-1"),
        "not the session's us-east-1"
    );
    assert_eq!(inbox.profile, "test");
}

#[test]
fn the_header_bar_names_the_browsers_own_account() {
    let dir = tempfile::tempdir().unwrap();
    let mine = Spy::new(mail_store());
    let theirs: Arc<dyn Store> = Arc::new(MemoryStore::new());
    // The shared session is the inbox's account; the browser is on another one.
    let ctx = testing::ctx(dir.path(), Some(theirs));
    let screen_view = BrowserScreen::with_session(session_on("mine", mine.clone()));
    let mut app = App::with_view(ctx, Box::new(screen_view));
    settle(&mut app);
    let s = screen(&mut app, 100, 12);
    let header = s.lines().next().unwrap();
    assert!(header.contains("mine (eu-west-1)"), "{header}");
    assert!(!header.contains("test (us-east-1)"), "{header}");
    // With the bar naming it, the body does not repeat it.
    assert!(!s.contains("as mine"), "{s}");
}
