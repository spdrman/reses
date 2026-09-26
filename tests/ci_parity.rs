//! `scripts/ci-docker.sh` is the local mirror of `.github/workflows/ci.yml`. Nothing else ties
//! them together, so these tests do: every cargo command one of them runs, the other
//! runs too, and the toolchain the CI image pins is the MSRV that Cargo.toml and the README claim.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// A command worth comparing: a cargo invocation, whitespace-folded.
fn command_of(line: &str) -> Option<String> {
    let line = line.trim();
    let line = line.strip_prefix("- run:").unwrap_or(line).trim();
    let line = line.strip_prefix("run:").unwrap_or(line).trim();
    let is_cargo = line.starts_with("cargo ") || line.contains(" cargo ");
    // In ci-docker.sh the last command of a case arm ends with the closing quote and `;;`.
    let line = line
        .trim_end_matches(";;")
        .trim_end()
        .trim_end_matches('\'');
    is_cargo.then(|| line.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn commands(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(command_of)
        .collect()
}

#[test]
fn every_ci_command_runs_locally_and_back() {
    let ci = commands(&read(".github/workflows/ci.yml"));
    let local = commands(&read("scripts/ci-docker.sh"));
    // A positive control: if either side parses to nothing, the comparison below proves nothing.
    assert!(ci.len() >= 5, "found too few commands in ci.yml: {ci:?}");
    assert!(
        local.len() >= 5,
        "found too few commands in ci-docker.sh: {local:?}"
    );

    // The deliberate differences: CI builds the macOS binary natively on a macOS runner, and
    // the local mirror cross-builds it from Linux with zigbuild. The macOS job also builds a
    // debug binary as the "already ran" side of tests/macos-replace-binary.sh (#15); the local
    // mirror runs that test with the release binary on both sides instead of a second build.
    const CI_ONLY: &[&str] = &["cargo build --release --locked"];
    const LOCAL_ONLY: &[&str] =
        &["cargo zigbuild --release --locked --target aarch64-apple-darwin"];
    for c in CI_ONLY {
        assert!(
            ci.contains(*c),
            "CI_ONLY names {c:?}, which ci.yml no longer runs"
        );
    }
    for c in LOCAL_ONLY {
        assert!(
            local.contains(*c),
            "LOCAL_ONLY names {c:?}, which ci-docker.sh no longer runs"
        );
    }

    let missing_locally: Vec<_> = ci
        .difference(&local)
        .filter(|c| !CI_ONLY.contains(&c.as_str()))
        .collect();
    let missing_in_ci: Vec<_> = local
        .difference(&ci)
        .filter(|c| !LOCAL_ONLY.contains(&c.as_str()))
        .collect();
    assert!(
        missing_locally.is_empty() && missing_in_ci.is_empty(),
        "ci.yml and scripts/ci-docker.sh have drifted\n  only in ci.yml: {missing_locally:#?}\n  only in ci-docker.sh: {missing_in_ci:#?}"
    );
}

fn major_minor(v: &str) -> String {
    v.split('.').take(2).collect::<Vec<_>>().join(".")
}

#[test]
fn every_msrv_claim_agrees() {
    let cargo = read("Cargo.toml");
    let declared = cargo
        .lines()
        .find_map(|l| l.strip_prefix("rust-version = \""))
        .and_then(|l| l.strip_suffix('"'))
        .expect("Cargo.toml declares rust-version");

    let dockerfile = read("docker/ci.Dockerfile");
    let image = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("FROM rust:"))
        .and_then(|l| l.split('-').next())
        .expect("the CI image is FROM rust:<version>");

    let ci = read(".github/workflows/ci.yml");
    let pinned: BTreeSet<String> = ci
        .lines()
        .filter_map(|l| l.trim().strip_prefix("toolchain:"))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        .map(|v| major_minor(&v))
        .collect();

    let readme = read("README.md");
    let advertised: BTreeSet<String> = readme
        .match_indices("Rust ")
        .filter_map(|(i, _)| {
            let rest = &readme[i + 5..];
            let v: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            (rest[v.len()..].starts_with('+') && v.contains('.')).then(|| major_minor(&v))
        })
        .collect();

    let declared = major_minor(declared);
    assert_eq!(
        major_minor(image),
        declared,
        "docker/ci.Dockerfile builds on a different Rust than Cargo.toml declares"
    );
    assert_eq!(
        pinned,
        BTreeSet::from([declared.clone()]),
        "ci.yml's MSRV job must install exactly the declared rust-version"
    );
    assert_eq!(
        advertised,
        BTreeSet::from([declared]),
        "README must advertise the declared rust-version as `Rust X.Y+`"
    );
}

#[test]
fn the_msrv_job_name_carries_no_version() {
    // Branch protection matches a required check by its exact name, so a version in the name
    // strands every open PR on the next bump.
    for line in read(".github/workflows/ci.yml").lines() {
        let Some(name) = line.trim().strip_prefix("name:") else {
            continue;
        };
        if name.contains("MSRV") {
            assert!(
                !name.chars().any(|c| c.is_ascii_digit()),
                "the MSRV job name carries a version: {line}"
            );
        }
    }
}

/// Lines that copy, move or link something into `dist/`, comments skipped.
fn dist_writes(text: &str) -> Vec<String> {
    let verbs = ["cp ", "mv ", "install ", "ln ", "rsync "];
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && l.contains("dist/"))
        .filter(|l| verbs.iter().any(|v| l.contains(v)) || l.contains("place-binary.sh"))
        .map(str::to_string)
        .collect()
}

#[test]
fn every_write_into_dist_goes_through_place_binary() {
    // #15: an in-place overwrite of a binary that has run, while anything holds it open, leaves
    // it dead for exec. scripts/place-binary.sh puts each build on a new inode; nothing else may
    // write a binary into dist/.
    for file in ["Makefile", "scripts/ci-docker.sh"] {
        let writes = dist_writes(&read(file));
        // Positive control: each file does place a binary, so an empty list means the
        // parsing broke, not that everything is fine.
        assert!(
            writes.iter().any(|l| l.contains("place-binary.sh")),
            "{file} places no binary through place-binary.sh: {writes:#?}"
        );
        let bad: Vec<_> = writes
            .iter()
            .filter(|l| !l.contains("place-binary.sh"))
            .collect();
        assert!(
            bad.is_empty(),
            "{file} writes into dist/ without scripts/place-binary.sh (#15): {bad:#?}"
        );
    }
}

#[test]
fn make_darwin_checks_the_build_before_it_becomes_dist_reses() {
    // ~/.local/bin/reses links to dist/reses, so whatever lands there is the installed command.
    // The goldens and the #15 replace test run on the fresh build first.
    let makefile = read("Makefile");
    let recipe: Vec<&str> = makefile
        .lines()
        .skip_while(|l| !l.starts_with("darwin:"))
        .skip(1)
        .take_while(|l| l.starts_with('\t'))
        .map(str::trim)
        .collect();
    let at = |needle: &str| {
        recipe
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("the darwin recipe has no step with {needle:?}: {recipe:#?}"))
    };
    let place = at("dist/reses-aarch64-apple-darwin dist/reses");
    assert!(
        at("check-goldens.sh") < place,
        "goldens run after dist/reses is replaced: {recipe:#?}"
    );
    assert!(
        at("macos-replace-binary.sh") < place,
        "the #15 test runs after dist/reses is replaced: {recipe:#?}"
    );
}

/// Every file git tracks, relative to the repo root. Only tracked files count, so local scratch
/// files and symlinks can't change the answer.
fn repo_files() -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("running git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    String::from_utf8(out.stdout)
        .expect("tracked paths are UTF-8")
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn the_only_python_left_is_the_configparser_oracle() {
    // The Python version of reses used to be the reference the goldens came from. It's gone
    // (#24); the one Python file that stays reads credentials the way the AWS CLI does.
    let files = repo_files();
    assert!(
        files.iter().any(|f| f == "Cargo.toml"),
        "the walk found nothing: {files:?}"
    );
    let python: Vec<_> = files.iter().filter(|f| f.ends_with(".py")).collect();
    assert_eq!(
        python,
        vec!["tests/profile_oracle.py"],
        "unexpected Python files"
    );
    for f in ["python/reses.py", "tests/fixtures/mail/regen.sh"] {
        assert!(!files.iter().any(|x| x == f), "{f} should be gone");
    }
    for rel in [
        ".github/workflows/ci.yml",
        ".github/workflows/release.yml",
        "scripts/ci-docker.sh",
    ] {
        assert!(
            !read(rel).contains("unittest"),
            "{rel} still runs the Python unit tests"
        );
    }
}
