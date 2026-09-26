//! The shared credentials file: parsing, byte-for-byte round trips, validation, and how it
//! lands on disk. Every test works in its own temp dir and passes paths explicitly.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use reses::aws_profile::{CredentialsFile, Profile, ProfileError, credentials_path_from};

const KEY_ID: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

/// Every construct the parser has to leave alone: `#` and `;` comments, a preamble before the
/// first section, blank lines, both `key=value` and `key = value`, odd spacing, trailing
/// whitespace, keys we don't know, CRLF endings, and a last section with no newline at EOF.
const FIXTURE_CRLF: &str = "# preamble comment\r\n\
; semicolon comment\r\n\
\r\n\
[default]\r\n\
aws_access_key_id=AKIDEXAMPLE\r\n\
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\r\n\
region=us-east-1\r\n\
output = json\r\n\
\r\n\
; comment between sections\r\n\
[work]\r\n\
aws_access_key_id = AKIDEXAMPLE2\r\n\
aws_secret_access_key   =    wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY2  \r\n\
unknown_key = keep me\r\n\
# trailing comment in section\r\n\
\r\n\
\r\n\
[last]\r\n\
aws_access_key_id=AKIDEXAMPLE3\r\n\
aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY3";

/// The same constructs with LF endings and a session token plus region to clear.
const FIXTURE_LF: &str = "# top\n\
[default]\n\
aws_access_key_id = AKIDEXAMPLE\n\
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
; keep this comment\n\
aws_session_token = FAKETOKENEXAMPLE\n\
cli_pager =\n\
region = eu-west-1\n\
\n\
[other]\n\
aws_access_key_id=AKIDEXAMPLE2\n\
aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY2\n";

fn write(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("credentials");
    fs::write(&path, contents).unwrap();
    path
}

fn profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        access_key_id: KEY_ID.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
        region: None,
    }
}

fn upsert_and_save(path: &Path, p: &Profile) -> String {
    let mut file = CredentialsFile::load(path).unwrap();
    file.upsert(p).unwrap();
    file.save().unwrap();
    fs::read_to_string(path).unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---- paths ----

#[test]
fn path_honours_shared_credentials_file_env() {
    let p = credentials_path_from(
        Some(OsString::from("/elsewhere/creds")),
        Some(OsString::from("/home/u")),
    );
    assert_eq!(p, PathBuf::from("/elsewhere/creds"));
}

#[test]
fn path_falls_back_to_home_aws_credentials() {
    let p = credentials_path_from(None, Some(OsString::from("/home/u")));
    assert_eq!(p, PathBuf::from("/home/u/.aws/credentials"));
}

#[test]
fn empty_env_var_counts_as_unset() {
    let p = credentials_path_from(Some(OsString::new()), Some(OsString::from("/home/u")));
    assert_eq!(p, PathBuf::from("/home/u/.aws/credentials"));
}

// ---- parsing ----

#[test]
fn missing_file_loads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nope");
    let file = CredentialsFile::load(&path).unwrap();
    assert!(file.profiles().is_empty());
    assert_eq!(file.path(), path);
}

#[test]
fn parses_every_profile_in_file_order() {
    let dir = tempfile::tempdir().unwrap();
    let file = CredentialsFile::load(&write(dir.path(), FIXTURE_CRLF)).unwrap();
    let names: Vec<_> = file.profiles().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["default", "work", "last"]);
}

#[test]
fn parses_both_spacing_styles_and_region() {
    let dir = tempfile::tempdir().unwrap();
    let file = CredentialsFile::load(&write(dir.path(), FIXTURE_CRLF)).unwrap();
    assert_eq!(
        file.get("default").unwrap(),
        Profile {
            name: "default".into(),
            access_key_id: KEY_ID.into(),
            secret_access_key: SECRET.into(),
            session_token: None,
            region: Some("us-east-1".into()),
        }
    );
    let work = file.get("work").unwrap();
    assert_eq!(work.access_key_id, "AKIDEXAMPLE2");
    assert_eq!(
        work.secret_access_key,
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY2"
    );
    assert_eq!(work.region, None);
    let last = file.get("last").unwrap();
    assert_eq!(
        last.secret_access_key,
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY3"
    );
}

#[test]
fn parses_session_token() {
    let dir = tempfile::tempdir().unwrap();
    let file = CredentialsFile::load(&write(dir.path(), FIXTURE_LF)).unwrap();
    let p = file.get("default").unwrap();
    assert_eq!(p.session_token.as_deref(), Some("FAKETOKENEXAMPLE"));
    assert_eq!(p.region.as_deref(), Some("eu-west-1"));
    assert!(file.get("missing").is_none());
}

#[test]
fn comment_lines_are_not_keys() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[a]\n# aws_access_key_id = AKIDCOMMENTED\n; region = xx-fake-1\n\
                aws_access_key_id = AKIDEXAMPLE\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n";
    let file = CredentialsFile::load(&write(dir.path(), text)).unwrap();
    let p = file.get("a").unwrap();
    assert_eq!(p.access_key_id, KEY_ID);
    assert_eq!(p.region, None);
}

#[test]
fn keys_are_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[a]\nAWS_ACCESS_KEY_ID = AKIDEXAMPLE\n\
                Aws_Secret_Access_Key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n";
    let file = CredentialsFile::load(&write(dir.path(), text)).unwrap();
    assert_eq!(file.get("a").unwrap().secret_access_key, SECRET);
}

#[test]
fn secret_containing_equals_sign_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[a]\naws_access_key_id=AKIDEXAMPLE\naws_secret_access_key=abc=def==\n";
    let file = CredentialsFile::load(&write(dir.path(), text)).unwrap();
    assert_eq!(file.get("a").unwrap().secret_access_key, "abc=def==");
}

#[test]
fn section_without_keys_is_not_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[only-region]\nregion = us-east-2\n";
    let file = CredentialsFile::load(&write(dir.path(), text)).unwrap();
    assert!(file.profiles().is_empty());
}

#[test]
fn debug_output_redacts_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let file = CredentialsFile::load(&write(dir.path(), FIXTURE_LF)).unwrap();
    let dbg = format!("{file:?}");
    assert!(!dbg.contains("EXAMPLEKEY"), "{dbg}");
    assert!(!dbg.contains("FAKETOKEN"), "{dbg}");
}

// ---- round trips ----

#[test]
fn load_then_save_is_byte_identical() {
    for fixture in [FIXTURE_CRLF, FIXTURE_LF] {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), fixture);
        CredentialsFile::load(&path).unwrap().save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), fixture.as_bytes());
    }
}

#[test]
fn upsert_with_unchanged_values_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_CRLF);
    let existing = CredentialsFile::load(&path).unwrap().get("work").unwrap();
    assert_eq!(upsert_and_save(&path, &existing), FIXTURE_CRLF);
}

#[test]
fn adding_a_new_profile_leaves_everything_else_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_CRLF);
    let mut p = profile("added");
    p.region = Some("ca-central-1".into());
    let out = upsert_and_save(&path, &p);
    let expected = format!(
        "{FIXTURE_CRLF}\r\n\r\n[added]\r\naws_access_key_id = {KEY_ID}\r\n\
         aws_secret_access_key = {SECRET}\r\nregion = ca-central-1\r\n"
    );
    assert_eq!(out.as_bytes(), expected.as_bytes());
}

#[test]
fn adding_to_an_empty_file_writes_just_the_section() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let out = upsert_and_save(&path, &profile("default"));
    assert_eq!(
        out,
        format!("[default]\naws_access_key_id = {KEY_ID}\naws_secret_access_key = {SECRET}\n")
    );
}

#[test]
fn updating_one_profile_rewrites_only_its_changed_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_CRLF);
    let mut p = CredentialsFile::load(&path).unwrap().get("work").unwrap();
    p.secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLENEW".into();
    let out = upsert_and_save(&path, &p);
    let expected = FIXTURE_CRLF.replace(
        "aws_secret_access_key   =    wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY2  \r\n",
        "aws_secret_access_key   =    wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLENEW  \r\n",
    );
    assert_ne!(expected, FIXTURE_CRLF, "the replace above must hit");
    assert_eq!(out.as_bytes(), expected.as_bytes());
}

#[test]
fn updating_a_compact_style_line_keeps_the_compact_style() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_CRLF);
    let mut p = CredentialsFile::load(&path)
        .unwrap()
        .get("default")
        .unwrap();
    p.region = Some("ap-south-1".into());
    let out = upsert_and_save(&path, &p);
    let expected = FIXTURE_CRLF.replace("region=us-east-1\r\n", "region=ap-south-1\r\n");
    assert_eq!(out.as_bytes(), expected.as_bytes());
}

#[test]
fn adding_a_key_to_the_eof_section_keeps_no_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_CRLF);
    let mut p = CredentialsFile::load(&path).unwrap().get("last").unwrap();
    p.session_token = Some("FAKETOKENEXAMPLE".into());
    let out = upsert_and_save(&path, &p);
    let expected = format!("{FIXTURE_CRLF}\r\naws_session_token=FAKETOKENEXAMPLE");
    assert_eq!(out.as_bytes(), expected.as_bytes());
}

#[test]
fn adding_a_key_to_a_middle_section_goes_after_its_last_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_CRLF);
    let mut p = CredentialsFile::load(&path).unwrap().get("work").unwrap();
    p.region = Some("us-west-2".into());
    let out = upsert_and_save(&path, &p);
    let expected = FIXTURE_CRLF.replace(
        "unknown_key = keep me\r\n",
        "unknown_key = keep me\r\nregion = us-west-2\r\n",
    );
    assert_eq!(out.as_bytes(), expected.as_bytes());
}

#[test]
fn clearing_optional_values_removes_their_lines_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_LF);
    let mut p = CredentialsFile::load(&path)
        .unwrap()
        .get("default")
        .unwrap();
    p.session_token = None;
    p.region = None;
    let out = upsert_and_save(&path, &p);
    let expected = FIXTURE_LF
        .replace("aws_session_token = FAKETOKENEXAMPLE\n", "")
        .replace("region = eu-west-1\n", "");
    assert_eq!(out.as_bytes(), expected.as_bytes());
    let reread = CredentialsFile::load(&path)
        .unwrap()
        .get("default")
        .unwrap();
    assert_eq!(reread.session_token, None);
    assert_eq!(reread.region, None);
}

#[test]
fn empty_optional_value_counts_as_clearing() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_LF);
    let mut p = CredentialsFile::load(&path)
        .unwrap()
        .get("default")
        .unwrap();
    p.session_token = Some(String::new());
    let out = upsert_and_save(&path, &p);
    assert_eq!(
        out,
        FIXTURE_LF.replace("aws_session_token = FAKETOKENEXAMPLE\n", "")
    );
}

#[test]
fn upsert_is_visible_before_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_LF);
    let mut file = CredentialsFile::load(&path).unwrap();
    file.upsert(&profile("new")).unwrap();
    assert_eq!(file.get("new").unwrap(), profile("new"));
    assert_eq!(fs::read_to_string(&path).unwrap(), FIXTURE_LF);
}

#[test]
fn duplicate_owned_keys_collapse_to_one_on_update() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[a]\naws_access_key_id = AKIDOLD\nnote = x\naws_access_key_id = AKIDOLDER\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n";
    let path = write(dir.path(), text);
    let out = upsert_and_save(&path, &profile("a"));
    assert_eq!(
        out,
        "[a]\naws_access_key_id = AKIDEXAMPLE\nnote = x\n\
         aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n"
    );
}

// ---- validation ----

#[test]
fn rejects_values_that_would_break_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_LF);
    let mut bad = Vec::new();
    for name in [
        "", " ", "a]b", "a[b", "[x]", "a\nb", "a\rb", " lead", "trail ",
    ] {
        bad.push(Profile {
            name: name.into(),
            ..profile("x")
        });
    }
    bad.push(Profile {
        access_key_id: String::new(),
        ..profile("x")
    });
    bad.push(Profile {
        secret_access_key: String::new(),
        ..profile("x")
    });
    bad.push(Profile {
        access_key_id: "AKID\nEXAMPLE".into(),
        ..profile("x")
    });
    bad.push(Profile {
        secret_access_key: "abc\r\n[evil]".into(),
        ..profile("x")
    });
    bad.push(Profile {
        session_token: Some("tok\nregion = x".into()),
        ..profile("x")
    });
    bad.push(Profile {
        region: Some("us-east-1\n".into()),
        ..profile("x")
    });
    bad.push(Profile {
        secret_access_key: " padded ".into(),
        ..profile("x")
    });
    for p in bad {
        let mut file = CredentialsFile::load(&path).unwrap();
        let err = file
            .upsert(&p)
            .expect_err(&format!("{p:?} should be rejected"));
        assert!(matches!(err, ProfileError::Invalid(_)), "{err:?}");
        assert!(file.get("x").is_none());
    }
}

#[test]
fn rejection_message_never_contains_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "");
    let mut file = CredentialsFile::load(&path).unwrap();
    let err = file
        .upsert(&Profile {
            secret_access_key: "wJalrXUtnFEMI\nSECRETEXAMPLE".into(),
            ..profile("x")
        })
        .unwrap_err();
    assert!(!err.to_string().contains("SECRETEXAMPLE"), "{err}");
}

// ---- writing to disk ----

#[test]
fn save_creates_the_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("home/.aws/credentials");
    upsert_and_save(&path, &profile("default"));
    assert!(path.is_file());
}

#[cfg(unix)]
#[test]
fn new_file_is_mode_0600() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".aws/credentials");
    upsert_and_save(&path, &profile("default"));
    assert_eq!(mode(&path), 0o600);
}

#[cfg(unix)]
#[test]
fn rewriting_a_0644_file_leaves_it_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_LF);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(mode(&path), 0o644);
    upsert_and_save(&path, &profile("other"));
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn save_leaves_no_temp_files_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_LF);
    upsert_and_save(&path, &profile("new"));
    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [OsString::from("credentials")]);
}

#[cfg(unix)]
#[test]
fn save_replaces_the_file_rather_than_writing_in_place() {
    // A hard link to the old file still sees the old contents: proof of temp-then-rename.
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), FIXTURE_LF);
    let before = fs::metadata(&path).unwrap().ino();
    let link = dir.path().join("old-link");
    fs::hard_link(&path, &link).unwrap();
    upsert_and_save(&path, &profile("new"));
    assert_ne!(fs::metadata(&path).unwrap().ino(), before);
    assert_eq!(fs::read_to_string(&link).unwrap(), FIXTURE_LF);
}

#[cfg(unix)]
#[test]
fn save_through_a_symlink_updates_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real-credentials");
    fs::write(&real, FIXTURE_LF).unwrap();
    let link = dir.path().join("credentials");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    upsert_and_save(&link, &profile("new"));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(CredentialsFile::load(&real).unwrap().get("new").is_some());
}

#[test]
fn unreadable_path_reports_read_error_with_path() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where the file should be.
    let path = dir.path().join("credentials");
    fs::create_dir(&path).unwrap();
    let err = CredentialsFile::load(&path).unwrap_err();
    assert!(matches!(err, ProfileError::Read { .. }), "{err:?}");
    assert!(err.to_string().contains("credentials"));
}

#[test]
fn clearing_the_last_line_of_the_file_keeps_no_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[a]\r\naws_access_key_id=AKIDEXAMPLE\r\n\
                aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\r\n\
                region=us-east-1";
    let path = write(dir.path(), text);
    let mut p = CredentialsFile::load(&path).unwrap().get("a").unwrap();
    p.region = None;
    let out = upsert_and_save(&path, &p);
    assert_eq!(
        out,
        "[a]\r\naws_access_key_id=AKIDEXAMPLE\r\n\
         aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
    );
}

// ---- reading goes through aws-config ----

/// A ` #` or ` ;` comment after a value is dropped, the way aws-config reads it.
#[test]
fn inline_comment_after_whitespace_is_dropped_like_the_sdk() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[a]\naws_access_key_id = AKIDEXAMPLE # the old key\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
                region = us-east-1 ; home\n";
    let file = CredentialsFile::load(&write(dir.path(), text)).unwrap();
    let p = file.get("a").unwrap();
    assert_eq!(p.access_key_id, KEY_ID);
    assert_eq!(p.region.as_deref(), Some("us-east-1"));
}

/// A `[profile x]` section in the credentials file isn't listed.
#[test]
fn profile_prefixed_section_is_ignored_like_the_sdk() {
    // aws-config ignores `[profile x]` in the credentials file, so reses can't connect with it
    // and shouldn't list it.
    let dir = tempfile::tempdir().unwrap();
    let text = "[profile x]\naws_access_key_id = AKIDEXAMPLE\n\
                aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n";
    let file = CredentialsFile::load(&write(dir.path(), text)).unwrap();
    assert!(file.profiles().is_empty(), "{:?}", file.profiles());
}

/// A file aws-config can't parse fails to load, and the error names the file.
#[test]
fn a_file_the_sdk_cannot_parse_fails_to_load_naming_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "[a]\naws_access_key_id: AKIDEXAMPLE\n");
    let err = CredentialsFile::load(&path).unwrap_err();
    assert!(matches!(err, ProfileError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("credentials"), "{err}");
}

/// Names aws-config would skip are refused, and every character it allows works.
#[test]
fn names_the_sdk_would_ignore_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = CredentialsFile::load(&dir.path().join("credentials")).unwrap();
    for name in [
        "my profile",
        "a#b",
        "a;b",
        "caf\u{e9}",
        "a\tb",
        "a=b",
        "a\"b",
    ] {
        let err = file
            .upsert(&Profile {
                name: name.into(),
                ..profile("x")
            })
            .expect_err(name);
        assert!(matches!(err, ProfileError::Invalid(_)), "{name}: {err:?}");
    }
    // Everything aws-config allows in a name still works.
    let fine = "Work_2-dev/eu.1%x@corp:+";
    file.upsert(&Profile {
        name: fine.into(),
        ..profile("x")
    })
    .unwrap();
    assert_eq!(file.get(fine).unwrap().name, fine);
}

/// A leading `~` in the override becomes the home directory.
#[test]
fn path_expands_a_leading_tilde_like_the_sdk() {
    let p = credentials_path_from(
        Some(OsString::from("~/creds")),
        Some(OsString::from("/home/u")),
    );
    assert_eq!(p, PathBuf::from("/home/u/creds"));
}
