//! The mail goldens held to an independent reference. tests/mail_oracle.py reads each fixture with
//! Python's standard `email` package and prints what reses should show; this test runs it and
//! checks the committed goldens against it, so no golden can quietly drift into whatever the
//! decoder under test happens to print. tests/mail_golden.rs then holds the decoder to the
//! goldens.
//!
//! A few goldens are pinned by hand, where the standards and Python's parser disagree or where the
//! body comes from HTML. tests/fixtures/mail/HAND-PINNED lists each with its reason and says how
//! much of it the oracle still checks. A pin that agrees with the oracle fails here, so pins
//! can't outlive the disagreement they were made for.
//!
//! python3 has to be on PATH, as for the configparser oracle. It is in the CI image and on the
//! GitHub runners, and a missing python3 fails these tests rather than skipping them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How much of a hand-pinned golden the oracle still vouches for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Everything down to and including "Message:" and the blank line after it.
    Body,
    /// Nothing.
    All,
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mail")
}

/// The HAND-PINNED manifest: golden file name to scope. Every entry has to give a reason.
fn pins() -> BTreeMap<String, Scope> {
    let text = fs::read_to_string(dir().join("HAND-PINNED")).expect("HAND-PINNED");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        // Comment lines explain the groups; everything else is one pin.
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut words = line.splitn(3, ' ');
        let file = words.next().unwrap().to_string();
        let scope = match words.next() {
            Some("body") => Scope::Body,
            Some("all") => Scope::All,
            other => panic!("{file}: unknown scope {other:?}"),
        };
        let reason = words.next().unwrap_or("").trim();
        assert!(!reason.is_empty(), "{file} is pinned without a reason");
        assert!(dir().join(&file).exists(), "{file} is pinned but doesn't exist");
        assert!(out.insert(file.clone(), scope).is_none(), "{file} is pinned twice");
    }
    out
}

/// The oracle's output for one fixture: `render`, `render --html` or `saved`.
fn oracle(args: &[&str]) -> String {
    let out = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mail_oracle.py"))
        .args(args)
        .output()
        .expect("python3 must be on PATH for the mail oracle");
    assert!(
        out.status.success(),
        "oracle {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("the oracle prints UTF-8")
}

/// The part of a render the oracle checks for a body-scoped pin.
fn above_body(render: &str) -> &str {
    match render.find("\nMessage:\n\n") {
        Some(i) => &render[..i + "\nMessage:\n\n".len()],
        None => render,
    }
}

#[test]
fn goldens_match_the_python_oracle() {
    let pins = pins();
    let mut fixtures: Vec<String> = fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".eml"))
        .collect();
    fixtures.sort();
    // An empty directory would make every check below pass without looking at anything.
    assert!(fixtures.len() >= 60, "found only {} fixtures", fixtures.len());

    let mut failures = Vec::new();
    let mut pins_used = 0;
    for eml in &fixtures {
        let base = eml.trim_end_matches(".eml");
        let path = dir().join(eml);
        let path = path.to_str().unwrap();
        // Each golden is compared in full, compared down to the body, or skipped, as its pin says.
        for (suffix, args) in [
            (".out", vec!["render", path]),
            (".html.out", vec!["render", "--html", path]),
            (".saved", vec!["saved", path]),
        ] {
            let file = format!("{base}{suffix}");
            let golden = fs::read_to_string(dir().join(&file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            let want = oracle(&args);
            match pins.get(&file) {
                None if golden != want => failures.push(format!("{file} differs from the oracle")),
                None => {}
                Some(scope) => {
                    pins_used += 1;
                    if golden == want {
                        failures.push(format!("{file} is pinned but agrees with the oracle"));
                    } else if *scope == Scope::Body && above_body(&golden) != above_body(&want) {
                        failures.push(format!("{file} differs from the oracle above the body"));
                    }
                }
            }
        }
    }
    assert_eq!(pins_used, pins.len(), "a pinned file has no fixture");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
