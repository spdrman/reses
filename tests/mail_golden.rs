//! Golden tests: every tests/fixtures/mail/NAME.eml has committed expected outputs next to it,
//! and the decoder has to match them byte for byte. The goldens come from tests/mail_oracle.py
//! (Python's standard email package), except the few pinned by hand in HAND-PINNED, and
//! tests/mail_oracle.rs keeps them honest; regen-goldens.sh rewrites them after an intended change.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail")
}

fn fixtures() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "eml"))
        .collect();
    found.sort();
    found
}

fn golden(eml: &Path, suffix: &str) -> String {
    let stem = eml.file_stem().unwrap().to_str().unwrap();
    let path = eml.with_file_name(format!("{stem}{suffix}"));
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    String::from_utf8(bytes).expect("goldens are utf-8")
}

/// Show the first differing line, since whole-message diffs are unreadable in test output.
fn first_difference(want: &str, got: &str) -> String {
    for (i, (w, g)) in want.split('\n').zip(got.split('\n')).enumerate() {
        if w != g {
            return format!("line {}:\n  want {w:?}\n  got  {g:?}", i + 1);
        }
    }
    format!(
        "one output is a prefix of the other ({} vs {} bytes)",
        want.len(),
        got.len()
    )
}

fn check_all(prefer_html: bool, suffix: &str) {
    let all = fixtures();
    // A glob that silently matches nothing would make this test pass vacuously.
    assert!(
        all.len() >= 60,
        "expected the full fixture set, found {}",
        all.len()
    );
    let mut failures = Vec::new();
    for eml in &all {
        let raw = fs::read(eml).unwrap();
        let want = golden(eml, suffix);
        let got = reses::mail::format_message(&raw, prefer_html);
        if got != want {
            failures.push(format!(
                "{}: {}",
                eml.file_name().unwrap().to_string_lossy(),
                first_difference(&want, &got)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} fixtures differ from their goldens{}:\n{}",
        failures.len(),
        all.len(),
        if prefer_html { " --html" } else { "" },
        failures.join("\n")
    );
}

#[test]
fn plain_output_matches_the_goldens() {
    check_all(false, ".out");
}

#[test]
fn html_output_matches_the_goldens() {
    check_all(true, ".html.out");
}

#[test]
fn saved_attachments_match_the_goldens() {
    let mut checked = 0;
    for eml in fixtures() {
        let want = golden(&eml, ".saved");
        let dir = tempfile::tempdir().unwrap();
        let raw = fs::read(&eml).unwrap();
        let paths = reses::mail::save_attachments(&raw, dir.path()).unwrap();
        let mut got = String::new();
        for p in &paths {
            assert_eq!(
                p.parent(),
                Some(dir.path()),
                "{} escaped the target dir",
                p.display()
            );
            let data = fs::read(p).unwrap();
            got.push_str(&format!(
                "{}\t{}\t{}\n",
                p.file_name().unwrap().to_string_lossy(),
                data.len(),
                hex::encode(Sha256::digest(&data))
            ));
        }
        assert_eq!(got, want, "{}", eml.display());
        checked += paths.len();
    }
    assert!(
        checked >= 30,
        "expected at least 30 saved attachments, saw {checked}"
    );
}
