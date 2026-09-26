//! `demo/check-frames.sh` is what stops a recording of a broken screen from replacing
//! docs/demo.gif, so it gets tests of its own: the known-good snapshots in
//! `tests/fixtures/demo/good.txt` pass, and the known-bad ones, and each targeted breakage of
//! the good ones below, fail with the reason named.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn good() -> String {
    fs::read_to_string(root().join("tests/fixtures/demo/good.txt")).unwrap()
}

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

fn check_text(frames: &str, messages: &str) -> (i32, String) {
    check(
        &scratch("frames.txt", frames),
        &scratch("messages.tsv", messages),
    )
}

fn assert_fails(frames: &str, messages: &str, reason: &str) {
    let (code, out) = check_text(frames, messages);
    assert_eq!(code, 1, "expected a failure naming {reason:?}:\n{out}");
    assert!(
        out.contains(reason),
        "the failure does not name {reason:?}:\n{out}"
    );
}

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

#[test]
fn a_missing_subject_fails() {
    let frames = good().replace("Photos from the workshop", "                        ");
    assert_fails(
        &frames,
        &messages(),
        "FAIL  the saved inbox, every row loaded",
    );
}

#[test]
fn a_missing_size_fails() {
    let frames = good().replace("Sep 23      979 B", "Sep 23           ");
    assert_fails(
        &frames,
        &messages(),
        "FAIL  the saved inbox, every row loaded",
    );
}

#[test]
fn a_seeded_message_missing_from_the_screen_fails() {
    // seed.sh and the check read the same list, so a message added there must show up here.
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

#[test]
fn missing_inputs_fail() {
    let messages = scratch("messages.tsv", &messages());
    let (code, out) = check(Path::new("/nonexistent/frames.txt"), &messages);
    assert_eq!(code, 1, "{out}");
    let (code, out) = check_text(&good(), "# only comments\n");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no messages"), "{out}");
}
