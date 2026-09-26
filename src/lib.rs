//! reses: read raw SES/RFC 5322 email, either one file at a time on the command line or as a
//! terminal inbox backed by an S3 bucket.
//!
//! This is the library half of the crate, and `main.rs` is only a thin front end over it. I keep
//! the real work here so the integration tests under `tests/` can drive the same code the binary
//! runs: `mail` decodes messages, `s3` talks to the bucket, `aws_profile` and `config` hold the
//! user's settings, and `tui` puts them together as the inbox.

pub mod aws_profile;
pub mod config;
pub mod mail;
pub mod s3;
pub mod tui;
