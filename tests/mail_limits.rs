//! Hostile input: nesting deep enough to overflow the stack, and inputs that make a naive
//! scanner quadratic. Everything runs on a thread with a 1 MiB stack (half what the inbox's
//! worker threads get) and under a deadline, so a stack overflow or a hang fails this binary on
//! its own instead of taking the other test binaries with it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use reses::mail::{format_message, looks_like_email, save_attachments, summarize};

const STACK: usize = 1 << 20;
/// Generous on purpose: the slowest case takes a few seconds in a debug build on a busy CI box,
/// while the quadratic versions these guard against took hours on the same input.
const DEADLINE: Duration = Duration::from_secs(60);

/// Run every decoder entry point on `raw` on a small stack, and return format_message's plain
/// output. Fails if the work doesn't finish before the deadline.
fn decode_all(label: &str, raw: Vec<u8>) -> String {
    let (tx, rx) = mpsc::channel();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    thread::Builder::new()
        .name(label.to_string())
        .stack_size(STACK)
        .spawn(move || {
            let plain = format_message(&raw, false);
            let _ = format_message(&raw, true);
            let _ = summarize(&raw);
            let _ = summarize(&raw[..raw.len().min(32 * 1024)]);
            let _ = looks_like_email(&raw);
            let _ = save_attachments(&raw, &path);
            let _ = tx.send(plain);
        })
        .unwrap();
    match rx.recv_timeout(DEADLINE) {
        Ok(out) => out,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{label}: still running after {DEADLINE:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{label}: the decoder thread panicked"),
    }
}

fn assert_shape(label: &str, out: &str) {
    assert!(out.starts_with("From: "), "{label}: {out:.200}");
    assert!(out.contains("\nMessage:\n\n"), "{label}: {out:.200}");
}

fn nested_rfc822(n: usize) -> Vec<u8> {
    let mut s = String::from("From: a@example.com\nSubject: deep\n");
    for _ in 0..n {
        s.push_str("Content-Type: message/rfc822\n\n");
    }
    s.push_str("Subject: inner\n\ninner body\n");
    s.into_bytes()
}

fn nested_multipart(n: usize) -> Vec<u8> {
    let mut s = String::from("From: a@example.com\nSubject: deep\n");
    for k in 0..n {
        s.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=b{k}\n\n--b{k}\n"
        ));
    }
    s.push_str("Content-Type: text/plain\n\nleaf\n");
    for k in (0..n).rev() {
        s.push_str(&format!("\n--b{k}--\n"));
    }
    s.into_bytes()
}

#[test]
fn fifty_thousand_nested_rfc822_parts() {
    let out = decode_all("rfc822", nested_rfc822(50_000));
    assert_shape("rfc822", &out);
}

#[test]
fn fifty_thousand_nested_multiparts() {
    let out = decode_all("multipart", nested_multipart(50_000));
    assert_shape("multipart", &out);
}

#[test]
fn twenty_thousand_parens_in_every_parsed_header() {
    let open = "(".repeat(20_000);
    let balanced = format!("{open}{}", ")".repeat(20_000));
    let cases = [
        (
            "from unclosed",
            format!("From: x {open}\nTo: b@example.org\n\nbody\n"),
        ),
        (
            "from balanced",
            format!("From: {balanced} <a@example.com>\n\nbody\n"),
        ),
        (
            "to groups",
            format!(
                "From: a@example.com\nTo: {}b@example.org\n\nbody\n",
                "g:".repeat(20_000)
            ),
        ),
        (
            "message-id",
            format!("From: a@example.com\nMessage-ID: {balanced}<i@example.com>\n\nbody\n"),
        ),
        (
            "content-type",
            format!("From: a@example.com\nContent-Type: text/plain {open}\n\nbody\n"),
        ),
        (
            "content-disposition",
            format!(
                "From: a@example.com\nContent-Type: multipart/mixed; boundary=B\n\n--B\n\n\
                 x\n--B\nContent-Disposition: attachment; filename=f {open}\n\ny\n--B--\n"
            ),
        ),
        (
            "delivered-to comments",
            format!("From: a@example.com\nDelivered-To: {balanced} h@example.org\n\nb\n"),
        ),
        (
            "delivered-to groups",
            format!(
                "From: a@example.com\nDelivered-To: {}h@example.org\n\nb\n",
                "g:".repeat(20_000)
            ),
        ),
        (
            "received for",
            format!("From: a@example.com\nReceived: {open} for <{balanced}@example.org>\n\nb\n"),
        ),
    ];
    for (label, raw) in cases {
        let out = decode_all(label, raw.into_bytes());
        assert_shape(label, &out);
    }
}

fn beyond_python() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail/beyond-python");
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "eml"))
        .collect();
    found.sort();
    found
}

/// One level past each depth reses.py survives. Python dies there, so there's no golden; the
/// decoder just has to come back with a normal-looking message.
#[test]
fn one_level_past_python_still_decodes() {
    let all = beyond_python();
    assert_eq!(all.len(), 8, "expected one fixture per nesting shape");
    for path in all {
        let label = path.file_name().unwrap().to_string_lossy().into_owned();
        let out = decode_all(&label, fs::read(&path).unwrap());
        assert_shape(&label, &out);
    }
}

#[test]
fn at_python_limit_fixtures_decode_on_a_small_stack() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail");
    let mut seen = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("limit-") && name.ends_with(".eml") {
            let out = decode_all(&name, fs::read(&path).unwrap());
            let want = fs::read_to_string(path.with_extension("out")).unwrap();
            assert_eq!(out, want, "{name}");
            seen += 1;
        }
    }
    assert_eq!(seen, 8);
}

/// About 1.2 MB of each input that used to take quadratic time.
#[test]
fn scanners_stay_linear_on_large_input() {
    let big = 1_200_000;
    let html = |body: String| {
        format!("From: a@example.com\nContent-Type: text/html; charset=utf-8\n\n{body}\n")
            .into_bytes()
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("unclosed style", html("<style".repeat(big / 6))),
        ("unclosed script", html("<SCRIPT>".repeat(big / 8))),
        ("unclosed tags", html("<a ".repeat(big / 3))),
        ("lone angle brackets", html("<".repeat(big))),
        ("br without close", html("<br   ".repeat(big / 6))),
        ("entities", html("&#&amp&".repeat(big / 7))),
        (
            "encoded-word lookalikes",
            format!(
                "From: a@example.com\nSubject: {}\n\nb\n",
                "=?a?q?".repeat(big / 6)
            )
            .into_bytes(),
        ),
        (
            "glued encoded words",
            format!(
                "From: a@example.com\nSubject: {}\n\nb\n",
                "a=?utf-8?q?b?=".repeat(big / 14)
            )
            .into_bytes(),
        ),
        (
            "encoded words in an address",
            format!("From: {}?= <a@example.com>\n\nb\n", "=?x ".repeat(big / 4)).into_bytes(),
        ),
        (
            "encoded words in a quoted name",
            format!(
                "To: \"{}?=\" <a@example.com>\n\nb\n",
                "=?x ".repeat(big / 4)
            )
            .into_bytes(),
        ),
        (
            "unterminated encoded words",
            format!(
                "From: a@example.com\nSubject: {}?=\n\nb\n",
                "=?x ".repeat(big / 4)
            )
            .into_bytes(),
        ),
        (
            "many parameters",
            format!(
                "From: a@example.com\nContent-Type: text/plain{}\n\nb\n",
                (0..big / 12)
                    .map(|i| format!("; p{i}=\"v\""))
                    .collect::<String>()
            )
            .into_bytes(),
        ),
        (
            "many envelope recipients",
            format!(
                "From: a@example.com\n{}\nb\n",
                (0..20_000)
                    .map(|i| format!("Delivered-To: r{i}@example.org\n"))
                    .collect::<String>()
            )
            .into_bytes(),
        ),
    ];
    for (label, raw) in cases {
        let out = decode_all(label, raw);
        assert_shape(label, &out);
    }
}
