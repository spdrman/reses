# reses

Converts a raw stored email (the RFC 5322 text Amazon SES drops into S3, or any `.eml`) into
something readable: From, Reply-To, To, Cc, Bcc, Date, Subject, Message-ID, attachments, then the
message body.

```
reses FILE [FILE ...]            print each message
reses FILE -o out.txt            write to a file
reses FILE --html                show the HTML part instead of plain text
reses FILE --save-attachments D  write attachments into D
cat FILE | reses                 read from stdin
```

Bcc never appears in a delivered message's headers... so I work it out from the envelope
recipients (`Delivered-To`, `X-Original-To`, and the `for <addr>` in `Received`) that aren't
already in To or Cc. It's standard library only, no dependencies.

## Install

```
ln -s "$PWD/reses.py" ~/.local/bin/reses
```

`~/.local/bin` has to be on your PATH (`export PATH="$HOME/.local/bin:$PATH"` in `~/.zshrc`).

## Tests

```
python3 -m unittest -v
```
