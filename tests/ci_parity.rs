//! `scripts/ci-docker.sh` is the local mirror of `.github/workflows/ci.yml`. Nothing else ties
//! them together, so these tests do: every cargo and python command one of them runs, the other
//! runs too, and the toolchain the CI image pins is the MSRV that Cargo.toml and the README claim.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// A command worth comparing: a cargo invocation or the python oracle suite, whitespace-folded.
fn command_of(line: &str) -> Option<String> {
    let line = line.trim();
    let line = line.strip_prefix("- run:").unwrap_or(line).trim();
    let line = line.strip_prefix("run:").unwrap_or(line).trim();
    let is_cargo = line.starts_with("cargo ") || line.contains(" cargo ");
    let is_python = line.contains("python3 -m unittest");
    (is_cargo || is_python).then(|| line.split_whitespace().collect::<Vec<_>>().join(" "))
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

    let missing_locally: Vec<_> = ci.difference(&local).collect();
    let missing_in_ci: Vec<_> = local.difference(&ci).collect();
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
