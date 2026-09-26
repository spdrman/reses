//! reses's INI parser pinned against Python's configparser, which is what botocore and the AWS
//! CLI read these files with. tests/profile_oracle.py prints what `RawConfigParser` makes of a
//! file; these tests print the same thing from reses's parser and compare.
//!
//! python3 has to be on PATH. It is in the CI image and on the GitHub runners, and a missing
//! python3 fails these tests rather than skipping them, since a skip would look like a pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use reses::aws_profile::{
    CredentialsFile, IniError, IniView, Profile, ProfileError, parse_ini, region_from_config_file,
};

const KEY_ID: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/profile_oracle.py")
}

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

fn h(s: &str) -> String {
    if s.is_empty() {
        "-".into()
    } else {
        hex::encode(s)
    }
}

fn error_line(e: &IniError) -> String {
    match e {
        IniError::MissingSectionHeader => "MissingSectionHeaderError".into(),
        IniError::Parsing => "ParsingError".into(),
        IniError::DuplicateSection { section } => format!("DuplicateSectionError {}", h(section)),
        IniError::DuplicateOption { section, option } => {
            format!("DuplicateOptionError {} {}", h(section), h(option))
        }
    }
}

/// The oracle's dump format, built from reses's parser.
fn ours(view: &IniView) -> String {
    let mut out = vec![match &view.strict_error {
        None => "strict ok".to_string(),
        Some(e) => format!("strict {}", error_line(e)),
    }];
    match &view.read {
        Err(e) => out.push(format!("nonstrict {}", error_line(e))),
        Ok(data) => {
            out.push("nonstrict ok".into());
            out.push("defaults".into());
            for (k, v) in &data.defaults {
                out.push(format!("item {} {}", h(k), h(v)));
            }
            for (name, items) in &data.sections {
                out.push(format!("section {}", h(name)));
                for (k, v) in items {
                    out.push(format!("item {} {}", h(k), h(v)));
                }
            }
        }
    }
    out.join("\n") + "\n"
}

fn oracle_dump(path: &Path) -> String {
    python(&["dump", path.to_str().unwrap()])
}

/// Every construct I could find where a hand-rolled INI reader and configparser disagree.
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

#[test]
fn parse_matches_configparser_for_every_case() {
    let dir = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for (i, (name, text)) in CASES.iter().enumerate() {
        let path = dir.path().join(format!("case{i}"));
        fs::write(&path, text).unwrap();
        let want = oracle_dump(&path);
        let got = ours(&parse_ini(text));
        if want != got {
            failures.push(format!("{name}:\n  python: {want:?}\n  reses:  {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---- what a section header means (item 17) ----

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

#[test]
fn save_refuses_anything_configparser_would_refuse() {
    for text in [
        "stray = line\n[a]\n",
        "[a]\nno delimiter here\n",
        "[a]\nk = 1\nk = 2\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        fs::write(&path, text).unwrap();
        let mut file = CredentialsFile::load(&path).unwrap();
        file.upsert(&profile("other")).unwrap();
        let err = file.save().unwrap_err();
        assert!(matches!(err, ProfileError::Invalid(_)), "{text:?}: {err:?}");
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

#[test]
fn default_section_values_are_inherited_like_configparser() {
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
    assert_eq!(file.get("a").unwrap().region.as_deref(), Some("us-east-2"));
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

// ---- continuation lines (item 20) ----

fn profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        access_key_id: KEY_ID.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
        region: None,
    }
}

/// The items configparser sees in one section of a file on disk.
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

fn item(k: &str, v: &str) -> String {
    format!("item {} {}", h(k), h(v))
}

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

#[test]
fn reading_joins_continuation_lines_like_configparser() {
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
        Some("FAKETOKENOLD\ncontinued-token\n\nmore-token")
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

#[test]
fn colon_delimited_keys_are_rewritten_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    fs::write(
        &path,
        "[a]\naws_access_key_id: AKIDOLD\naws_secret_access_key:wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEOLD\n",
    )
    .unwrap();
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&profile("a")).unwrap();
    file.save().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("[a]\naws_access_key_id: {KEY_ID}\naws_secret_access_key:{SECRET}\n")
    );
}

// ---- region from the config file ----

const AWS_CONFIG: &str = "[default]\nregion = us-east-2\n\
[profile work]\nregion=eu-central-1\n\
[profile   spaced  ]\nregion = ap-northeast-1\n\
[profile \"quoted name\"]\nregion = ca-central-1\n\
[profile a b]\nregion = xx-three-words-1\n\
[profilework2]\nregion = xx-no-space-1\n\
[plain]\nregion = xx-not-a-profile-1\n\
[profile later]\nregion = us-west-1\n\
[profile later ]\nregion = us-west-2\n\
[profile continued]\nregion = sa-east-1\n  tail\n\
[profile noregion]\noutput = text\n\
[profile emptyregion]\nregion =\n\
[profile \"unbalanced]\nregion = xx-bad-quote-1\n";

const REGION_PROFILES: &[&str] = &[
    "default",
    "work",
    "spaced",
    "quoted name",
    "a b",
    "a",
    "work2",
    "plain",
    "later",
    "continued",
    "noregion",
    "emptyregion",
    "\"unbalanced",
    "absent",
];

fn region_matches(text: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    fs::write(&path, text).unwrap();
    for profile in REGION_PROFILES {
        let want = python(&["region", path.to_str().unwrap(), profile]);
        let got = match region_from_config_file(&path, profile) {
            Some(r) => h(&r),
            None => "none".into(),
        };
        assert_eq!(got, want.trim_end(), "profile {profile:?} in {text:?}");
    }
}

#[test]
fn region_matches_botocore_for_every_profile() {
    region_matches(AWS_CONFIG);
}

#[test]
fn region_with_default_inherited_matches_botocore() {
    region_matches(
        "[DEFAULT]\nregion = us-east-1\n[profile work]\n[default]\nregion = ap-south-1\n",
    );
}

#[test]
fn region_from_a_file_configparser_refuses_is_none_like_botocore() {
    region_matches("[profile work]\nregion = eu-west-1\n[profile work]\noutput = json\n");
    region_matches("[profile work]\nregion = eu-west-1\nbogus line\n");
}

#[test]
fn profile_default_spelling_matches_botocore() {
    region_matches("[profile default]\nregion = sa-east-1\n");
    region_matches("[profile default]\nregion = sa-east-1\n[default]\nregion = us-east-2\n");
    region_matches("[default]\nregion = us-east-2\n[profile default]\nregion = sa-east-1\n");
}
