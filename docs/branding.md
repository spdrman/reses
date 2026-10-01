# re:SES branding: Tidewater

This is how re:SES looks wherever it draws something bigger than a terminal: the HTML reader that
`h` opens in the browser, and anything else that needs the brand. It's calm and legible, and
quietly blue. The mail is the loud part, so everything around it stays out of its way.

The sole logo asset is [`assets/brand/reSES-logo.svg`](../assets/brand/reSES-logo.svg). Its
`#cubes` and `#wordmark` groups let a consumer split the mark without duplicating it; the default
render shows the complete vertical logo and adapts its outlined wordmark to light or dark colour
schemes. The terminal build composes those same groups into the horizontal raster required by
terminal image protocols. The text-mode fallback draws the cubes as `▄▀▄` in the three cube blues.

## Colour

| Name | Hex | Use |
|---|---|---|
| Ink | `#0E1B2C` | body text and headings |
| Slate | `#4A5A6E` | labels, secondary text (6.8:1 on Paper) |
| Paper | `#F7F9FC` | the page ground |
| Card | `#FFFFFF` | the header, footer and message cards |
| Line | `#DDE5EF` | card borders and dividers |
| Tint | `#E8F3FD` | avatar ground, the "blocked" pill |
| Shallows | `#2EA8F2` | the top cube, the colon in the wordmark |
| Current | `#2A8FE9` | the lower left cube |
| Channel | `#1D78DE` | the lower right cube, icons |
| Deep | `#1462CF` | links and link-coloured text (5.4:1 on Card) |
| Pass | `#0B6E62` on `#E6F4F1` | a check that passed |
| Caution | `#A4520A` on `#FDF1E6` | softfail, neutral, none |
| Fail | `#B42318` on `#FDECEA` | a check that failed |

The cube blues come straight from the logo's faces and are for marks, never for text: they don't
reach 4.5:1 on white. Text that needs to be blue uses Deep. A status never relies on colour
alone: it also carries a word (pass, fail) and an icon.

The reader declares both light and dark colour schemes and follows the browser's
`prefers-color-scheme` setting automatically. Dark mode uses the same semantic palette with
lighter text and links on navy paper and cards. The sender's HTML remains isolated and untouched;
browser-default colours inside it follow the active scheme, while colours authored by the sender
remain as sent.

## Type

- **Poppins 600** for display: the subject line and section titles. It's the wordmark's face.
- **IBM Plex Sans** 400, 500 and 600 for everything else in the chrome: names, labels, the footer.
- **IBM Plex Mono** for things you'd copy exactly: addresses, the Message-ID, the S3 path, and
  the small uppercase field labels (TO, CC, DELIVERY CHECKS).

The reader page is a local file under a Content-Security-Policy that blocks remote fonts, so it
names these faces first and falls back to the system's own sans and mono. It never pulls fonts
from the network.

## The reader page

`h` on a message writes one self-contained page and opens it in the default browser.

- **The top bar** has the cubes and wordmark, "Static copy of one message", and a pill saying
  remote content is blocked.
- **The header card** leads with the subject, then the sender's initials, name and address,
  with the date on the right. Below that are To, Cc, Bcc (from the SES envelope) and Reply-To,
  then the attachments as chips with their sizes. The only actions are Reply and Reply all,
  as `mailto:` links. For a threaded reply or a forward with attachments, `o` in reses opens
  the message itself in your mail app.
- **The message** is the sender's HTML, untouched, inside a shadow root so its styles and
  ours can't reach each other.
- **The footer, "About this message",** has three cards:
  - delivery checks: SPF, DKIM and DMARC from Authentication-Results, plus SES's spam and
    virus verdicts;
  - the message: Message-ID, sent, received, size and parts;
  - this copy: its S3 location, when it was opened and by which re:SES version, and what
    the page blocks.
- **The last line** says it's a static copy: nothing on the page can send, reply or delete.

The inspiration is the calm of Apple Mail's header, the card rhythm of Fastmail and Hey, and
the SPF, DKIM and DMARC summary that Gmail's "Show original" gives. It borrows no layout
outright.
