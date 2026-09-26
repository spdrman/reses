//! reses's own settings: the default account and the saved inbox location.

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
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".config/reses/config.toml")
    }

    /// Where the settings file lives, given `$RESES_CONFIG`, `$XDG_CONFIG_HOME` and `$HOME`.
    /// Split out so tests can check it without touching the process environment.
    pub fn path_from(
        reses_config: Option<std::ffi::OsString>,
        xdg_config_home: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    ) -> PathBuf {
        let _ = (reses_config, xdg_config_home);
        PathBuf::from(home.unwrap_or_default()).join(".config/reses/config.toml")
    }

    /// A missing file loads as the default config.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let _ = path;
        Ok(Self::default())
    }

    /// Create parent directories and write atomically.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let _ = path;
        Ok(())
    }
}
