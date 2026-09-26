//! Behaviour of the mail helpers the inbox uses: summarize, looks_like_email, save_attachments.

use std::fs;
use std::path::Path;

use reses::mail::{looks_like_email, save_attachments, summarize};
use time::macros::datetime;

fn fixture(name: &str) -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail").join(name))
        .unwrap()
}

#[test]
fn summarize_fills_every_field() {
    let s = summarize(&fixture("attachments-crlf.eml"));
    assert_eq!(s.from, "Files <files@example.com>");
    assert_eq!(s.to, "you@example.org");
    assert_eq!(s.cc, "");
    assert_eq!(s.subject, "Attachments of every shape");
    assert_eq!(s.date_raw, "Sat, 10 Oct 2026 14:00:00 +0000");
    assert_eq!(s.date, Some(datetime!(2026-10-10 14:00:00 +00:00)));
    assert_eq!(s.message_id, "<att-1@example.com>");
    assert!(s.has_attachments);
}

#[test]
fn summarize_decodes_headers_like_format_message() {
    let s = summarize(&fixture("encoded-words.eml"));
    assert_eq!(s.from, "Éloïse Example <eloise@example.com>");
    assert_eq!(s.to, "Jörg <joerg@example.org>, Quoted EW <qew@example.org>");
    assert_eq!(s.cc, "\"Ann \\\"A\\\"\" <ann@example.org>");
    assert_eq!(
        s.subject,
        "Café menu for Friday and ✓ done plus raw end badtail"
    );
    assert_eq!(s.date, Some(datetime!(2026-10-07 10:00:00 +01:00)));
    assert!(!s.has_attachments);
}

#[test]
fn summarize_keeps_the_offset_of_the_date() {
    let s = summarize(&fixture("base64-body.eml"));
    assert_eq!(s.date, Some(datetime!(2026-10-08 23:59:59 +05:30)));
}

#[test]
fn summarize_handles_missing_and_bad_dates() {
    let s = summarize(&fixture("missing-date.eml"));
    assert_eq!((s.date, s.date_raw.as_str()), (None, ""));
    assert_eq!(s.message_id, "");

    let s = summarize(&fixture("bad-date.eml"));
    assert_eq!(s.date, None);
    assert_eq!(s.date_raw, "sometime last tuesday, probably");

    let s = summarize(&fixture("impossible-date.eml"));
    assert_eq!(s.date, None);
    assert_eq!(s.date_raw, "Mon, 31 Feb 2026 25:61:00 +0000");
}

#[test]
fn summarize_treats_a_naive_date_as_utc() {
    let s = summarize(&fixture("explicit-bcc.eml"));
    assert_eq!(s.date, Some(datetime!(2026-10-09 07:07:07 +00:00)));
    assert_eq!(s.date_raw, "Fri, 9 Oct 2026 07:07:07 -0000");
}

#[test]
fn summarize_works_on_a_prefix_that_stops_inside_the_headers() {
    let raw = fixture("ses-received-crlf.eml");
    let text = String::from_utf8(raw.clone()).unwrap();
    // Cut in the middle of the To: line, after From/Date/Message-ID/Subject.
    let cut = text.find("To: inbox@").unwrap() + 6;
    let s = summarize(&raw[..cut]);
    assert_eq!(s.from, "Sender Person <sender@example.com>");
    assert_eq!(s.subject, "A message the way SES stores it");
    assert_eq!(s.date, Some(datetime!(2026-09-25 17:01:55 -07:00)));
    assert_eq!(s.message_id, "<CAexample+abc=def@mail.example.com>");
    // The unfinished To: line is dropped rather than shown truncated.
    assert_eq!(s.to, "");
    assert!(!s.has_attachments);

    let subject_end = text.find("\r\nSubject:").unwrap()
        + "\r\nSubject: A message the way SES stores it\r".len();
    // Every other cut point must not panic, and anything already complete stays right.
    for n in 0..raw.len() {
        let s = summarize(&raw[..n]);
        if n >= subject_end {
            assert_eq!(s.subject, "A message the way SES stores it", "cut at {n}");
        }
    }
}

#[test]
fn summarize_on_a_prefix_that_stops_in_a_folded_header() {
    let raw = b"From: a@example.com\r\nSubject: first half\r\n second half\r\nTo: b@exam";
    let s = summarize(raw);
    assert_eq!(s.from, "a@example.com");
    assert_eq!(s.subject, "first half second half");
    assert_eq!(s.to, "");
}

#[test]
fn every_fixture_looks_like_email() {
    for entry in fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail"))
        .unwrap()
    {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "eml") {
            let raw = fs::read(&path).unwrap();
            assert!(looks_like_email(&raw), "{}", path.display());
            // The inbox only fetches a prefix, so a short one has to work too.
            assert!(looks_like_email(&raw[..raw.len().min(40)]), "{} prefix", path.display());
        }
    }
}

#[test]
fn ses_objects_starting_with_transport_headers_look_like_email() {
    assert!(looks_like_email(b"Return-Path: <a@example.com>\r\nReceived: from x"));
    assert!(looks_like_email(b"Received: from mx.example.com (mx.example.com [192.0.2.1])\r\n by in"));
    assert!(looks_like_email(b"From MAILER-DAEMON Fri Sep 25 17:01:31 2026\nFrom: a@example.com\n"));
    assert!(looks_like_email(b"Delivered-To: a@example.org\nX-Custom: 1\nSubject: hi\n\nbody"));
}

#[test]
fn other_objects_do_not_look_like_email() {
    let cases: &[(&str, &[u8])] = &[
        ("empty", b""),
        ("whitespace", b"\r\n\r\n"),
        ("json", b"{\"notificationType\":\"Received\",\"mail\":{}}"),
        ("json array", b"[1, 2, 3]"),
        ("html", b"<!DOCTYPE html>\n<html><head><title>x</title>"),
        ("html lower", b"<html>\n<body>From: a@example.com</body>"),
        ("xml", b"<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code>"),
        ("pdf", b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj"),
        ("png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"),
        ("jpeg", b"\xff\xd8\xff\xe0\0\x10JFIF\0"),
        ("gif", b"GIF89a\x01\0\x01\0"),
        ("zip", b"PK\x03\x04\x14\0\0\0"),
        ("gzip", b"\x1f\x8b\x08\0\0\0\0\0"),
        ("plain text", b"Hello there, this is just a note.\nNothing else.\n"),
        ("csv", b"name,email\nalice,alice@example.com\n"),
        ("url-ish first line", b"https://example.com: not a header\n"),
        ("binary", b"\0\x01\x02\x03From: a@example.com\n"),
        (
            "ses setup notification",
            b"Date: Tue, 31 May 2016 10:46:23 +0000\r\nTo: inbox@example.org\r\n\
              From: Amazon Web Services <no-reply-aws@example.com>\r\n\
              Subject: Amazon SES Setup Notification\r\n\r\nHello,\r\n\r\nYou received this.",
        ),
    ];
    for (name, data) in cases {
        assert!(!looks_like_email(data), "{name} was taken for an email");
    }
}

#[test]
fn save_attachments_never_overwrites() {
    let raw = fixture("attachments-crlf.eml");
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("report.pdf"), b"already here").unwrap();

    let first = save_attachments(&raw, dir.path()).unwrap();
    let names: Vec<String> = first
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names[0], "report-1.pdf");
    assert_eq!(fs::read(dir.path().join("report.pdf")).unwrap(), b"already here");

    // A second run finds everything taken and moves each name along.
    let second = save_attachments(&raw, dir.path()).unwrap();
    let names: Vec<String> = second
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "report-2.pdf",
            "notes-2.txt",
            "résumé data-1.csv",
            "logo é-1.png",
            "evil-1.bin",
            "notes-3.txt",
            "a-very-long-filename-in-parts-1.dat",
        ]
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 15);
}

#[test]
fn save_attachments_strips_path_components() {
    let raw = b"From: a@example.com\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"/etc/passwd\"\r\n\r\nx\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"..\\\\..\\\\win.ini\"\r\n\r\ny\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"..\"\r\n\r\nz\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"archive.tar.gz\"\r\n\r\nw\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"archive.tar.gz\"\r\n\r\nv\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\".profile\"\r\n\r\nu\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\".profile\"\r\n\r\nt\r\n\
--B--\r\n";
    let dir = tempfile::tempdir().unwrap();
    let saved = save_attachments(raw, dir.path()).unwrap();
    let names: Vec<String> = saved
        .iter()
        .map(|p| {
            assert_eq!(p.parent(), Some(dir.path()));
            p.file_name().unwrap().to_string_lossy().into_owned()
        })
        .collect();
    assert_eq!(
        names,
        [
            "passwd",
            "win.ini",
            "attachment",
            "archive.tar.gz",
            "archive.tar-1.gz",
            ".profile",
            ".profile-1",
        ]
    );
    assert_eq!(fs::read(dir.path().join("passwd")).unwrap(), b"x");
}

#[test]
fn save_attachments_creates_the_directory() {
    let raw = fixture("html-alternative-attach.eml");
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("new/sub");
    let saved = save_attachments(&raw, &target).unwrap();
    assert_eq!(saved, [target.join("readme.txt")]);
}
