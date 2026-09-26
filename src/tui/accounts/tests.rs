use std::fs;
use std::path::Path;
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::AccountsScreen;
use crate::aws_profile::{CredentialsFile, Profile};
use crate::config::AppConfig;
use crate::s3::{MemoryStore, Store};
use crate::tui::testing::{self, chars, key, screen, settle};
use crate::tui::{App, Session, Status};

// Obviously fake keys. The secret is distinctive so a leak into the rendered screen is easy
// to spot, and the pieces below are what a partial leak would look like.
const SECRET: &str = "FAKEsecretQZX9fakeFAKE0000";
const TOKEN: &str = "FAKEtokenJWV7fakeFAKE1111";

const TWO_PROFILES: &str = "\
[default]
aws_access_key_id = AKIAFAKEDEFAULT00001
aws_secret_access_key = fakeDefaultSecret
region = eu-west-1

[work]
aws_access_key_id = AKIAFAKEWORK00000002
aws_secret_access_key = fakeWorkSecret
region = ap-southeast-2
";

fn store() -> Arc<MemoryStore> {
    let s = MemoryStore::new();
    s.create_bucket("mail-archive");
    s.create_bucket("photos");
    Arc::new(s)
}

/// An accounts screen whose connections go to `store` instead of real S3.
fn app(dir: &Path, creds: Option<&str>, default: Option<&str>) -> App {
    if let Some(text) = creds {
        fs::write(dir.join("credentials"), text).unwrap();
    }
    let mut ctx = testing::ctx(dir, None);
    ctx.config.default_profile = default.map(str::to_string);
    let store = store();
    let screen = AccountsScreen::new(&mut ctx).with_connector(move |profile: Profile| {
        let region = profile.region.clone().unwrap_or_else(|| "us-east-1".into());
        Session {
            profile,
            region,
            store: Arc::clone(&store) as Arc<dyn Store>,
        }
    });
    let mut app = App::with_view(ctx, Box::new(screen));
    settle(&mut app);
    app
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn press(app: &mut App, code: KeyCode) {
    app.key(key(code));
    settle(app);
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

/// Type into the add form: name, key id, secret, token, region, one field per Tab.
fn fill_form(app: &mut App, name: &str, key_id: &str, secret: &str, token: &str, region: &str) {
    for (i, value) in [name, key_id, secret, token, region].iter().enumerate() {
        if i > 0 {
            press(app, KeyCode::Tab);
        }
        chars(app, value);
    }
}

fn save(app: &mut App) {
    app.key(ctrl('s'));
    settle(app);
}

// ---- the list ----

#[test]
fn lists_every_profile_with_region_and_marks_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some(TWO_PROFILES), Some("work"));
    let s = screen(&mut app, 80, 12);
    assert!(s.contains("Accounts"), "{s}");
    let default_line = line_with(&s, "eu-west-1");
    assert!(default_line.contains("default"), "{s}");
    let work = line_with(&s, "work");
    assert!(work.contains("ap-southeast-2"), "{s}");
    assert!(work.contains("(default)"), "work is the default: {s}");
    assert!(
        !default_line.contains("(default)"),
        "only the configured default gets the mark: {s}"
    );
}

#[test]
fn a_missing_credentials_file_shows_a_hint() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    let s = screen(&mut app, 100, 12);
    assert!(s.contains("does not exist"), "{s}");
    assert!(s.contains("press a"), "{s}");
}

#[test]
fn an_empty_credentials_file_shows_a_hint() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some("# nothing here yet\n"), None);
    let s = screen(&mut app, 100, 12);
    assert!(s.contains("No profiles"), "{s}");
    assert!(s.contains("press a"), "{s}");
}

#[test]
fn footer_shows_the_key_hints() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some(TWO_PROFILES), None);
    let s = screen(&mut app, 100, 12);
    let footer = s.lines().last().unwrap();
    for hint in ["enter", "connect", "add", "default"] {
        assert!(footer.contains(hint), "missing {hint}: {footer}");
    }
}

#[test]
fn enter_connects_the_selected_profile_and_opens_the_browser() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some(TWO_PROFILES), None);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    let session = app.ctx.session.as_ref().expect("connected");
    assert_eq!(session.profile.name, "work");
    assert_eq!(session.profile.access_key_id, "AKIAFAKEWORK00000002");
    assert_eq!(app.stack.len(), 2);
    let s = screen(&mut app, 80, 12);
    assert!(s.contains("Browse S3"), "{s}");
    assert!(s.contains("work (ap-southeast-2)"), "{s}");
    assert!(s.contains("mail-archive"), "the browser lists buckets: {s}");
}

#[test]
fn arrows_and_k_move_the_selection() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some(TWO_PROFILES), None);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down); // stays on the last row
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Up); // stays on the first row
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.ctx.session.as_ref().unwrap().profile.name, "work");
}

#[test]
fn d_makes_the_selected_profile_the_default_and_saves_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some(TWO_PROFILES), Some("default"));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('d'));
    assert_eq!(app.ctx.config.default_profile.as_deref(), Some("work"));
    let saved = AppConfig::load(&dir.path().join("config.toml")).unwrap();
    assert_eq!(saved.default_profile.as_deref(), Some("work"));
    let s = screen(&mut app, 80, 12);
    assert!(line_with(&s, "ap-southeast-2").contains("(default)"), "{s}");
    assert!(!line_with(&s, "eu-west-1").contains("(default)"), "{s}");
}

#[test]
fn going_back_from_the_browser_returns_to_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some(TWO_PROFILES), None);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.stack.len(), 1);
    let s = screen(&mut app, 80, 12);
    assert!(s.contains("Accounts"), "{s}");
}

// ---- the add form ----

#[test]
fn a_opens_the_add_form_with_every_field() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    let s = screen(&mut app, 100, 20);
    for label in [
        "Profile name",
        "Access key ID",
        "Secret access key",
        "Session token",
        "Region",
    ] {
        assert!(s.contains(label), "missing {label}: {s}");
    }
    let footer = s.lines().last().unwrap();
    assert!(
        footer.contains("esc") && footer.contains("cancel"),
        "{footer}"
    );
}

#[test]
fn the_secret_never_renders_while_masked() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(
        &mut app,
        "fake",
        "AKIAFAKEFAKE00000003",
        SECRET,
        TOKEN,
        "us-west-2",
    );
    for (w, h) in [(100, 20), (60, 16), (200, 40)] {
        let s = screen(&mut app, w, h);
        for leak in [SECRET, "QZX9", "FAKEsecret", TOKEN, "JWV7", "FAKEtoken"] {
            assert!(!s.contains(leak), "{leak} leaked at {w}x{h}:\n{s}");
        }
        assert!(
            s.contains("AKIAFAKEFAKE00000003"),
            "the key id is not secret: {s}"
        );
        assert!(s.contains("us-west-2"), "{s}");
    }
    // The masked field still shows that something was typed.
    let s = screen(&mut app, 100, 20);
    assert!(line_with(&s, "Secret access key").contains("****"), "{s}");
}

#[test]
fn ctrl_r_reveals_the_secret_and_hides_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(&mut app, "fake", "AKIAFAKEFAKE00000003", SECRET, "", "");
    app.key(ctrl('r'));
    let s = screen(&mut app, 100, 20);
    assert!(s.contains(SECRET), "revealed: {s}");
    app.key(ctrl('r'));
    let s = screen(&mut app, 100, 20);
    assert!(
        !s.contains(SECRET) && !s.contains("QZX9"),
        "masked again: {s}"
    );
}

#[test]
fn tab_and_shift_tab_move_between_fields() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    chars(&mut app, "abc");
    press(&mut app, KeyCode::Tab);
    chars(&mut app, "AKIA");
    press(&mut app, KeyCode::BackTab);
    chars(&mut app, "def");
    let s = screen(&mut app, 100, 20);
    assert!(line_with(&s, "Profile name").contains("abcdef"), "{s}");
    assert!(line_with(&s, "Access key ID").contains("AKIA"), "{s}");
    assert!(!line_with(&s, "Access key ID").contains("def"), "{s}");
    press(&mut app, KeyCode::Backspace);
    let s = screen(&mut app, 100, 20);
    assert!(line_with(&s, "Profile name").contains("abcde"), "{s}");
    assert!(!line_with(&s, "Profile name").contains("abcdef"), "{s}");
}

#[test]
fn saving_an_empty_form_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    save(&mut app);
    assert!(status_error(&app).contains("name"), "{:?}", app.ctx.status);
    assert!(!dir.path().join("credentials").exists());
    assert!(app.ctx.session.is_none());
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("Profile name"), "the form stays open: {s}");
}

#[test]
fn validation_rejects_bad_fields() {
    let cases: [(&str, &str, &str, &str, &str, &str); 5] = [
        ("has space", "AKIAFAKEFAKE00000003", SECRET, "", "", "name"),
        ("[bad]", "AKIAFAKEFAKE00000003", SECRET, "", "", "name"),
        ("ok", "", SECRET, "", "", "access key"),
        ("ok", "AKIA-not/valid", SECRET, "", "", "access key"),
        ("ok", "AKIAFAKEFAKE00000003", "", "", "", "secret"),
    ];
    for (name, key_id, secret, token, region, complaint) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), None, None);
        press(&mut app, KeyCode::Char('a'));
        fill_form(&mut app, name, key_id, secret, token, region);
        save(&mut app);
        let err = status_error(&app).to_lowercase();
        assert!(err.contains(complaint), "{name}/{key_id}: {err}");
        assert!(!dir.path().join("credentials").exists(), "{name}");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(
        &mut app,
        "ok",
        "AKIAFAKEFAKE00000003",
        SECRET,
        "",
        "Not A Region",
    );
    save(&mut app);
    assert!(status_error(&app).to_lowercase().contains("region"));
    assert!(!dir.path().join("credentials").exists());
}

#[test]
fn a_validation_error_does_not_echo_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(
        &mut app,
        "ok",
        "AKIAFAKEFAKE00000003",
        "FAKE secret with spaces QZX9",
        "",
        "",
    );
    save(&mut app);
    let err = status_error(&app);
    assert!(!err.contains("QZX9"), "{err}");
    let s = screen(&mut app, 120, 20);
    assert!(!s.contains("QZX9"), "{s}");
}

#[test]
fn saving_writes_the_profile_makes_it_default_and_connects() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(
        &mut app,
        "newacct",
        "AKIAFAKEFAKE00000003",
        SECRET,
        TOKEN,
        "us-west-2",
    );
    save(&mut app);

    let file = CredentialsFile::load(&dir.path().join("credentials")).unwrap();
    let written = file.get("newacct").expect("profile written");
    assert_eq!(
        written,
        Profile {
            name: "newacct".into(),
            access_key_id: "AKIAFAKEFAKE00000003".into(),
            secret_access_key: SECRET.into(),
            session_token: Some(TOKEN.into()),
            region: Some("us-west-2".into()),
        }
    );
    let saved = AppConfig::load(&dir.path().join("config.toml")).unwrap();
    assert_eq!(saved.default_profile.as_deref(), Some("newacct"));

    assert_eq!(app.ctx.session.as_ref().unwrap().profile.name, "newacct");
    let s = screen(&mut app, 80, 12);
    assert!(s.contains("Browse S3"), "{s}");
    assert!(s.contains("mail-archive"), "{s}");
    // Back from the browser lands on a list that now holds the new profile.
    press(&mut app, KeyCode::Esc);
    let s = screen(&mut app, 80, 12);
    assert!(line_with(&s, "newacct").contains("us-west-2"), "{s}");
}

#[test]
fn saving_keeps_an_existing_default_and_the_other_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), Some(TWO_PROFILES), Some("work"));
    press(&mut app, KeyCode::Char('a'));
    fill_form(&mut app, "third", "AKIAFAKEFAKE00000003", SECRET, "", "");
    save(&mut app);
    assert_eq!(app.ctx.config.default_profile.as_deref(), Some("work"));
    let file = CredentialsFile::load(&dir.path().join("credentials")).unwrap();
    let names: Vec<String> = file.profiles().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["default", "work", "third"]);
    let third = file.get("third").unwrap();
    assert_eq!(third.session_token, None, "an empty token is not written");
    assert_eq!(third.region, None, "an empty region is not written");
}

#[test]
fn a_duplicate_name_asks_before_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let mut app = app(dir.path(), Some(TWO_PROFILES), None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(&mut app, "work", "AKIAFAKEFAKE00000009", SECRET, "", "");
    save(&mut app);
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("already exists"), "{s}");
    assert!(s.contains("y"), "{s}");
    assert_eq!(fs::read_to_string(&path).unwrap(), TWO_PROFILES);

    // Anything but y goes back to the form without writing.
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(fs::read_to_string(&path).unwrap(), TWO_PROFILES);
    assert!(app.ctx.session.is_none());
    let s = screen(&mut app, 100, 20);
    assert!(s.contains("Profile name"), "{s}");

    save(&mut app);
    press(&mut app, KeyCode::Char('y'));
    let file = CredentialsFile::load(&path).unwrap();
    assert_eq!(
        file.get("work").unwrap().access_key_id,
        "AKIAFAKEFAKE00000009"
    );
    assert_eq!(file.profiles().len(), 2);
    assert_eq!(app.ctx.session.as_ref().unwrap().profile.name, "work");
}

#[test]
fn esc_cancels_the_form_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(&mut app, "newacct", "AKIAFAKEFAKE00000003", SECRET, "", "");
    press(&mut app, KeyCode::Esc);
    assert!(!dir.path().join("credentials").exists());
    assert!(!dir.path().join("config.toml").exists());
    assert!(app.ctx.session.is_none());
    assert_eq!(app.stack.len(), 1);
    let s = screen(&mut app, 100, 12);
    assert!(s.contains("does not exist"), "back on the list: {s}");
    assert!(!s.contains("Profile name"), "{s}");
}

#[test]
fn q_on_the_form_is_typed_not_quit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    chars(&mut app, "qd");
    assert!(!app.quit);
    let s = screen(&mut app, 100, 20);
    assert!(line_with(&s, "Profile name").contains("qd"), "{s}");
}

#[test]
fn pasted_values_are_trimmed_before_saving() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    fill_form(
        &mut app,
        " pasted ",
        " AKIAFAKEFAKE00000003 ",
        &format!(" {SECRET} "),
        "",
        " us-west-2 ",
    );
    save(&mut app);
    assert!(app.ctx.session.is_some(), "{:?}", app.ctx.status);
    let file = CredentialsFile::load(&dir.path().join("credentials")).unwrap();
    let p = file.get("pasted").expect("saved under the trimmed name");
    assert_eq!(p.access_key_id, "AKIAFAKEFAKE00000003");
    assert_eq!(p.secret_access_key, SECRET);
    assert_eq!(p.region.as_deref(), Some("us-west-2"));
}

#[test]
fn the_mask_does_not_give_away_the_length() {
    let dir = tempfile::tempdir().unwrap();
    let mut masks = Vec::new();
    for secret in [
        "FAKEx",
        "FAKEsecretQZX9fakeFAKE0000FAKEsecretQZX9fakeFAKE0000",
    ] {
        let mut app = app(dir.path(), None, None);
        press(&mut app, KeyCode::Char('a'));
        fill_form(&mut app, "fake", "AKIAFAKEFAKE00000003", secret, secret, "");
        // Move off the masked fields so only the mask itself differs.
        press(&mut app, KeyCode::Tab);
        let s = screen(&mut app, 120, 20);
        let secret_line = line_with(&s, "Secret access key").to_string();
        let token_line = line_with(&s, "Session token").to_string();
        assert!(secret_line.contains('*'), "{s}");
        masks.push((secret_line, token_line));
    }
    assert_eq!(
        masks[0], masks[1],
        "a 5 and a 52 character secret look the same"
    );
}

#[test]
fn an_empty_masked_field_shows_no_mask() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path(), None, None);
    press(&mut app, KeyCode::Char('a'));
    let s = screen(&mut app, 120, 20);
    assert!(!line_with(&s, "Secret access key").contains('*'), "{s}");
}

#[test]
fn the_footer_says_back_not_quit_when_pushed_over_another_screen() {
    let dir = tempfile::tempdir().unwrap();
    // As the first screen there is no session yet, and q quits.
    let mut root = app(dir.path(), Some(TWO_PROFILES), None);
    let s = screen(&mut root, 100, 12);
    let footer = s.lines().last().unwrap();
    assert!(
        footer.contains("quit") && !footer.contains("back"),
        "{footer}"
    );

    // Pushed from the inbox with `u`, a session is already open and q goes back.
    fs::write(dir.path().join("credentials"), TWO_PROFILES).unwrap();
    let mut ctx = testing::ctx(dir.path(), Some(store() as Arc<dyn Store>));
    let screen_view = AccountsScreen::new(&mut ctx);
    let mut pushed = App::with_view(ctx, Box::new(screen_view));
    let s = screen(&mut pushed, 100, 12);
    let footer = s.lines().last().unwrap();
    assert!(
        footer.contains("back") && !footer.contains("quit"),
        "{footer}"
    );

    // The empty-file hint follows the same rule.
    let empty = tempfile::tempdir().unwrap();
    let mut ctx = testing::ctx(empty.path(), Some(store() as Arc<dyn Store>));
    let screen_view = AccountsScreen::new(&mut ctx);
    let mut pushed = App::with_view(ctx, Box::new(screen_view));
    let s = screen(&mut pushed, 100, 12);
    let footer = s.lines().last().unwrap();
    assert!(
        footer.contains("back") && !footer.contains("quit"),
        "{footer}"
    );
}
