//! Region lookup from the AWS config file, where sections are `[default]` and `[profile NAME]`.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use reses::aws_profile::{config_path_from, region_from_config_file};

const CONFIG: &str = "# aws config\n\
[default]\n\
region = us-east-2\n\
output=json\n\
\n\
[profile work]\n\
region=eu-central-1\n\
\n\
[profile   spaced  ]\n\
region = ap-northeast-1\n\
\n\
[plain]\n\
region = xx-not-a-profile-1\n\
\n\
[profile noregion]\n\
output = text\n\
; region = xx-commented-1\n\
\n\
[sso-session corp]\n\
sso_region = us-west-1\n";

fn config_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    fs::write(&path, CONFIG).unwrap();
    (dir, path)
}

#[test]
fn path_honours_aws_config_file_env() {
    let p = config_path_from(
        Some(OsString::from("/x/aws-config")),
        Some(OsString::from("/home/u")),
    );
    assert_eq!(p, PathBuf::from("/x/aws-config"));
}

#[test]
fn path_falls_back_to_home_aws_config() {
    assert_eq!(
        config_path_from(None, Some(OsString::from("/home/u"))),
        PathBuf::from("/home/u/.aws/config")
    );
    assert_eq!(
        config_path_from(Some(OsString::new()), Some(OsString::from("/home/u"))),
        PathBuf::from("/home/u/.aws/config")
    );
}

#[test]
fn default_profile_reads_the_default_section() {
    let (_d, path) = config_file();
    assert_eq!(
        region_from_config_file(&path, "default").as_deref(),
        Some("us-east-2")
    );
}

#[test]
fn named_profile_reads_the_profile_prefixed_section() {
    let (_d, path) = config_file();
    assert_eq!(
        region_from_config_file(&path, "work").as_deref(),
        Some("eu-central-1")
    );
    assert_eq!(
        region_from_config_file(&path, "spaced").as_deref(),
        Some("ap-northeast-1")
    );
}

#[test]
fn bare_section_is_not_a_named_profile_in_the_config_file() {
    let (_d, path) = config_file();
    assert_eq!(region_from_config_file(&path, "plain"), None);
}

#[test]
fn profile_without_region_or_missing_profile_is_none() {
    let (_d, path) = config_file();
    assert_eq!(region_from_config_file(&path, "noregion"), None);
    assert_eq!(region_from_config_file(&path, "absent"), None);
    assert_eq!(region_from_config_file(&path, "corp"), None);
}

#[test]
fn profile_default_spelling_also_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    fs::write(&path, "[profile default]\nregion = sa-east-1\n").unwrap();
    assert_eq!(
        region_from_config_file(&path, "default").as_deref(),
        Some("sa-east-1")
    );
}

#[test]
fn missing_config_file_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        region_from_config_file(&dir.path().join("nope"), "default"),
        None
    );
}

#[test]
fn profile_prefix_needs_a_space_before_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    fs::write(&path, "[profilework]\nregion = xx-wrong-1\n").unwrap();
    assert_eq!(region_from_config_file(&path, "work"), None);
}
