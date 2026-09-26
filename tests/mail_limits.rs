//! Hostile input: nesting deep enough to overflow the stack, huge and broken headers, boundaries
//! that never close, invalid UTF-8, and inputs that make a naive scanner quadratic. mail-parser
//! keeps its part stack on the heap and stops unpacking nested messages after three levels, and
//! these tests hold the whole decoder, HTML conversion and saving included, to the same standard. Everything runs on a thread with a 1 MiB stack (half what the inbox's
//! worker threads get) and under a deadline, so a stack overflow or a hang fails this binary on
//! its own instead of taking the other test binaries with it.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use reses::mail::{
    format_message, looks_like_email, save_attachments, save_attachments_report, summarize,
};

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

/// A render still has the layout: headers first, then "Message:" and the body.
fn assert_shape(label: &str, out: &str) {
    assert!(out.starts_with("From: "), "{label}: {out:.200}");
    assert!(out.contains("\nMessage:\n\n"), "{label}: {out:.200}");
}

/// A message with `n` forwarded messages each inside the last.
fn nested_rfc822(n: usize) -> Vec<u8> {
    let mut s = String::from("From: a@example.com\nSubject: deep\n");
    for _ in 0..n {
        s.push_str("Content-Type: message/rfc822\n\n");
    }
    s.push_str("Subject: inner\n\ninner body\n");
    s.into_bytes()
}

/// A message with `n` multiparts each inside the last, all closed properly.
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

/// Forwards nested 50,000 deep.
#[test]
fn fifty_thousand_nested_rfc822_parts() {
    let out = decode_all("rfc822", nested_rfc822(50_000));
    assert_shape("rfc822", &out);
}

/// Multiparts nested 50,000 deep.
#[test]
fn fifty_thousand_nested_multiparts() {
    let out = decode_all("multipart", nested_multipart(50_000));
    assert_shape("multipart", &out);
}

/// Twenty thousand parens or groups in each header that gets parsed.
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

/// About a megabyte of each input that makes a naive scanner quadratic.
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

/// Headers far bigger than any real one: a megabyte on one line, a hundred thousand
/// continuation lines, and a hundred thousand headers.
#[test]
fn huge_headers() {
    let long = format!(
        "From: a@example.com\r\nSubject: {}\r\n\r\nbody\r\n",
        "x".repeat(1 << 20)
    );
    let folded = format!(
        "From: a@example.com\r\nSubject: start{}\r\n\r\nbody\r\n",
        "\r\n more".repeat(100_000)
    );
    let many = format!(
        "From: a@example.com\r\n{}\r\nbody\r\n",
        (0..100_000)
            .map(|i| format!("X-H{i}: v\r\n"))
            .collect::<String>()
    );
    for (label, raw) in [
        ("one long line", long),
        ("folded", folded),
        ("many headers", many),
    ] {
        assert_shape(label, &decode_all(label, raw.into_bytes()));
    }
}

/// Boundaries that open and never close, nested and not, and one that never appears at all.
#[test]
fn boundaries_that_never_close() {
    let mut nested = String::from("From: a@example.com\r\n");
    for k in 0..10_000 {
        nested.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=b{k}\r\n\r\n--b{k}\r\n"
        ));
    }
    nested.push_str("Content-Type: text/plain\r\n\r\nleaf\r\n");
    let missing = "From: a@example.com\r\nContent-Type: multipart/mixed; boundary=nowhere\r\n\r\n\
                   text with no boundary line at all\r\n"
        .to_string()
        + &"filler line\r\n".repeat(50_000);
    let open_attachment = format!(
        "From: a@example.com\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\n\
         Content-Type: application/pdf; name=a.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
        "QUJD".repeat(250_000)
    );
    for (label, raw) in [
        ("nested", nested),
        ("missing", missing),
        ("open attachment", open_attachment),
    ] {
        assert_shape(label, &decode_all(label, raw.into_bytes()));
    }
}

/// Bytes that aren't UTF-8 in every place they can go.
#[test]
fn invalid_utf8_everywhere() {
    let mut raw = b"From: \xff\xfe <a@example.com>\r\nTo: \xc3\r\nSubject: =?utf-8?b?/w==?= \x80\x81\r\n\
Content-Type: multipart/mixed; boundary=\xff\r\n\r\n--\xff\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>\xe2\x28\xa1</p>\r\n\
--\xff\r\nContent-Type: text/plain; name=\"\xff\xfe.txt\"\r\n\r\n\xc0\xaf\r\n--\xff--\r\n"
        .to_vec();
    // Then a megabyte of every byte value, over and over.
    raw.extend((0..(1u32 << 20)).map(|i| (i % 256) as u8));
    let out = decode_all("invalid utf-8", raw);
    assert_shape("invalid utf-8", &out);
    let noise: Vec<u8> = (0..(1u32 << 20))
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let _ = decode_all("noise", noise);
}

/// HTML nested deep enough to exhaust a recursive tree walk.
#[test]
fn deeply_nested_html() {
    for (label, open, close) in [
        ("divs", "<div>", "</div>"),
        ("tables", "<table><tr><td>", "</td></tr></table>"),
        ("unclosed", "<b><i>", ""),
    ] {
        let body = format!("{}deep{}", open.repeat(50_000), close.repeat(50_000));
        let raw = format!("From: a@example.com\r\nContent-Type: text/html\r\n\r\n{body}\r\n");
        assert_shape(label, &decode_all(label, raw.into_bytes()));
    }
}

/// The panel's 32k parts with one name (N18): the old save rescanned the names already taken for
/// every part and took twenty minutes. With a per-name suffix and a cap it has to finish fast,
/// write only the cap, count the rest, and still refuse to overwrite anything.
#[test]
fn thirty_two_thousand_attachments_with_one_name() {
    let mut raw =
        String::from("From: a@example.com\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n");
    for _ in 0..32_000 {
        raw.push_str("--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=same.bin\r\n\r\nx\r\n");
    }
    raw.push_str("--B--\r\n");
    let (tx, rx) = mpsc::channel();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("same.bin"), b"keep").unwrap();
    let path = dir.path().to_path_buf();
    thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            let _ = tx.send(
                save_attachments_report(raw.as_bytes(), &path).map(|r| (r.saved.len(), r.skipped)),
            );
        })
        .unwrap();
    let (saved, skipped) = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("saving 32k same-named attachments took more than 20 s")
        .unwrap();
    assert_eq!((saved, skipped), (1_000, 31_000));
    assert_eq!(std::fs::read(dir.path().join("same.bin")).unwrap(), b"keep");
    assert!(dir.path().join("same-1000.bin").exists());
}
