//! reses's own settings: the default account, saved inbox and message temporary directory.
//!
//! They live in one small TOML file (see [`AppConfig::default_path`]) that serde reads and
//! writes whole. I keep them apart from the AWS files on purpose: those belong to the user and
//! the AWS tools, while this file is reses's alone, so I can rewrite it freely. Invalid paths
//! and inboxes are caught on load and save, before they can break startup or S3 browsing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the inbox lives: a profile from the credentials file plus an S3 bucket and prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbox {
    pub profile: String,
    pub bucket: String,
    /// Folder prefix inside the bucket, "" for the bucket root, otherwise ending in '/'.
    pub prefix: String,
    /// Bucket region, remembered so startup does not need to discover it again.
    pub region: Option<String>,
}

/// Everything in the settings file. All fields are optional so a first run uses the OS
/// temporary directory and starts on the account picker until an inbox is saved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    /// The account the accounts screen marks as default and starts on.
    pub default_profile: Option<String>,
    /// The saved inbox location, if the user has picked one.
    pub inbox: Option<Inbox>,
    /// Parent for private opened-message directories, or the OS default when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<PathBuf>,
}

/// Why the settings file couldn't be read or written. Each variant names the path, so the
/// message alone tells the user which file to look at.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl Inbox {
    /// Check the parts S3 cares about: a non-empty prefix ends in '/', and the bucket is a name
    /// S3 could have. I accept the legacy us-east-1 names too (capitals and underscores, up to
    /// 255 characters), because reses saves whatever ListBuckets returned and an older bucket
    /// that refused to load would lock reses out at startup.
    pub fn validate(&self) -> Result<(), String> {
        // The prefix is a folder, so it has to end where a folder does.
        if !self.prefix.is_empty() && !self.prefix.ends_with('/') {
            return Err(format!(
                "inbox prefix {:?} must be empty or end in '/'",
                self.prefix
            ));
        }
        // The bucket name, by the loosest rules S3 has ever used.
        let b = &self.bucket;
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
        let ok = (3..=255).contains(&b.len())
            && b.chars().all(allowed)
            && b.starts_with(|c: char| c.is_ascii_alphanumeric());
        if !ok {
            return Err(format!("inbox bucket {b:?} is not a valid S3 bucket name"));
        }
        Ok(())
    }
}

impl AppConfig {
    /// Where the settings file lives for this process: `$RESES_CONFIG`, else
    /// `$XDG_CONFIG_HOME/reses/config.toml`, else `~/.config/reses/config.toml`.
    pub fn default_path() -> PathBuf {
        Self::path_from(
            std::env::var_os("RESES_CONFIG"),
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
    }

    /// Where the settings file lives, given `$RESES_CONFIG`, `$XDG_CONFIG_HOME` and `$HOME`.
    /// Split out so tests can check it without touching the process environment. Empty values
    /// count as unset, and so does a relative `$XDG_CONFIG_HOME`, which the XDG spec says to
    /// ignore.
    pub fn path_from(
        reses_config: Option<OsString>,
        xdg_config_home: Option<OsString>,
        home: Option<OsString>,
    ) -> PathBuf {
        if let Some(p) = reses_config.filter(|p| !p.is_empty()) {
            return PathBuf::from(p);
        }
        if let Some(xdg) = xdg_config_home
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            return xdg.join("reses/config.toml");
        }
        PathBuf::from(home.unwrap_or_default()).join(".config/reses/config.toml")
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(inbox) = &self.inbox {
            inbox.validate()?;
        }
        if let Some(dir) = &self.temp_dir
            && !dir.is_absolute()
        {
            return Err(format!(
                "message temporary directory {} must be absolute",
                dir.display()
            ));
        }
        Ok(())
    }

    /// Read the settings at `path`. A missing file loads as the default config, and a saved
    /// inbox that fails [`Inbox::validate`] is a parse error, so a hand-edited mistake is caught
    /// here rather than as a confusing S3 failure later.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        // A missing file just means nothing is saved yet.
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        // Bad TOML and a bad inbox both come back as a parse error naming the file.
        let parse_err = |message| ConfigError::Parse {
            path: path.to_path_buf(),
            message,
        };
        let config: Self = toml::from_str(&text).map_err(|e| parse_err(e.to_string()))?;
        config.validate().map_err(parse_err)?;
        Ok(config)
    }

    /// Write the settings to `path`, creating parent directories and replacing the file
    /// atomically, so a crash mid-save leaves the old settings rather than half of the new ones.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let write_err = |source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        };
        // Writing an invalid setting would stop reses from starting on the next run.
        self.validate()
            .map_err(|m| write_err(std::io::Error::new(std::io::ErrorKind::InvalidInput, m)))?;
        // Serialise, then hand the bytes to the same atomic writer the credentials file uses.
        let text = toml::to_string_pretty(self)
            .map_err(|e| write_err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        crate::aws_profile::write_atomic(path, text.as_bytes(), None, None).map_err(write_err)
    }
}
