//! `.github/workflows/release.yml` turns a merge into the `release` branch into a GitHub release
//! with three binaries. Nothing runs it locally, so these tests pin its contract: what triggers
//! it, what it builds and where, what it checks before publishing, and that only a push to
//! `release` can publish.

use std::fs;
use std::path::Path;

fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/release.yml");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The lines of one top-level job, from `  name:` down to the next job.
fn job<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let header = format!("  {name}:");
    let mut lines = text.lines().skip_while(|l| *l != header);
    let Some(first) = lines.next() else {
        panic!("release.yml has no `{name}` job");
    };
    std::iter::once(first)
        .chain(lines.take_while(|l| l.is_empty() || l.starts_with("    ") || l.starts_with('#')))
        .collect()
}

fn has(lines: &[&str], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

#[test]
fn it_runs_on_a_push_to_release_and_on_prs_that_touch_it() {
    let text = workflow();
    let on: Vec<&str> = text
        .lines()
        .skip_while(|l| *l != "on:")
        .take_while(|l| !l.starts_with("permissions") && !l.starts_with("jobs"))
        .collect();
    assert!(has(&on, "push:"), "no push trigger: {on:#?}");
    assert!(
        has(&on, "branches: [release]"),
        "push must be limited to release: {on:#?}"
    );
    assert!(
        has(&on, "pull_request:"),
        "no pull_request trigger for the dry run: {on:#?}"
    );
    assert!(
        has(&on, ".github/workflows/release.yml"),
        "the dry run must cover edits to the workflow itself: {on:#?}"
    );
}

#[test]
fn it_builds_the_three_binaries_each_on_its_own_platform() {
    let text = workflow();
    let build = job(&text, "build");
    for (target, runner) in [
        ("x86_64-unknown-linux-musl", "ubuntu-latest"),
        ("aarch64-unknown-linux-musl", "ubuntu-24.04-arm"),
        ("aarch64-apple-darwin", "macos-latest"),
    ] {
        let line = build
            .iter()
            .find(|l| l.contains(target) && l.contains("runner"))
            .unwrap_or_else(|| panic!("no matrix entry for {target}: {build:#?}"));
        assert!(
            line.contains(runner),
            "{target} must build natively on {runner}: {line}"
        );
    }
    assert!(
        has(&build, "cargo build --release --locked --target"),
        "no release build: {build:#?}"
    );
}

#[test]
fn every_binary_is_checked_on_its_platform_before_anything_is_published() {
    let text = workflow();
    let build = job(&text, "build");
    assert!(
        has(&build, "scripts/check-goldens.sh"),
        "binaries aren't checked against the goldens: {build:#?}"
    );
    assert!(
        has(&build, "tests/macos-replace-binary.sh"),
        "the macOS binary skips the #15 test: {build:#?}"
    );
    assert!(
        has(&job(&text, "test"), "cargo test --locked --no-fail-fast"),
        "no test job"
    );

    let publish = job(&text, "publish");
    let needs = publish
        .iter()
        .find(|l| l.trim_start().starts_with("needs:"))
        .expect("publish has needs");
    for dep in ["version", "test", "build"] {
        assert!(
            needs.contains(dep),
            "publish must wait for `{dep}`: {needs}"
        );
    }
}

#[test]
fn only_a_push_to_release_publishes() {
    let text = workflow();
    let publish = job(&text, "publish");
    let cond = publish
        .iter()
        .find(|l| l.trim_start().starts_with("if:"))
        .expect("publish must be guarded by an if:");
    assert!(
        cond.contains("github.event_name == 'push'"),
        "publish guard: {cond}"
    );
    assert!(
        cond.contains("github.ref == 'refs/heads/release'"),
        "publish guard: {cond}"
    );
    assert!(
        has(&publish, "SHA256SUMS"),
        "no checksums published: {publish:#?}"
    );
    assert!(
        has(&publish, "gh release create"),
        "nothing creates the release: {publish:#?}"
    );
}

#[test]
fn an_existing_tag_stops_the_run_before_it_builds() {
    let text = workflow();
    let version = job(&text, "version");
    assert!(
        has(&version, "Cargo.toml"),
        "the version must come from Cargo.toml: {version:#?}"
    );
    assert!(
        has(&version, "already exists"),
        "an existing tag must fail the run: {version:#?}"
    );
    let build = job(&text, "build");
    assert!(
        build
            .iter()
            .any(|l| l.trim_start().starts_with("needs:") && l.contains("version")),
        "build must wait for the version check: {build:#?}"
    );
}
