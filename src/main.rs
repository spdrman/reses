//! The `reses` binary: open the S3 inbox, or decode message files to text.
//!
//! I pick the mode from the arguments and from whether stdin and stdout are terminals, the way
//! `less` or `cat` would: with files or piped input I decode, and with neither (and a terminal on
//! both ends) I open the inbox. Everything past that choice lives in the library, so this file is
//! only argument parsing, reading the inputs and writing the output. It stays this thin because
//! tests can call the library directly, but can only reach the binary by running it.

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
    /// Print control characters as they are, even to a terminal. Without it, a terminal gets
    /// them written out visibly (`\x1b`), so a message can't send escape sequences to it.
    #[arg(long)]
    raw: bool,
    /// Open the inbox on the accounts screen instead of the saved inbox. It still starts when
    /// the saved inbox can't load (a broken settings file, a deleted bucket), so you can pick
    /// another one.
    #[arg(long)]
    accounts: bool,
}

/// The decoded text as it should reach stdout. A message is untrusted: its subject, names and
/// body can carry escape sequences, and a terminal acts on those (retitling the window,
/// writing the clipboard, redrawing the screen). So when stdout is a terminal I write every
/// control character out visibly, unless --raw asks for the bytes. A pipe or `-o` file gets
/// them untouched, since nothing there interprets them.
fn for_stdout(text: String, stdout_tty: bool, cli: &Cli) -> String {
    if !stdout_tty || cli.raw || cli.output.is_some() {
        return text;
    }
    reses::tui::text::escape_text(&text).into_owned()
}

/// The line printed for each saved attachment. Its name came from the message, so it's
/// escaped: stderr is usually the terminal.
fn saved_line(path: &std::path::Path) -> String {
    format!(
        "saved {}",
        reses::tui::text::escape(&path.display().to_string())
    )
}

/// What a run does, from the arguments and whether stdin and stdout are terminals.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Open the inbox.
    Tui,
    /// Decode the files, or stdin, to text.
    Decode,
    /// Stop with this message, because the arguments ask for something the run can't do.
    Refuse(&'static str),
}

/// Decide what this run does. Any file or piped input means decoding, whatever else is set. With
/// neither, I only open the inbox when no decode flag was given and stdout is a terminal, since a
/// flag with nothing to act on, or an inbox drawn into a pipe, is almost certainly a mistake and
/// I'd rather say so than guess.
fn mode(cli: &Cli, stdin_tty: bool, stdout_tty: bool) -> Mode {
    if cli.accounts && (!cli.files.is_empty() || !stdin_tty) {
        return Mode::Refuse("--accounts only applies to the inbox, not to decoding a message");
    }
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

/// Parse the arguments, then either hand over to the inbox or decode every input and write the
/// result to stdout or `-o`. Several inputs get a `==> name <==` banner each, like `head`.
fn main() -> anyhow::Result<()> {
    // Read this before anything spawns a thread: on Unix it fails once there are others.
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let cli = Cli::parse();

    // The inbox takes over from here, and a refusal ends the run before anything is read.
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
                if cli.accounts {
                    reses::tui::Start::Accounts
                } else {
                    reses::tui::Start::SavedInbox
                },
            );
        }
        Mode::Refuse(why) => anyhow::bail!(why),
        Mode::Decode => {}
    }

    // Read every input up front, stdin when no files were named, so a missing file fails before
    // any output is written.
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

    // Decode each input, saving its attachments on the way when asked.
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
            let paths = mail::save_attachments(data, dir)?;
            for path in &paths {
                eprintln!("{}", saved_line(path));
            }
            // Marked as downloaded on macOS; a failure there is a note, not a failed save.
            if let Some(note) = reses::tui::saved::quarantine_all(&paths) {
                eprintln!("reses: {note}");
            }
        }
    }
    // A file gets the text as it is, and stdout gets it escaped when it's a terminal.
    let out = chunks.join("\n");
    match &cli.output {
        Some(path) => {
            std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?
        }
        None => {
            let out = for_stdout(out, std::io::stdout().is_terminal(), &cli);
            std::io::stdout().write_all(out.as_bytes())?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `args` as if they followed `reses` on the command line.
    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("reses").chain(args.iter().copied()))
    }

    /// The inbox needs no input at all and a terminal on stdout.
    #[test]
    fn the_inbox_opens_only_with_no_input_and_terminals_both_ways() {
        assert_eq!(mode(&cli(&[]), true, true), Mode::Tui);
        assert!(matches!(mode(&cli(&[]), true, false), Mode::Refuse(_)));
    }

    /// Any input decodes, even alongside flags that would be refused without it.
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

    /// An OSC 52 clipboard write reaches a terminal as visible text, while tabs and newlines stay
    /// as they are.
    #[test]
    fn a_terminal_gets_control_characters_written_out() {
        let text = "Subject: \u{1b}]52;c;aGk=\u{7}hi\nbody\ttab\n".to_string();
        let escaped = for_stdout(text.clone(), true, &cli(&["m.eml"]));
        assert_eq!(escaped, "Subject: \\x1b]52;c;aGk=\\x07hi\nbody\ttab\n");
        assert!(!escaped.contains('\u{1b}'));
    }

    /// A pipe, `--raw` and `-o` all get the escape bytes untouched.
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

    /// An attachment name carrying escapes is written out visibly on stderr.
    #[test]
    fn a_saved_attachment_name_is_escaped_on_stderr() {
        let line = saved_line(std::path::Path::new("dl/evil\u{1b}]0;x\u{7}.pdf"));
        assert_eq!(line, "saved dl/evil\\x1b]0;x\\x07.pdf");
    }

    /// Each decode flag on its own, with nothing to decode, is refused rather than ignored.
    #[test]
    fn accounts_opens_the_inbox_and_refuses_to_decode() {
        assert_eq!(mode(&cli(&["--accounts"]), true, true), Mode::Tui);
        assert!(matches!(
            mode(&cli(&["--accounts", "m.eml"]), true, true),
            Mode::Refuse(_)
        ));
        assert!(matches!(
            mode(&cli(&["--accounts"]), false, true),
            Mode::Refuse(_)
        ));
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
