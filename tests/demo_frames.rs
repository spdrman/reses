//! `demo/check-frames.sh` is what stops a recording of a broken screen from replacing
//! docs/demo.gif, so it gets tests of its own: the known-good snapshots in
//! `tests/fixtures/demo/good.txt` pass, and the known-bad ones, and each targeted breakage of
//! the good ones below, fail with the reason named.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The repo root, which every path in these tests hangs off.
fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The known-good snapshots, which each breakage test edits one thing in.
fn good() -> String {
    fs::read_to_string(root().join("tests/fixtures/demo/good.txt")).unwrap()
}

/// The demo's real message list, the same one seed.sh uploads.
fn messages() -> String {
    fs::read_to_string(root().join("demo/messages.tsv")).unwrap()
}

/// A scratch file under the system temp dir, unique to this process and call.
fn scratch(name: &str, contents: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "reses-demo-frames-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path
}

/// Run the check; returns (exit code, stdout + stderr).
fn check(frames: &Path, messages: &Path) -> (i32, String) {
    let out = Command::new("bash")
        .arg(root().join("demo/check-frames.sh"))
        .arg(frames)
        .arg(messages)
        .output()
        .expect("bash runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// Run the check on snapshots and a message list held in memory, by writing both to scratch files.
fn check_text(frames: &str, messages: &str) -> (i32, String) {
    check(
        &scratch("frames.txt", frames),
        &scratch("messages.tsv", messages),
    )
}

/// The check exits 1 and its output names `reason`. A failure for some other reason would let a
/// broken check pass these tests, so the reason matters as much as the exit code.
fn assert_fails(frames: &str, messages: &str, reason: &str) {
    let (code, out) = check_text(frames, messages);
    assert_eq!(code, 1, "expected a failure naming {reason:?}:\n{out}");
    assert!(
        out.contains(reason),
        "the failure does not name {reason:?}:\n{out}"
    );
}

/// The positive control: the good fixture passes, with every snapshot and inbox row counted.
#[test]
fn the_known_good_snapshots_pass() {
    let (code, out) = check(
        &root().join("tests/fixtures/demo/good.txt"),
        &root().join("demo/messages.tsv"),
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("all 8 snapshots checked, 10 inbox rows found"),
        "{out}"
    );
}

/// The bad fixture, a real broken recording, fails on both of the things wrong with it.
#[test]
fn the_known_bad_snapshots_fail() {
    let (code, out) = check(
        &root().join("tests/fixtures/demo/bad.txt"),
        &root().join("demo/messages.tsv"),
    );
    assert_eq!(code, 1, "{out}");
    // A blanked Date cell, and an error on the status line.
    assert!(
        out.contains("FAIL  the saved inbox, every row loaded"),
        "{out}"
    );
    assert!(out.contains("FAIL  error text on screen"), "{out}");
    assert!(out.contains("Could not fetch"), "{out}");
}

/// An inbox row whose subject never loaded fails the inbox check.
#[test]
fn a_missing_subject_fails() {
    let frames = good().replace("Photos from the workshop", "                        ");
    assert_fails(
        &frames,
        &messages(),
        "FAIL  the saved inbox, every row loaded",
    );
}

/// An inbox row with a blank size cell fails the inbox check too.
#[test]
fn a_missing_size_fails() {
    let frames = good().replace("Sep 23      979 B", "Sep 23           ");
    assert_fails(
        &frames,
        &messages(),
        "FAIL  the saved inbox, every row loaded",
    );
}

/// A message in the list that never reached the screen fails. seed.sh and the check read the
/// same list, so a message added there must show up here.
#[test]
fn a_seeded_message_missing_from_the_screen_fails() {
    let messages = format!(
        "{}{}\n",
        messages(),
        "ffffffffffffffffffffffffffffffffffffffff\t1 hour ago\tNew Sender\tnew@example.com\tA row nobody recorded"
    );
    assert_fails(
        &good(),
        &messages,
        "FAIL  the saved inbox, every row loaded",
    );
}

/// Each kind of error the app can print is caught, in any case, wherever it lands on screen.
#[test]
fn error_text_is_caught_whatever_its_case() {
    for phrase in [
        "could not list inbound/: timed out",
        "This message no longer exists: inbound/x",
        "Unexpected reply for inbound/x",
        " Nothing matches /zzz",
        "ERROR",
    ] {
        let frames = good().replacen("Hi all,", &format!("Hi all, {phrase}"), 1);
        assert_fails(&frames, &messages(), "FAIL  error text on screen");
    }
}

/// A recording cut short, so its last snapshot is an open message, fails.
#[test]
fn ending_anywhere_but_the_inbox_fails() {
    let good = good();
    let sep = "─".repeat(20);
    // Keep the snapshots up to and including the scrolled message (the first six).
    let mut kept = String::new();
    let mut snapshots = 0;
    for line in good.lines() {
        kept.push_str(line);
        kept.push('\n');
        if line.starts_with(&sep) {
            snapshots += 1;
            if snapshots == 6 {
                break;
            }
        }
    }
    assert_eq!(
        snapshots, 6,
        "the fixture has fewer snapshots than expected"
    );
    assert_fails(
        &kept,
        &messages(),
        "FAIL  the last snapshot is not the inbox",
    );
}

/// A missing snapshot file fails, and so does a message list with nothing but comments in it.
#[test]
fn missing_inputs_fail() {
    let messages = scratch("messages.tsv", &messages());
    let (code, out) = check(Path::new("/nonexistent/frames.txt"), &messages);
    assert_eq!(code, 1, "{out}");
    let (code, out) = check_text(&good(), "# only comments\n");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no messages"), "{out}");
}

/// A header without the re:SES logo fails, on whichever screen it went missing.
#[test]
fn a_header_without_the_logo_fails() {
    // The header as it was before the logo: " reses  Accounts".
    let frames = good().replace(" ■ re:SES  ", " reses  ");
    assert_ne!(frames, good(), "the fixture has no logo to take out");
    assert_fails(&frames, &messages(), "FAIL  the accounts screen");
    // Losing it on a later screen fails too, at that screen.
    let frames = good().replace(" ■ re:SES  Message", " reses  Message");
    assert_fails(&frames, &messages(), "FAIL  the opened message");
}
