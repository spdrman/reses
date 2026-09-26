//! The shared AWS credentials file (`~/.aws/credentials`, INI format).
//!
//! Loading and saving must round-trip everything we do not own: other profiles, comments,
//! blank lines and unknown keys stay exactly as they were.
//!
//! I keep the file as its original lines, each with its own line ending, and only ever edit the
//! handful of lines that hold the keys we own. Everything else is written back untouched, which
//! is what makes the round trip byte for byte.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_ID: &str = "aws_access_key_id";
const SECRET: &str = "aws_secret_access_key";
const TOKEN: &str = "aws_session_token";
const REGION: &str = "region";

#[derive(Clone, PartialEq, Eq, Default)]
pub struct Profile {
    pub name: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: Option<String>,
}

// Never print a secret, not even in a debug log.
impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("region", &self.region)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid profile: {0}")]
    Invalid(String),
}

/// One physical line of the file, split from its terminator so edits can keep the terminator.
#[derive(Clone, Default)]
struct Line {
    text: String,
    /// "\n", "\r\n", or "" for a last line with no newline.
    eol: &'static str,
}

#[derive(Clone, Default)]
pub struct CredentialsFile {
    path: PathBuf,
    lines: Vec<Line>,
}

// The lines hold secrets, so Debug shows only the path and the profile names.
impl fmt::Debug for CredentialsFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self.profiles().into_iter().map(|p| p.name).collect();
        f.debug_struct("CredentialsFile")
            .field("path", &self.path)
            .field("profiles", &names)
            .finish()
    }
}

impl CredentialsFile {
    /// `$AWS_SHARED_CREDENTIALS_FILE`, else `~/.aws/credentials`.
    pub fn default_path() -> PathBuf {
        credentials_path_from(
            std::env::var_os("AWS_SHARED_CREDENTIALS_FILE"),
            std::env::var_os("HOME"),
        )
    }

    /// A missing file loads as empty.
    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(ProfileError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            lines: split_lines(&text),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Profiles in file order. A section counts as a profile once it has both an access key id
    /// and a secret; a section repeated later in the file is ignored, as the AWS CLI refuses it.
    pub fn profiles(&self) -> Vec<Profile> {
        let mut seen = Vec::<&str>::new();
        let mut out = Vec::new();
        for sec in sections(&self.lines) {
            if seen.contains(&sec.name) {
                continue;
            }
            seen.push(sec.name);
            let value = |key: &str| {
                owned_lines(&self.lines, &sec, key)
                    .last()
                    .and_then(|&i| kv(&self.lines[i].text))
                    .map(|(_, v)| v.to_string())
                    .filter(|v| !v.is_empty())
            };
            if let (Some(access_key_id), Some(secret_access_key)) = (value(KEY_ID), value(SECRET)) {
                out.push(Profile {
                    name: sec.name.to_string(),
                    access_key_id,
                    secret_access_key,
                    session_token: value(TOKEN),
                    region: value(REGION),
                });
            }
        }
        out
    }

    pub fn get(&self, name: &str) -> Option<Profile> {
        self.profiles().into_iter().find(|p| p.name == name)
    }

    /// Add the profile, or replace the keys we own in an existing section of that name.
    ///
    /// Only aws_access_key_id, aws_secret_access_key, aws_session_token and region are touched.
    /// An empty or `None` session token or region removes that line.
    pub fn upsert(&mut self, profile: &Profile) -> Result<(), ProfileError> {
        validate(profile)?;
        let wanted: [(&str, Option<&str>); 4] = [
            (KEY_ID, Some(&profile.access_key_id)),
            (SECRET, Some(&profile.secret_access_key)),
            (TOKEN, non_empty(&profile.session_token)),
            (REGION, non_empty(&profile.region)),
        ];
        let exists = sections(&self.lines).any(|s| s.name == profile.name);
        if !exists {
            self.append_section(&profile.name, &wanted);
            return Ok(());
        }
        for (key, value) in wanted {
            self.set_key(&profile.name, key, value);
        }
        Ok(())
    }

    /// Write atomically with mode 0600.
    pub fn save(&self) -> Result<(), ProfileError> {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(&line.text);
            text.push_str(line.eol);
        }
        write_atomic(&self.path, text.as_bytes(), Some(0o600), Some(0o700)).map_err(|source| {
            ProfileError::Write {
                path: self.path.clone(),
                source,
            }
        })
    }

    /// The line ending the file already uses, "\n" for a new file.
    fn eol(&self) -> &'static str {
        self.lines
            .iter()
            .map(|l| l.eol)
            .find(|e| !e.is_empty())
            .unwrap_or("\n")
    }

    fn append_section(&mut self, name: &str, wanted: &[(&str, Option<&str>)]) {
        let eol = self.eol();
        if let Some(last) = self.lines.last_mut() {
            if last.eol.is_empty() {
                last.eol = eol;
            }
            if !last.text.trim().is_empty() {
                self.lines.push(Line {
                    text: String::new(),
                    eol,
                });
            }
        }
        self.lines.push(Line {
            text: format!("[{name}]"),
            eol,
        });
        for (key, value) in wanted {
            if let Some(v) = value {
                self.lines.push(Line {
                    text: format!("{key} = {v}"),
                    eol,
                });
            }
        }
    }

    /// Make the first section called `name` hold `key = value` exactly once, or not at all.
    fn set_key(&mut self, name: &str, key: &str, value: Option<&str>) {
        let Some(sec) = sections(&self.lines).find(|s| s.name == name) else {
            return;
        };
        let found = owned_lines(&self.lines, &sec, key);
        let span = (sec.header, sec.end);
        let keep = match (value, found.first()) {
            (Some(v), Some(&i)) => {
                rewrite_value(&mut self.lines[i].text, v);
                Some(i)
            }
            (Some(v), None) => {
                self.insert_key(span, key, v);
                None
            }
            (None, _) => None,
        };
        // Drop every other copy, last first so earlier indices stay valid.
        for &i in found.iter().rev() {
            if Some(i) != keep {
                self.remove_line(i);
            }
        }
    }

    /// Insert after the section's last key, copying the spacing around `=` from its first key.
    fn insert_key(&mut self, (header, end): (usize, usize), key: &str, value: &str) {
        let body = header + 1..end;
        let last_key = body
            .clone()
            .rev()
            .find(|&i| kv(&self.lines[i].text).is_some() || is_continuation(&self.lines, i));
        let sep = body
            .clone()
            .find_map(|i| separator(&self.lines[i].text))
            .unwrap_or_else(|| " = ".to_string());
        let after = last_key.unwrap_or(header);
        let file_eol = self.eol();
        let prev = &mut self.lines[after];
        // At EOF the new line takes over "no newline at the end", so the file keeps its shape.
        let eol = if prev.eol.is_empty() {
            prev.eol = file_eol;
            ""
        } else {
            prev.eol
        };
        self.lines.insert(
            after + 1,
            Line {
                text: format!("{key}{sep}{value}"),
                eol,
            },
        );
    }

    fn remove_line(&mut self, i: usize) {
        let removed = self.lines.remove(i);
        // Removing the last line must not add a trailing newline the file did not have.
        if removed.eol.is_empty()
            && i == self.lines.len()
            && let Some(prev) = self.lines.last_mut()
        {
            prev.eol = "";
        }
    }
}

fn non_empty(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

fn validate(p: &Profile) -> Result<(), ProfileError> {
    let bad = |what: &str, why: &str| Err(ProfileError::Invalid(format!("{what} {why}")));
    let breaks_line = |s: &str| s.chars().any(|c| c == '\n' || c == '\r');
    if p.name.trim().is_empty() {
        return bad("profile name", "is empty");
    }
    if breaks_line(&p.name) || p.name.contains('[') || p.name.contains(']') {
        return bad("profile name", "contains a newline or a bracket");
    }
    if p.name.trim() != p.name {
        return bad("profile name", "starts or ends with whitespace");
    }
    // Values are named but never echoed, since they are secrets.
    let values: [(&str, Option<&str>, bool); 4] = [
        ("access key id", Some(&p.access_key_id), true),
        ("secret access key", Some(&p.secret_access_key), true),
        ("session token", p.session_token.as_deref(), false),
        ("region", p.region.as_deref(), false),
    ];
    for (what, value, required) in values {
        let Some(v) = value else { continue };
        if v.is_empty() {
            if required {
                return bad(what, "is empty");
            }
            continue;
        }
        if breaks_line(v) {
            return bad(what, "contains a newline");
        }
        if v.trim() != v {
            return bad(what, "starts or ends with whitespace");
        }
    }
    Ok(())
}

fn split_lines(text: &str) -> Vec<Line> {
    text.split_inclusive('\n')
        .map(|raw| {
            if let Some(t) = raw.strip_suffix("\r\n") {
                Line {
                    text: t.to_string(),
                    eol: "\r\n",
                }
            } else if let Some(t) = raw.strip_suffix('\n') {
                Line {
                    text: t.to_string(),
                    eol: "\n",
                }
            } else {
                Line {
                    text: raw.to_string(),
                    eol: "",
                }
            }
        })
        .collect()
}

struct Section<'a> {
    name: &'a str,
    header: usize,
    /// One past the last line of the section.
    end: usize,
}

fn header(text: &str) -> Option<&str> {
    let t = text.trim();
    t.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

fn is_comment_or_blank(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty() || t.starts_with('#') || t.starts_with(';')
}

/// `key = value` or `key=value`, both trimmed. The first `=` splits, so values may contain `=`.
fn kv(text: &str) -> Option<(&str, &str)> {
    if is_comment_or_blank(text) || header(text).is_some() {
        return None;
    }
    let (k, v) = text.split_once('=')?;
    let k = k.trim();
    (!k.is_empty()).then_some((k, v.trim()))
}

/// An indented line after a key continues that key's value, the way Python's configparser (and
/// so the AWS CLI) reads it. It is never a key of its own.
fn is_continuation(lines: &[Line], i: usize) -> bool {
    let starts_indented = lines[i].text.starts_with([' ', '\t']);
    if !starts_indented || is_comment_or_blank(&lines[i].text) {
        return false;
    }
    lines[..i]
        .iter()
        .rev()
        .find(|l| !is_comment_or_blank(&l.text))
        .is_some_and(|l| header(&l.text).is_none())
}

fn sections(lines: &[Line]) -> impl Iterator<Item = Section<'_>> {
    let headers: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| header(&l.text).map(|n| (i, n)))
        .collect();
    let ends: Vec<usize> = headers
        .iter()
        .skip(1)
        .map(|&(i, _)| i)
        .chain(std::iter::once(lines.len()))
        .collect();
    headers
        .into_iter()
        .zip(ends)
        .map(|((header, name), end)| Section { name, header, end })
}

/// Indices of the lines in `sec` that set `key` (case-insensitive), in file order.
fn owned_lines(lines: &[Line], sec: &Section, key: &str) -> Vec<usize> {
    (sec.header + 1..sec.end)
        .filter(|&i| !is_continuation(lines, i))
        .filter(|&i| kv(&lines[i].text).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)))
        .collect()
}

/// The exact text between the key and the value, e.g. "=" or " = ".
fn separator(text: &str) -> Option<String> {
    kv(text)?;
    let eq = text.find('=')?;
    let before = &text[..eq];
    let after = &text[eq + 1..];
    let lead = &before[before.trim_end().len()..];
    let trail = &after[..after.len() - after.trim_start().len()];
    Some(format!("{lead}={trail}"))
}

/// Swap the value on a `key = value` line, keeping everything around it.
fn rewrite_value(text: &mut String, value: &str) {
    let Some(eq) = text.find('=') else { return };
    let after = &text[eq + 1..];
    let start = eq + 1 + (after.len() - after.trim_start().len());
    let end = eq + 1 + after.trim_end().len();
    if &text[start..end.max(start)] != value {
        text.replace_range(start..end.max(start), value);
    }
}

/// Write `bytes` to `path` through a temp file in the same directory and a rename, so a reader
/// never sees half a file. Creates missing parent directories with `dir_mode`, and gives the new
/// file `file_mode` (both Unix only; `None` leaves the umask default). A symlink at `path` is
/// followed, so the file it points at is the one replaced.
pub(crate) fn write_atomic(
    path: &Path,
    bytes: &[u8],
    file_mode: Option<u32>,
    dir_mode: Option<u32>,
) -> io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    create_dirs(&dir, dir_mode)?;

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (tmp, mut file) = loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{base}.tmp-{}-{n}", std::process::id()));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = file_mode {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        match opts.open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        #[cfg(unix)]
        if let Some(mode) = file_mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = file_mode;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    // Make the rename itself durable. Best effort: not every filesystem allows it.
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn create_dirs(dir: &Path, mode: Option<u32>) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    builder.create(dir)
}

/// Where the credentials file lives, given the values of `$AWS_SHARED_CREDENTIALS_FILE` and
/// `$HOME`. Split out so tests can check it without touching the process environment.
pub fn credentials_path_from(
    shared_credentials_file: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    match shared_credentials_file {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(home.unwrap_or_default()).join(".aws/credentials"),
    }
}

/// Where the AWS config file lives, given `$AWS_CONFIG_FILE` and `$HOME`.
pub fn config_path_from(config_file: Option<OsString>, home: Option<OsString>) -> PathBuf {
    match config_file {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(home.unwrap_or_default()).join(".aws/config"),
    }
}

/// Region for a profile from the AWS config file (`$AWS_CONFIG_FILE`, else `~/.aws/config`),
/// used when the credentials file has none.
pub fn region_from_config(profile: &str) -> Option<String> {
    let path = config_path_from(
        std::env::var_os("AWS_CONFIG_FILE"),
        std::env::var_os("HOME"),
    );
    region_from_config_file(&path, profile)
}

/// Region for a profile from an AWS config file at an explicit path. The config file names
/// sections `[default]` and `[profile NAME]`; `[profile default]` also counts for "default".
pub fn region_from_config_file(path: &Path, profile: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let lines = split_lines(&text);
    let matches = |section: &str| {
        let named = section
            .strip_prefix("profile")
            .filter(|rest| rest.starts_with([' ', '\t']))
            .map(str::trim);
        named == Some(profile) || (profile == "default" && section == "default")
    };
    // I prefer `[default]` over `[profile default]` when a file has both.
    let mut secs: Vec<Section> = sections(&lines).filter(|s| matches(s.name)).collect();
    secs.sort_by_key(|s| s.name != "default");
    secs.iter().find_map(|sec| {
        owned_lines(&lines, sec, REGION)
            .last()
            .and_then(|&i| kv(&lines[i].text))
            .map(|(_, v)| v.to_string())
            .filter(|v| !v.is_empty())
    })
}
