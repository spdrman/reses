//! Shared reading of the GitHub workflow files for tests/ci_parity.rs and
//! tests/release_workflow.rs. I parse each workflow as YAML, so a job-level `if:`, a
//! `continue-on-error` or an `env:` entry is seen wherever it sits, rather than only when a
//! line happens to look the way a grep expects. The one thing YAML throws away is comments, and
//! the `# version` note after a pinned action lives in a comment, so the pin check also reads
//! the raw lines and cross-checks them against what the parser found.
//!
//! Each test file pulls this in with `#[path = "support/workflows.rs"] mod workflows;`, and
//! neither uses every helper, hence the `dead_code` allowance.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use yaml_rust2::{Yaml, YamlLoader};

/// The only job-level `if:` a CI gate job may carry. I skip a `synchronize` run for a PR from
/// this repo, because the push to its branch already ran the same jobs on the same commit, and
/// keep it for a fork, whose pushes never reach this repo's `push` trigger.
pub const FORK_GATE: &str = "github.event_name != 'pull_request' || github.event.action != 'synchronize' || github.event.pull_request.head.repo.fork";

/// Every action either workflow may use: owner/name, the commit it's pinned to, and the version
/// that commit is. I checked each pair against GitHub when I wrote it down (the commit behind
/// the tag, or for dtolnay/rust-toolchain the head of the branch named for the toolchain). A
/// bump changes the SHA, the comment in the workflow and this row together, so the reviewer
/// sees all three.
pub const PINS: &[(&str, &str, &str)] = &[
    (
        "actions/checkout",
        "11d5960a326750d5838078e36cf38b85af677262",
        "v4.4.0",
    ),
    (
        "dtolnay/rust-toolchain",
        "ce678459e9fc7500d337468f904b95f1b5c10b5e",
        "1.98.1",
    ),
    (
        "dtolnay/rust-toolchain",
        "62ae3a85dbdd2bedbb5819da8ce45635129289a1",
        "1.98.0",
    ),
    (
        "Swatinem/rust-cache",
        "6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
        "v2.9.2",
    ),
    (
        "actions/setup-python",
        "a26af69be951a213d495a4c3e4e4022e16d87065",
        "v5.6.0",
    ),
    (
        "taiki-e/install-action",
        "9983c65e42da123ff25d1f78505eb6de315aa172",
        "v2.87.20",
    ),
    (
        "actions/upload-artifact",
        "ea165f8d65b6e75b540449e92b4886f43607fa02",
        "v4.6.2",
    ),
    (
        "actions/download-artifact",
        "d3f86a106a0bac45b974a628896c90dbdf5c8093",
        "v4.3.0",
    ),
];

/// A file from the repo root, as text.
pub fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// One parsed workflow, with its raw text kept for the checks that need comments.
pub struct Workflow {
    pub rel: String,
    pub text: String,
    pub doc: Yaml,
}

/// I load the file and refuse anything that isn't a single YAML mapping with a `jobs` mapping,
/// so a parse that silently comes back empty can't make every later check pass.
pub fn load(rel: &str) -> Workflow {
    let text = read(rel);
    let mut docs =
        YamlLoader::load_from_str(&text).unwrap_or_else(|e| panic!("{rel} is not YAML: {e}"));
    assert_eq!(docs.len(), 1, "{rel} should hold exactly one YAML document");
    let doc = docs.remove(0);
    assert!(
        doc["jobs"].as_hash().is_some_and(|h| !h.is_empty()),
        "{rel} has no jobs mapping"
    );
    Workflow {
        rel: rel.to_string(),
        text,
        doc,
    }
}

impl Workflow {
    /// Every job as (id, body), in file order.
    pub fn jobs(&self) -> Vec<(String, &Yaml)> {
        self.doc["jobs"]
            .as_hash()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str().expect("job ids are strings").to_string(), v))
            .collect()
    }

    /// One job by id; a missing job is a test failure, not an empty answer.
    pub fn job(&self, id: &str) -> &Yaml {
        let j = &self.doc["jobs"][id];
        assert!(!j.is_badvalue(), "{} has no `{id}` job", self.rel);
        j
    }

    /// The workflow-level `env:`.
    pub fn env(&self) -> BTreeMap<String, String> {
        scalar_map(&self.doc["env"])
    }

    /// The `on:` block. YAML 1.2 keeps `on` a string key, so this is never read as `true`.
    pub fn on(&self) -> &Yaml {
        let on = &self.doc["on"];
        assert!(!on.is_badvalue(), "{} has no `on:`", self.rel);
        on
    }

    /// Every `run:` script in the workflow, one entry per step.
    pub fn all_runs(&self) -> Vec<String> {
        self.jobs().iter().flat_map(|(_, j)| runs(j)).collect()
    }

    /// Every `uses:` value in the workflow, as the parser sees it.
    pub fn all_uses(&self) -> Vec<String> {
        self.jobs()
            .iter()
            .flat_map(|(_, j)| steps(j))
            .filter_map(|s| s["uses"].as_str().map(str::to_string))
            .collect()
    }
}

/// A job's steps, empty when it has none.
pub fn steps(job: &Yaml) -> Vec<&Yaml> {
    job["steps"]
        .as_vec()
        .map(|v| v.iter().collect())
        .unwrap_or_default()
}

/// The `run:` scripts of a job's steps.
pub fn runs(job: &Yaml) -> Vec<String> {
    steps(job)
        .into_iter()
        .filter_map(|s| s["run"].as_str().map(str::to_string))
        .collect()
}

/// A scalar as the string GitHub would substitute: strings as they are, numbers and booleans
/// written out, anything else refused.
pub fn scalar(y: &Yaml) -> String {
    match y {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        other => panic!("expected a scalar, got {other:?}"),
    }
}

/// A mapping of scalars (an `env:` or a `with:`), empty when the key is absent.
pub fn scalar_map(y: &Yaml) -> BTreeMap<String, String> {
    match y.as_hash() {
        None => BTreeMap::new(),
        Some(h) => h.iter().map(|(k, v)| (scalar(k), scalar(v))).collect(),
    }
}

/// The step whose `run:` or `uses:` mentions `needle`; exactly one must.
pub fn step_with<'a>(job: &'a Yaml, needle: &str) -> &'a Yaml {
    let found: Vec<&Yaml> = steps(job)
        .into_iter()
        .filter(|s| {
            s["run"].as_str().is_some_and(|r| r.contains(needle))
                || s["uses"].as_str().is_some_and(|u| u.contains(needle))
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one step mentioning {needle:?}, found {found:#?}"
    );
    found[0]
}

/// Each raw `uses:` line as (action, ref, version comment). The comment is the text after `#`,
/// or empty when there's none.
pub fn uses_lines(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            l.strip_prefix("- uses:")
                .or_else(|| l.strip_prefix("uses:"))
        })
        .map(|rest| {
            let (spec, comment) = rest.split_once('#').unwrap_or((rest, ""));
            let spec = spec.trim();
            let (action, git_ref) = spec.split_once('@').unwrap_or((spec, ""));
            (
                action.to_string(),
                git_ref.to_string(),
                comment.trim().to_string(),
            )
        })
        .collect()
}
