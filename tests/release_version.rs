//! `scripts/release-version.sh` decides whether a release run may go ahead: it reads the
//! version from Cargo.toml, refuses a version whose tag already exists (on a push), refuses a
//! release commit that isn't on main, and treats "couldn't ask origin" as an error rather than
//! as "no tag". These tests run the real script against throwaway git repos.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Repo {
    _dir: tempfile::TempDir,
    work: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("running git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A bare origin with `main` holding Cargo.toml at `version`, and a clone of it with the real
/// script copied in at the same relative path.
fn repo(version: Option<&str>) -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin.git");
    let work = dir.path().join("work");
    git(
        dir.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    git(&work, &["checkout", "-q", "-b", "main"]);
    let manifest = match version {
        Some(v) => format!("[package]\nname = \"reses\"\nversion = \"{v}\"\n"),
        None => "[package]\nname = \"reses\"\n".to_string(),
    };
    fs::write(work.join("Cargo.toml"), manifest).unwrap();
    fs::create_dir_all(work.join("scripts")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release-version.sh");
    fs::copy(&script, work.join("scripts/release-version.sh")).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(&work, &["push", "-q", "origin", "main"]);
    Repo { _dir: dir, work }
}

fn head(repo: &Repo) -> String {
    String::from_utf8(git(&repo.work, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string()
}

fn run(repo: &Repo, event: &str, sha: &str) -> (i32, String, String) {
    let out = Command::new("bash")
        .arg(repo.work.join("scripts/release-version.sh"))
        .args([event, sha])
        .current_dir(&repo.work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("running the script");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_fresh_version_on_main_goes_ahead_and_prints_only_the_output_line() {
    let r = repo(Some("1.2.3"));
    let sha = head(&r);
    for event in ["push", "pull_request"] {
        let (code, stdout, stderr) = run(&r, event, &sha);
        assert_eq!(code, 0, "{event}: {stderr}");
        // stdout goes straight into $GITHUB_OUTPUT, so nothing else may be printed there.
        assert_eq!(stdout, "version=1.2.3\n", "{event}");
    }
}

#[test]
fn an_existing_tag_stops_a_push_but_only_warns_a_dry_run() {
    let r = repo(Some("1.2.3"));
    let sha = head(&r);
    git(&r.work, &["tag", "v1.2.3"]);
    git(&r.work, &["push", "-q", "origin", "v1.2.3"]);

    let (code, stdout, stderr) = run(&r, "push", &sha);
    assert_eq!(code, 1, "a push must stop: {stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stdout.is_empty(), "no output line on failure: {stdout:?}");

    let (code, stdout, stderr) = run(&r, "pull_request", &sha);
    assert_eq!(code, 0, "a dry run carries on: {stderr}");
    assert!(
        stderr.contains("already exists"),
        "the dry run still warns: {stderr}"
    );
    assert_eq!(stdout, "version=1.2.3\n");
}

#[test]
fn a_tag_for_another_version_does_not_count() {
    let r = repo(Some("1.2.3"));
    git(&r.work, &["tag", "v1.2.30"]);
    git(&r.work, &["push", "-q", "origin", "v1.2.30"]);
    let (code, _, stderr) = run(&r, "push", &head(&r));
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn not_being_able_to_ask_origin_is_an_error_not_a_missing_tag() {
    let r = repo(Some("1.2.3"));
    let sha = head(&r);
    git(
        &r.work,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    for event in ["push", "pull_request"] {
        let (code, stdout, stderr) = run(&r, event, &sha);
        assert_eq!(code, 1, "{event}: {stderr}");
        assert!(stderr.contains("couldn't ask origin"), "{event}: {stderr}");
        assert!(stdout.is_empty(), "{event}: {stdout:?}");
    }
}

#[test]
fn a_push_of_a_commit_that_is_not_on_main_is_refused() {
    let r = repo(Some("1.2.3"));
    git(&r.work, &["checkout", "-q", "-b", "side"]);
    fs::write(r.work.join("side.txt"), "not on main").unwrap();
    git(&r.work, &["add", "side.txt"]);
    git(&r.work, &["commit", "-q", "-m", "side"]);
    let side = head(&r);

    let (code, _, stderr) = run(&r, "push", &side);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("isn't on main"), "{stderr}");

    // A dry run is a PR, whose commit is never on main yet, so it isn't checked there.
    let (code, _, stderr) = run(&r, "pull_request", &side);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn an_older_main_commit_is_still_on_main() {
    let r = repo(Some("1.2.3"));
    let older = head(&r);
    fs::write(r.work.join("later.txt"), "main moved on").unwrap();
    git(&r.work, &["add", "later.txt"]);
    git(&r.work, &["commit", "-q", "-m", "later"]);
    git(&r.work, &["push", "-q", "origin", "main"]);
    let (code, _, stderr) = run(&r, "push", &older);
    assert_eq!(
        code, 0,
        "main moving on after the push must not fail the release: {stderr}"
    );
}

#[test]
fn a_manifest_without_a_version_is_an_error() {
    let r = repo(None);
    let (code, stdout, stderr) = run(&r, "pull_request", &head(&r));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("no version"), "{stderr}");
    assert!(stdout.is_empty());
}
