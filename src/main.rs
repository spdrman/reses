use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use reses::aws_profile::CredentialsFile;
use reses::config::AppConfig;
use reses::mail;

/// A terminal inbox for raw SES email stored in S3. Run it with no arguments in a terminal to
/// open the inbox; give it message files (or pipe one in) to decode them instead.
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
    /// Stub for the red tests.
    #[arg(long)]
    raw: bool,
}

/// Stub for the red tests: everything goes out as it is.
fn for_stdout(text: String, stdout_tty: bool, cli: &Cli) -> String {
    let _ = (stdout_tty, cli.raw);
    text
}

/// What a run does, from the arguments and whether stdin and stdout are terminals.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Tui,
    Decode,
    Refuse(&'static str),
}

fn mode(cli: &Cli, stdin_tty: bool, stdout_tty: bool) -> Mode {
    if !cli.files.is_empty() || !stdin_tty {
        return Mode::Decode;
    }
    // No files and nothing piped in: that's only the inbox if nothing asked for decoding.
    if cli.output.is_some() || cli.html || cli.save_attachments.is_some() {
        return Mode::Refuse(
            "nothing to decode: give a message file or pipe one in (-o, --html and \
             --save-attachments only apply to decoding)",
        );
    }
    if !stdout_tty {
        return Mode::Refuse(
            "the inbox needs a terminal on stdout; give a message file to decode instead",
        );
    }
    Mode::Tui
}

fn main() -> anyhow::Result<()> {
    // Read this before anything spawns a thread: on Unix it fails once there are others.
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let cli = Cli::parse();

    match mode(
        &cli,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        Mode::Tui => {
            return reses::tui::run(
                AppConfig::default_path(),
                CredentialsFile::default_path(),
                local_offset,
            );
        }
        Mode::Refuse(why) => anyhow::bail!(why),
        Mode::Decode => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("reses").chain(args.iter().copied()))
    }

    #[test]
    fn the_inbox_opens_only_with_no_input_and_terminals_both_ways() {
        assert_eq!(mode(&cli(&[]), true, true), Mode::Tui);
        assert!(matches!(mode(&cli(&[]), true, false), Mode::Refuse(_)));
    }

    #[test]
    fn piped_input_or_files_decode() {
        assert_eq!(mode(&cli(&[]), false, true), Mode::Decode);
        assert_eq!(mode(&cli(&["--html"]), false, true), Mode::Decode);
        assert_eq!(mode(&cli(&["--html"]), false, false), Mode::Decode);
        assert_eq!(mode(&cli(&["m.eml"]), true, true), Mode::Decode);
        assert_eq!(
            mode(&cli(&["-o", "out.txt", "m.eml"]), true, true),
            Mode::Decode
        );
    }

    #[test]
    fn a_terminal_gets_control_characters_written_out() {
        let text = "Subject: \u{1b}]52;c;aGk=\u{7}hi\nbody\ttab\n".to_string();
        let escaped = for_stdout(text.clone(), true, &cli(&["m.eml"]));
        assert_eq!(escaped, "Subject: \\x1b]52;c;aGk=\\x07hi\nbody\ttab\n");
        assert!(!escaped.contains('\u{1b}'));
    }

    #[test]
    fn pipes_files_and_raw_get_the_bytes_as_they_are() {
        let text = "a \u{1b}[2J b\n".to_string();
        assert_eq!(for_stdout(text.clone(), false, &cli(&["m.eml"])), text);
        assert_eq!(
            for_stdout(text.clone(), true, &cli(&["--raw", "m.eml"])),
            text
        );
        assert_eq!(
            for_stdout(text.clone(), true, &cli(&["-o", "out.txt", "m.eml"])),
            text
        );
    }

    #[test]
    fn decode_flags_with_no_input_are_an_error() {
        for args in [
            &["--html"][..],
            &["-o", "out.txt"][..],
            &["--save-attachments", "dir"][..],
        ] {
            assert!(
                matches!(mode(&cli(args), true, true), Mode::Refuse(_)),
                "{args:?}"
            );
        }
    }
}
