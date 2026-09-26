//! reses: read raw SES/RFC 5322 email, either one file at a time on the command line or as a
//! terminal inbox backed by an S3 bucket.

pub mod aws_profile;
pub mod config;
pub mod mail;
pub mod s3;
pub mod tui;
