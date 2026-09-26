//! reses's own settings file.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use reses::config::{AppConfig, ConfigError, Inbox};

fn sample() -> AppConfig {
    AppConfig {
        default_profile: Some("work".into()),
        inbox: Some(Inbox {
            profile: "work".into(),
            bucket: "mail-bucket".into(),
            prefix: "inbound/".into(),
            region: Some("eu-west-1".into()),
        }),
    }
}

fn s(v: &str) -> Option<OsString> {
    Some(OsString::from(v))
}

#[test]
fn path_prefers_reses_config() {
    assert_eq!(
        AppConfig::path_from(s("/x/reses.toml"), s("/xdg"), s("/home/u")),
        PathBuf::from("/x/reses.toml")
    );
}

#[test]
fn path_then_xdg_config_home() {
    assert_eq!(
        AppConfig::path_from(None, s("/xdg"), s("/home/u")),
        PathBuf::from("/xdg/reses/config.toml")
    );
}

#[test]
fn path_then_home_dot_config() {
    assert_eq!(
        AppConfig::path_from(None, None, s("/home/u")),
        PathBuf::from("/home/u/.config/reses/config.toml")
    );
}

#[test]
fn empty_or_relative_env_values_are_skipped() {
    assert_eq!(
        AppConfig::path_from(s(""), s(""), s("/home/u")),
        PathBuf::from("/home/u/.config/reses/config.toml")
    );
    // The XDG spec says a relative XDG_CONFIG_HOME is invalid and should be ignored.
    assert_eq!(
        AppConfig::path_from(None, s("relative/dir"), s("/home/u")),
        PathBuf::from("/home/u/.config/reses/config.toml")
    );
}

#[test]
fn missing_file_loads_as_default() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        AppConfig::load(&dir.path().join("config.toml")).unwrap(),
        AppConfig::default()
    );
}

#[test]
fn save_then_load_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    sample().save(&path).unwrap();
    assert_eq!(AppConfig::load(&path).unwrap(), sample());
}

#[test]
fn default_config_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    AppConfig::default().save(&path).unwrap();
    assert_eq!(AppConfig::load(&path).unwrap(), AppConfig::default());
}

#[test]
fn loads_a_hand_written_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        "default_profile = \"work\"\n\n[inbox]\nprofile = \"work\"\nbucket = \"mail-bucket\"\n\
         prefix = \"inbound/\"\nregion = \"eu-west-1\"\n",
    )
    .unwrap();
    assert_eq!(AppConfig::load(&path).unwrap(), sample());
}

#[test]
fn save_creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a/b/reses/config.toml");
    sample().save(&path).unwrap();
    assert_eq!(AppConfig::load(&path).unwrap(), sample());
}

#[test]
fn save_overwrites_and_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "default_profile = \"old\"\n").unwrap();
    sample().save(&path).unwrap();
    assert_eq!(AppConfig::load(&path).unwrap(), sample());
    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [OsString::from("config.toml")]);
}

#[cfg(unix)]
#[test]
fn save_replaces_the_file_rather_than_writing_in_place() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "default_profile = \"old\"\n").unwrap();
    let link = dir.path().join("old-link");
    fs::hard_link(&path, &link).unwrap();
    let before = fs::metadata(&path).unwrap().ino();
    sample().save(&path).unwrap();
    assert_ne!(fs::metadata(&path).unwrap().ino(), before);
    assert_eq!(
        fs::read_to_string(&link).unwrap(),
        "default_profile = \"old\"\n"
    );
}

#[test]
fn parse_error_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.toml");
    fs::write(&path, "default_profile = [unterminated\n").unwrap();
    let err = AppConfig::load(&path).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
    assert!(err.to_string().contains("broken.toml"), "{err}");
}

#[test]
fn wrong_shape_is_a_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "[inbox]\nprofile = \"work\"\n").unwrap();
    assert!(matches!(
        AppConfig::load(&path).unwrap_err(),
        ConfigError::Parse { .. }
    ));
}

#[test]
fn unreadable_path_is_a_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        AppConfig::load(&path).unwrap_err(),
        ConfigError::Read { .. }
    ));
}

#[test]
fn save_failure_is_a_write_error_with_path() {
    let dir = tempfile::tempdir().unwrap();
    // A file where a parent directory needs to be.
    let blocker = dir.path().join("blocker");
    fs::write(&blocker, "").unwrap();
    let path = blocker.join("config.toml");
    let err = sample().save(&path).unwrap_err();
    assert!(matches!(err, ConfigError::Write { .. }), "{err:?}");
    assert!(err.to_string().contains("blocker"), "{err}");
}

// ---- the inbox is checked when it loads (item 24) ----

fn inbox_file(bucket: &str, prefix: &str) -> String {
    format!("[inbox]\nprofile = \"work\"\nbucket = \"{bucket}\"\nprefix = \"{prefix}\"\n")
}

#[test]
fn bad_inbox_is_a_parse_error_naming_the_path() {
    let cases = [
        ("mail-bucket", "inbound"),
        ("mail-bucket", "a/b"),
        ("mail/bucket", ""),
        ("", ""),
        ("mail bucket", ""),
        ("mail-bucket\\n", ""),
        ("-leading-dash", ""),
        ("..", ""),
    ];
    for (bucket, prefix) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, inbox_file(bucket, prefix)).unwrap();
        let err = AppConfig::load(&path).expect_err(&format!("{bucket:?} {prefix:?}"));
        assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
        assert!(err.to_string().contains("config.toml"), "{err}");
    }
}

#[test]
fn good_inbox_loads() {
    // Buckets made before March 2018 in us-east-1 may use capitals and underscores, and reses
    // saves whatever ListBuckets returned, so those have to load too.
    let cases = [
        ("mail-bucket", ""),
        ("mail-bucket", "inbound/"),
        ("mail.bucket.example", "a/b/"),
        ("Legacy_Bucket", "x/"),
        ("abc", "/"),
    ];
    for (bucket, prefix) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, inbox_file(bucket, prefix)).unwrap();
        let cfg = AppConfig::load(&path).unwrap_or_else(|e| panic!("{bucket:?} {prefix:?}: {e}"));
        assert_eq!(cfg.inbox.unwrap().bucket, bucket);
    }
}

#[test]
fn save_refuses_an_inbox_that_would_not_load_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = sample();
    cfg.inbox.as_mut().unwrap().prefix = "no-slash".into();
    assert!(matches!(
        cfg.save(&path).unwrap_err(),
        ConfigError::Write { .. }
    ));
    assert!(!path.exists());
}

#[test]
fn inbox_validate_is_usable_before_saving() {
    let mut inbox = sample().inbox.unwrap();
    assert_eq!(inbox.validate(), Ok(()));
    inbox.bucket = "a/b".into();
    assert!(inbox.validate().is_err());
}
