//! The shared AWS credentials file (`~/.aws/credentials`, INI format).
//!
//! Loading and saving must round-trip everything we do not own: other profiles, comments,
//! blank lines and unknown keys stay exactly as they were.
//!
//! I keep the file as its original lines, each with its own line ending, and only ever edit the
//! handful of lines that hold the keys we own. Everything else is written back untouched, which
//! is what makes the round trip byte for byte.
//!
//! botocore, and so the AWS CLI, reads these files with Python's `configparser`, so that is what
//! decides what a line means. The parser here follows `RawConfigParser._read` rule for rule
//! (section headers, `=` and `:` delimiters, indented continuation lines, `[DEFAULT]`, universal
//! newlines), and tests/profile_oracle.rs checks it against the real thing.

use std::collections::{BTreeSet, HashSet};
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
/// configparser's default section: never listed as a section, inherited by all of them.
const DEFAULT_SECTION: &str = "DEFAULT";

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

/// Why Python's configparser refuses a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IniError {
    /// A line that isn't blank or a comment comes before the first section header.
    MissingSectionHeader,
    /// A line that is none of header, `key = value`, continuation, comment or blank.
    Parsing,
    /// Strict mode only: the same section header twice.
    DuplicateSection { section: String },
    /// Strict mode only: the same key twice in one section.
    DuplicateOption { section: String, option: String },
}

/// Sections and items as `RawConfigParser(strict=False)` reads them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IniData {
    /// The `[DEFAULT]` section's items.
    pub defaults: Vec<(String, String)>,
    /// Every other section in first-seen order, with items as `items(section)` returns them:
    /// inherited defaults first, then the section's own keys.
    pub sections: Vec<(String, Vec<(String, String)>)>,
}

/// What Python's configparser makes of an INI text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniView {
    /// The error `RawConfigParser()` raises, which is what botocore uses.
    pub strict_error: Option<IniError>,
    /// What `RawConfigParser(strict=False)` reads, or the error it raises.
    pub read: Result<IniData, IniError>,
}

/// Read `text` the way Python's `configparser.RawConfigParser` would.
pub fn parse_ini(text: &str) -> IniView {
    let lines = split_lines(text);
    let parsed = parse(&lines);
    IniView {
        strict_error: parsed.strict_error.map(|(e, _)| e),
        read: match parsed.nonstrict_error {
            Some((e, _)) => Err(e),
            None => Ok(data(&lines, &parsed.kinds)),
        },
    }
}

/// One physical line of the file, split from its terminator so edits can keep the terminator.
#[derive(Clone, Default)]
struct Line {
    text: String,
    /// "\n", "\r\n", "\r", or "" for a last line with no newline.
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

    /// Profiles in file order: every section with a non-empty access key id and secret, read the
    /// way `RawConfigParser(strict=False)` reads it (so `[DEFAULT]` values are inherited and a
    /// repeated section merges into the first). This stays lenient about files the AWS CLI would
    /// refuse, so the accounts still show; `save` is where a broken file is refused.
    pub fn profiles(&self) -> Vec<Profile> {
        let parsed = parse(&self.lines);
        let data = data(&self.lines, &parsed.kinds);
        data.sections
            .into_iter()
            .filter_map(|(name, items)| {
                let value = |key: &str| {
                    items
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.clone())
                        .filter(|v| !v.is_empty())
                };
                Some(Profile {
                    access_key_id: value(KEY_ID)?,
                    secret_access_key: value(SECRET)?,
                    session_token: value(TOKEN),
                    region: value(REGION),
                    name,
                })
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<Profile> {
        self.profiles().into_iter().find(|p| p.name == name)
    }

    /// Whether a section called `name` exists, complete profile or not. Like configparser's
    /// `has_section`, this is false for `DEFAULT`.
    pub fn has_section(&self, name: &str) -> bool {
        name != DEFAULT_SECTION
            && parse(&self.lines)
                .kinds
                .iter()
                .any(|k| matches!(k, Kind::Header { name: n } if n == name))
    }

    /// Add the profile, or replace the keys we own in an existing section of that name.
    ///
    /// Only aws_access_key_id, aws_secret_access_key, aws_session_token and region are touched,
    /// together with any continuation lines under them. An empty or `None` session token or
    /// region removes that key.
    pub fn upsert(&mut self, profile: &Profile) -> Result<(), ProfileError> {
        validate(profile)?;
        let wanted: [(&str, Option<&str>); 4] = [
            (KEY_ID, Some(&profile.access_key_id)),
            (SECRET, Some(&profile.secret_access_key)),
            (TOKEN, non_empty(&profile.session_token)),
            (REGION, non_empty(&profile.region)),
        ];
        if !self.has_section(&profile.name) {
            self.append_section(&profile.name, &wanted);
            return Ok(());
        }
        for (key, value) in wanted {
            self.set_key(&profile.name, key, value);
        }
        Ok(())
    }

    /// Write atomically with mode 0600. Refuses to write anything configparser would reject
    /// (a repeated section, say), since the AWS CLI would then refuse the whole file.
    pub fn save(&self) -> Result<(), ProfileError> {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(&line.text);
            text.push_str(line.eol);
        }
        if let Some((err, line)) = parse(&self.lines).strict_error {
            return Err(ProfileError::Invalid(refusal(&self.path, &err, line)));
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
            if !py_trim(&last.text).is_empty() {
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
    /// Every line that belongs to a replaced or removed value goes with it.
    fn set_key(&mut self, name: &str, key: &str, value: Option<&str>) {
        let kinds = parse(&self.lines).kinds;
        let Some((header, end)) = first_run(&kinds, name) else {
            return;
        };
        let found: Vec<(usize, usize)> = (header + 1..end)
            .filter_map(|i| match &kinds[i] {
                Kind::Option { key: k, delim } if k == key => Some((i, *delim)),
                _ => None,
            })
            .collect();
        let keep = match (value, found.first()) {
            (Some(v), Some(&(i, delim))) => {
                rewrite_value(&mut self.lines[i].text, delim, v);
                Some(i)
            }
            (Some(v), None) => {
                self.insert_key(&kinds, header, end, key, v);
                return;
            }
            (None, _) => None,
        };
        let mut drop = BTreeSet::new();
        for &(i, _) in &found {
            if Some(i) != keep {
                drop.insert(i);
            }
            drop.extend(value_tail(&kinds, i));
        }
        self.remove_lines(&drop);
    }

    /// Insert after the section's last value line, copying the spacing around the delimiter from
    /// its first key. The new line is never indented, so it can't read as a continuation.
    fn insert_key(&mut self, kinds: &[Kind], header: usize, end: usize, key: &str, value: &str) {
        let body = header + 1..end;
        let last_value = body
            .clone()
            .rev()
            .find(|&i| matches!(kinds[i], Kind::Option { .. } | Kind::Continuation { .. }));
        let sep = body
            .clone()
            .find_map(|i| match kinds[i] {
                Kind::Option { delim, .. } => Some(separator(&self.lines[i].text, delim)),
                _ => None,
            })
            .unwrap_or_else(|| " = ".to_string());
        let after = last_value.unwrap_or(header);
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

    fn remove_lines(&mut self, drop: &BTreeSet<usize>) {
        let Some(last) = self.lines.len().checked_sub(1) else {
            return;
        };
        // Removing the last line must not add a trailing newline the file did not have.
        let open_end = drop.contains(&last) && self.lines[last].eol.is_empty();
        let mut i = 0;
        self.lines.retain(|_| {
            i += 1;
            !drop.contains(&(i - 1))
        });
        if open_end && let Some(l) = self.lines.last_mut() {
            l.eol = "";
        }
    }
}

fn non_empty(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

fn refusal(path: &Path, err: &IniError, line: usize) -> String {
    let why = match err {
        IniError::DuplicateSection { section } => {
            format!("the section '{section}' appears more than once")
        }
        IniError::DuplicateOption { section, option } => {
            format!("the key '{option}' appears more than once in section '{section}'")
        }
        IniError::MissingSectionHeader => format!("line {} comes before any [section]", line + 1),
        IniError::Parsing => format!(
            "line {} is not a key = value line, a comment or a [section]",
            line + 1
        ),
    };
    format!(
        "{} can't be saved: {why}, and the AWS CLI refuses such a file. Fix it by hand first.",
        path.display()
    )
}

fn validate(p: &Profile) -> Result<(), ProfileError> {
    let bad = |what: &str, why: &str| Err(ProfileError::Invalid(format!("{what} {why}")));
    let breaks_line = |s: &str| s.contains(['\n', '\r']);
    if py_trim(&p.name).is_empty() {
        return bad("profile name", "is empty");
    }
    if breaks_line(&p.name) || p.name.contains(['[', ']']) {
        return bad("profile name", "contains a newline or a bracket");
    }
    if py_trim(&p.name) != p.name {
        return bad("profile name", "starts or ends with whitespace");
    }
    if p.name == DEFAULT_SECTION {
        return bad(
            "profile name",
            "DEFAULT is reserved for values every profile inherits",
        );
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
        if py_trim(v) != v {
            return bad(what, "starts or ends with whitespace");
        }
    }
    Ok(())
}

// ---- configparser, rule for rule ----

/// Python's `str.isspace`, which is what `strip()` and the regex `\s` use. It is Rust's
/// whitespace plus the four ASCII separators U+001C to U+001F.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn py_trim(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(is_py_space)
}

/// Split on universal newlines ("\r\n", "\n" and a lone "\r"), as Python's text mode does.
fn split_lines(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (text, eol, next) = match rest.find(['\r', '\n']) {
            None => (rest, "", ""),
            Some(i) if rest[i..].starts_with("\r\n") => (&rest[..i], "\r\n", &rest[i + 2..]),
            Some(i) if rest[i..].starts_with('\r') => (&rest[..i], "\r", &rest[i + 1..]),
            Some(i) => (&rest[..i], "\n", &rest[i + 1..]),
        };
        lines.push(Line {
            text: text.to_string(),
            eol,
        });
        rest = next;
    }
    lines
}

/// What configparser takes each line to be.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// A comment, or a blank line outside any value.
    Skip,
    /// A blank line while the option on line `owner` is open. configparser adds it to the
    /// value, and the final `rstrip` drops it again unless a continuation follows.
    Blank {
        owner: usize,
    },
    Header {
        name: String,
    },
    /// `key = value` or `key: value`. `key` is lowercased, `delim` is the byte offset of the
    /// delimiter in the line.
    Option {
        key: String,
        delim: usize,
    },
    /// An indented line that extends the value of the option on line `owner`.
    Continuation {
        owner: usize,
    },
    /// A line configparser can't read, including one before the first section.
    Bogus,
}

struct Parsed {
    kinds: Vec<Kind>,
    /// The error, and the line it points at, of each read mode.
    strict_error: Option<(IniError, usize)>,
    nonstrict_error: Option<(IniError, usize)>,
}

/// `RawConfigParser._read` with its defaults: `#` and `;` full-line comments only, no inline
/// comments, `=` and `:` delimiters, empty lines allowed in values, no valueless keys.
///
/// Python stops at the first line before a section header; I keep going (marking it bogus) so
/// the lenient read used for listing profiles still sees the rest.
fn parse(lines: &[Line]) -> Parsed {
    let mut kinds = Vec::with_capacity(lines.len());
    let mut section: Option<String> = None;
    let mut open_option: Option<usize> = None;
    let mut indent_level = 0usize;
    let mut sections_seen = HashSet::new();
    let mut options_seen = HashSet::new();
    let mut missing_header = None;
    let mut first_bogus = None;
    let mut first_strict = None;

    for (i, line) in lines.iter().enumerate() {
        let text = line.text.as_str();
        let value = py_trim(text);
        if value.starts_with(['#', ';']) {
            kinds.push(Kind::Skip);
            continue;
        }
        if value.is_empty() {
            kinds.push(match (open_option, &section) {
                (Some(owner), Some(_)) => Kind::Blank { owner },
                _ => Kind::Skip,
            });
            continue;
        }
        let indent = text.chars().take_while(|&c| is_py_space(c)).count();
        if let (Some(owner), Some(_)) = (open_option, &section)
            && indent > indent_level
        {
            kinds.push(Kind::Continuation { owner });
            continue;
        }
        indent_level = indent;
        if let Some(name) = header(value) {
            if name != DEFAULT_SECTION && !sections_seen.insert(name.to_string()) {
                first_strict.get_or_insert((
                    IniError::DuplicateSection {
                        section: name.to_string(),
                    },
                    i,
                ));
            }
            section = Some(name.to_string());
            open_option = None;
            kinds.push(Kind::Header {
                name: name.to_string(),
            });
            continue;
        }
        let Some(sect) = &section else {
            missing_header.get_or_insert(i);
            kinds.push(Kind::Bogus);
            continue;
        };
        match value.find(['=', ':']) {
            Some(d) if !py_rstrip(&value[..d]).is_empty() => {
                let key = py_rstrip(&value[..d]).to_lowercase();
                if !options_seen.insert((sect.clone(), key.clone())) {
                    first_strict.get_or_insert((
                        IniError::DuplicateOption {
                            section: sect.clone(),
                            option: key.clone(),
                        },
                        i,
                    ));
                }
                let lead = text.len() - text.trim_start_matches(is_py_space).len();
                open_option = Some(i);
                kinds.push(Kind::Option {
                    key,
                    delim: lead + d,
                });
            }
            Some(_) => {
                // An empty key: Python records the error and leaves no option open.
                first_bogus.get_or_insert(i);
                open_option = None;
                kinds.push(Kind::Bogus);
            }
            None => {
                first_bogus.get_or_insert(i);
                kinds.push(Kind::Bogus);
            }
        }
    }

    // A line before any header raises at once in both modes, and nothing strict can come
    // earlier. Otherwise strict raises at the first duplicate, and both modes raise
    // ParsingError at the end for any unreadable line.
    let missing = missing_header.map(|i| (IniError::MissingSectionHeader, i));
    let bogus = first_bogus.map(|i| (IniError::Parsing, i));
    Parsed {
        kinds,
        strict_error: missing.clone().or(first_strict).or(bogus.clone()),
        nonstrict_error: missing.or(bogus),
    }
}

/// configparser's `\[(?P<header>.+)\]` matched at the start of the stripped line: `.+` is greedy,
/// so the name runs to the last `]`, and anything after it is ignored.
fn header(value: &str) -> Option<&str> {
    let rest = value.strip_prefix('[')?;
    let close = rest.rfind(']')?;
    (close > 0).then(|| &rest[..close])
}

/// The lines after option `owner` that belong to its value: continuations, and the blank lines
/// between them. Blank lines after the last continuation are left alone.
fn value_tail(kinds: &[Kind], owner: usize) -> Vec<usize> {
    let tail: Vec<usize> = (owner + 1..kinds.len())
        .take_while(|&j| {
            matches!(kinds[j], Kind::Skip)
                || matches!(kinds[j], Kind::Blank { owner: o } | Kind::Continuation { owner: o } if o == owner)
        })
        .collect();
    let Some(&last) = tail
        .iter()
        .rev()
        .find(|&&j| matches!(kinds[j], Kind::Continuation { .. }))
    else {
        return Vec::new();
    };
    tail.into_iter()
        .filter(|&j| j <= last && !matches!(kinds[j], Kind::Skip))
        .collect()
}

/// The full value of option `owner`: continuation lines joined with "\n", then right-stripped.
fn value_of(lines: &[Line], kinds: &[Kind], owner: usize) -> String {
    let Kind::Option { delim, .. } = kinds[owner] else {
        return String::new();
    };
    let mut parts = vec![py_trim(&lines[owner].text[delim + 1..])];
    for j in owner + 1..kinds.len() {
        match kinds[j] {
            Kind::Skip => {}
            Kind::Blank { owner: o } if o == owner => parts.push(""),
            Kind::Continuation { owner: o } if o == owner => parts.push(py_trim(&lines[j].text)),
            _ => break,
        }
    }
    py_rstrip(&parts.join("\n")).to_string()
}

/// Sections and items the way `RawConfigParser(strict=False)` stores them: a repeated section
/// merges into the first, and a repeated key keeps its first position but its last value.
fn data(lines: &[Line], kinds: &[Kind]) -> IniData {
    type Items = Vec<(String, usize)>;
    let mut defaults: Items = Vec::new();
    let mut sections: Vec<(String, Items)> = Vec::new();
    // None: no section yet. Some(None): DEFAULT. Some(Some(i)): sections[i].
    let mut current: Option<Option<usize>> = None;
    for (i, kind) in kinds.iter().enumerate() {
        match kind {
            Kind::Header { name } if name == DEFAULT_SECTION => current = Some(None),
            Kind::Header { name } => {
                let idx = sections
                    .iter()
                    .position(|(n, _)| n == name)
                    .unwrap_or_else(|| {
                        sections.push((name.clone(), Vec::new()));
                        sections.len() - 1
                    });
                current = Some(Some(idx));
            }
            Kind::Option { key, .. } => {
                let items = match current {
                    Some(None) => &mut defaults,
                    Some(Some(s)) => &mut sections[s].1,
                    None => continue,
                };
                match items.iter_mut().find(|(k, _)| k == key) {
                    Some(slot) => slot.1 = i,
                    None => items.push((key.clone(), i)),
                }
            }
            _ => {}
        }
    }
    let resolve = |items: &Items| -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, i)| (k.clone(), value_of(lines, kinds, *i)))
            .collect()
    };
    let defaults = resolve(&defaults);
    let sections = sections
        .iter()
        .map(|(name, items)| {
            // items(section): a copy of the defaults updated with the section's own keys.
            let own = resolve(items);
            let mut merged = defaults.clone();
            for (k, v) in own {
                match merged.iter_mut().find(|(mk, _)| *mk == k) {
                    Some(slot) => slot.1 = v,
                    None => merged.push((k, v)),
                }
            }
            (name.clone(), merged)
        })
        .collect();
    IniData { defaults, sections }
}

/// The header line of the first section called `name`, and one past its last line.
fn first_run(kinds: &[Kind], name: &str) -> Option<(usize, usize)> {
    let header = kinds
        .iter()
        .position(|k| matches!(k, Kind::Header { name: n } if n == name))?;
    let end = (header + 1..kinds.len())
        .find(|&j| matches!(kinds[j], Kind::Header { .. }))
        .unwrap_or(kinds.len());
    Some((header, end))
}

/// The exact text between the key and the value, e.g. "=", " = " or ": ".
fn separator(text: &str, delim: usize) -> String {
    let key_end = py_rstrip(&text[..delim]).len();
    let after = &text[delim + 1..];
    let value_start = delim + 1 + (after.len() - after.trim_start_matches(is_py_space).len());
    text[key_end..value_start].to_string()
}

/// Swap the value on a `key = value` line, keeping everything around it.
fn rewrite_value(text: &mut String, delim: usize, value: &str) {
    let after = &text[delim + 1..];
    let start = delim + 1 + (after.len() - after.trim_start_matches(is_py_space).len());
    let end = (delim + 1 + py_rstrip(after).len()).max(start);
    if text[start..end] != *value {
        text.replace_range(start..end, value);
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

/// Region for a profile from an AWS config file at an explicit path, resolved the way botocore
/// does it. A `profile NAME` section must shell-split into exactly two words, `[default]` is a
/// profile too, and a later section for the same profile replaces an earlier one. A file that
/// strict configparser refuses gives no region at all, as botocore gives up on it. An empty
/// region, or a nested block, counts as none.
pub fn region_from_config_file(path: &Path, profile: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let view = parse_ini(&text);
    if view.strict_error.is_some() {
        return None;
    }
    let data = view.read.ok()?;
    let mut found = None;
    for (key, items) in &data.sections {
        let name = if key.starts_with("profile") {
            match shlex_split(key) {
                Some(parts) if parts.len() == 2 => parts[1].clone(),
                _ => continue,
            }
        } else if key == "default" {
            key.clone()
        } else {
            continue;
        };
        if name == profile {
            found = Some(items);
        }
    }
    found?
        .iter()
        .find(|(k, _)| k == REGION)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty() && !v.starts_with('\n'))
}

/// Python's `shlex.split` (POSIX mode, no comments). None where it raises ValueError.
fn shlex_split(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if let Some(w) = word.take() {
                    words.push(w);
                }
            }
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        // Inside double quotes a backslash only escapes `"` and itself.
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\') => w.push(c),
                            c => {
                                w.push('\\');
                                w.push(c);
                            }
                        },
                        c => w.push(c),
                    }
                }
            }
            '\\' => word.get_or_insert_with(String::new).push(chars.next()?),
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Some(words)
}

/// Stub: the region lookup through aws-config with an explicit environment.
pub fn region_from_config_in(env: &[(&str, &str)], profile: &str) -> Option<String> {
    let _ = (env, profile);
    None
}
