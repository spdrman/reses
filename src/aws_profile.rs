//! The shared AWS credentials file (`~/.aws/credentials`, INI format).
//!
//! Loading and saving must round-trip everything we do not own: other profiles, comments,
//! blank lines and unknown keys stay exactly as they were.

use std::fmt;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Default)]
pub struct CredentialsFile {
    path: PathBuf,
}

impl CredentialsFile {
    /// `$AWS_SHARED_CREDENTIALS_FILE`, else `~/.aws/credentials`.
    pub fn default_path() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".aws/credentials")
    }

    /// A missing file loads as empty.
    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Profiles in file order.
    pub fn profiles(&self) -> Vec<Profile> {
        Vec::new()
    }

    pub fn get(&self, name: &str) -> Option<Profile> {
        self.profiles().into_iter().find(|p| p.name == name)
    }

    /// Add the profile, or replace the keys we own in an existing section of that name.
    pub fn upsert(&mut self, profile: &Profile) -> Result<(), ProfileError> {
        let _ = profile;
        Ok(())
    }

    /// Write atomically with mode 0600.
    pub fn save(&self) -> Result<(), ProfileError> {
        Ok(())
    }
}

/// Region for a profile from the AWS config file (`$AWS_CONFIG_FILE`, else `~/.aws/config`),
/// used when the credentials file has none.
pub fn region_from_config(profile: &str) -> Option<String> {
    let _ = profile;
    None
}

/// Where the credentials file lives, given the values of `$AWS_SHARED_CREDENTIALS_FILE` and
/// `$HOME`. Split out so tests can check it without touching the process environment.
pub fn credentials_path_from(
    shared_credentials_file: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    let _ = shared_credentials_file;
    PathBuf::from(home.unwrap_or_default()).join(".aws/credentials")
}

/// Where the AWS config file lives, given `$AWS_CONFIG_FILE` and `$HOME`.
pub fn config_path_from(
    config_file: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    let _ = config_file;
    PathBuf::from(home.unwrap_or_default()).join(".aws/config")
}

/// Region for a profile from an AWS config file at an explicit path.
pub fn region_from_config_file(path: &Path, profile: &str) -> Option<String> {
    let _ = (path, profile);
    None
}
