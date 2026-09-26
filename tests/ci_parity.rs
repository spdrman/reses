//! `scripts/ci-docker.sh` is the local mirror of `.github/workflows/ci.yml`. Nothing else ties
//! them together, so these tests do. Every cargo command one side runs, the other runs too; the
//! environment CI builds with is the one the local gate builds with; no gate job can be skipped
//! or allowed to fail; and the toolchain the CI image pins is the MSRV that Cargo.toml and the
//! README claim. ci.yml is read as YAML (tests/support/workflows.rs), and ci-docker.sh, which is
//! a shell script, line by line.

use std::collections::{BTreeMap, BTreeSet};

#[path = "support/workflows.rs"]
mod workflows;

use workflows::{FORK_GATE, load, read, runs, scalar, scalar_map, steps};

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

/// The cargo commands in a shell script's lines, comments skipped.
fn commands(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(command_of)
        .collect()
}

/// The cargo commands in every `run:` script of ci.yml. Each script can hold several lines, and
/// each line is a command of its own.
fn ci_commands() -> BTreeSet<String> {
    load(".github/workflows/ci.yml")
        .all_runs()
        .iter()
        .flat_map(|r| commands(r))
        .collect()
}

#[test]
fn every_ci_command_runs_locally_and_back() {
    let ci = ci_commands();
    let local = commands(&read("scripts/ci-docker.sh"));
    // A positive control: if either side parses to nothing, the comparison below proves nothing.
    assert!(ci.len() >= 5, "found too few commands in ci.yml: {ci:?}");
    assert!(
        local.len() >= 5,
        "found too few commands in ci-docker.sh: {local:?}"
    );

    // The deliberate differences: CI builds the macOS binary natively on a macOS runner, and
    // the local mirror cross-builds it from Linux with zigbuild. Both run
    // tests/macos-replace-binary.sh (#15) with that one release binary on both sides.
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

    // Every toolchain CI installs by version, read from the steps' `with:` blocks. The MSRV job
    // is the only one that names one; the rest take it from the pinned action's branch.
    let ci = load(".github/workflows/ci.yml");
    let named: BTreeSet<String> = ci
        .jobs()
        .iter()
        .flat_map(|(_, j)| steps(j))
        .filter_map(|s| s["with"]["toolchain"].as_str().map(str::to_string))
        .collect();
    let pinned: BTreeSet<String> = named.iter().map(|v| major_minor(v)).collect();

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

    // `rust-version = "1.98"` promises 1.98.0, and "1.98" on its own would install the newest
    // 1.98 patch release, so the MSRV job has to name the .0 release in full.
    let full = if declared.matches('.').count() == 2 {
        declared.to_string()
    } else {
        format!("{declared}.0")
    };
    assert_eq!(
        named,
        BTreeSet::from([full]),
        "the MSRV job must install the first release of the declared rust-version, in full"
    );

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
    // (#24). Two test oracles stay: one reads credentials the way the AWS CLI does, and one reads
    // mail with Python's standard email package to vouch for the mail goldens (#26).
    let files = repo_files();
    assert!(
        files.iter().any(|f| f == "Cargo.toml"),
        "the walk found nothing: {files:?}"
    );
    let python: Vec<_> = files.iter().filter(|f| f.ends_with(".py")).collect();
    assert_eq!(
        python,
        vec!["tests/mail_oracle.py", "tests/profile_oracle.py"],
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

/// The `-e NAME=VALUE` settings a shell script passes to `docker run`, in order. I only read
/// literal values; a value built from a variable comes back as written, `$` and all.
fn docker_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let words: Vec<&str> = line.split_whitespace().collect();
        for pair in words.windows(2) {
            if pair[0] == "-e"
                && let Some((k, v)) = pair[1].split_once('=')
            {
                // The last word of a bash array ends with its closing `)`.
                let v = v.strip_suffix(')').unwrap_or(v);
                out.insert(k.to_string(), v.trim_matches('"').to_string());
            }
        }
    }
    out
}

/// The body of the local gate, the `GATE='...'` block in ci-docker.sh.
fn local_gate() -> String {
    let text = read("scripts/ci-docker.sh");
    let start = text.find("GATE='").expect("ci-docker.sh defines GATE") + "GATE='".len();
    let len = text[start..].find('\'').expect("GATE is closed");
    text[start..start + len].to_string()
}

#[test]
fn no_gate_job_can_be_skipped_or_fail_quietly() {
    // A job that's skipped, or allowed to fail, still shows a green row, so a gate that
    // can't go red is worse than none. The fork gate is the one `if:` allowed, and every job
    // carries it, so a push to a PR from this repo runs each job once, not twice.
    let ci = load(".github/workflows/ci.yml");
    let jobs = ci.jobs();
    assert!(jobs.len() >= 8, "found too few jobs: {}", jobs.len());
    for (id, job) in &jobs {
        assert_eq!(
            job["if"].as_str(),
            Some(FORK_GATE),
            "job `{id}` must carry exactly the fork gate as its `if:`"
        );
        assert!(
            job["continue-on-error"].is_badvalue(),
            "job `{id}` has continue-on-error"
        );
        for step in steps(job) {
            assert!(
                step["if"].is_badvalue(),
                "a step in `{id}` has an if: {step:?}"
            );
            assert!(
                step["continue-on-error"].is_badvalue(),
                "a step in `{id}` has continue-on-error: {step:?}"
            );
        }
        for run in runs(job) {
            assert!(
                !run.contains("|| true"),
                "a step in `{id}` swallows its failure: {run}"
            );
        }
    }
}

#[test]
fn the_local_gate_builds_with_the_environment_ci_builds_with() {
    // RUSTFLAGS=-Dwarnings changes what gets compiled (a warning anywhere is an error), so the
    // gate has to set it too. The colour setting only changes how output looks, so it's the
    // one difference allowed.
    let ci = load(".github/workflows/ci.yml").env();
    assert_eq!(
        ci.get("RUSTFLAGS").map(String::as_str),
        Some("-Dwarnings"),
        "ci.yml should deny warnings: {ci:?}"
    );
    let local = docker_env(&read("scripts/ci-docker.sh"));
    for (k, v) in ci.iter().filter(|(k, _)| k.as_str() != "CARGO_TERM_COLOR") {
        assert_eq!(
            local.get(k),
            Some(v),
            "ci.yml sets {k}={v}, and ci-docker.sh must pass the same to the container: {local:?}"
        );
    }

    // The MinIO suite logs in with the same keys on both sides. Only the endpoint differs:
    // CI reaches MinIO on localhost, the local mirror over a private Docker network.
    let integration = scalar_map(&load(".github/workflows/ci.yml").job("integration")["env"]);
    let local_keys: BTreeSet<&String> = local
        .keys()
        .filter(|k| k.starts_with("RESES_TEST_"))
        .collect();
    assert_eq!(
        integration.keys().collect::<BTreeSet<_>>(),
        local_keys,
        "the integration job and ci-docker.sh --integration set different variables"
    );
    for (k, v) in integration
        .iter()
        .filter(|(k, _)| k.as_str() != "RESES_TEST_S3_ENDPOINT")
    {
        assert_eq!(
            local.get(k),
            Some(v),
            "{k} differs between CI and the local mirror"
        );
    }
}

#[test]
fn every_push_to_a_fork_pr_gets_ci() {
    // `push` only fires for branches in this repo, so a fork's later commits only reach CI
    // through `synchronize`. The release branch is left to release.yml, which runs its own
    // tests and reads this workflow's results on the commit instead.
    let ci = load(".github/workflows/ci.yml");
    let on = ci.on();
    let types: BTreeSet<String> = on["pull_request"]["types"]
        .as_vec()
        .expect("pull_request lists its types")
        .iter()
        .map(scalar)
        .collect();
    assert_eq!(
        types,
        ["opened", "reopened", "ready_for_review", "synchronize"]
            .map(String::from)
            .into(),
    );
    let branches: Vec<String> = on["push"]["branches"]
        .as_vec()
        .expect("push lists its branches")
        .iter()
        .map(scalar)
        .collect();
    assert_eq!(branches, ["**", "!release"]);
}

#[test]
fn ci_builds_the_static_musl_binaries_the_release_ships() {
    // The release's Linux binaries are static musl builds. Without this job, the first musl
    // build of a changed Cargo.lock would be the release itself.
    let ci = load(".github/workflows/ci.yml");
    let musl = ci.job("musl");
    let matrix: BTreeSet<(String, String)> = musl["strategy"]["matrix"]["include"]
        .as_vec()
        .expect("the musl job has a matrix")
        .iter()
        .map(|row| (scalar(&row["target"]), scalar(&row["runner"])))
        .collect();
    assert_eq!(
        matrix,
        [
            ("x86_64-unknown-linux-musl", "ubuntu-latest"),
            ("aarch64-unknown-linux-musl", "ubuntu-24.04-arm"),
        ]
        .map(|(t, r)| (t.to_string(), r.to_string()))
        .into(),
    );
    assert_eq!(musl["runs-on"].as_str(), Some("${{ matrix.runner }}"));
    let build = r#"TARGET_CC=musl-gcc cargo build --release --locked --target "$MUSL_TARGET""#;
    let musl_runs = runs(musl).join("\n");
    for needle in [build, "scripts/check-static.sh", "scripts/check-goldens.sh"] {
        assert!(
            musl_runs.contains(needle),
            "the musl job doesn't run {needle}"
        );
    }

    // The local gate builds the musl target of whatever machine it runs on, with the same
    // command, and checks the result the same way.
    let gate = local_gate();
    for needle in [
        build,
        "MUSL_TARGET=\"$(uname -m)-unknown-linux-musl\"",
        "scripts/check-static.sh \"/target/$MUSL_TARGET/release/reses\"",
        "scripts/check-goldens.sh \"/target/$MUSL_TARGET/release/reses\"",
    ] {
        assert!(gate.contains(needle), "the local gate doesn't run {needle}");
    }
}

#[test]
fn cargo_deny_gates_ci_and_the_local_run_with_one_version() {
    let deny = "cargo deny check advisories bans licenses sources";
    let ci = load(".github/workflows/ci.yml");
    assert!(
        runs(ci.job("deny")).iter().any(|r| r.trim() == deny),
        "ci.yml's deny job doesn't run `{deny}`"
    );
    assert!(
        local_gate().contains(deny),
        "the local gate doesn't run `{deny}`"
    );

    // CI installs a prebuilt cargo-deny and the image builds one; both must be one version.
    let tool = steps(ci.job("deny"))
        .into_iter()
        .find_map(|s| s["with"]["tool"].as_str().map(str::to_string))
        .expect("the deny job installs its tool");
    let dockerfile = read("docker/ci.Dockerfile");
    let image = dockerfile
        .split_whitespace()
        .find(|w| w.starts_with("cargo-deny@"))
        .expect("the CI image installs cargo-deny@<version>");
    assert_eq!(
        tool, image,
        "CI and the CI image install different cargo-deny"
    );
}

#[test]
fn every_advisory_exception_says_why_and_links_the_advisory() {
    // An ignore with no reason is how an advisory quietly becomes permanent. The set is pinned
    // too, so adding one is a visible change to this test, not a one-line edit to deny.toml.
    let deny: toml::Table = read("deny.toml").parse().expect("deny.toml is TOML");
    let ignores = deny["advisories"]["ignore"]
        .as_array()
        .expect("deny.toml lists its advisory exceptions");
    let mut ids = BTreeSet::new();
    for entry in ignores {
        let id = entry["id"].as_str().expect("each exception names its id");
        let reason = entry["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(&format!("https://rustsec.org/advisories/{id}")),
            "{id}'s reason must link the advisory: {reason:?}"
        );
        assert!(reason.len() > 80, "{id}'s reason is too thin: {reason:?}");
        ids.insert(id.to_string());
    }
    assert_eq!(ids, ["RUSTSEC-2024-0436"].map(String::from).into());
}

#[test]
fn the_test_suite_runs_on_macos_too() {
    // The goldens alone don't cover the file modes, paths and terminal code a Mac exercises.
    let ci = load(".github/workflows/ci.yml");
    let found = ci.jobs().into_iter().any(|(_, j)| {
        j["runs-on"].as_str() == Some("macos-latest")
            && runs(j)
                .iter()
                .any(|r| r.trim() == "cargo test --locked --no-fail-fast")
    });
    assert!(found, "no macOS job runs cargo test");
}

#[test]
fn every_job_that_runs_the_oracle_uses_the_pinned_python() {
    // tests/profile_oracle.rs compares reses with configparser, and configparser can change
    // between Python releases, so CI installs the version .python-version names and the
    // oracle test refuses any other.
    let pinned = read(".python-version");
    assert_eq!(
        pinned.trim(),
        "3.11",
        ".python-version should name a minor release"
    );
    let mut checked = 0;
    for rel in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        let wf = load(rel);
        for (id, job) in wf.jobs() {
            if !runs(job)
                .iter()
                .any(|r| r.trim() == "cargo test --locked --no-fail-fast")
            {
                continue;
            }
            let setup = steps(job).into_iter().find(|s| {
                s["uses"]
                    .as_str()
                    .is_some_and(|u| u.starts_with("actions/setup-python@"))
            });
            let file = setup.and_then(|s| s["with"]["python-version-file"].as_str());
            assert_eq!(
                file,
                Some(".python-version"),
                "{rel} job `{id}` runs the oracle without the pinned Python"
            );
            checked += 1;
        }
    }
    // ci.yml's Linux and macOS test jobs and the release's test job.
    assert!(checked >= 3, "only {checked} jobs run the full suite");
    // The CI image is Debian bookworm, whose python3 is 3.11.
    assert!(
        read("docker/ci.Dockerfile").contains("-bookworm@sha256:"),
        "the CI image is no longer bookworm, so its python3 may not be {}",
        pinned.trim()
    );
}

#[test]
fn the_15_control_fails_in_ci() {
    // In ci.yml a kernel that stops reproducing #15 should turn the job red, so someone notices
    // the test can no longer catch it. Only the release is allowed to carry on with a warning.
    let ci = load(".github/workflows/ci.yml");
    for (id, job) in ci.jobs() {
        for step in steps(job) {
            if step["run"]
                .as_str()
                .is_some_and(|r| r.contains("macos-replace-binary.sh"))
            {
                assert!(
                    step["env"]["RESES_REPLACE_CONTROL"].is_badvalue()
                        && job["env"]["RESES_REPLACE_CONTROL"].is_badvalue(),
                    "`{id}` softens the #15 control in CI"
                );
            }
        }
    }
    assert!(!ci.env().contains_key("RESES_REPLACE_CONTROL"));
}
