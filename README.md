<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/reSES-logo-light.svg">
    <img src="assets/brand/reSES-logo.svg" alt="re:SES" width="300">
  </picture>
</h1>

<p align="center">
  <a href="https://github.com/spdrman/reses/actions/workflows/ci.yml"><img src="https://github.com/spdrman/reses/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/spdrman/reses/actions/workflows/release.yml"><img src="https://github.com/spdrman/reses/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="https://github.com/spdrman/reses/releases/latest"><img src="https://img.shields.io/github/v/release/spdrman/reses" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/spdrman/reses" alt="License"></a>
</p>

![reses in a terminal: picking an AWS profile, browsing into the mail-inbound bucket where stored emails are marked, saving the folder as the inbox, then opening and scrolling a message from the From, Subject, Date and Size list](docs/demo.gif)

<sub>Enter connects and opens, `i` saves the inbox, Enter opens a message, `q` goes back. All the mail is synthetic.</sub>

## Install

**macOS** (Apple Silicon), with Homebrew:

```
brew install spdrman/reses/reses
```

**Debian and Ubuntu** (amd64 and arm64), with apt:

```
curl -fsSL https://raw.githubusercontent.com/spdrman/reses/main/install.sh | sh
```

The installer fetches the latest release's `.deb`, checks it against the release's `SHA256SUMS`, and installs it with apt.

| | Homebrew | apt |
|---|---|---|
| Upgrade | `brew upgrade reses` | run the install command again (there's no apt repository, so `apt upgrade` won't update it) |
| Uninstall | `brew uninstall reses` | `sudo apt remove reses` |

Homebrew works on Linux too, with the same `brew install` command. Uninstalling leaves your settings (`~/.config/reses`) and your AWS credentials alone.

Binaries for other Linux systems (static, x86_64 and arm64) are on the [releases page](https://github.com/spdrman/reses/releases).

A terminal inbox for the raw email that Amazon SES stores in S3.

SES can drop every incoming message into an S3 bucket, but what lands there is the raw RFC 5322 text: pages of `Received:`, DKIM and ARC headers, MIME boundaries and base64. Run `reses` and it opens an inbox over the bucket, so you can read and delete that mail without downloading anything by hand... and if you do have a stored message on disk, it can decode that too.

Rust 1.98+ is needed to build it, but you never need Rust installed, since every build runs in Docker on a remote x86_64 host (see [CONTRIBUTING.md](CONTRIBUTING.md) for `RESES_NAS`).

## Usage

```
reses                            open the inbox (in a terminal, this is all you need)
reses FILE [FILE ...]            optional: decode stored messages instead
```

## The inbox

```
reses
```

No file or path is needed.

1. **Pick an account.** It lists the profiles in `~/.aws/credentials`. `a` adds one: the keys you type are written back to that file in the standard AWS format. Other profiles, comments and layout stay as they were, and the file is kept at mode 0600. `d` makes a profile the default.
2. **Find the mail.** Browse buckets and folders. Objects that are stored email get marked as you scroll, and `s` searches down from the current folder for the folders that hold email.
3. **Save the inbox.** `i` saves the current bucket and folder. From then on, `reses` opens straight into it.

The inbox shows From, Subject, Date and Size, newest first. Enter opens a message, `d` deletes it from S3 after you confirm with `y`, `/` filters, `r` refreshes, and `u` goes back to the accounts. In a message, `h` switches to the HTML part, `w` saves the text and `a` saves the attachments, both into `~/Downloads`.

Settings live in `~/.config/reses/config.toml` (`$RESES_CONFIG` or `$XDG_CONFIG_HOME` move it). `$AWS_SHARED_CREDENTIALS_FILE` and `$AWS_CONFIG_FILE` are honoured the way the AWS CLI honours them.

The header shows the logo as an image in iTerm2, WezTerm, Ghostty, kitty, foot and mlterm, and as a styled `re:SES` everywhere else. It goes by the environment and never asks the terminal anything, so a terminal that doesn't answer can't swallow your keys. `RESES_LOGO=text` always uses the text, and `RESES_LOGO=image` asks the terminal what it supports, for trying an unlisted one (a terminal that never replies can then eat keypresses until you quit).

## Decode a stored message (optional)

If you have a raw message file, for example one downloaded from the bucket, reses can print it readably instead of opening the inbox:

```
reses FILE [FILE ...]            print each message
reses FILE -o out.txt            write to a file
reses FILE --html                show the HTML part instead of plain text
reses FILE --save-attachments D  write attachments into D
cat FILE | reses                 read from stdin
```

```
From: Alice <alice@example.com>
Reply-To:
To: bob@example.org
Cc:
Bcc: hidden@example.org
Date: Fri, 25 Sep 2026 17:01:31 -0700
Subject: Hello there
Message-ID: <id-1@example.com>
Message:

plain body
```

Bcc never survives delivery as a header, so I work it out from the envelope recipients (`Delivered-To`, `X-Original-To`, and the `for <addr>` in `Received`) that aren't already in To or Cc.

The expected output for each test message lives in `tests/fixtures/mail/` next to it.

## Build and install

Everything runs in Docker on a remote x86_64 host through `scripts/ci-docker.sh`, and the Makefile wraps it. Set `RESES_NAS=user@host` to your own Docker host first; [CONTRIBUTING.md](CONTRIBUTING.md) says what it needs.

```
make gate          fmt, clippy, check, docs, tests, cargo deny and a static musl build
make integration   the S3 tests, against a throwaway MinIO
make darwin        cross-build the macOS binary into dist/ and check it against the goldens
make install       make darwin, then link ~/.local/bin/reses to dist/reses
make demo          re-record docs/demo.gif against a throwaway MinIO, failing unless the checked snapshots show the inbox
```

`~/.local/bin` has to be on your PATH. If something is already there under that name, `make install` leaves it alone and tells you to move it aside first.

On macOS, never install a new build by `cp` over an existing copy. Once a binary has run, overwriting its file in place while any process holds it open (a `reses` that is still running, for one) makes macOS kill it on every later launch, silently, with exit 137. `make install` links to `dist/reses`, and every build is renamed into place on a new file, so it never hits this. If you copy a binary somewhere yourself, copy to a new name and `mv` it over the old one. See #15.

## License

MIT, see [LICENSE](LICENSE).
