//! Inbox tests that need a real job pool or a big listing: the order work gets served in,
//! how much sorting a long listing costs, dates at the edge of the calendar, and a delete
//! racing a refresh. The stores here hold their calls at a gate so each test can line up
//! exactly the queue it means, with no timing guesses.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::KeyCode;
use time::OffsetDateTime;
use time::macros::datetime;

use super::fixtures::*;
use super::*;
use crate::config::AppConfig;
use crate::s3::{Bucket, Listing, ObjectInfo, S3Error, Store};
use crate::tui::jobs::Jobs;
use crate::tui::testing::{self, key, screen, settle};
use crate::tui::{App, Ctx};

/// A ceiling for a broken fix, never a wait for something to happen by itself.
const LIMIT: Duration = Duration::from_secs(20);

/// A gate the store's calls wait at until the test opens it.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn shut() -> Self {
        Self::default()
    }

    fn opened() -> Self {
        let g = Self::default();
        g.release();
        g
    }

    fn pass(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

/// `pages` listing pages of 1000 synthetic keys. Key i was received at N - i, so the inbox
/// shows them in key order. Header peeks and gets wait at `gate`; every call is logged.
struct Pages {
    pages: usize,
    lists: AtomicUsize,
    gate: Gate,
    log: Mutex<Vec<String>>,
}

impl Pages {
    fn new(pages: usize, gated: bool) -> Arc<Self> {
        Arc::new(Self {
            pages,
            lists: AtomicUsize::new(0),
            gate: if gated { Gate::shut() } else { Gate::opened() },
            log: Mutex::new(Vec::new()),
        })
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

fn keyname(i: usize) -> String {
    format!("{PREFIX}k{i:07}")
}

impl Store for Pages {
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        Ok(Vec::new())
    }
    fn list(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        token: Option<&str>,
    ) -> Result<Listing, S3Error> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        let page: usize = token.map_or(0, |t| t.parse().unwrap());
        let total = self.pages * 1000;
        let objects = (page * 1000..(page + 1) * 1000)
            .map(|i| ObjectInfo {
                key: keyname(i),
                size: 100,
                last_modified: Some(
                    OffsetDateTime::UNIX_EPOCH + time::Duration::seconds((total - i) as i64),
                ),
            })
            .collect();
        Ok(Listing {
            prefixes: Vec::new(),
            objects,
            next_token: (page + 1 < self.pages).then(|| (page + 1).to_string()),
        })
    }
    fn get_range(&self, _: &str, key: &str, _: u64, _: u64) -> Result<Vec<u8>, S3Error> {
        self.log.lock().unwrap().push(format!("peek {key}"));
        self.gate.pass();
        Ok(b"From: a@example.com\r\nSubject: s\r\n\r\n".to_vec())
    }
    fn get(&self, _: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        self.log.lock().unwrap().push(format!("get {key}"));
        self.gate.pass();
        Ok(b"From: a@example.com\r\nSubject: s\r\n\r\nbody\r\n".to_vec())
    }
    fn delete(&self, _: &str, key: &str) -> Result<(), S3Error> {
        self.log.lock().unwrap().push(format!("delete {key}"));
        Ok(())
    }
}

fn app(store: Arc<dyn Store>, jobs: Jobs, dir: &std::path::Path) -> App {
    let mut ctx = Ctx::new(
        AppConfig::default(),
        dir.join("c.toml"),
        dir.join("creds"),
        jobs,
    );
    ctx.session = testing::ctx(dir, Some(store)).session;
    let view = InboxScreen::new(inbox()).with_downloads_dir(dir.join("dl"));
    App::with_view(ctx, Box::new(view))
}

/// Pump until `done` holds, failing the test at the ceiling.
fn pump_until(app: &mut App, what: &str, mut done: impl FnMut(&mut App) -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !done(app) {
        assert!(Instant::now() < deadline, "never happened: {what}");
        app.pump();
        std::thread::yield_now();
    }
}

#[test]
fn a_long_listing_sorts_each_row_about_once() {
    let pages = 50;
    let dir = tempfile::tempdir().unwrap();
    let store = Pages::new(pages, false);
    let mut app = app(store.clone(), Jobs::inline(), dir.path());
    sorted_rows_reset();
    // The run loop: draw, then pump.
    for _ in 0..100_000 {
        let _ = screen(&mut app, 100, 30);
        if app.pump() == 0 && store.lists.load(Ordering::SeqCst) >= pages {
            break;
        }
    }
    let _ = screen(&mut app, 100, 30);
    let rows = pages * 1000;
    let sorted = sorted_rows();
    // Sorting every row again after every page is quadratic: about 1.3 million here.
    assert!(
        sorted <= 2 * rows,
        "{sorted} rows went through a sort for a {rows}-row listing"
    );
    assert!(screen(&mut app, 100, 30).contains(&format!("{rows} messages")));
}

#[test]
fn after_a_fast_scroll_the_rows_on_screen_are_peeked_first() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pages::new(10, true);
    let mut app = app(store.clone(), Jobs::pool(1), dir.path());
    pump_until(&mut app, "the listing and the first peek", |_| {
        store.lists.load(Ordering::SeqCst) >= 10 && !store.log().is_empty()
    });
    // The only worker is now held inside the first peek. Hold PageDown for 40 pages.
    for _ in 0..40 {
        app.key(key(KeyCode::PageDown));
        app.pump();
    }
    let _ = screen(&mut app, 100, 26);
    app.pump();
    let visible = format!("peek {}", keyname(40 * DEFAULT_PAGE));
    store.gate.release();
    pump_until(&mut app, "the selected row's peek", |_| {
        store.log().contains(&visible)
    });
    let log = store.log();
    let at = log.iter().position(|l| *l == visible).unwrap();
    // Served within the current window, not behind forty windows' worth of stale peeks.
    assert!(
        at <= 2 * DEFAULT_PAGE + 1,
        "the row on screen was peek {} of {}",
        at + 1,
        log.len()
    );
}

#[test]
fn opens_from_closed_message_screens_never_run_ahead_of_the_open_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pages::new(1, true);
    let mut app = app(store.clone(), Jobs::pool(2), dir.path());
    pump_until(&mut app, "the listed rows", |app| {
        screen(app, 100, 30).contains("1000 messages")
    });
    for _ in 0..10 {
        app.key(key(KeyCode::Enter));
        app.key(key(KeyCode::Char('q')));
        app.key(key(KeyCode::Down));
    }
    app.key(key(KeyCode::Enter));
    let wanted = format!("get {}", keyname(10));
    store.gate.release();
    pump_until(&mut app, "the open message's get", |_| {
        store.log().contains(&wanted)
    });
    let gets: Vec<String> = store
        .log()
        .into_iter()
        .filter(|l| l.starts_with("get "))
        .collect();
    let at = gets.iter().position(|l| *l == wanted).unwrap();
    // At most the two gets the workers had already started can come first.
    assert!(
        at <= 2,
        "the message on screen was get {} of {gets:?}",
        at + 1
    );
}

/// A store whose deletes and listings can each be held at their own gate. A held listing
/// takes its snapshot first, the way a real one in flight already has.
struct Race {
    inner: Arc<Timed>,
    hold_lists: AtomicBool,
    /// Set once a held listing has taken its snapshot and is waiting at the gate.
    list_held: AtomicBool,
    lists: Gate,
    hold_deletes: AtomicBool,
    deletes: Gate,
}

impl Store for Race {
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
        let snapshot = self.inner.list(bucket, prefix, delimiter, token);
        if self.hold_lists.load(Ordering::SeqCst) {
            self.list_held.store(true, Ordering::SeqCst);
            self.lists.pass();
        }
        snapshot
    }
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        self.inner.get_range(bucket, key, start, end)
    }
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        self.inner.get(bucket, key)
    }
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        if self.hold_deletes.load(Ordering::SeqCst) {
            self.deletes.pass();
        }
        self.inner.delete(bucket, key)
    }
}

#[test]
fn a_delete_that_finishes_during_a_refresh_does_not_bring_the_row_back() {
    let inner = Timed::new();
    inner.put(
        BUCKET,
        "mail/keep",
        &email(
            "k@example.com",
            "Keep me",
            "Fri, 25 Sep 2026 08:00:00 +0000",
        ),
    );
    inner.put(
        BUCKET,
        "mail/gone",
        &email(
            "g@example.com",
            "Delete me",
            "Fri, 25 Sep 2026 09:00:00 +0000",
        ),
    );
    let store = Arc::new(Race {
        inner: inner.clone(),
        hold_lists: AtomicBool::new(false),
        list_held: AtomicBool::new(false),
        lists: Gate::shut(),
        hold_deletes: AtomicBool::new(true),
        deletes: Gate::shut(),
    });
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(store.clone(), Jobs::pool(2), dir.path());
    pump_until(&mut app, "both rows", |app| {
        let s = screen(app, 100, 12);
        s.contains("Delete me") && s.contains("Keep me")
    });

    // Newest first, so "Delete me" is selected. Its delete gets held inside the store.
    app.key(key(KeyCode::Char('d')));
    app.key(key(KeyCode::Char('y')));
    // Refresh while it's held: the new listing still sees the object, and is held too.
    store.hold_lists.store(true, Ordering::SeqCst);
    app.key(key(KeyCode::Char('r')));
    pump_until(&mut app, "the refresh listing to be in flight", |_| {
        store.list_held.load(Ordering::SeqCst)
    });
    // The delete finishes while the listing is in flight, then the listing lands.
    store.deletes.release();
    pump_until(&mut app, "the delete", |app| {
        !inner.contains(BUCKET, "mail/gone")
            && matches!(app.ctx.status, Some(crate::tui::Status::Info(ref m)) if m.contains("Deleted"))
    });
    store.lists.release();
    pump_until(&mut app, "the refreshed listing", |app| {
        screen(app, 100, 12).contains("Keep me")
    });
    settle_pool(&mut app);
    let scr = screen(&mut app, 100, 12);
    assert!(
        !scr.contains("Delete me"),
        "the deleted row came back:\n{scr}"
    );
    assert!(scr.contains("1 message"), "{scr}");
}

/// Pump a pool-backed app until a few rounds in a row bring nothing.
fn settle_pool(app: &mut App) {
    let deadline = Instant::now() + LIMIT;
    let mut quiet = 0;
    while quiet < 50 {
        assert!(Instant::now() < deadline, "the pool never went quiet");
        if app.pump() == 0 {
            quiet += 1;
            std::thread::sleep(Duration::from_millis(1));
        } else {
            quiet = 0;
        }
    }
}

/// Render one message dated `date` in a terminal at `offset`, as the whole app would.
fn render_dated(date: &str, offset: time::UtcOffset) -> String {
    let dir = tempfile::tempdir().unwrap();
    let store = Timed::new();
    store.put_received(
        BUCKET,
        "mail/edge",
        &email("x@example.com", "Edge of time", date),
        datetime!(2026-09-25 12:00 UTC),
    );
    let ctx = testing::ctx(dir.path(), Some(store as Arc<dyn Store>)).with_local_offset(offset);
    let view = InboxScreen::new(inbox())
        .with_now(datetime!(2026-09-25 15:00 UTC))
        .with_downloads_dir(dir.path().join("dl"));
    let mut app = App::with_view(ctx, Box::new(view));
    settle(&mut app);
    let _ = screen(&mut app, 100, 10);
    settle(&mut app);
    screen(&mut app, 100, 10)
}

#[test]
fn dates_at_the_edge_of_the_calendar_render_instead_of_crashing() {
    let hours = |h| time::UtcOffset::from_hms(h, 0, 0).unwrap();
    for (date, offset, shown) in [
        ("Fri, 31 Dec 9999 23:30:00 -0100", hours(0), "9999-12-31"),
        ("Fri, 31 Dec 9999 12:00:00 +0000", hours(13), "9999-12-31"),
        ("Mon, 1 Jan 0001 00:30:00 +0100", hours(-2), "0001-01-01"),
    ] {
        let scr = render_dated(date, offset);
        let row = scr
            .lines()
            .find(|l| l.contains("Edge of time"))
            .unwrap_or_else(|| panic!("{date} at {offset}: no row:\n{scr}"));
        assert!(row.contains(shown), "{date} at {offset}: {row:?}");
    }
}

#[test]
fn the_clock_itself_converts_safely_too() {
    // "Now" at the very end of the calendar, in a terminal ahead of UTC.
    let dir = tempfile::tempdir().unwrap();
    let store = Timed::new();
    store.put(
        BUCKET,
        "mail/a",
        &email("x@example.com", "Plain", "Fri, 25 Sep 2026 12:00:00 +0000"),
    );
    let ctx = testing::ctx(dir.path(), Some(store as Arc<dyn Store>))
        .with_local_offset(time::UtcOffset::from_hms(14, 0, 0).unwrap());
    let view = InboxScreen::new(inbox()).with_now(datetime!(9999-12-31 23:59 UTC));
    let mut app = App::with_view(ctx, Box::new(view));
    settle(&mut app);
    assert!(screen(&mut app, 100, 10).contains("Plain"));
}
