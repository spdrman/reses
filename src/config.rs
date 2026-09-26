//! reses's own settings: the default account and the saved inbox location.

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub default_profile: Option<String>,
    pub inbox: Option<Inbox>,
}

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

impl AppConfig {
    /// `$RESES_CONFIG`, else `$XDG_CONFIG_HOME/reses/config.toml`, else `~/.config/reses/config.toml`.
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

    /// A missing file loads as the default config.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
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
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Create parent directories and write atomically.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let write_err = |source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        };
        let text = toml::to_string_pretty(self)
            .map_err(|e| write_err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        crate::aws_profile::write_atomic(path, text.as_bytes(), None, None).map_err(write_err)
    }
}
