//! `.github/workflows/release.yml` turns a merge into the `release` branch into a GitHub release
//! with three binaries. Nothing runs it locally, so these tests pin its contract: what triggers
//! it, what it builds and where, what it checks before publishing, and that only a push to
//! `release` can publish. Each check is written against a specific way the workflow could
//! quietly break, and was mutated to make sure it goes red.

use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn workflow() -> String {
    read(".github/workflows/release.yml")
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

/// A job's steps, each as its lines (a step starts at `      - `).
fn steps<'a>(job: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut out: Vec<Vec<&str>> = Vec::new();
    for l in job {
        if l.starts_with("      - ") {
            out.push(vec![l]);
        } else if let Some(step) = out.last_mut()
            && l.starts_with("        ")
        {
            step.push(l);
        }
    }
    out
}

/// The one step whose lines mention `needle`.
fn step_with<'a>(job: &[&'a str], needle: &str) -> Vec<&'a str> {
    let found: Vec<_> = steps(job).into_iter().filter(|s| has(s, needle)).collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one step mentioning {needle:?}, found {found:#?}"
    );
    found.into_iter().next().unwrap()
}

fn has(lines: &[&str], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

fn has_line(lines: &[&str], exact: &str) -> bool {
    lines.iter().any(|l| l.trim() == exact)
}

#[test]
fn it_runs_on_a_push_to_release_and_on_prs_that_touch_what_it_depends_on() {
    let text = workflow();
    let on: Vec<&str> = text
        .lines()
        .skip_while(|l| *l != "on:")
        .take_while(|l| !l.starts_with("permissions") && !l.starts_with("jobs"))
        .collect();
    assert!(
        has_line(&on, "branches: [release]"),
        "push must be limited to release: {on:#?}"
    );
    assert!(has_line(&on, "pull_request:"), "no dry run on PRs: {on:#?}");
    // Everything the release run executes, so a PR that breaks any of it gets a dry run.
    for path in [
        ".github/workflows/release.yml",
        "tests/release_workflow.rs",
        "tests/release_version.rs",
        "scripts/release-version.sh",
        "scripts/check-goldens.sh",
        "scripts/place-binary.sh",
        "tests/macos-replace-binary.sh",
        "Cargo.toml",
    ] {
        assert!(
            has_line(&on, &format!("- {path}")),
            "the dry run doesn't cover {path}: {on:#?}"
        );
    }
}

#[test]
fn it_builds_the_three_binaries_each_natively_on_its_own_platform() {
    let text = workflow();
    let build = job(&text, "build");
    for (target, runner) in [
        ("x86_64-unknown-linux-musl", "ubuntu-latest"),
        ("aarch64-unknown-linux-musl", "ubuntu-24.04-arm"),
        ("aarch64-apple-darwin", "macos-latest"),
    ] {
        assert!(
            has_line(
                &build,
                &format!("- {{ target: {target}, runner: {runner} }}")
            ),
            "{target} must build on {runner}: {build:#?}"
        );
    }
    assert!(
        has_line(&build, "runs-on: ${{ matrix.runner }}"),
        "the build must run on the matrix runner: {build:#?}"
    );
    assert!(
        has(
            &build,
            "cargo build --release --locked --target \"$TARGET\""
        ),
        "no release build: {build:#?}"
    );
}

#[test]
fn every_check_is_real_and_runs_where_it_should() {
    let text = workflow();
    let build = job(&text, "build");
    let test = job(&text, "test");
    // A check that can't fail isn't a check.
    for (name, lines) in [("build", &build), ("test", &test)] {
        assert!(
            !has(lines, "continue-on-error"),
            "{name} has continue-on-error: {lines:#?}"
        );
        for step in steps(lines) {
            if has(&step, "check-goldens")
                || has(&step, "macos-replace")
                || has(&step, "cargo test")
                || has(&step, "readelf")
            {
                assert!(
                    !has(&step, "|| true"),
                    "a check in {name} swallows its failure: {step:#?}"
                );
            }
        }
    }
    assert!(
        has_line(&test, "- run: cargo test --locked --no-fail-fast"),
        "no test run: {test:#?}"
    );

    let goldens = step_with(&build, "scripts/check-goldens.sh");
    assert!(
        !has(&goldens, "if:"),
        "the golden check must run for every target: {goldens:#?}"
    );

    // The #15 script exits 0 off macOS, so running it anywhere else silently tests nothing.
    let replace = step_with(&build, "tests/macos-replace-binary.sh");
    assert!(
        has_line(&replace, "if: runner.os == 'macOS'"),
        "{replace:#?}"
    );

    let stat = step_with(&build, "readelf");
    assert!(
        has_line(&stat, "if: contains(matrix.target, 'musl')"),
        "{stat:#?}"
    );
    assert!(
        has(&stat, "command -v readelf"),
        "a missing readelf must fail, not pass: {stat:#?}"
    );
    assert!(
        has(&stat, "static-pie linked"),
        "no positive check of what file(1) says: {stat:#?}"
    );
}

#[test]
fn publish_waits_for_everything_and_only_a_push_to_release_publishes() {
    let text = workflow();
    let publish = job(&text, "publish");
    assert!(
        has_line(&publish, "needs: [version, test, build]"),
        "{publish:#?}"
    );
    // The exact guard. A substring check let `||` and `always() ||` through.
    assert!(
        has_line(
            &publish,
            "if: github.event_name == 'push' && github.ref == 'refs/heads/release'"
        ),
        "publish guard changed: {publish:#?}"
    );
    assert_eq!(
        publish
            .iter()
            .filter(|l| l.trim_start().starts_with("if:"))
            .count(),
        2,
        "publish should have its job guard plus the cleanup step's, nothing else: {publish:#?}"
    );
}

#[test]
fn publish_checks_the_set_and_never_leaves_a_half_release() {
    let text = workflow();
    let publish = job(&text, "publish");
    assert!(
        has(&publish, "expected 3 archives"),
        "no count of the archives: {publish:#?}"
    );
    assert!(has(&publish, "SHA256SUMS"), "no checksums: {publish:#?}");
    // The tag is created first, at this commit, and creating it fails if it exists; the release
    // then has to use exactly that tag.
    assert!(
        has(&publish, "git/refs"),
        "the tag must be created explicitly: {publish:#?}"
    );
    assert!(has(&publish, "--verify-tag"), "{publish:#?}");
    // Draft first, check all four assets arrived, then publish; clean up a failed run's draft.
    assert!(has(&publish, "--draft "), "{publish:#?}");
    assert!(has(&publish, "expected 4 assets"), "{publish:#?}");
    assert!(has(&publish, "--draft=false"), "{publish:#?}");
    let cleanup = step_with(&publish, "gh release delete");
    assert!(has(&cleanup, "if: failure()"), "{cleanup:#?}");

    let package = step_with(&job(&text, "build"), "tar -C stage");
    for file in ["README.md", "LICENSE"] {
        assert!(
            has(&package, file),
            "the archive must include {file}: {package:#?}"
        );
    }
}

#[test]
fn the_version_check_is_the_tested_script_and_sees_main() {
    let text = workflow();
    let version = job(&text, "version");
    assert!(
        has_line(
            &version,
            r#"run: scripts/release-version.sh "$GITHUB_EVENT_NAME" "$GITHUB_SHA" >> "$GITHUB_OUTPUT""#
        ),
        "the version job must run scripts/release-version.sh: {version:#?}"
    );
    // The on-main check needs history, which a depth-1 checkout doesn't have.
    assert!(has_line(&version, "fetch-depth: 0"), "{version:#?}");
    let build = job(&text, "build");
    assert!(
        has_line(&build, "needs: version"),
        "build must wait for the version check: {build:#?}"
    );
}

#[test]
fn every_action_is_pinned_to_a_commit_and_holds_no_token_on_disk() {
    let text = workflow();
    for line in text
        .lines()
        .filter(|l| l.trim_start().starts_with("- uses:") || l.trim_start().starts_with("uses:"))
    {
        let spec = line.split("uses:").nth(1).unwrap().trim();
        let (_, rest) = spec
            .split_once('@')
            .unwrap_or_else(|| panic!("unpinned action: {line}"));
        let sha: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        assert!(
            sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "actions must be pinned to a full commit SHA: {line}"
        );
        assert!(rest.contains('#'), "say which version the SHA is: {line}");
    }
    let checkouts = text.matches("actions/checkout@").count();
    assert!(
        checkouts >= 3,
        "expected a checkout per job that builds or tests"
    );
    assert_eq!(
        text.matches("persist-credentials: false").count(),
        checkouts,
        "every checkout must drop its token"
    );
}

#[test]
fn releases_and_ci_build_with_the_pinned_rust() {
    let dockerfile = read("docker/ci.Dockerfile");
    let pinned = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("FROM rust:"))
        .and_then(|l| l.split('-').next())
        .expect("the CI image is FROM rust:<version>");
    let release = workflow();
    let toolchains: Vec<&str> = release
        .lines()
        .filter(|l| l.contains("dtolnay/rust-toolchain@"))
        .collect();
    assert!(!toolchains.is_empty());
    for l in &toolchains {
        assert!(
            l.contains(&format!("# {pinned}")),
            "release builds must use Rust {pinned}: {l}"
        );
    }
    let ci = read(".github/workflows/ci.yml");
    for l in ci.lines().filter(|l| l.contains("dtolnay/rust-toolchain@")) {
        assert!(
            !l.contains("@stable"),
            "CI must build with the same pinned Rust as releases: {l}"
        );
    }
}
