//! AWS profiles for reses: listing them, their regions, and adding a new one to
//! `~/.aws/credentials`.
//!
//! I read everything through aws-config's own profile loader, the same one the S3 client connects
//! with, so reses lists exactly the profiles and regions the SDK will use (and honours
//! `AWS_SHARED_CREDENTIALS_FILE`, `AWS_CONFIG_FILE` and `~` the way the SDK does).
//!
//! Writing is the part no crate does for me. The file is the user's, so a new profile has to go in
//! without disturbing a byte of anything else, and the result has to stay readable by the AWS CLI
//! too, which reads it through Python's configparser. I keep the file as its original lines and
//! only edit the lines holding the keys reses owns. Before writing I check the result against
//! configparser's rules and against aws-config, and refuse anything either would reject.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use aws_config::profile::ProfileSet;
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_types::os_shim_internal::{Env, Fs};

const KEY_ID: &str = "aws_access_key_id";
const SECRET: &str = "aws_secret_access_key";
const TOKEN: &str = "aws_session_token";
/// The old name for the session token. botocore still reads it, and before `aws_session_token`.
const LEGACY_TOKEN: &str = "aws_security_token";
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
    /// I print the name, key id and region, and stand-ins for the secret and token.
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
    /// I print the path and the profile names, never a line of the file.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self.profiles().into_iter().map(|p| p.name).collect();
        f.debug_struct("CredentialsFile")
            .field("path", &self.path)
            .field("profiles", &names)
            .finish()
    }
}

impl CredentialsFile {
    /// `$AWS_SHARED_CREDENTIALS_FILE`, else `~/.aws/credentials`, the file aws-config reads.
    pub fn default_path() -> PathBuf {
        credentials_path_from(
            std::env::var_os("AWS_SHARED_CREDENTIALS_FILE"),
            std::env::var_os("HOME"),
        )
    }

    /// Read the file at `path`. A missing file loads as empty. A file aws-config can't parse is
    /// an `Invalid` error naming the path, because the S3 client couldn't use any profile in it.
    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        // A missing file is simply no profiles yet.
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
        // I parse it once here so a broken file shows up now rather than as an empty list.
        if let Err(e) = sdk_credentials(&text) {
            return Err(ProfileError::Invalid(format!("{}: {e}", path.display())));
        }
        Ok(Self {
            path: path.to_path_buf(),
            lines: split_lines(&text),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Profiles in file order, as aws-config reads them: every profile with a non-empty access
    /// key id and secret. The session token is `aws_security_token` when that key is present and
    /// `aws_session_token` otherwise, which is botocore's order.
    pub fn profiles(&self) -> Vec<Profile> {
        let Ok(set) = sdk_credentials(&self.text()) else {
            return Vec::new();
        };
        // Pull the keys reses cares about out of each profile the SDK found.
        let mut out: Vec<Profile> = set
            .profiles()
            .filter_map(|name| {
                let p = set.get_profile(name)?;
                let value = |key: &str| p.get(key).filter(|v| !v.is_empty()).map(str::to_string);
                // botocore takes the first of these keys that is present, even when it's empty.
                let token = match p.get(LEGACY_TOKEN) {
                    Some(_) => value(LEGACY_TOKEN),
                    None => value(TOKEN),
                };
                Some(Profile {
                    name: name.to_string(),
                    access_key_id: value(KEY_ID)?,
                    secret_access_key: value(SECRET)?,
                    session_token: token,
                    region: value(REGION),
                })
            })
            .collect();
        // The SDK hands profiles back in hash order, so I put them back in file order.
        let kinds = parse(&self.lines).kinds;
        out.sort_by_cached_key(|p| (first_run(&kinds, &p.name).map(|(h, _)| h), p.name.clone()));
        out
    }

    pub fn get(&self, name: &str) -> Option<Profile> {
        self.profiles().into_iter().find(|p| p.name == name)
    }

    /// Whether the file has a section called `name`, complete profile or not, so the caller can
    /// ask before writing keys into something the user already has. False for `DEFAULT`, which
    /// reses never writes.
    pub fn has_section(&self, name: &str) -> bool {
        name != DEFAULT_SECTION && first_run(&parse(&self.lines).kinds, name).is_some()
    }

    /// Add the profile, or replace the keys reses owns in an existing section of that name.
    ///
    /// Only aws_access_key_id, aws_secret_access_key, aws_session_token, aws_security_token and
    /// region are touched, together with any continuation lines under them. The token always
    /// goes in as aws_session_token, and an empty or `None` token or region removes that key.
    pub fn upsert(&mut self, profile: &Profile) -> Result<(), ProfileError> {
        validate(profile)?;
        let wanted: [(&str, Option<&str>); 5] = [
            (KEY_ID, Some(&profile.access_key_id)),
            (SECRET, Some(&profile.secret_access_key)),
            (TOKEN, non_empty(&profile.session_token)),
            (LEGACY_TOKEN, None),
            (REGION, non_empty(&profile.region)),
        ];
        // A new name gets a section of its own at the end of the file.
        if !self.has_section(&profile.name) {
            self.append_section(&profile.name, &wanted);
            return Ok(());
        }
        // An existing one gets each owned key set, rewritten or removed in place.
        for (key, value) in wanted {
            self.set_key(&profile.name, key, value);
        }
        Ok(())
    }

    /// Write atomically with mode 0600. I refuse to write anything configparser would reject (a
    /// repeated section, say), since the AWS CLI would then refuse the whole file, or anything
    /// aws-config couldn't read back.
    pub fn save(&self) -> Result<(), ProfileError> {
        let text = self.text();
        // Check the result the way the AWS CLI will read it...
        if let Some((err, line)) = parse(&self.lines).strict_error {
            return Err(ProfileError::Invalid(refusal(&self.path, &err, line)));
        }
        // ...and the way reses itself will.
        if let Err(e) = sdk_credentials(&text) {
            return Err(ProfileError::Invalid(format!(
                "{} can't be saved: aws-config couldn't read the result ({e})",
                self.path.display()
            )));
        }
        write_atomic(&self.path, text.as_bytes(), Some(0o600), Some(0o700)).map_err(|source| {
            ProfileError::Write {
                path: self.path.clone(),
                source,
            }
        })
    }

    /// The file as it stands, every line with its own ending.
    fn text(&self) -> String {
        self.lines
            .iter()
            .flat_map(|l| [l.text.as_str(), l.eol])
            .collect()
    }

    /// The line ending the file already uses, "\n" for a new file.
    fn eol(&self) -> &'static str {
        self.lines
            .iter()
            .map(|l| l.eol)
            .find(|e| !e.is_empty())
            .unwrap_or("\n")
    }

    /// Append `[name]` and its keys, after a blank line if the file doesn't end in one.
    fn append_section(&mut self, name: &str, wanted: &[(&str, Option<&str>)]) {
        let eol = self.eol();
        // Close off the last line first, so the header starts on a line of its own.
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
        // Rewrite the first copy in place, or add the key when the section has none.
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
        // Everything else that sets this key goes, along with its continuation lines.
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

    /// Drop the lines at `drop`, keeping the file's "no newline at the end" if it had one.
    fn remove_lines(&mut self, drop: &BTreeSet<usize>) {
        let Some(last) = self.lines.len().checked_sub(1) else {
            return;
        };
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

/// `Some(v)` only for a non-empty value, so an empty token or region means "remove it".
fn non_empty(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

// ---- reading through aws-config ----

/// Parse `text` as a credentials file with aws-config. It's all in memory, so no environment or
/// filesystem gets involved.
fn sdk_credentials(text: &str) -> Result<ProfileSet, String> {
    let files = EnvConfigFiles::builder()
        .with_contents(EnvConfigFileKind::Credentials, text)
        .build();
    sdk_load(&files, &Env::from_slice(&[]), &Fs::from_slice(&[]))
}

/// Run aws-config's profile loader. It's async only in name here: it reads files with blocking
/// calls and never waits, so pollster just drives it to the end on this thread.
fn sdk_load(files: &EnvConfigFiles, env: &Env, fs: &Fs) -> Result<ProfileSet, String> {
    pollster::block_on(aws_config::profile::load(fs, env, files, None)).map_err(|e| e.to_string())
}

/// Region for a profile from the AWS config file (`$AWS_CONFIG_FILE`, else `~/.aws/config`),
/// used when the credentials file has none.
pub fn region_from_config(profile: &str) -> Option<String> {
    region_from_default_config(&Env::real(), profile)
}

/// The same lookup with the environment given as name/value pairs rather than read from the
/// process, so tests can check `AWS_CONFIG_FILE` and `~` handling without touching either.
pub fn region_from_config_in(env: &[(&str, &str)], profile: &str) -> Option<String> {
    region_from_default_config(&Env::from_slice(env), profile)
}

/// aws-config finds the config file itself here, from `AWS_CONFIG_FILE` or `~/.aws/config`.
fn region_from_default_config(env: &Env, profile: &str) -> Option<String> {
    let files = EnvConfigFiles::builder()
        .include_default_config_file(true)
        .build();
    region_of(&sdk_load(&files, env, &Fs::real()).ok()?, profile)
}

/// Region for a profile from an AWS config file at an explicit path, read by aws-config: its
/// sections are `[default]` and `[profile NAME]`, and `[profile default]` wins over `[default]`.
/// A missing or unreadable file gives no region.
pub fn region_from_config_file(path: &Path, profile: &str) -> Option<String> {
    let files = EnvConfigFiles::builder()
        .with_file(EnvConfigFileKind::Config, path)
        .build();
    region_of(
        &sdk_load(&files, &Env::from_slice(&[]), &Fs::real()).ok()?,
        profile,
    )
}

/// The non-empty `region` of one profile in a loaded set.
fn region_of(set: &ProfileSet, profile: &str) -> Option<String> {
    set.get_profile(profile)?
        .get(REGION)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
}

/// Where the credentials file lives, given the values of `$AWS_SHARED_CREDENTIALS_FILE` and
/// `$HOME`. Split out so tests can check it without touching the process environment.
pub fn credentials_path_from(
    shared_credentials_file: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    resolve_path(shared_credentials_file, home, ".aws/credentials")
}

/// Where the AWS config file lives, given `$AWS_CONFIG_FILE` and `$HOME`.
pub fn config_path_from(config_file: Option<OsString>, home: Option<OsString>) -> PathBuf {
    resolve_path(config_file, home, ".aws/config")
}

/// The override when it's set, with a leading `~` swapped for the home directory as aws-config
/// does it, else `default` under the home directory. An empty override counts as unset.
fn resolve_path(var: Option<OsString>, home: Option<OsString>, default: &str) -> PathBuf {
    let home = PathBuf::from(home.unwrap_or_default());
    match var.filter(|v| !v.is_empty()).map(PathBuf::from) {
        Some(p) => match p.strip_prefix("~") {
            Ok(rest) => home.join(rest),
            Err(_) => p,
        },
        None => home.join(default),
    }
}

// ---- checking a new profile ----

/// What aws-config accepts in a profile name. Anything else it skips with a warning, so a
/// profile saved under such a name would silently never show up.
fn sdk_identifier(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-/.%@:+".contains(c)
}

/// Refuse a profile whose name or values would break the file for either reader. Values are
/// named in the message but never echoed, since they are secrets.
fn validate(p: &Profile) -> Result<(), ProfileError> {
    let bad = |what: &str, why: &str| Err(ProfileError::Invalid(format!("{what} {why}")));
    // The name has to be one aws-config reads, and not configparser's special DEFAULT.
    if p.name.is_empty() {
        return bad("profile name", "is empty");
    }
    if !p.name.chars().all(sdk_identifier) {
        return bad(
            "profile name",
            "may only use letters, digits and _ - / . % @ : +",
        );
    }
    if p.name == DEFAULT_SECTION {
        return bad(
            "profile name",
            "DEFAULT is reserved for values every profile inherits",
        );
    }
    // Keys, secrets, tokens and regions never contain whitespace, and aws-config would cut a
    // value short at " #" or " ;", so any whitespace at all is a mistake.
    let values: [(&str, Option<&str>, bool); 4] = [
        ("access key id", Some(&p.access_key_id), true),
        ("secret access key", Some(&p.secret_access_key), true),
        ("session token", p.session_token.as_deref(), false),
        ("region", p.region.as_deref(), false),
    ];
    for (what, value, required) in values {
        match value {
            Some("") if required => return bad(what, "is empty"),
            Some(v) if v.chars().any(is_py_space) => return bad(what, "contains whitespace"),
            _ => {}
        }
    }
    Ok(())
}

/// Why configparser refuses a file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IniError {
    /// A line that isn't blank or a comment comes before the first section header.
    MissingSectionHeader,
    /// A line that is none of header, `key = value`, continuation, comment or blank.
    Parsing,
    /// The same section header twice.
    DuplicateSection { section: String },
    /// The same key twice in one section.
    DuplicateOption { section: String, option: String },
}

/// The message for a save refused because of `err` on line `line` (0-based).
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

// ---- configparser's reading of the lines, for the writer ----

/// Python's `str.isspace`, which is what `strip()` and the regex `\s` use. It is Rust's
/// whitespace plus the four ASCII separators U+001C to U+001F.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `str.strip()`.
fn py_trim(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python's `str.rstrip()`.
fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(is_py_space)
}

/// Split on universal newlines ("\r\n", "\n" and a lone "\r"), as Python's text mode does, and
/// remember each line's own ending.
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

/// Each line's kind, and the error strict configparser raises with the line it points at.
struct Parsed {
    kinds: Vec<Kind>,
    strict_error: Option<(IniError, usize)>,
}

/// `RawConfigParser._read` with its defaults: `#` and `;` full-line comments only, no inline
/// comments, `=` and `:` delimiters, empty lines allowed in values, no valueless keys. Python
/// stops at the first line before a section header; I keep going, marking it bogus, so the
/// writer still knows where everything else is.
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
        // Full-line comments never touch the parser's state.
        if value.starts_with(['#', ';']) {
            kinds.push(Kind::Skip);
            continue;
        }
        // A blank line joins an open value, or is nothing.
        if value.is_empty() {
            kinds.push(match (open_option, &section) {
                (Some(owner), Some(_)) => Kind::Blank { owner },
                _ => Kind::Skip,
            });
            continue;
        }
        // Indented deeper than the open option's line: part of its value.
        let indent = text.chars().take_while(|&c| is_py_space(c)).count();
        if let (Some(owner), Some(_)) = (open_option, &section)
            && indent > indent_level
        {
            kinds.push(Kind::Continuation { owner });
            continue;
        }
        indent_level = indent;
        // A section header, which strict mode refuses to see twice.
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
        // Anything else before the first header is an error in both modes.
        let Some(sect) = &section else {
            missing_header.get_or_insert(i);
            kinds.push(Kind::Bogus);
            continue;
        };
        // An option line, split at the first `=` or `:`.
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

    // A line before any header raises at once, and nothing strict can come earlier. Otherwise
    // strict raises at the first duplicate, and ParsingError at the end for any unreadable line.
    let missing = missing_header.map(|i| (IniError::MissingSectionHeader, i));
    let bogus = first_bogus.map(|i| (IniError::Parsing, i));
    Parsed {
        kinds,
        strict_error: missing.or(first_strict).or(bogus),
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

/// The header line of the first section called `name`, and one past its last line. I compare
/// names trimmed of spaces and tabs, as aws-config does, so `[ work ]` is the section `work`.
fn first_run(kinds: &[Kind], name: &str) -> Option<(usize, usize)> {
    let header = kinds.iter().position(
        |k| matches!(k, Kind::Header { name: n } if n.trim_matches([' ', '\t']) == name),
    )?;
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

// ---- writing ----

/// Write `bytes` to `path` so a reader never sees half a file. tempfile makes the new file in the
/// same directory (mode 0600, removed again if anything fails), and persisting it is a rename over
/// the old one. I create missing parent directories with `dir_mode` and give the file `file_mode`
/// (both Unix only). A symlink at `path` is followed, so the file it points at is the one replaced.
pub(crate) fn write_atomic(
    path: &Path,
    bytes: &[u8],
    file_mode: Option<u32>,
    dir_mode: Option<u32>,
) -> io::Result<()> {
    // Replace what a symlink points at, not the link itself.
    let target = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    create_dirs(&dir, dir_mode)?;

    // Fill the temp file and get it onto the disk before it takes the real name.
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    #[cfg(unix)]
    if let Some(mode) = file_mode {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = file_mode;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&target).map_err(|e| e.error)?;

    // Make the rename itself durable. Best effort: not every filesystem allows it.
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Create `dir` and any missing parents, with `mode` on the ones I create (Unix only).
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
