//! `.github/workflows/release.yml` turns a merge into the `release` branch into a GitHub release
//! with three binaries. Nothing runs it locally, so these tests pin its contract: what triggers
//! it, what it builds and where, what it checks before publishing, and that only a push to
//! `release` can publish. Each check is written against a specific way the workflow could
//! quietly break, and was mutated to make sure it goes red.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[path = "support/workflows.rs"]
mod workflows;

use workflows::{PINS, load, scalar, scalar_map, uses_lines};

/// A repo file by its path from the root, with the path in the panic if it can't be read.
fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// release.yml as raw text, for the checks that read it line by line.
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

/// Whether any of the lines mentions `needle` anywhere.
fn has(lines: &[&str], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

/// Whether one of the lines is exactly `exact` once trimmed. I use this where a substring would
/// also match a weakened version of the line.
fn has_line(lines: &[&str], exact: &str) -> bool {
    lines.iter().any(|l| l.trim() == exact)
}

/// The workflow runs for a push to `release`, and as a dry run for any PR that touches a file
/// the release builds from or runs.
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
    // Everything the release run executes or builds from, so a PR that breaks any of it gets
    // a dry run. The release builds src/ against Cargo.lock and runs the whole test suite, so
    // those count as much as the scripts do.
    let wf = load(".github/workflows/release.yml");
    let paths: BTreeSet<String> = wf.on()["pull_request"]["paths"]
        .as_vec()
        .expect("the dry run lists its paths")
        .iter()
        .map(scalar)
        .collect();
    for path in [
        ".github/workflows/release.yml",
        ".python-version",
        "Cargo.toml",
        "Cargo.lock",
        "src/**",
        "tests/**",
        "install.sh",
        "scripts/release-version.sh",
        "scripts/check-goldens.sh",
        "scripts/check-static.sh",
        "scripts/place-binary.sh",
    ] {
        assert!(
            paths.contains(path),
            "the dry run doesn't cover {path}: {paths:#?}"
        );
    }
}

/// The build matrix is the two musl targets and the Mac, each on a runner of its own platform.
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

/// The build and test jobs can't fail softly, and each check on the binaries runs on the targets
/// it applies to.
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
                || has(&step, "check-static")
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

    let goldens = step_with(&build, "scripts/check-goldens.sh \"target/");
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

    // The staticness check is scripts/check-static.sh, which ci.yml's musl job and the local
    // gate run too, so all three check the same way.
    let stat = step_with(&build, "scripts/check-static.sh");
    assert!(
        has_line(&stat, "if: contains(matrix.target, 'musl')"),
        "{stat:#?}"
    );
    let script = read("scripts/check-static.sh");
    assert!(
        script.contains("for tool in readelf file; do")
            && script.contains("command -v \"$tool\" >/dev/null || {"),
        "a missing readelf or file must fail, not pass"
    );
    assert!(
        script.contains("static-pie linked"),
        "no positive check of what file(1) says"
    );
}

/// Publish needs every other job, and its guard lets only a push to `release` through.
#[test]
fn publish_waits_for_everything_and_only_a_push_to_release_publishes() {
    let text = workflow();
    let publish = job(&text, "publish");
    assert!(
        has_line(&publish, "needs: [version, test, integration, build]"),
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

/// Publish counts what it's about to ship, tags the commit explicitly, and goes through a draft
/// it deletes if anything fails, so a half-finished release never shows up.
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
    assert!(has(&publish, "--draft=false"), "{publish:#?}");
    let cleanup = step_with(&publish, "gh release delete");
    assert!(has(&cleanup, "if: failure()"), "{cleanup:#?}");

    // Each archive carries the README and licence alongside the binary.
    let package = step_with(&job(&text, "build"), "tar -C stage");
    for file in ["README.md", "LICENSE"] {
        assert!(
            has(&package, file),
            "the archive must include {file}: {package:#?}"
        );
    }
}

/// The version job runs scripts/release-version.sh itself (the one tests/release_version.rs
/// covers) on a full-history checkout, and the build waits for it.
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

/// Every `uses:` in either workflow is a row of the pin table. A comment is only a claim about
/// the SHA before it, so each (action, SHA, version) has to be a row of the table in
/// tests/support/workflows.rs, which I checked against GitHub.
#[test]
fn every_action_in_both_workflows_is_a_row_of_the_pin_table() {
    let mut used = BTreeSet::new();
    for rel in [".github/workflows/release.yml", ".github/workflows/ci.yml"] {
        let wf = load(rel);
        let lines = uses_lines(&wf.text);
        // The raw lines and the parser have to agree on how many actions there are, so a
        // `uses:` the line reader misses can't slip past the table.
        assert_eq!(
            lines.len(),
            wf.all_uses().len(),
            "{rel}: the line reader and the YAML parser count different actions"
        );
        assert!(lines.len() >= 5, "{rel}: found too few actions");
        for (action, sha, version) in &lines {
            let row = PINS
                .iter()
                .find(|(a, s, _)| a == action && s == sha)
                .unwrap_or_else(|| {
                    panic!(
                        "{rel}: {action}@{sha} isn't in the pin table (a tag, or an unknown SHA)"
                    )
                });
            assert_eq!(
                version, row.2,
                "{rel}: {action}@{sha} is {}, but the comment says {version:?}",
                row.2
            );
            used.insert((row.0, row.1));
        }

        // Every checkout drops its token, so no later step can push with it.
        let checkouts = lines
            .iter()
            .filter(|(a, _, _)| a == "actions/checkout")
            .count();
        assert!(checkouts >= 3, "{rel}: expected a checkout per job");
        assert_eq!(
            wf.text.matches("persist-credentials: false").count(),
            checkouts,
            "{rel}: every checkout must drop its token"
        );
    }
    // A row nothing uses is a pin nobody is checking any more.
    for (a, s, v) in PINS {
        assert!(
            used.contains(&(*a, *s)),
            "the pin table's {a} {v} row is unused"
        );
    }
}

/// Every Rust toolchain either workflow installs is the CI image's, except the MSRV job's, which
/// is the declared rust-version's first release.
#[test]
fn releases_and_ci_build_with_the_pinned_rust() {
    let dockerfile = read("docker/ci.Dockerfile");
    let image = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("FROM rust:"))
        .and_then(|l| l.split('-').next())
        .expect("the CI image is FROM rust:<version>");
    let declared = read("Cargo.toml")
        .lines()
        .find_map(|l| l.strip_prefix("rust-version = \""))
        .and_then(|l| l.strip_suffix('"'))
        .map(|v| format!("{v}.0"))
        .expect("Cargo.toml declares rust-version");
    for rel in [".github/workflows/release.yml", ".github/workflows/ci.yml"] {
        let wf = load(rel);
        let mut seen = 0;
        for (id, job) in wf.jobs() {
            for step in workflows::steps(job) {
                let Some(uses) = step["uses"].as_str() else {
                    continue;
                };
                let Some(sha) = uses.strip_prefix("dtolnay/rust-toolchain@") else {
                    continue;
                };
                let version = PINS
                    .iter()
                    .find(|(a, s, _)| *a == "dtolnay/rust-toolchain" && *s == sha)
                    .map(|r| r.2)
                    .unwrap_or_else(|| panic!("{rel} `{id}`: unknown toolchain pin {sha}"));
                let want = if id == "msrv" {
                    declared.as_str()
                } else {
                    image
                };
                assert_eq!(version, want, "{rel} `{id}` builds with Rust {version}");
                seen += 1;
            }
        }
        assert!(seen >= 2, "{rel}: found only {seen} toolchain steps");
    }
}

/// A skipped or soft-failing job reads as green, so the only job-level `if:` is publish's, and
/// nothing anywhere carries continue-on-error.
#[test]
fn only_publish_can_be_skipped_and_nothing_may_fail_quietly() {
    let wf = load(".github/workflows/release.yml");
    for (id, job) in wf.jobs() {
        if id != "publish" {
            assert!(job["if"].is_badvalue(), "job `{id}` has an if:");
        }
        assert!(
            job["continue-on-error"].is_badvalue(),
            "job `{id}` has continue-on-error"
        );
        for step in workflows::steps(job) {
            assert!(
                step["continue-on-error"].is_badvalue(),
                "a step in `{id}` has continue-on-error: {step:?}"
            );
        }
    }
}

/// scripts/release-version.sh asks GitHub whether every ci.yml run on the commit passed. The
/// run's own token reads that with `actions: read` added to what the workflow grants, and it only
/// ever lives in the environment.
#[test]
fn the_version_job_can_read_ci_results_with_the_run_token() {
    let wf = load(".github/workflows/release.yml");
    let version = wf.job("version");
    let perms = scalar_map(&version["permissions"]);
    assert_eq!(
        perms,
        [("actions", "read"), ("contents", "read")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into(),
        "the version job's permissions"
    );
    let env = scalar_map(&version["env"]);
    assert_eq!(
        env.get("GH_TOKEN").map(String::as_str),
        Some("${{ github.token }}")
    );
    assert_eq!(
        env.get("GH_REPO").map(String::as_str),
        Some("${{ github.repository }}")
    );
}

/// The release's integration job matches ci.yml's. The S3 client is what the inbox stands on,
/// and only the MinIO suite drives it against a real server, so a release doesn't publish
/// without it.
#[test]
fn the_release_runs_the_minio_suite_the_way_ci_does() {
    let release = load(".github/workflows/release.yml");
    let ci = load(".github/workflows/ci.yml");
    let (r, c) = (release.job("integration"), ci.job("integration"));
    assert_eq!(
        scalar_map(&r["env"]),
        scalar_map(&c["env"]),
        "the MinIO settings differ"
    );
    let (rr, cr) = (workflows::runs(r).join("\n"), workflows::runs(c).join("\n"));
    assert!(
        rr.contains("cargo test --locked --no-fail-fast -- --ignored --test-threads=1"),
        "the release doesn't run the MinIO suite"
    );
    let digest = |s: &str| {
        s.split_whitespace()
            .find(|w| w.starts_with("cgr.dev/chainguard/minio@sha256:"))
            .map(str::to_string)
    };
    assert!(
        digest(&rr).is_some(),
        "the release doesn't start a pinned MinIO"
    );
    assert_eq!(
        digest(&rr),
        digest(&cr),
        "the release and CI start different MinIO images"
    );
}

/// The .deb's Maintainer field points at the GitHub account, not a personal email address.
#[test]
fn the_deb_names_its_maintainer_by_github_account() {
    let text = workflow();
    let package = step_with(&job(&text, "build"), "dpkg-deb");
    assert!(
        has(
            &package,
            "\"Maintainer: spdrman <https://github.com/spdrman>\""
        ),
        "{package:#?}"
    );
    assert_eq!(
        text.matches("Maintainer:").count(),
        1,
        "exactly one Maintainer field"
    );
}

/// A macOS runner image whose kernel stops reproducing #15 says nothing about reses, so it
/// shouldn't stop a release. ci.yml still fails on it (tests/ci_parity.rs).
#[test]
fn the_15_control_only_warns_in_the_release() {
    let wf = load(".github/workflows/release.yml");
    let step = workflows::step_with(wf.job("build"), "tests/macos-replace-binary.sh");
    assert_eq!(
        step["env"]["RESES_REPLACE_CONTROL"].as_str(),
        Some("warn"),
        "{step:?}"
    );
    assert!(
        read("tests/macos-replace-binary.sh").contains("RESES_REPLACE_CONTROL"),
        "the #15 script ignores RESES_REPLACE_CONTROL"
    );
}

/// Each Linux build packs a .deb, installs it with apt and runs the goldens on the installed
/// binary, and publish ships both debs with checksums.
#[test]
fn the_linux_builds_ship_a_deb_that_apt_installs_before_publishing() {
    let text = workflow();
    let build = job(&text, "build");
    let package = step_with(&build, "dpkg-deb");
    assert!(
        has_line(&package, "if: contains(matrix.target, 'musl')"),
        "{package:#?}"
    );
    assert!(has(&package, "--root-owner-group --build"), "{package:#?}");
    assert!(has(&package, "Package: reses"), "{package:#?}");
    // The .deb is installed with apt on its own runner and the installed binary is checked
    // against the goldens, so a broken package can't be published.
    let install = step_with(&build, "\"$PWD/reses_");
    assert!(
        has_line(&install, "if: contains(matrix.target, 'musl')"),
        "{install:#?}"
    );
    assert!(
        has(&install, "scripts/check-goldens.sh /usr/bin/reses"),
        "{install:#?}"
    );
    let upload = step_with(&build, "actions/upload-artifact");
    assert!(
        has(&upload, ".deb"),
        "the .deb must be uploaded with the archive: {upload:#?}"
    );

    // Publish counts the debs, checksums them and attaches them.
    let publish = job(&text, "publish");
    assert!(has(&publish, "expected 2 debs"), "{publish:#?}");
    assert!(
        has(&publish, "sha256sum *.tar.gz *.deb > SHA256SUMS"),
        "the checksums must cover the debs: {publish:#?}"
    );
    assert!(
        has(&publish, "expected 6 assets"),
        "3 archives, 2 debs and SHA256SUMS: {publish:#?}"
    );
    assert!(
        has(&publish, "dist/*.deb"),
        "the debs must be attached: {publish:#?}"
    );
}

/// The debs are xz-compressed so Debian 11 can read them, a Debian 11 container installs and
/// runs one, and a prerelease version sorts before its release.
#[test]
fn the_debs_install_on_older_debian_and_order_prereleases_right() {
    let text = workflow();
    let build = job(&text, "build");
    let package = step_with(&build, "dpkg-deb -Zxz");
    // Newer dpkg-deb defaults to zstd, which dpkg on Debian 11 can't read.
    assert!(
        has(&package, "-Zxz"),
        "the .deb must be xz-compressed: {package:#?}"
    );
    // A semver prerelease like 1.0.0-rc.1 becomes 1.0.0~rc.1, which dpkg sorts before 1.0.0.
    assert!(
        has(&package, "${VERSION/-/~}"),
        "prereleases must map - to ~: {package:#?}"
    );
    // The Debian 11 install step runs the installed binary, not just dpkg.
    let bullseye = step_with(&build, "debian:bullseye");
    assert!(
        has_line(&bullseye, "if: contains(matrix.target, 'musl')"),
        "{bullseye:#?}"
    );
    assert!(
        has(&bullseye, "reses --version"),
        "the Debian 11 install must run the binary: {bullseye:#?}"
    );
}
