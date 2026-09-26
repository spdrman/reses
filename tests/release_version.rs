//! `scripts/release-version.sh` decides whether a release run may go ahead: it reads the
//! version from Cargo.toml, refuses a version whose tag already exists (on a push), refuses a
//! release commit that isn't on main, and treats "couldn't ask origin" as an error rather than
//! as "no tag". On a push it also refuses a commit whose ci.yml runs didn't all pass. These
//! tests run the real script against throwaway git repos, with a fake `gh` on PATH standing in
//! for GitHub's API, so no test ever talks to GitHub.

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

/// A stand-in for the `gh` CLI. It answers the two API calls the script makes from files: `runs`
/// (or `runs.N` for the Nth call, so a run can finish between polls) for the workflow's runs,
/// and `jobs-ID` for one run's jobs, each already in the line format the script's `--jq`
/// produces. A `fail` file makes every call exit 1, and each call's arguments are logged.
struct FakeGh {
    dir: tempfile::TempDir,
}

impl FakeGh {
    /// I write the fake as a shell script into its own bin directory, with no data yet.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let script = r#"#!/usr/bin/env bash
set -eu
d="$FAKE_GH"
echo "$*" >> "$d/calls"
[ -e "$d/fail" ] && { echo "gh: HTTP 502" >&2; exit 1; }
n=$(wc -l < "$d/calls" | tr -d ' ')
case "$*" in
  *"/actions/workflows/ci.yml/runs?"*)
    if [ -e "$d/runs.$n" ]; then cat "$d/runs.$n"; elif [ -e "$d/runs" ]; then cat "$d/runs"; fi ;;
  *"/actions/runs/"*"/jobs"*)
    all="$*"; id=${all#*/actions/runs/}; id=${id%%/*}
    if [ -e "$d/jobs-$id" ]; then cat "$d/jobs-$id"; fi ;;
  *) echo "fake gh: unexpected call: $*" >&2; exit 3 ;;
esac
"#;
        let path = bin.join("gh");
        fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    /// One CI run on the commit that passed, with every job passing.
    fn green() -> Self {
        let gh = Self::new();
        gh.set("runs", "101 completed success\n");
        gh.set(
            "jobs-101",
            "success\tCheck & Lint\nsuccess\tTest\nsuccess\tMSRV\n",
        );
        gh
    }

    /// Write one of the fake's answer files.
    fn set(&self, name: &str, contents: &str) {
        fs::write(self.dir.path().join(name), contents).unwrap();
    }

    /// Every call the script made, one per line.
    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }
}

/// Run the script with a CI history in which everything passed.
fn run(repo: &Repo, event: &str, sha: &str) -> (i32, String, String) {
    run_with(repo, event, sha, &FakeGh::green(), 0)
}

/// Run the script against `gh`, allowing it `wait` seconds for CI runs still in progress.
fn run_with(repo: &Repo, event: &str, sha: &str, gh: &FakeGh, wait: u32) -> (i32, String, String) {
    let path = format!(
        "{}:{}",
        gh.dir.path().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(repo.work.join("scripts/release-version.sh"))
        .args([event, sha])
        .current_dir(&repo.work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("PATH", path)
        .env("FAKE_GH", gh.dir.path())
        .env("GH_REPO", "example/reses")
        .env("RESES_CI_WAIT_SECS", wait.to_string())
        .env("RESES_CI_POLL_SECS", "0")
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

#[test]
fn a_push_goes_ahead_only_when_every_ci_run_on_the_commit_passed() {
    let r = repo(Some("1.2.3"));
    let sha = head(&r);
    let gh = FakeGh::green();
    let (code, stdout, stderr) = run_with(&r, "push", &sha, &gh, 0);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "version=1.2.3\n");
    // It asked about this exact commit, only for ci.yml's push runs, and read the run's jobs.
    let calls = gh.calls();
    assert!(
        calls.contains(&format!(
            "repos/example/reses/actions/workflows/ci.yml/runs?head_sha={sha}&event=push"
        )),
        "{calls}"
    );
    assert!(
        calls.contains("repos/example/reses/actions/runs/101/jobs"),
        "{calls}"
    );
}

#[test]
fn a_failed_ci_run_on_the_commit_stops_a_push() {
    let r = repo(Some("1.2.3"));
    let gh = FakeGh::green();
    gh.set("runs", "101 completed success\n102 completed failure\n");
    let (code, stdout, stderr) = run_with(&r, "push", &head(&r), &gh, 0);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("102") && stderr.contains("failure"),
        "{stderr}"
    );
    assert!(stdout.is_empty(), "{stdout:?}");
}

#[test]
fn a_commit_ci_never_ran_on_is_refused() {
    // No runs at all is the commit going out untested, not a pass.
    let r = repo(Some("1.2.3"));
    let (code, _, stderr) = run_with(&r, "push", &head(&r), &FakeGh::new(), 0);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("no CI run"), "{stderr}");
}

#[test]
fn a_skipped_or_failed_job_inside_a_passing_run_is_refused() {
    // GitHub calls a run a success when some of its jobs were skipped, so each job is read.
    let r = repo(Some("1.2.3"));
    for bad in ["skipped", "failure", "cancelled"] {
        let gh = FakeGh::green();
        gh.set("jobs-101", &format!("success\tTest\n{bad}\tMSRV\n"));
        let (code, _, stderr) = run_with(&r, "push", &head(&r), &gh, 0);
        assert_eq!(code, 1, "{bad}: {stderr}");
        assert!(
            stderr.contains("MSRV") && stderr.contains(bad),
            "{bad}: {stderr}"
        );
    }
}

#[test]
fn a_run_with_no_jobs_listed_is_refused() {
    let r = repo(Some("1.2.3"));
    let gh = FakeGh::green();
    gh.set("jobs-101", "");
    let (code, _, stderr) = run_with(&r, "push", &head(&r), &gh, 0);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("no jobs"), "{stderr}");
}

#[test]
fn cancelled_runs_prove_nothing_but_do_not_block_a_passing_one() {
    let r = repo(Some("1.2.3"));
    let gh = FakeGh::new();
    gh.set("runs", "101 completed cancelled\n");
    let (code, _, stderr) = run_with(&r, "push", &head(&r), &gh, 0);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("cancelled"), "{stderr}");

    let gh = FakeGh::green();
    gh.set("runs", "100 completed cancelled\n101 completed success\n");
    let (code, _, stderr) = run_with(&r, "push", &head(&r), &gh, 0);
    assert_eq!(code, 0, "a cancelled run beside a passing one: {stderr}");
}

#[test]
fn a_run_still_going_is_waited_for_and_then_judged() {
    let r = repo(Some("1.2.3"));
    // The first poll sees it running, the second sees it finished.
    let gh = FakeGh::green();
    gh.set("runs.1", "101 in_progress \n");
    let (code, _, stderr) = run_with(&r, "push", &head(&r), &gh, 60);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("waiting"), "{stderr}");

    // With no time left to wait, a run still going is a refusal.
    let gh = FakeGh::green();
    gh.set("runs", "101 in_progress \n");
    let (code, _, stderr) = run_with(&r, "push", &head(&r), &gh, 0);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("still running"), "{stderr}");
}

#[test]
fn not_being_able_to_ask_github_is_an_error_not_a_pass() {
    let r = repo(Some("1.2.3"));
    let gh = FakeGh::green();
    gh.set("fail", "");
    let (code, stdout, stderr) = run_with(&r, "push", &head(&r), &gh, 0);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("couldn't ask GitHub"), "{stderr}");
    assert!(stdout.is_empty());
}

#[test]
fn a_dry_run_never_asks_about_ci() {
    // A PR's commit isn't on main and CI may not have finished on it, so the dry run skips it.
    let r = repo(Some("1.2.3"));
    let gh = FakeGh::new();
    gh.set("fail", "");
    let (code, _, stderr) = run_with(&r, "pull_request", &head(&r), &gh, 0);
    assert_eq!(code, 0, "{stderr}");
    assert!(gh.calls().is_empty(), "{}", gh.calls());
}
