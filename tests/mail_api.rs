//! Behaviour of the mail helpers the inbox uses: summarize, looks_like_email, save_attachments.
//!
//! These are the calls the inbox makes on every object it lists or opens, so they get tested
//! here as public API rather than only through the TUI. I feed them the committed fixtures in
//! tests/fixtures/mail, prefixes of those (the inbox only fetches the head of each object for
//! its list), and hand-built messages where a case needs a specific shape. Saving runs in a
//! temp dir per test, since the promises there (never overwrite, never escape the directory)
//! are about what ends up on disk.

use std::fs;
use std::path::Path;

use reses::mail::{looks_like_email, save_attachments, save_attachments_report, summarize};
use time::macros::datetime;

/// A mail fixture's bytes.
fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mail")
            .join(name),
    )
    .unwrap()
}

/// Every field of the summary is filled from a message that has them all.
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

/// The summary decodes headers the same way the full view does.
#[test]
fn summarize_decodes_headers_like_format_message() {
    let s = summarize(&fixture("encoded-words.eml"));
    assert_eq!(s.from, "Éloïse Example <eloise@example.com>");
    assert_eq!(
        s.to,
        "Jörg <joerg@example.org>, Quoted EW <qew@example.org>"
    );
    assert_eq!(s.cc, "\"Ann \\\"A\\\"\" <ann@example.org>");
    assert_eq!(
        s.subject,
        // mail-parser's reading of a glued encoded word; see HAND-PINNED for encoded-words.out.
        "Café menu for Friday and ✓ done plus raw end bad tail"
    );
    assert_eq!(s.date, Some(datetime!(2026-10-07 10:00:00 +01:00)));
    assert!(!s.has_attachments);
}

/// A half-hour zone survives into the parsed date.
#[test]
fn summarize_keeps_the_offset_of_the_date() {
    let s = summarize(&fixture("base64-body.eml"));
    assert_eq!(s.date, Some(datetime!(2026-10-08 23:59:59 +05:30)));
}

/// Missing, unparseable and impossible dates have no parsed value but keep what was written.
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

/// A -0000 zone sorts as UTC.
#[test]
fn summarize_treats_a_naive_date_as_utc() {
    let s = summarize(&fixture("explicit-bcc.eml"));
    assert_eq!(s.date, Some(datetime!(2026-10-09 07:07:07 +00:00)));
    assert_eq!(s.date_raw, "Fri, 9 Oct 2026 07:07:07 -0000");
}

/// A prefix cut anywhere never panics, and every header it holds in full is right.
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

    let subject_end =
        text.find("\r\nSubject:").unwrap() + "\r\nSubject: A message the way SES stores it\r".len();
    // Every other cut point must not panic, and anything already complete stays right.
    for n in 0..raw.len() {
        let s = summarize(&raw[..n]);
        if n >= subject_end {
            assert_eq!(s.subject, "A message the way SES stores it", "cut at {n}");
        }
    }
}

/// A folded header that's complete is read whole; the cut line after it is left out.
#[test]
fn summarize_on_a_prefix_that_stops_in_a_folded_header() {
    let raw = b"From: a@example.com\r\nSubject: first half\r\n second half\r\nTo: b@exam";
    let s = summarize(raw);
    assert_eq!(s.from, "a@example.com");
    assert_eq!(s.subject, "first half second half");
    assert_eq!(s.to, "");
}

/// Every fixture message, and a short prefix of each, looks like mail.
#[test]
fn every_fixture_looks_like_email() {
    for entry in
        fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail")).unwrap()
    {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "eml") {
            let raw = fs::read(&path).unwrap();
            assert!(looks_like_email(&raw), "{}", path.display());
            // The inbox only fetches a prefix, so a short one has to work too.
            assert!(
                looks_like_email(&raw[..raw.len().min(40)]),
                "{} prefix",
                path.display()
            );
        }
    }
}

/// Objects the way SES stores them, and an mbox envelope line, look like mail.
#[test]
fn ses_objects_starting_with_transport_headers_look_like_email() {
    assert!(looks_like_email(
        b"Return-Path: <a@example.com>\r\nReceived: from x"
    ));
    assert!(looks_like_email(
        b"Received: from mx.example.com (mx.example.com [192.0.2.1])\r\n by in"
    ));
    assert!(looks_like_email(
        b"From MAILER-DAEMON Fri Sep 25 17:01:31 2026\nFrom: a@example.com\n"
    ));
    assert!(looks_like_email(
        b"Delivered-To: a@example.org\nX-Custom: 1\nSubject: hi\n\nbody"
    ));
}

/// Other kinds of file in a bucket don't.
#[test]
fn other_objects_do_not_look_like_email() {
    let cases: &[(&str, &[u8])] = &[
        ("empty", b""),
        ("whitespace", b"\r\n\r\n"),
        ("json", b"{\"notificationType\":\"Received\",\"mail\":{}}"),
        ("json array", b"[1, 2, 3]"),
        ("html", b"<!DOCTYPE html>\n<html><head><title>x</title>"),
        ("html lower", b"<html>\n<body>From: a@example.com</body>"),
        (
            "xml",
            b"<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code>",
        ),
        ("pdf", b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj"),
        ("png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"),
        ("jpeg", b"\xff\xd8\xff\xe0\0\x10JFIF\0"),
        ("gif", b"GIF89a\x01\0\x01\0"),
        ("zip", b"PK\x03\x04\x14\0\0\0"),
        ("gzip", b"\x1f\x8b\x08\0\0\0\0\0"),
        (
            "plain text",
            b"Hello there, this is just a note.\nNothing else.\n",
        ),
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

/// Saving twice into the same place moves names along and leaves every earlier file alone.
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
    assert_eq!(
        fs::read(dir.path().join("report.pdf")).unwrap(),
        b"already here"
    );

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
            "attachment-7-1.zip",
            "a-very-long-filename-in-parts-1.dat",
        ]
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 17);
}

/// Names can't climb out of the directory, whichever path separator they use.
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

/// The target directory is made if it's missing.
#[test]
fn save_attachments_creates_the_directory() {
    let raw = fixture("html-alternative-attach.eml");
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("new/sub");
    let saved = save_attachments(&raw, &target).unwrap();
    assert_eq!(saved, [target.join("readme.txt")]);
}

/// A message with a text body and one small attachment for each name, in order.
fn one_attachment_each(names: &[&str]) -> Vec<u8> {
    let mut raw = String::from(
        "From: a@example.com\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n\
         --B\r\nContent-Type: text/plain\r\n\r\nbody\r\n",
    );
    for (i, name) in names.iter().enumerate() {
        raw.push_str(&format!(
            "--B\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"{name}\"\r\n\r\npayload {i}\r\n"
        ));
    }
    raw.push_str("--B--\r\n");
    raw.into_bytes()
}

/// A name past the length limit is shortened, extension kept, and later attachments still save.
#[test]
fn save_attachments_shortens_long_names_and_keeps_going() {
    let long = format!("{}.pdf", "é".repeat(150)); // 304 bytes
    let raw = one_attachment_each(&[&long, "after.txt"]);
    let dir = tempfile::tempdir().unwrap();
    let saved = save_attachments(&raw, dir.path()).unwrap();
    assert_eq!(saved.len(), 2, "{saved:?}");
    let first = saved[0].file_name().unwrap().to_str().unwrap().to_string();
    assert!(first.len() <= 200, "{} bytes", first.len());
    assert!(first.ends_with(".pdf"), "{first}");
    assert!(first.starts_with("éé"), "{first}");
    assert_eq!(fs::read(&saved[0]).unwrap(), b"payload 0");
    assert_eq!(saved[1].file_name().unwrap(), "after.txt");

    // Saving again still finds a free, short-enough name.
    let again = save_attachments(&raw, dir.path()).unwrap();
    let name = again[0].file_name().unwrap().to_str().unwrap();
    assert!(name.len() <= 210 && name.ends_with("-1.pdf"), "{name}");
}

/// Characters that could disguise a name are replaced.
#[test]
fn save_attachments_replaces_control_and_bidi_characters() {
    let raw = one_attachment_each(&[
        "invoice\u{202e}fdp.exe",
        "tab\there\u{7}bell.txt",
        "iso\u{2066}late\u{2069}.txt",
        "marks\u{200e}\u{200f}\u{61c}.txt",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = save_attachments(&raw, dir.path())
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "invoice_fdp.exe",
            "tab_here_bell.txt",
            "iso_late_.txt",
            "marks___.txt"
        ]
    );
}

/// Real mail the panel found hidden (N31): odd but harmless header syntax, a byte order mark, a
/// blank line in front. Hiding a message costs more than showing a stray file, so these pass.
#[test]
fn tolerant_header_syntax_still_looks_like_email() {
    for name in [
        "space-before-colon.eml",
        "line-without-colon.eml",
        "byte-order-mark.eml",
        "leading-blank-line.eml",
        "smtputf8-no-charset.eml",
        "resent.eml",
    ] {
        assert!(looks_like_email(&fixture(name)), "{name}");
    }
}

/// Files that look a little like headers but aren't mail (N31), and the SES setup notice with its
/// subject either plain or encoded.
#[test]
fn header_like_files_and_the_setup_notice_do_not_look_like_email() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail/sniff/reject");
    let mut seen = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let raw = fs::read(&path).unwrap();
        assert!(
            !looks_like_email(&raw),
            "{} was taken for an email",
            path.display()
        );
        seen += 1;
    }
    assert_eq!(seen, 5, "expected five reject fixtures");
}

/// A prefix that stops before the headers end gets the benefit of the doubt: one real mail header
/// and nothing that isn't a header is enough, since the anchor header may simply come later.
#[test]
fn a_short_prefix_needs_only_one_mail_header() {
    assert!(looks_like_email(
        b"Date: Tue, 22 Sep 2026 10:00:00 +0000\r\nX-Mailer: x"
    ));
    assert!(looks_like_email(b"Subject: hi\r\nTo: b@exa"));
    // Once the header block is complete, the full rule applies.
    assert!(!looks_like_email(
        b"Date: Tue, 22 Sep 2026 10:00:00 +0000\r\nSubject: x\r\n\r\nbody"
    ));
    assert!(looks_like_email(
        b"From: a@example.com\r\nSubject: x\r\n\r\nbody"
    ));
    // A From that isn't an address doesn't anchor anything.
    assert!(!looks_like_email(b"From: tool\r\nSubject: x\r\n\r\nbody"));
}

/// Zone names read with their real offsets (N33), and a zone nobody knows sorts as UTC.
#[test]
fn summarize_reads_zone_names() {
    let s = summarize(&fixture("date-zone-name-cest.eml"));
    assert_eq!(s.date, Some(datetime!(2026-09-22 10:00:00 +02:00)));
    // A zone nobody can pin down is "-0000" in RFC 5322 terms; the inbox sorts it as UTC.
    let s = summarize(&fixture("date-gmt-plus-hours.eml"));
    assert_eq!(s.date, Some(datetime!(2026-09-22 10:00:00 +00:00)));
    let s = summarize(&fixture("date-iso-8601.eml"));
    assert_eq!(s.date, None);
    assert_eq!(s.date_raw, "2026-09-22T10:00:00Z");
}

/// Each attachment shape the panel found hidden now counts (N30).
#[test]
fn summarize_counts_attachments_the_panel_found_missing() {
    for name in [
        "forward-as-attachment.eml",
        "apple-inline-pdf.eml",
        "smime-signed.eml",
        "mailman-wrap.eml",
        "single-part-pdf.eml",
        "attachment-without-name.eml",
        "delivery-status.eml",
        "inline-named-text.eml",
        "calendar-only.eml",
    ] {
        assert!(summarize(&fixture(name)).has_attachments, "{name}");
    }
    assert!(!summarize(&fixture("plain-lf.eml")).has_attachments);
}

/// A message with more attachments than one save writes (N18): the rest are counted, not lost
/// silently, and still nothing is overwritten.
#[test]
fn save_reports_what_the_cap_skipped() {
    let names: Vec<String> = (0..1_200).map(|i| format!("f{}.txt", i % 3)).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let raw = one_attachment_each(&refs);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f0.txt"), b"mine").unwrap();
    let report = save_attachments_report(&raw, dir.path()).unwrap();
    assert_eq!(report.saved.len(), 1_000);
    assert_eq!(report.skipped, 200);
    assert_eq!(fs::read(dir.path().join("f0.txt")).unwrap(), b"mine");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1_001);
    // The per-name suffixes carry on from where each name's run left off.
    assert_eq!(report.saved[0].file_name().unwrap(), "f0-1.txt");
    assert_eq!(report.saved[3].file_name().unwrap(), "f0-2.txt");
    assert_eq!(report.saved[4].file_name().unwrap(), "f1-1.txt");
}

/// The Bcc line only names envelope recipients missing from To and Cc, compared without case.
#[test]
fn bcc_leaves_out_visible_recipients_whatever_their_case() {
    // Addresses are case-insensitive in practice, so the envelope's alice@example.com is the
    // same person as To's Alice@Example.COM and not a blind copy. The fixture is hand-written
    // (tests/fixtures/bcc/mixed-case.eml) and so is the expected line: only hidden@example.net
    // reached the mailbox without being named in To or Cc.
    let raw =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bcc/mixed-case.eml"))
            .unwrap();
    let out = reses::mail::format_message(&raw, false);
    let bcc: Vec<&str> = out.lines().filter(|l| l.starts_with("Bcc:")).collect();
    assert_eq!(bcc, ["Bcc: hidden@example.net"], "{out}");
}
