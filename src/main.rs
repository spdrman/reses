use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use reses::aws_profile::CredentialsFile;
use reses::config::AppConfig;
use reses::mail;

/// Read raw SES/RFC 5322 email. With files (or piped input) it prints them decoded; with
/// neither it opens the terminal inbox.
#[derive(Parser)]
#[command(name = "reses", version)]
struct Cli {
    /// Raw message files to decode.
    files: Vec<PathBuf>,
    /// Write to this file instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Show the HTML part instead of plain text.
    #[arg(long)]
    html: bool,
    /// Write attachments into this directory.
    #[arg(long, value_name = "DIR")]
    save_attachments: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.files.is_empty() && std::io::stdin().is_terminal() {
        return reses::tui::run(AppConfig::default_path(), CredentialsFile::default_path());
    }

    let mut sources: Vec<(String, Vec<u8>)> = Vec::new();
    if cli.files.is_empty() {
        let mut data = Vec::new();
        std::io::stdin().read_to_end(&mut data)?;
        sources.push(("<stdin>".into(), data));
    }
    for f in &cli.files {
        let data = std::fs::read(f).with_context(|| format!("reading {}", f.display()))?;
        sources.push((f.display().to_string(), data));
    }

    let many = sources.len() > 1;
    let mut chunks = Vec::new();
    for (name, data) in &sources {
        let text = mail::format_message(data, cli.html);
        chunks.push(if many {
            format!("==> {name} <==\n{text}")
        } else {
            text
        });
        if let Some(dir) = &cli.save_attachments {
            for path in mail::save_attachments(data, dir)? {
                eprintln!("saved {}", path.display());
            }
        }
    }
    let out = chunks.join("\n");
    match &cli.output {
        Some(path) => {
            std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?
        }
        None => std::io::stdout().write_all(out.as_bytes())?,
    }
    Ok(())
}
