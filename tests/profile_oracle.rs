//! The credentials writer, refereed by the two readers that matter.
//!
//! reses reads profiles through aws-config, and the AWS CLI reads the same file through botocore,
//! which uses Python's configparser. Whatever reses writes has to work for both, so these tests
//! check every write against both. aws-config gets called directly, and configparser through
//! tests/profile_oracle.py, which prints what `RawConfigParser` makes of a file.
//!
//! python3 has to be on PATH. It is in the CI image and on the GitHub runners, and if python3 is
//! missing these tests fail rather than skip, since a skip would look like a pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_types::os_shim_internal::{Env, Fs};
use reses::aws_profile::{CredentialsFile, Profile, ProfileError};

const KEY_ID: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

/// The oracle script, found from the crate root so the test works from any directory.
fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/profile_oracle.py")
}

/// I run the oracle and hand back what it printed, failing loudly if python3 can't run it.
fn python(args: &[&str]) -> String {
    let out = Command::new("python3")
        .arg(script())
        .args(args)
        .output()
        .expect("python3 must be on PATH for the configparser oracle");
    assert!(
        out.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The oracle's string encoding: hex of the UTF-8 bytes, "-" for empty.
fn h(s: &str) -> String {
    if s.is_empty() {
        "-".into()
    } else {
        hex::encode(s)
    }
}

/// What configparser makes of a file on disk.
fn oracle_dump(path: &Path) -> String {
    python(&["dump", path.to_str().unwrap()])
}

/// Whether aws-config itself accepts `text` as a credentials file. I ask the SDK directly here,
/// not through reses, so this is an independent verdict.
fn sdk_accepts(text: &str) -> bool {
    let files = EnvConfigFiles::builder()
        .with_contents(EnvConfigFileKind::Credentials, text)
        .build();
    pollster::block_on(aws_config::profile::load(
        &Fs::from_slice(&[]),
        &Env::from_slice(&[]),
        &files,
        None,
    ))
    .is_ok()
}

/// A complete profile under `name`, with the fake test keys.
fn profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        access_key_id: KEY_ID.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
        region: None,
    }
}

/// The items configparser sees in one section of a file on disk. The file must pass strict.
fn oracle_section(path: &Path, section: &str) -> Vec<String> {
    let dump = oracle_dump(path);
    assert!(
        dump.starts_with("strict ok\n"),
        "configparser refused it: {dump}"
    );
    let header = format!("section {}", h(section));
    dump.lines()
        .skip_while(|l| *l != header)
        .skip(1)
        .take_while(|l| l.starts_with("item "))
        .map(str::to_string)
        .collect()
}

/// Every section of a non-strict dump except `skip`, as (header line, item lines).
fn other_sections(dump: &str, skip: &str) -> Vec<(String, Vec<String>)> {
    let skip = format!("section {}", h(skip));
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in dump.lines() {
        if line.starts_with("section ") {
            out.push((line.to_string(), Vec::new()));
        } else if line.starts_with("item ")
            && let Some(last) = out.last_mut()
        {
            last.1.push(line.to_string());
        }
    }
    out.retain(|(header, _)| *header != skip);
    out
}

/// One item line in the oracle's dump format.
fn item(k: &str, v: &str) -> String {
    format!("item {} {}", h(k), h(v))
}

/// Constructs where a hand-rolled INI reader, aws-config and configparser can disagree.
const CASES: &[(&str, &str)] = &[
    (
        "plain",
        "[default]\naws_access_key_id = AKIDEXAMPLE\naws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n",
    ),
    (
        "header trailing comment",
        "[work]\nregion = us-east-1\n[personal]   # my own account\nregion = eu-west-1\n",
    ),
    ("greedy header", "[a] # [b]\nk = v\n"),
    ("header brackets inside", "[]]\nk = v\n[a]b]\nk = w\n"),
    ("header inner spaces kept", "[ spaced ]\nk = v\n"),
    ("empty header is bogus", "[a]\n[]\nk = v\n"),
    ("indented header", "  [a]\n  k = v\n"),
    ("duplicate section", "[a]\nk = 1\n[b]\nk = 2\n[a]\nj = 3\n"),
    ("duplicate option", "[a]\nk = 1\nK = 2\n"),
    (
        "duplicate option across repeated sections",
        "[a]\nk = 1\n[a]\nk = 2\n",
    ),
    ("missing section header", "k = v\n[a]\nk = v\n"),
    ("comments before first section", "# c\n; c\n\n[a]\nk = v\n"),
    ("bogus line", "[a]\nk = v\nnot an option\n"),
    ("empty key", "[a]\n= v\n"),
    (
        "continuation",
        "[a]\nsecret = abc\n  def\n\tghi\nnext = x\n",
    ),
    (
        "continuation with comment and blanks",
        "[a]\nk = one\n\n  # not part of it\n  two\n\n\nj = x\n",
    ),
    (
        "indented header is a continuation",
        "[a]\nk = v\n  [b]\nj = w\n",
    ),
    ("trailing blank lines stripped", "[a]\nk = v\n\n\n"),
    (
        "indented option first after header",
        "[a]\n    k = v\n  j = w\n   m = x\n",
    ),
    ("colon delimiter", "[a]\nk: v\nj : w\nm=n:o\np:q=r\n"),
    ("empty value", "[a]\nk =\nj = \n"),
    (
        "inline comment is part of the value",
        "[a]\nregion = us-east-1 # home\nk = v ; x\n",
    ),
    ("uppercase keys", "[a]\nAWS_Access_Key_ID = AKIDEXAMPLE\n"),
    (
        "default section inherited",
        "[DEFAULT]\nregion = us-east-1\nk = d\n[a]\nk = v\n[b]\n",
    ),
    (
        "default section twice",
        "[DEFAULT]\nk = 1\n[DEFAULT]\nj = 2\n[a]\n",
    ),
    ("crlf", "[a]\r\nk = v\r\n  more\r\n[b]\r\nj = w"),
    ("lone cr", "[a]\rk = v\r  more\rj = w\r"),
    ("no newline at eof", "[a]\nk = v"),
    (
        "unicode whitespace",
        "[a]\n\u{a0}k = v\u{a0}\n\x1cj = w\x1c\n",
    ),
    ("value with equals", "[a]\nk = a=b==\n"),
    ("byte order mark", "\u{feff}[a]\nk = v\n"),
    ("empty file", ""),
    ("only comments", "# nothing here\n"),
];

/// For every case, adding a profile either works for both readers or is refused, and never
/// changes a byte of the file when it's refused. aws-config refusing the file shows up as a load
/// error, configparser refusing it (with the new profile added) as a save error.
#[test]
fn adding_a_profile_works_for_both_readers_or_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for (i, (name, text)) in CASES.iter().enumerate() {
        let path = dir.path().join(format!("case{i}"));
        fs::write(&path, text).unwrap();
        let before = oracle_dump(&path);
        let sdk_ok = sdk_accepts(text);
        let outcome = CredentialsFile::load(&path).and_then(|mut f| {
            f.upsert(&profile("zzz-new"))?;
            f.save()
        });
        match outcome {
            Err(ProfileError::Invalid(_)) => {
                // Refused: fine only if one of the readers would have refused it too.
                let strict_ok = before.starts_with("strict ok\n");
                if sdk_ok && strict_ok {
                    failures.push(format!("{name}: refused a file both readers accept"));
                }
                if fs::read_to_string(&path).unwrap() != *text {
                    failures.push(format!("{name}: a refusal changed the file"));
                }
            }
            Err(e) => failures.push(format!("{name}: unexpected error {e}")),
            Ok(()) => {
                // Written: both readers must accept the result and see the new profile, and
                // configparser must see every other section exactly as before.
                let after = oracle_dump(&path);
                let new_text = fs::read_to_string(&path).unwrap();
                if !sdk_ok || !sdk_accepts(&new_text) {
                    failures.push(format!("{name}: wrote a file aws-config refuses"));
                }
                if !after.starts_with("strict ok\n") {
                    failures.push(format!(
                        "{name}: wrote a file configparser refuses: {after}"
                    ));
                    continue;
                }
                // configparser folds any DEFAULT items in too, so I only ask for mine.
                let added = oracle_section(&path, "zzz-new");
                let mine = [
                    item("aws_access_key_id", KEY_ID),
                    item("aws_secret_access_key", SECRET),
                ];
                if !mine.iter().all(|i| added.contains(i)) {
                    failures.push(format!("{name}: new section reads as {added:?}"));
                }
                if other_sections(&before, "zzz-new") != other_sections(&after, "zzz-new") {
                    failures.push(format!(
                        "{name}: other sections changed:\n{before}\n{after}"
                    ));
                }
                match CredentialsFile::load(&path).map(|f| f.get("zzz-new")) {
                    Ok(Some(p)) if p == profile("zzz-new") => {}
                    other => failures.push(format!("{name}: reses reads back {other:?}")),
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A guard for the test above: if every case landed on one side it would prove little, so I
/// check that the list really has files each reader refuses and files both accept.
#[test]
fn the_cases_cover_both_verdicts_of_both_readers() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sdk_no, mut cli_no, mut both_ok) = (0, 0, 0);
    for (i, (_, text)) in CASES.iter().enumerate() {
        let path = dir.path().join(format!("case{i}"));
        fs::write(&path, text).unwrap();
        let strict_ok = oracle_dump(&path).starts_with("strict ok\n");
        let sdk_ok = sdk_accepts(text);
        sdk_no += usize::from(!sdk_ok);
        cli_no += usize::from(sdk_ok && !strict_ok);
        both_ok += usize::from(sdk_ok && strict_ok);
    }
    assert!(
        sdk_no >= 3 && cli_no >= 3 && both_ok >= 10,
        "{sdk_no} {cli_no} {both_ok}"
    );
}

// ---- what a section header means ----

#[test]
fn header_with_trailing_comment_is_its_own_profile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(
        &path,
        "[work]\naws_access_key_id = AKIDEXAMPLE\n\
         aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
         [personal]   # my own account\naws_access_key_id = AKIDEXAMPLE2\n\
         aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY2\n",
    )
    .unwrap();
    let file = CredentialsFile::load(&path).unwrap();
    assert_eq!(file.get("work").unwrap().access_key_id, "AKIDEXAMPLE");
    assert_eq!(file.get("personal").unwrap().access_key_id, "AKIDEXAMPLE2");
}

#[test]
fn updating_a_commented_header_section_does_not_add_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let text = "[personal]   # my own account\naws_access_key_id = AKIDOLD\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEOLD\n";
    fs::write(&path, text).unwrap();
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&profile("personal")).unwrap();
    file.save().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!(
            "[personal]   # my own account\naws_access_key_id = {KEY_ID}\n\
             aws_secret_access_key = {SECRET}\n"
        )
    );
    assert!(oracle_dump(&path).starts_with("strict ok\n"));
}

/// An upsert lands in `[ work ]` rather than adding a second `work` section.
#[test]
fn updating_a_padded_header_edits_that_section() {
    // aws-config trims `[ work ]` to `work`, so an upsert of `work` has to land in it rather
    // than add a second section the SDK would merge with it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let text = "[ work ]\naws_access_key_id = AKIDOLD\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEOLD\n";
    fs::write(&path, text).unwrap();
    let mut file = CredentialsFile::load(&path).unwrap();
    assert!(file.has_section("work"));
    file.upsert(&profile("work")).unwrap();
    file.save().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("[ work ]\naws_access_key_id = {KEY_ID}\naws_secret_access_key = {SECRET}\n")
    );
}

#[test]
fn save_refuses_a_file_with_duplicate_sections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let text = "[a]\naws_access_key_id = AKIDEXAMPLE\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
                [a]\nregion = us-east-1\n";
    fs::write(&path, text).unwrap();
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&profile("other")).unwrap();
    let err = file.save().unwrap_err();
    assert!(matches!(err, ProfileError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("'a'"), "{err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

/// A file either reader refuses is never written, and stays byte for byte as it was.
#[test]
fn a_file_either_reader_refuses_is_never_written() {
    for text in [
        // aws-config refuses these, so load does.
        "stray = line\n[a]\n",
        "[a]\nno delimiter here\n",
        "[a]\nk: v\n",
        // aws-config reads this one, but configparser refuses the repeated key, so save does.
        "[a]\nk = 1\nk = 2\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        fs::write(&path, text).unwrap();
        let outcome = CredentialsFile::load(&path).and_then(|mut f| {
            f.upsert(&profile("other"))?;
            f.save()
        });
        let err = outcome.expect_err(text);
        assert!(matches!(err, ProfileError::Invalid(_)), "{text:?}: {err:?}");
        assert!(err.to_string().contains("credentials"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}

#[test]
fn default_is_not_a_profile_name_reses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = CredentialsFile::load(&dir.path().join("credentials")).unwrap();
    let err = file.upsert(&profile("DEFAULT")).unwrap_err();
    assert!(matches!(err, ProfileError::Invalid(_)), "{err:?}");
}

/// DEFAULT values don't leak into other profiles, because aws-config doesn't do that.
#[test]
fn default_section_is_not_inherited_by_the_sdk() {
    // configparser would give `a` the DEFAULT region, but aws-config treats DEFAULT as one more
    // profile, and aws-config is what reses reads with now.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(
        &path,
        "[DEFAULT]\nregion = us-east-2\n[a]\naws_access_key_id = AKIDEXAMPLE\n\
         aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n",
    )
    .unwrap();
    let file = CredentialsFile::load(&path).unwrap();
    let names: Vec<_> = file.profiles().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["a"]);
    assert_eq!(file.get("a").unwrap().region, None);
}

// ---- has_section ----

#[test]
fn has_section_sees_incomplete_sections_too() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(
        &path,
        "[only-region]\nregion = us-east-2\n[personal]  # note\n[DEFAULT]\nk = v\n",
    )
    .unwrap();
    let file = CredentialsFile::load(&path).unwrap();
    assert!(file.profiles().is_empty());
    assert!(file.has_section("only-region"));
    assert!(file.has_section("personal"));
    assert!(!file.has_section("missing"));
    assert!(
        !file.has_section("DEFAULT"),
        "configparser never lists DEFAULT"
    );
    // An indented header under a key is a continuation line, not a section.
    fs::write(&path, "[a]\nk = v\n  [hidden]\n").unwrap();
    assert!(!CredentialsFile::load(&path).unwrap().has_section("hidden"));
}

#[test]
fn has_section_sees_a_section_added_by_upsert() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = CredentialsFile::load(&dir.path().join("credentials")).unwrap();
    assert!(!file.has_section("new"));
    file.upsert(&profile("new")).unwrap();
    assert!(file.has_section("new"));
}

// ---- the legacy aws_security_token ----

const LEGACY: &str = "[a]\n\
aws_access_key_id = AKIDOLD\n\
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEOLD\n\
aws_security_token = FAKELEGACYTOKEN\n\
\x20\x20continued-legacy\n\
aws_session_token = FAKESESSIONTOKEN\n\
note = keep\n";

/// With both token keys present, the legacy one is what reses reads, as botocore does.
#[test]
fn security_token_wins_over_session_token_like_botocore() {
    // botocore's SharedCredentialProvider checks TOKENS = ['aws_security_token',
    // 'aws_session_token'] in order and takes the first key present.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(&path, LEGACY).unwrap();
    let p = CredentialsFile::load(&path).unwrap().get("a").unwrap();
    assert_eq!(
        p.session_token.as_deref(),
        Some("FAKELEGACYTOKEN\ncontinued-legacy")
    );
}

/// A file with only the legacy token key still gives the profile a token.
#[test]
fn security_token_alone_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(
        &path,
        "[a]\naws_access_key_id = AKIDEXAMPLE\n\
         aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
         aws_security_token = FAKELEGACYTOKEN\n",
    )
    .unwrap();
    let p = CredentialsFile::load(&path).unwrap().get("a").unwrap();
    assert_eq!(p.session_token.as_deref(), Some("FAKELEGACYTOKEN"));
}

/// An empty legacy token still wins, so the profile has no token, as in botocore.
#[test]
fn an_empty_security_token_still_shadows_the_session_token_like_botocore() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(
        &path,
        "[a]\naws_access_key_id = AKIDEXAMPLE\n\
         aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
         aws_security_token =\naws_session_token = FAKESESSIONTOKEN\n",
    )
    .unwrap();
    let p = CredentialsFile::load(&path).unwrap().get("a").unwrap();
    assert_eq!(p.session_token, None);
}

/// Upsert swaps the legacy token for aws_session_token, continuation lines and all.
#[test]
fn upsert_replaces_the_legacy_token_with_the_session_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(&path, LEGACY).unwrap();
    let mut p = profile("a");
    p.session_token = Some("FAKENEWTOKEN".into());
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&p).unwrap();
    file.save().unwrap();
    assert_eq!(
        oracle_section(&path, "a"),
        [
            item("aws_access_key_id", KEY_ID),
            item("aws_secret_access_key", SECRET),
            item("aws_session_token", "FAKENEWTOKEN"),
            item("note", "keep"),
        ]
    );
    assert_eq!(
        CredentialsFile::load(&path).unwrap().get("a").unwrap(),
        p,
        "what botocore would use is what I wrote"
    );
}

/// Clearing the token removes both spellings of it and nothing else.
#[test]
fn clearing_the_token_removes_both_spellings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(&path, LEGACY).unwrap();
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&profile("a")).unwrap();
    file.save().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!(
            "[a]\naws_access_key_id = {KEY_ID}\naws_secret_access_key = {SECRET}\nnote = keep\n"
        )
    );
}

// ---- continuation lines ----

const CONTINUED: &str = "[a]\n\
aws_access_key_id = AKIDOLD\n\
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEOLD\n\
\x20\x20continued-secret\n\
aws_session_token = FAKETOKENOLD\n\
\x20\x20\x20\x20continued-token\n\
\n\
\x20\x20\x20\x20more-token\n\
region = us-east-1\n\
\x20\x20continued-region\n\
note = keep\n\
\x20\x20continued-note\n\
[b]\n\
aws_access_key_id = AKIDEXAMPLE2\n\
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY2\n";

/// Continuation lines join into the value the way aws-config joins them.
#[test]
fn reading_joins_continuation_lines_like_the_sdk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(&path, CONTINUED).unwrap();
    let p = CredentialsFile::load(&path).unwrap().get("a").unwrap();
    assert_eq!(
        p.secret_access_key,
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEOLD\ncontinued-secret"
    );
    assert_eq!(
        p.session_token.as_deref(),
        Some("FAKETOKENOLD\ncontinued-token\nmore-token")
    );
}

#[test]
fn rewriting_a_key_drops_its_continuation_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(&path, CONTINUED).unwrap();
    let before_b = oracle_section(&path, "b");
    let mut p = profile("a");
    p.session_token = Some("FAKETOKENNEW".into());
    p.region = Some("eu-west-1".into());
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&p).unwrap();
    file.save().unwrap();
    assert_eq!(
        oracle_section(&path, "a"),
        [
            item("aws_access_key_id", KEY_ID),
            item("aws_secret_access_key", SECRET),
            item("aws_session_token", "FAKETOKENNEW"),
            item("region", "eu-west-1"),
            item("note", "keep\ncontinued-note"),
        ]
    );
    assert_eq!(oracle_section(&path, "b"), before_b);
}

#[test]
fn removing_a_token_or_region_drops_its_continuation_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(&path, CONTINUED).unwrap();
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&profile("a")).unwrap();
    file.save().unwrap();
    assert_eq!(
        oracle_section(&path, "a"),
        [
            item("aws_access_key_id", KEY_ID),
            item("aws_secret_access_key", SECRET),
            item("note", "keep\ncontinued-note"),
        ]
    );
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("continued-token"), "{text}");
    assert!(!text.contains("more-token"), "{text}");
    assert!(!text.contains("continued-region"), "{text}");
    assert_eq!(
        text,
        format!(
            "[a]\naws_access_key_id = {KEY_ID}\naws_secret_access_key = {SECRET}\n\
             note = keep\n  continued-note\n[b]\naws_access_key_id = AKIDEXAMPLE2\n\
             aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY2\n"
        )
    );
}

#[test]
fn adding_a_key_after_a_continued_value_does_not_split_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let text = "[a]\naws_access_key_id = AKIDEXAMPLE\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
                note = one\n  two\n";
    fs::write(&path, text).unwrap();
    let mut p = profile("a");
    p.region = Some("us-west-2".into());
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&p).unwrap();
    file.save().unwrap();
    assert_eq!(
        oracle_section(&path, "a"),
        [
            item("aws_access_key_id", KEY_ID),
            item("aws_secret_access_key", SECRET),
            item("note", "one\ntwo"),
            item("region", "us-west-2"),
        ]
    );
}
