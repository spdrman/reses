//! `install.sh` is the one-line installer for Debian and Ubuntu. I run the real script against
//! stand-in `uname`, `dpkg`, `curl`, `apt-get` and `reses` commands on a private PATH, so each
//! test can pick the machine's architecture, what the "release" serves and what its checksums
//! say, and then check what the script actually asked apt to install. Nothing touches the
//! network or the real package manager.
//!
//! These only run on Linux. The script is for Debian and Ubuntu and leans on GNU coreutils
//! (sha256sum), which a Mac doesn't ship, so on macOS every test would only prove the script
//! refuses a machine it was never meant for. ci.yml's macOS test job skips the file for that
//! reason, and the Linux test job and the local gate still run all of it.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The tag the fake "latest release" redirect points at.
const LATEST: &str = "v9.9.9";

/// One fake machine: a temp dir holding the stub commands in `bin/`, the files the "release"
/// serves in `served/`, and `calls.log`, where the stubs note every download and apt call.
struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    /// A machine of the given architecture, whose "latest release" serves `debs` (file name to
    /// contents) and a SHA256SUMS listing `sums` (file name to the checksum it claims).
    fn new(os: &str, arch: &str, debs: &[(&str, &str)], sums: &[(&str, &str)]) -> Self {
        // Lay out what the release serves: the packages and their checksum listing.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let served = dir.path().join("served");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&served).unwrap();
        for (name, body) in debs {
            fs::write(served.join(name), body).unwrap();
        }
        let listing: String = sums
            .iter()
            .map(|(name, sum)| format!("{sum}  {name}\n"))
            .collect();
        fs::write(served.join("SHA256SUMS"), listing).unwrap();

        // Then the stub commands, each a tiny sh script on the private PATH.
        let log = dir.path().join("calls.log");
        let script = |name: &str, body: String| {
            let path = bin.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        script("uname", format!("echo {os}"));
        script(
            "dpkg",
            format!("[ \"$1\" = --print-architecture ] && echo {arch}"),
        );
        script("reses", "echo reses 9.9.9".into());
        // The user id comes from FAKE_UID so a test can take the non-root path on any machine, and
        // sudo just runs its command on this PATH, so the stub apt-get still answers it.
        script("id", "[ \"$1\" = -u ] && echo \"${FAKE_UID:-0}\"".into());
        script(
            "sudo",
            format!("echo \"sudo $*\" >> {}; exec \"$@\"", log.display()),
        );
        script(
            "apt-get",
            format!(
                "echo \"apt-get $*\" >> {}; [ -z \"${{FAKE_APT_FAIL:-}}\" ]",
                log.display()
            ),
        );
        // curl: -I with -w %{{url_effective}} answers the "latest" redirect; -o FILE URL copies
        // the file of that name out of the served directory, or fails like a 404.
        script(
            "curl",
            format!(
                r#"out=""; url=""; head=0
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift ;;
    -w) shift ;;
    -*I*) head=1 ;;
    -*) ;;
    *) url="$1" ;;
  esac
  shift
done
echo "curl $url" >> {log}
if [ "$head" = 1 ]; then [ -n "${{FAKE_NO_LATEST:-}}" ] && exit 6; echo "${{FAKE_LATEST:-https://github.com/spdrman/reses/releases/tag/{LATEST}}}"; exit 0; fi
f="{served}/${{url##*/}}"
[ -f "$f" ] || exit 22
cp "$f" "$out""#,
                log = log.display(),
                served = served.display()
            ),
        );
        Fake { dir }
    }

    /// Run the real install.sh on this machine with a clean environment plus `env`, and return
    /// its exit code, stdout and stderr. The stubs come first on PATH, so they shadow the real
    /// commands, and the system dirs after them supply the ordinary tools.
    fn run(&self, env: &[(&str, &str)]) -> (i32, String, String) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
        let bin: PathBuf = self.dir.path().join("bin");
        let mut cmd = Command::new("sh");
        cmd.arg(&script)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("running install.sh");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Everything the stubs logged, one call per line, or nothing if no stub ever ran.
    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls.log")).unwrap_or_default()
    }
}

/// The SHA-256 of `body` in hex, from the same sha256sum the script checks with.
fn sha256(body: &str) -> String {
    let out = Command::new("sha256sum").arg("-").stdin_bytes(body);
    out.split_whitespace().next().unwrap().to_string()
}

/// Feed a string to a command's stdin and collect its stdout, which `Command` can't do in one
/// call on its own.
trait StdinBytes {
    /// Run the command with `body` on stdin and return what it printed.
    fn stdin_bytes(&mut self, body: &str) -> String;
}

impl StdinBytes for Command {
    /// Spawn with piped stdin and stdout, write `body`, and wait. Dropping stdin after the write
    /// closes it, so the command sees the end of its input.
    fn stdin_bytes(&mut self, body: &str) -> String {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = self
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    }
}

/// The apt-get line the script produced, if any.
fn apt_line(calls: &str) -> Option<&str> {
    calls.lines().find(|l| l.starts_with("apt-get "))
}

/// On amd64 and arm64 alike, the script fetches the latest release's checksums and hands apt the
/// package for this machine, never the other architecture's.
#[test]
fn it_installs_the_latest_release_for_this_machines_architecture() {
    for arch in ["amd64", "arm64"] {
        let deb = format!("reses_9.9.9_{arch}.deb");
        let other = if arch == "amd64" {
            "reses_9.9.9_arm64.deb"
        } else {
            "reses_9.9.9_amd64.deb"
        };
        let fake = Fake::new(
            "Linux",
            arch,
            &[(&deb, "the right package"), (other, "the wrong package")],
            &[
                (&deb, &sha256("the right package")),
                (other, &sha256("the wrong package")),
            ],
        );
        let (code, _, err) = fake.run(&[]);
        assert_eq!(code, 0, "{arch}: {err}");
        let calls = fake.calls();
        let apt = apt_line(&calls).unwrap_or_else(|| panic!("{arch}: apt-get never ran: {calls}"));
        assert!(apt.contains("install -y"), "{apt}");
        assert!(
            apt.ends_with(&format!("/{deb}")),
            "{arch}: installed the wrong file: {apt}"
        );
        assert!(
            calls.contains("releases/download/v9.9.9/SHA256SUMS"),
            "{calls}"
        );
    }
}

/// A package whose bytes don't match its listed checksum never reaches apt.
#[test]
fn a_checksum_mismatch_installs_nothing() {
    let deb = "reses_9.9.9_amd64.deb";
    let fake = Fake::new(
        "Linux",
        "amd64",
        &[(deb, "tampered")],
        &[(deb, &sha256("the real one"))],
    );
    let (code, _, err) = fake.run(&[]);
    assert_ne!(code, 0);
    assert!(err.contains("checksum"), "{err}");
    assert_eq!(
        apt_line(&fake.calls()),
        None,
        "apt ran on a package that failed its checksum"
    );
}

/// A package the checksum listing doesn't mention at all is refused too.
#[test]
fn a_package_missing_from_the_checksums_installs_nothing() {
    let deb = "reses_9.9.9_amd64.deb";
    let fake = Fake::new("Linux", "amd64", &[(deb, "real")], &[]);
    let (code, _, err) = fake.run(&[]);
    assert_ne!(code, 0);
    assert!(err.contains("checksum"), "{err}");
    assert_eq!(apt_line(&fake.calls()), None);
}

/// With RESES_VERSION set, the script downloads that release and never asks for the latest.
#[test]
fn a_pinned_version_skips_the_latest_lookup() {
    let deb = "reses_1.2.3_amd64.deb";
    let fake = Fake::new("Linux", "amd64", &[(deb, "old")], &[(deb, &sha256("old"))]);
    let (code, _, err) = fake.run(&[("RESES_VERSION", "v1.2.3")]);
    assert_eq!(code, 0, "{err}");
    let calls = fake.calls();
    assert!(
        calls.contains("releases/download/v1.2.3/reses_1.2.3_amd64.deb"),
        "{calls}"
    );
    assert!(
        !calls.contains("releases/latest"),
        "it looked up latest anyway: {calls}"
    );
}

/// An architecture the release has no package for is refused before anything is downloaded.
#[test]
fn an_unsupported_architecture_is_refused() {
    let fake = Fake::new("Linux", "riscv64", &[], &[]);
    let (code, _, err) = fake.run(&[]);
    assert_ne!(code, 0);
    assert!(err.contains("no reses package for riscv64"), "{err}");
    assert!(
        !fake.calls().contains("curl"),
        "it downloaded for an unsupported arch: {}",
        fake.calls()
    );
    assert_eq!(apt_line(&fake.calls()), None);
}

/// Run on a Mac, the script stops and names the Homebrew command instead.
#[test]
fn a_mac_is_pointed_at_homebrew() {
    let fake = Fake::new("Darwin", "arm64", &[], &[]);
    let (code, _, err) = fake.run(&[]);
    assert_ne!(code, 0);
    assert!(err.contains("brew install spdrman/reses/reses"), "{err}");
}

/// A normal user's install goes through sudo, and root's calls apt directly.
#[test]
fn a_non_root_user_installs_through_sudo_and_root_does_not() {
    let deb = "reses_9.9.9_amd64.deb";
    for (uid, wants_sudo) in [("1000", true), ("0", false)] {
        let fake = Fake::new("Linux", "amd64", &[(deb, "pkg")], &[(deb, &sha256("pkg"))]);
        let (code, _, err) = fake.run(&[("FAKE_UID", uid)]);
        assert_eq!(code, 0, "uid {uid}: {err}");
        let calls = fake.calls();
        assert!(
            apt_line(&calls).is_some(),
            "uid {uid}: apt-get never ran: {calls}"
        );
        assert_eq!(
            calls.contains("sudo apt-get install"),
            wants_sudo,
            "uid {uid}: {calls}"
        );
    }
}

impl Fake {
    /// Take one stub off this machine's PATH, for a test about a machine that lacks the tool.
    fn remove_tool(&self, name: &str) {
        fs::remove_file(self.dir.path().join("bin").join(name)).unwrap();
    }
}

/// The everyday case: an amd64 machine and a release whose one package checks out.
fn amd64_release() -> Fake {
    let deb = "reses_9.9.9_amd64.deb";
    Fake::new("Linux", "amd64", &[(deb, "pkg")], &[(deb, &sha256("pkg"))])
}

/// When apt fails, so does the installer, and it doesn't claim the install worked.
#[test]
fn a_failed_install_is_reported_as_a_failure() {
    let fake = amd64_release();
    let (code, _, err) = fake.run(&[("FAKE_APT_FAIL", "1")]);
    assert_ne!(code, 0, "apt failed but the installer succeeded: {err}");
    assert!(!err.contains("installed"), "it claimed success: {err}");
}

/// If GitHub doesn't answer the latest-release lookup, the script stops and says so.
#[test]
fn no_answer_about_the_latest_release_installs_nothing() {
    let fake = amd64_release();
    let (code, _, err) = fake.run(&[("FAKE_NO_LATEST", "1")]);
    assert_ne!(code, 0);
    assert!(err.contains("latest release"), "{err}");
    assert_eq!(apt_line(&fake.calls()), None);
}

/// A latest redirect that doesn't end in a version tag leaves the script not knowing which
/// release to fetch, so it fetches none.
#[test]
fn a_latest_redirect_that_isnt_a_version_installs_nothing() {
    let fake = amd64_release();
    let (code, _, err) = fake.run(&[("FAKE_LATEST", "https://github.com/spdrman/reses/releases")]);
    assert_ne!(code, 0);
    assert!(err.contains("which release"), "{err}");
    assert!(
        !fake.calls().contains("releases/download"),
        "it downloaded anyway: {}",
        fake.calls()
    );
}

/// A RESES_VERSION with path segments in it could point the download at another repo, so it's
/// refused before any URL is built from it.
#[test]
fn a_version_that_could_steer_the_download_is_refused() {
    let fake = amd64_release();
    let (code, _, err) = fake.run(&[(
        "RESES_VERSION",
        "v1/../../../../evil/repo/releases/download/v1",
    )]);
    assert_ne!(code, 0);
    assert!(err.contains("which release"), "{err}");
    assert!(
        !fake.calls().contains("evil"),
        "it followed the crafted version: {}",
        fake.calls()
    );
}

/// `RESES_VERSION=1.2.3` works the same as `v1.2.3`.
#[test]
fn a_version_without_the_v_is_accepted() {
    let deb = "reses_1.2.3_amd64.deb";
    let fake = Fake::new("Linux", "amd64", &[(deb, "old")], &[(deb, &sha256("old"))]);
    let (code, _, err) = fake.run(&[("RESES_VERSION", "1.2.3")]);
    assert_eq!(code, 0, "{err}");
    assert!(
        fake.calls()
            .contains("releases/download/v1.2.3/reses_1.2.3_amd64.deb"),
        "{}",
        fake.calls()
    );
}

/// A decoy checksum line whose name only starts with the package name must not stand in for
/// the package's own line.
#[test]
fn only_the_exact_package_line_in_the_checksums_counts() {
    let deb = "reses_9.9.9_amd64.deb";
    let fake = Fake::new(
        "Linux",
        "amd64",
        &[(deb, "tampered")],
        &[
            (&format!("{deb}.sig"), &sha256("tampered")),
            (deb, &sha256("genuine")),
        ],
    );
    let (code, _, err) = fake.run(&[]);
    assert_ne!(code, 0, "{err}");
    assert_eq!(apt_line(&fake.calls()), None);
}

/// A machine without apt is told it needs apt and dpkg, and nothing is downloaded first.
#[test]
fn a_machine_without_apt_is_told_so_and_nothing_is_downloaded() {
    let fake = amd64_release();
    fake.remove_tool("apt-get");
    // Only the tools the script needs, so a real apt-get on this machine can't stand in.
    let tools = fake.dir.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    for t in [
        "sh",
        "grep",
        "awk",
        "mktemp",
        "chmod",
        "rm",
        "cat",
        "cp",
        "sha256sum",
        "printf",
    ] {
        if let Some(p) = ["/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(t))
            .find(|p| p.exists())
        {
            std::os::unix::fs::symlink(p, tools.join(t)).unwrap();
        }
    }
    let path = format!(
        "{}:{}",
        fake.dir.path().join("bin").display(),
        tools.display()
    );
    let (code, _, err) = fake.run(&[("PATH", &path)]);
    assert_ne!(code, 0);
    assert!(err.contains("apt and dpkg are needed"), "{err}");
    assert!(
        !fake.calls().contains("curl"),
        "it downloaded before checking for apt: {}",
        fake.calls()
    );
}

/// With only `reses_..._amd64.deb.sig` listed, the package itself has no checksum. A prefix
/// match would mistake the decoy for it; the exact match refuses it as unlisted.
#[test]
fn a_checksum_listed_only_under_a_longer_name_doesnt_count() {
    let deb = "reses_9.9.9_amd64.deb";
    let fake = Fake::new(
        "Linux",
        "amd64",
        &[(deb, "pkg")],
        &[(&format!("{deb}.sig"), &sha256("pkg"))],
    );
    let (code, _, err) = fake.run(&[]);
    assert_ne!(code, 0);
    assert!(err.contains("isn't in the release's checksums"), "{err}");
    assert_eq!(apt_line(&fake.calls()), None);
}
