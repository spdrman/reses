//! Tests for the terminal-facing findings in #28 that cut across screens: what reaches the
//! terminal byte for byte, what a paste or a chord can do, how the filter checks rows nobody
//! has scrolled to, the delete confirmation's layout, colours that hold up on any theme, emoji
//! widths, and the key hints at 80 columns.
//!
//! I drive the real screens through `App`, the way the run loop does, against in-memory stores.

use std::io::Write;
use std::sync::{Arc, Mutex};

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::browser::BrowserScreen;
use super::inbox::InboxScreen;
use super::inbox::fixtures::*;
use super::testing::{self, buffer, chars, key, screen, settle};
use super::{App, Status};
use crate::s3::{MemoryStore, Store};

/// Everything the backend writes, kept where the test can read it back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The bytes a real terminal would receive for one frame of `app`.
fn bytes(app: &mut App, width: u16, height: u16) -> Vec<u8> {
    let out = Capture::default();
    let mut term = Terminal::with_options(
        CrosstermBackend::new(out.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
        },
    )
    .unwrap();
    term.draw(|f| app.render(f)).unwrap();
    drop(term);
    out.0.lock().unwrap().clone()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A key that writes the clipboard (OSC 52) and then moves the cursor off the bottom of any
/// screen the test draws (CSI 99;1H): what a hostile object name can carry.
const HOSTILE: &str = "mail/\u{1b}]52;c;UEFXTkVE\u{7}\u{1b}[99;1Hx.eml";

fn hostile_store() -> Arc<Timed> {
    let s = Timed::new();
    s.put(
        BUCKET,
        HOSTILE,
        &email(
            "x@example.com",
            "Hostile",
            "Fri, 25 Sep 2026 09:30:00 +0000",
        ),
    );
    s
}

fn inbox_app(store: Arc<dyn Store>) -> (App, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = testing::ctx(dir.path(), Some(store));
    let view = InboxScreen::new(inbox())
        .with_now(time::macros::datetime!(2026-09-25 15:00 UTC))
        .with_downloads_dir(dir.path().join("dl"));
    let mut app = App::with_view(ctx, Box::new(view));
    settle(&mut app);
    (app, dir)
}

fn with_mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// Assert no raw escape sequence from `HOSTILE` reached the terminal, and its visible
/// spelling did.
fn assert_escaped(out: &[u8], what: &str) {
    assert!(
        !contains(out, b"\x1b]52"),
        "{what}: OSC 52 reached the terminal"
    );
    assert!(
        !contains(out, b"\x1b[99;1H"),
        "{what}: the cursor move reached the terminal"
    );
    assert!(
        !contains(out, b"\x07"),
        "{what}: a BEL reached the terminal"
    );
    assert!(
        contains(out, b"\\x1b]52"),
        "{what}: the escaped key isn't on screen"
    );
}

#[test]
fn a_hostile_key_in_the_delete_confirmation_is_written_out_not_sent() {
    let (mut app, _d) = inbox_app(hostile_store());
    app.key(key(KeyCode::Char('d')));
    let out = bytes(&mut app, 100, 20);
    assert_escaped(&out, "confirmation");
}

#[test]
fn a_hostile_key_in_the_browser_is_written_out_not_sent() {
    let store = Arc::new(MemoryStore::new());
    store.put(BUCKET, HOSTILE, b"not mail");
    let dir = tempfile::tempdir().unwrap();
    let ctx = testing::ctx(dir.path(), Some(store as Arc<dyn Store>));
    let mut app = App::with_view(ctx, Box::new(BrowserScreen::new()));
    settle(&mut app);
    // Into the bucket, then the folder.
    app.key(key(KeyCode::Enter));
    settle(&mut app);
    app.key(key(KeyCode::Enter));
    settle(&mut app);
    let out = bytes(&mut app, 100, 20);
    assert_escaped(&out, "browser row");
}

#[test]
fn a_hostile_error_or_title_is_written_out_not_sent() {
    let (mut app, _d) = inbox_app(hostile_store());
    app.ctx.error(format!("could not delete {HOSTILE}"));
    let out = bytes(&mut app, 100, 20);
    assert_escaped(&out, "status line");

    // A prefix with a control in it reaches the header title.
    let dir = tempfile::tempdir().unwrap();
    let store = Timed::new();
    store.create_bucket(BUCKET);
    let ctx = testing::ctx(dir.path(), Some(store as Arc<dyn Store>));
    let mut inbox = inbox();
    inbox.prefix = HOSTILE.to_string() + "/";
    let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox)));
    settle(&mut app);
    let out = bytes(&mut app, 200, 20);
    assert_escaped(&out, "header title");
}

#[test]
fn a_paste_is_never_read_as_keys() {
    let store = hostile_store();
    let (mut app, _d) = inbox_app(store.clone());
    // "dy" typed would delete the selected message; pasted, it must not.
    app.paste("dy");
    settle(&mut app);
    assert!(store.contains(BUCKET, HOSTILE));
    assert!(!screen(&mut app, 100, 20).contains("Press y"));
    // A paste into an open confirmation is a no, not a y.
    app.key(key(KeyCode::Char('d')));
    app.paste("y");
    settle(&mut app);
    assert!(store.contains(BUCKET, HOSTILE));
    assert!(!screen(&mut app, 100, 20).contains("Press y"));
}

#[test]
fn only_a_bare_d_and_a_bare_y_delete() {
    let store = hostile_store();
    let (mut app, _d) = inbox_app(store.clone());
    for m in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        app.key(with_mods(KeyCode::Char('d'), m));
        assert!(
            !screen(&mut app, 100, 20).contains("Press y"),
            "{m:?}-d opened the confirmation"
        );
    }
    for m in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        app.key(key(KeyCode::Char('d')));
        app.key(with_mods(KeyCode::Char('y'), m));
        settle(&mut app);
        assert!(store.contains(BUCKET, HOSTILE), "{m:?}-y deleted it");
    }
    // The bare pair still works.
    app.key(key(KeyCode::Char('d')));
    app.key(key(KeyCode::Char('y')));
    settle(&mut app);
    assert!(!store.contains(BUCKET, HOSTILE));
}

#[test]
fn the_message_screen_takes_only_a_bare_d_and_y_too() {
    let store = hostile_store();
    let (mut app, _d) = inbox_app(store.clone());
    app.key(key(KeyCode::Enter));
    settle(&mut app);
    app.key(with_mods(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(!screen(&mut app, 100, 20).contains("Press y"));
    app.key(key(KeyCode::Char('d')));
    app.paste("y");
    app.key(with_mods(KeyCode::Char('y'), KeyModifiers::CONTROL));
    settle(&mut app);
    assert!(store.contains(BUCKET, HOSTILE));
}

#[test]
fn a_paste_goes_into_the_filter_as_text() {
    let (mut app, _d) = inbox_app(three_senders());
    app.key(key(KeyCode::Char('/')));
    app.paste("Alice\nExample");
    settle(&mut app);
    let scr = screen(&mut app, 100, 20);
    assert!(scr.contains("/AliceExample"), "{scr}");
}

#[test]
fn ctrl_and_alt_chords_are_not_typed_into_filters() {
    let (mut app, _d) = inbox_app(three_senders());
    app.key(key(KeyCode::Char('/')));
    app.key(with_mods(KeyCode::Char('a'), KeyModifiers::CONTROL));
    app.key(with_mods(KeyCode::Char('x'), KeyModifiers::ALT));
    chars(&mut app, "bob");
    let scr = screen(&mut app, 100, 20);
    assert!(scr.contains("/bob_"), "{scr}");
    assert!(!scr.contains("/abob") && !scr.contains("/xbob"), "{scr}");

    // The browser's filter too.
    let store = Arc::new(MemoryStore::new());
    store.put(BUCKET, "mail/one", b"x");
    let dir = tempfile::tempdir().unwrap();
    let ctx = testing::ctx(dir.path(), Some(store as Arc<dyn Store>));
    let mut app = App::with_view(ctx, Box::new(BrowserScreen::new()));
    settle(&mut app);
    app.key(key(KeyCode::Char('/')));
    app.key(with_mods(KeyCode::Char('a'), KeyModifiers::CONTROL));
    app.key(with_mods(KeyCode::Char('x'), KeyModifiers::ALT));
    app.key(key(KeyCode::Char('i')));
    let scr = screen(&mut app, 100, 20);
    assert!(scr.contains("/i_"), "{scr}");
}

/// Three senders, newest first: Alice, Bob, Carol.
fn three_senders() -> Arc<Timed> {
    let s = Timed::new();
    for (key, from, subject, date) in [
        (
            "mail/a",
            "Alice Example <alice@example.com>",
            "From Alice",
            "25 Sep 2026 09:00:00 +0000",
        ),
        (
            "mail/b",
            "bob@example.com",
            "From Bob",
            "24 Sep 2026 09:00:00 +0000",
        ),
        (
            "mail/c",
            "carol@example.com",
            "From Carol",
            "23 Sep 2026 09:00:00 +0000",
        ),
    ] {
        s.put(BUCKET, key, &email(from, subject, date));
    }
    s
}

/// 100 messages, newest first, with one match for "Needle" near the end.
fn haystack() -> Arc<Timed> {
    let s = Timed::new();
    for i in 0..100 {
        let subject = if i == 90 {
            "Needle in here".to_string()
        } else {
            format!("Hay {i:03}")
        };
        s.put_received(
            BUCKET,
            &format!("mail/m{i:03}"),
            &email("a@example.com", &subject, "25 Sep 2026 10:00:00 +0000"),
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(1000 - i),
        );
    }
    s
}

#[test]
fn the_filter_finds_rows_nobody_scrolled_to() {
    let (mut app, _d) = inbox_app(haystack());
    let _ = screen(&mut app, 100, 20);
    settle(&mut app);
    app.key(key(KeyCode::Char('/')));
    chars(&mut app, "Needle");
    let scr = screen(&mut app, 100, 20);
    assert!(scr.contains("Needle in here"), "{scr}");
    assert!(!scr.contains("Loading"), "{scr}");
}

#[test]
fn the_filter_says_how_much_it_has_checked_so_far() {
    let (mut app, _d) = inbox_app(haystack());
    let _ = screen(&mut app, 100, 20);
    settle(&mut app);
    app.key(key(KeyCode::Char('/')));
    app.paste("Needle");
    // Before the batch it just asked for comes back.
    let scr = screen(&mut app, 100, 20);
    assert!(scr.contains("of 100 checked"), "{scr}");
    assert!(!scr.contains("Loading"), "{scr}");
    settle(&mut app);
    let scr = screen(&mut app, 100, 20);
    assert!(scr.contains("Needle in here"), "{scr}");
    assert!(!scr.contains("checked"), "all checked, so no count:\n{scr}");
}

#[test]
fn the_confirmation_keeps_its_prompt_whatever_the_key() {
    // A key with spaces made word wrap take more rows than the box had.
    let key_name = format!("mail/{}", "word ".repeat(30));
    let s = Timed::new();
    s.put(
        BUCKET,
        &key_name,
        &email(
            "x@example.com",
            "Long key",
            "Fri, 25 Sep 2026 09:30:00 +0000",
        ),
    );
    let (mut app, _d) = inbox_app(s);
    app.key(key(KeyCode::Char('d')));
    for (w, h) in [(100u16, 20u16), (60, 20), (44, 16), (44, 8), (30, 6)] {
        let scr = screen(&mut app, w, h);
        assert!(scr.contains("Press y"), "{w}x{h}:\n{scr}");
        if h >= 16 {
            // Every word of the key is inside the box somewhere.
            let inside: String = scr
                .lines()
                .filter_map(|l| l.split('│').nth(1))
                .collect::<Vec<_>>()
                .join("");
            // Rows are cut by width, so a word can straddle two; count with the spaces out.
            let words = inside.replace(' ', "").matches("word").count();
            assert_eq!(words, 30, "{w}x{h}: {words} of 30 words:\n{scr}");
        }
    }
}

/// Every cell's colours, for scanning a frame.
fn colours(buf: &Buffer) -> Vec<(u16, u16, Color, Color)> {
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            out.push((x, y, c.fg, c.bg));
        }
    }
    out
}

/// Fixed colours that measured unreadable on some common theme.
fn is_fragile(c: Color) -> bool {
    matches!(
        c,
        Color::DarkGray
            | Color::Gray
            | Color::Yellow
            | Color::LightYellow
            | Color::Green
            | Color::LightGreen
            | Color::Blue
            | Color::Cyan
    )
}

fn assert_no_fragile_colours(app: &mut App, what: &str) {
    let buf = buffer(app, 100, 20);
    for (x, y, fg, bg) in colours(&buf) {
        assert!(!is_fragile(fg), "{what}: fg {fg:?} at {x},{y}");
        assert!(!is_fragile(bg), "{what}: bg {bg:?} at {x},{y}");
    }
}

#[test]
fn no_screen_uses_a_colour_that_vanishes_on_some_theme() {
    // The inbox with its placeholder rows, before any header has come back: one pump
    // handles the listing and leaves the peeks queued.
    let dir = tempfile::tempdir().unwrap();
    let ctx = testing::ctx(dir.path(), Some(haystack() as Arc<dyn Store>));
    let mut app = App::with_view(ctx, Box::new(InboxScreen::new(inbox())));
    app.pump();
    assert!(
        screen(&mut app, 100, 20).contains("loading"),
        "no placeholder rows to check"
    );
    assert_no_fragile_colours(&mut app, "inbox placeholders");
    // And once the rows are in, with the filter line.
    let (mut app, _d) = inbox_app(haystack());
    assert_no_fragile_colours(&mut app, "inbox");
    app.key(key(KeyCode::Char('/')));
    assert_no_fragile_colours(&mut app, "inbox filter");
    // Info on the status line.
    app.ctx.info("Deleted s3://b/k");
    assert_no_fragile_colours(&mut app, "info status");

    // The browser: buckets, then a folder with folders, email and not-email marks.
    let store = Arc::new(MemoryStore::new());
    store.put(BUCKET, "mail/sub/x", b"x");
    store.put(
        BUCKET,
        "mail/m",
        &email("a@example.com", "s", "25 Sep 2026 10:00:00 +0000"),
    );
    let dir = tempfile::tempdir().unwrap();
    let ctx = testing::ctx(dir.path(), Some(store as Arc<dyn Store>));
    let mut app = App::with_view(ctx, Box::new(BrowserScreen::new()));
    settle(&mut app);
    assert_no_fragile_colours(&mut app, "bucket list");
    app.key(key(KeyCode::Enter));
    settle(&mut app);
    app.key(key(KeyCode::Enter));
    settle(&mut app);
    assert_no_fragile_colours(&mut app, "folder");
    app.key(key(KeyCode::Char('s')));
    settle(&mut app);
    assert_no_fragile_colours(&mut app, "search");

    // The accounts list and the add-account form.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials"),
        "[default]\naws_access_key_id = AKIDEXAMPLE\naws_secret_access_key = x\n",
    )
    .unwrap();
    let mut ctx = testing::ctx(dir.path(), None);
    ctx.config.default_profile = Some("default".into());
    let accounts = super::accounts::AccountsScreen::new(&mut ctx);
    let mut app = App::with_view(ctx, Box::new(accounts));
    assert_no_fragile_colours(&mut app, "accounts");
    app.key(key(KeyCode::Char('a')));
    assert_no_fragile_colours(&mut app, "add account");
}

#[test]
fn an_error_says_so_in_words_not_only_in_red() {
    let (mut app, _d) = inbox_app(three_senders());
    app.ctx.error("could not list the folder");
    let scr = screen(&mut app, 100, 20);
    assert!(
        scr.lines()
            .last()
            .unwrap()
            .starts_with(" error: could not list"),
        "{scr}"
    );
    app.ctx.status = Some(Status::Info("Deleted s3://b/k".into()));
    let scr = screen(&mut app, 100, 20);
    assert!(!scr.lines().last().unwrap().contains("error"), "{scr}");
}

#[test]
fn an_emoji_subject_keeps_the_columns_lined_up() {
    let s = Timed::new();
    s.put(
        BUCKET,
        "mail/heart",
        &email(
            "a@example.com",
            "I \u{2764}\u{fe0f} this",
            "Sun, 20 Sep 2026 09:00:00 +0000",
        ),
    );
    s.put(
        BUCKET,
        "mail/plain",
        &email("a@example.com", "Plain", "Sat, 19 Sep 2026 09:00:00 +0000"),
    );
    let (mut app, _d) = inbox_app(s);
    let buf = buffer(&mut app, 100, 10);
    // The column each row's date starts in, counted in cells.
    let date_col = |needle: &str| {
        (0..buf.area.height)
            .find_map(|y| {
                let row: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                let joined: String = row.concat();
                joined.contains(needle).then(|| {
                    (0..buf.area.width)
                        .find(|&x| buf[(x, y)].symbol() == "S" && buf[(x + 1, y)].symbol() == "e")
                        .unwrap()
                })
            })
            .unwrap_or_else(|| panic!("no row with {needle}"))
    };
    assert_eq!(date_col("this"), date_col("Plain"));
}

#[test]
fn quit_stays_in_the_hints_at_80_columns_and_less() {
    let (mut app, _d) = inbox_app(three_senders());
    for w in [80u16, 60, 40, 24] {
        let scr = screen(&mut app, w, 10);
        let footer = scr.lines().last().unwrap();
        assert!(footer.contains(" q  quit"), "{w}: {footer:?}");
    }
    // The most important hints stay too, as long as they fit.
    let scr = screen(&mut app, 80, 10);
    assert!(scr.lines().last().unwrap().contains("enter  open"), "{scr}");

    app.key(key(KeyCode::Enter));
    settle(&mut app);
    for w in [80u16, 50] {
        let scr = screen(&mut app, w, 10);
        assert!(
            scr.lines().last().unwrap().contains(" q  back"),
            "{w}:\n{scr}"
        );
    }
}
