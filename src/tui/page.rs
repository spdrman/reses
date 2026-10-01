//! A message's HTML part as a page for the browser, dressed as a re:SES reader.
//!
//! `h` on the message screen shows the HTML part the way it was meant to be seen, in the
//! default browser, wrapped in a header and footer in the re:SES look (`docs/branding.md`).
//!
//! The HTML is untrusted: it's whatever the sender wrote, and senders put tracking pixels,
//! remote fonts and sometimes scripts in mail. So the page opens with a Content-Security-Policy
//! that blocks every fetch and script, and allows only inline styles and `data:` images and
//! fonts. A policy the message carries itself can only tighten that. Links still open when
//! clicked, because following one is the reader's choice.
//!
//! The sender's HTML goes inside a declarative shadow root, so its styles can't restyle the
//! header and footer, and theirs can't reach into it. It also can't close that root early to
//! draw a fake header of its own: I break every `</template` in it. Every header value I print
//! is HTML-escaped.
//!
//! The only actions are Reply and Reply all, as `mailto:` links for the default mail app.
//! Anything more (a threaded reply, a forward with attachments) is `o` in reses, which opens
//! the message itself in the mail app.
//!
//! Pages and `.eml` copies are written to their own files (readable only by the user) and kept,
//! because the browser or mail app reads them after reses has moved on. They live in the
//! system temp dir, which the OS clears.

use std::io;
use std::path::{Path, PathBuf};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use super::text::human_size;
use crate::mail::Details;

/// What I put first in every page's head: the charset, since the HTML part was decoded to
/// UTF-8, and the policy that stops the page fetching or running anything.
pub(crate) const GUARD: &str = concat!(
    "<meta charset=\"utf-8\">",
    "<meta http-equiv=\"Content-Security-Policy\" content=\"",
    "default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:; ",
    "form-action 'none'; base-uri 'none'",
    "\">",
);

/// The policy for the copy that shows remote content, when the reader asks for it (#69): images,
/// stylesheets, fonts and media may come from the network, and scripts and form posts still can't.
pub(crate) const GUARD_REMOTE: &str = concat!(
    "<meta charset=\"utf-8\">",
    "<meta http-equiv=\"Content-Security-Policy\" content=\"",
    "default-src 'none'; img-src data: https: http:; style-src 'unsafe-inline' https: http:; ",
    "font-src data: https: http:; media-src https: http:; form-action 'none'; base-uri 'none'",
    "\">",
);

/// Where this copy came from and when it was opened, for the footer.
pub(crate) struct Copy<'a> {
    /// The S3 location, `s3://bucket/key`.
    pub location: &'a str,
    /// When the page was made, in the reader's own offset.
    pub opened: OffsetDateTime,
    /// Whether this copy lets remote content load (#69). The blocked copy is the one that opens.
    pub remote: bool,
    /// The file name of the other copy, which this one's chip links to.
    pub other: &'a str,
}

/// The canonical logo, inlined once as an SVG sprite. Its named groups let the reader compose
/// the cubes and wordmark independently without carrying separate logo assets.
const LOGO_SVG: &str = include_str!("../../assets/brand/reSES-logo.svg");
const CUBES_MARK: &str =
    r##"<svg viewBox="182 5 556 520" aria-hidden="true"><use href="#cubes"></use></svg>"##;
const WORDMARK_MARK: &str =
    r##"<svg viewBox="20 525 870 235" aria-hidden="true"><use href="#wordmark"></use></svg>"##;

/// The reader's styles, in the Tidewater palette and type from `docs/branding.md`. The page
/// can't fetch fonts, so each stack names the brand face first and falls back to the system's.
const STYLE: &str = r#"
:root{color-scheme:light dark;
--ink:#0E1B2C;--slate:#4A5A6E;--paper:#F7F9FC;--card:#FFFFFF;--line:#DDE5EF;
--tint:#E8F3FD;--deep:#1462CF;--channel:#1D78DE;--link-hover:#0F52BE;
--shown-bg:#FDF1E6;--shown-text:#A4520A;--pass-bg:#E6F4F1;--pass-text:#0B6E62;
--caution-bg:#FDF1E6;--caution-text:#A4520A;--fail-bg:#FDECEA;--fail-text:#B42318;
--sans:'IBM Plex Sans',-apple-system,'Segoe UI',system-ui,sans-serif;
--display:'Poppins',-apple-system,'Segoe UI',system-ui,sans-serif;
--mono:'IBM Plex Mono',ui-monospace,'SF Mono',Menlo,Consolas,monospace}
@media (prefers-color-scheme:dark){:root{
--ink:#F7F9FC;--slate:#AEBBCD;--paper:#0B1220;--card:#141F30;--line:#2B3A4F;
--tint:#152F4D;--deep:#66B8FF;--channel:#65B7FF;--link-hover:#91CCFF;
--shown-bg:#3C2A18;--shown-text:#FFB86B;--pass-bg:#15352F;--pass-text:#62D5C5;
--caution-bg:#3C2A18;--caution-text:#FFB86B;--fail-bg:#432523;--fail-text:#FF8F87}}
*{box-sizing:border-box}
html{background:var(--paper)}
body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.5 var(--sans)}
a{color:var(--deep)}a:hover{color:var(--link-hover)}
.bar{display:flex;align-items:center;justify-content:space-between;gap:16px;flex-wrap:wrap;
padding:14px 40px;background:var(--card);border-bottom:1px solid var(--line)}
.brand{display:flex;align-items:center;gap:12px}
.brand .cubes svg{display:block;height:30px;width:auto}
.brand .wordmark svg{display:block;height:20px;width:auto;color:var(--ink)}
.brand .what{margin-left:8px;font-size:14px;color:var(--slate)}
.logo-source{position:absolute;width:0;height:0;overflow:hidden}
.pill{display:inline-flex;align-items:center;gap:6px;padding:4px 10px;border-radius:999px;
font-size:13px;font-weight:600;white-space:nowrap}
.pill svg{width:12px;height:12px;flex-shrink:0}
.blocked{background:var(--tint);color:var(--deep);padding:7px 14px}
.shown{background:var(--shown-bg);color:var(--shown-text);padding:7px 14px}
a.pill{text-decoration:none}a.pill:hover{filter:brightness(0.96);text-decoration:underline}
.pass{background:var(--pass-bg);color:var(--pass-text)}
.caution{background:var(--caution-bg);color:var(--caution-text)}
.fail{background:var(--fail-bg);color:var(--fail-text)}
main{max-width:920px;margin:0 auto;padding:40px 16px 56px;display:flex;flex-direction:column;gap:24px}
.card{background:var(--card);border:1px solid var(--line);border-radius:18px}
.head{padding:32px 36px;display:flex;flex-direction:column;gap:22px}
.top{display:flex;align-items:flex-start;justify-content:space-between;gap:24px}
h1{margin:0;font:600 30px/1.2 var(--display);letter-spacing:-0.015em;overflow-wrap:anywhere}
.when{text-align:right;flex-shrink:0;padding-top:6px}
.when b{display:block;font-weight:500;font-size:14px}.when span{font-size:13px;color:var(--slate)}
.sender{display:flex;align-items:center;gap:16px;flex-wrap:wrap}
.avatar{width:48px;height:48px;border-radius:999px;background:var(--tint);color:var(--deep);
display:flex;align-items:center;justify-content:center;font:600 17px var(--display);flex-shrink:0}
.who{flex-grow:1;min-width:0}.who b{display:block;font-size:17px}
.mono{font-family:var(--mono);font-size:14px;color:var(--slate);overflow-wrap:anywhere}
.actions{display:flex;gap:10px}
.actions a{display:inline-flex;align-items:center;gap:8px;min-height:44px;padding:0 16px;
border-radius:12px;border:1px solid var(--line);color:var(--ink);text-decoration:none;
font-weight:600;font-size:14px}
.actions a:hover{background:var(--paper)}
.actions svg{width:16px;height:16px}
.fields{display:grid;grid-template-columns:92px minmax(0,1fr);gap:10px 16px;padding-top:20px;
border-top:1px solid var(--line)}
.label{font:12px/1.9 var(--mono);letter-spacing:0.08em;text-transform:uppercase;color:var(--slate)}
.fields div{overflow-wrap:anywhere}
.files{display:flex;flex-direction:column;gap:12px;padding-top:20px;border-top:1px solid var(--line)}
.files .sum{font-size:14px;color:var(--slate)}.files .sum b{color:var(--ink)}
.chips{display:flex;flex-wrap:wrap;gap:10px}
.chip{display:inline-flex;align-items:center;gap:10px;padding:10px 14px;border-radius:12px;
background:var(--paper);border:1px solid var(--line);font-size:14px;font-weight:500}
.chip svg{width:18px;height:18px}.chip span{font-weight:400;font-size:13px;color:var(--slate)}
.message{padding:40px 44px;overflow:auto}
.about{display:flex;flex-direction:column;gap:16px}
.about header{display:flex;align-items:baseline;justify-content:space-between;gap:16px;flex-wrap:wrap}
h2{margin:0;font:600 19px var(--display)}.about header span{font-size:13px;color:var(--slate)}
.cols{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:16px}
.cols .card{padding:22px 24px;display:flex;flex-direction:column;gap:12px;font-size:14px}
.row{display:flex;justify-content:space-between;align-items:center;gap:12px}
.row small{color:var(--slate);font-size:13px}
.fact span{display:block;color:var(--slate);font-size:13px}
.fact .mono{font-size:13px;color:var(--ink)}
.none{color:var(--slate)}
.foot{display:flex;align-items:center;justify-content:space-between;gap:24px;flex-wrap:wrap;
padding-top:20px;border-top:1px solid var(--line);font-size:13px;color:var(--slate)}
.foot .brand .cubes svg{height:19px}
@media (max-width:760px){.cols{grid-template-columns:minmax(0,1fr)}.bar{padding:12px 16px}
.head{padding:24px 20px}.message{padding:24px 20px}.top{flex-direction:column}.when{text-align:left}}
"#;

/// Icons, drawn as inline strokes so they need nothing from the network.
const ICON_LOCK: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="4" y="10" width="16" height="10" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/></svg>"#;
const ICON_EYE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"/><circle cx="12" cy="12" r="3"/></svg>"#;
const ICON_REPLY: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 14L4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 5 5v5"/></svg>"#;
const ICON_FILE: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="var(--channel)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/></svg>"##;
const ICON_PASS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12l5 5L20 7"/></svg>"#;
const ICON_CAUTION: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 8v5"/><path d="M12 16.5v.5"/></svg>"#;
const ICON_FAIL: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 6l12 12"/><path d="M18 6L6 18"/></svg>"#;

/// Everything but letters, digits and `.-_~` is percent-encoded in a `mailto:` query value.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');
/// An address before the `?` keeps its `@` and `+` readable.
const ADDRESS: &AsciiSet = &QUERY.remove(b'@').remove(b'+');

/// How the reader writes a date: "Fri 25 Sep 2026, 09:30".
const WHEN: &[FormatItem<'static>] = format_description!(
    "[weekday repr:short] [day padding:none] [month repr:short] [year], [hour]:[minute]"
);

/// I build the whole reader page for a message: the header, the message's HTML part in its
/// shadow root, and the footer.
pub(crate) fn reader(details: &Details, copy: &Copy<'_>) -> String {
    let mut page = String::with_capacity(16 * 1024 + details.html.as_ref().map_or(0, String::len));
    // The policy is the first thing in the head, before anything the page could load.
    page.push_str("<!doctype html>\n<html lang=\"en\"><head>");
    page.push_str(if copy.remote { GUARD_REMOTE } else { GUARD });
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    page.push_str("<base target=\"_blank\">");
    page.push_str(&format!("<title>{}</title>", esc(&title(details))));
    page.push_str("<style>");
    page.push_str(STYLE);
    page.push_str("</style></head><body><div class=\"logo-source\" aria-hidden=\"true\">");
    page.push_str(svg(LOGO_SVG));
    page.push_str("</div>");
    page.push_str(&top_bar(copy));
    page.push_str("<main>");
    page.push_str(&header(details));
    page.push_str(&message(details));
    page.push_str(&about(details, copy));
    page.push_str(&format!(
        "<div class=\"foot\"><div class=\"brand\"><span class=\"cubes\">{CUBES_MARK}</span>\
         <span>Read with re:SES, a terminal inbox for the mail Amazon SES keeps in S3.</span></div>\
         <span>A static copy: nothing on this page can send, reply or delete. Reply only opens \
         your own mail app.</span></div>"
    ));
    page.push_str("</main></body></html>\n");
    page
}

/// The tab's title: the subject, or a stand-in when there's none.
fn title(details: &Details) -> String {
    if details.subject.trim().is_empty() {
        "(no subject) · re:SES".to_string()
    } else {
        format!("{} · re:SES", details.subject.trim())
    }
}

/// The bar across the top: the logo, what the page is, and that remote content is blocked.
fn top_bar(copy: &Copy<'_>) -> String {
    // The chip is the toggle (#69): a link, in this tab, to the other copy of the page. A page's
    // policy can't be loosened from inside it, so showing remote content means the other copy.
    let chip = if copy.remote {
        format!(
            "<a class=\"pill shown\" href=\"{}\" target=\"_self\" title=\"Back to the copy that \
             blocks remote content\">{ICON_EYE}Remote content shown · Block it</a>",
            esc(copy.other)
        )
    } else {
        format!(
            "<a class=\"pill blocked\" href=\"{}\" target=\"_self\" title=\"Loading remote content \
             lets the sender see you opened it\">{ICON_LOCK}Remote content blocked · Load remote \
             content</a>",
            esc(copy.other)
        )
    };
    format!(
        "<div class=\"bar\"><div class=\"brand\"><span class=\"cubes\">{CUBES_MARK}</span>\
         <span class=\"wordmark\" role=\"img\" aria-label=\"re:SES\">{WORDMARK_MARK}</span>\
         <span class=\"what\">Static copy of one message</span></div>{chip}</div>"
    )
}

/// The header card: subject and date, the sender with the reply links, every recipient line,
/// and the attachments.
fn header(d: &Details) -> String {
    let mut out = String::from("<section class=\"card head\">");
    // The subject, with the date the sender gave it on the right.
    let subject = if d.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        d.subject.trim().to_string()
    };
    let when = match d.date {
        Some(date) => format!(
            "<b>{}</b><span>{}</span>",
            date.format(WHEN).map(|s| esc(&s)).unwrap_or_default(),
            esc(&offset_label(date))
        ),
        None if !d.date_raw.is_empty() => format!("<b>{}</b>", esc(&d.date_raw)),
        None => "<b>No date</b>".to_string(),
    };
    out.push_str(&format!(
        "<div class=\"top\"><h1>{}</h1><div class=\"when\">{when}</div></div>",
        esc(&subject)
    ));
    // The sender, with their initials, and the only two actions the page has.
    let sender = d.from.first();
    let (name, address) = sender.map_or(("", ""), |m| (m.name.as_str(), m.address.as_str()));
    let shown = if name.is_empty() { address } else { name };
    out.push_str(&format!(
        "<div class=\"sender\"><div class=\"avatar\" aria-hidden=\"true\">{}</div>\
         <div class=\"who\"><b>{}</b><div class=\"mono\">{}</div></div>{}</div>",
        esc(&initials(name, address)),
        esc(if shown.is_empty() {
            "Unknown sender"
        } else {
            shown
        }),
        esc(if name.is_empty() { "" } else { address }),
        reply_links(d)
    ));
    // Every recipient line the message has, in the order a mail reader shows them.
    let mut fields = String::new();
    for (label, list) in [("To", &d.to), ("Cc", &d.cc)] {
        if !list.is_empty() {
            fields.push_str(&field(label, &mailbox_list(list)));
        }
    }
    if !d.bcc.trim().is_empty() {
        fields.push_str(&field("Bcc", &esc(d.bcc.trim())));
    }
    if !d.reply_to.is_empty() {
        fields.push_str(&field("Reply-To", &mailbox_list(&d.reply_to)));
    }
    if !fields.is_empty() {
        out.push_str(&format!("<div class=\"fields\">{fields}</div>"));
    }
    // The attachments, named with their sizes. They're saved from reses, not from here.
    if !d.attachments.is_empty() {
        let total: usize = d.attachments.iter().map(|(_, n)| n).sum();
        let chips: String = d
            .attachments
            .iter()
            .map(|(name, bytes)| {
                format!(
                    "<div class=\"chip\">{ICON_FILE}{}<span>{}</span></div>",
                    esc(name),
                    human_size(*bytes as u64)
                )
            })
            .collect();
        out.push_str(&format!(
            "<div class=\"files\"><div class=\"sum\"><b>{}</b> · {} · saved from reses with \
             <span class=\"mono\">a</span></div><div class=\"chips\">{chips}</div></div>",
            plural(d.attachments.len(), "attachment"),
            human_size(total as u64)
        ));
    }
    out.push_str("</section>");
    out
}

/// The sender's HTML in a declarative shadow root, whose host resets every inherited style, so the
/// message starts from the browser defaults an email expects rather than from the reader's fonts.
/// I break every `</template` in it, so it can't close the root and draw outside, and drop any
/// `<meta http-equiv>`, so it can't refresh or redirect the page.
fn message(d: &Details) -> String {
    let html = d.html.as_deref().unwrap_or("");
    format!(
        "<section class=\"card message\"><div class=\"mail\"><template shadowrootmode=\"open\">\
         <style>:host{{all:initial;display:block;overflow-wrap:anywhere;color-scheme:light dark;\
         color:CanvasText}}img{{max-width:100%;height:auto}}</style>\
         {}</template></div></section>",
        contained(html)
    )
}

/// `html` made safe to sit inside the shadow root's `<template>`.
fn contained(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while at < html.len() {
        let rest = &lower[at..];
        if rest.starts_with("</template") {
            // Written as text, it shows up rather than ending the root.
            out.push_str("&lt;/template");
            at += "</template".len();
        } else if rest.starts_with("<meta")
            && rest[..rest.find('>').unwrap_or(rest.len())].contains("http-equiv")
        {
            at += rest.find('>').map_or(rest.len(), |end| end + 1);
        } else {
            let next = rest[1..].find('<').map_or(html.len(), |i| at + 1 + i);
            out.push_str(&html[at..next]);
            at = next;
        }
    }
    out
}

/// The "About this message" footer: delivery checks, the message itself, and this copy.
fn about(d: &Details, copy: &Copy<'_>) -> String {
    // Delivery checks: each Authentication-Results check, then SES's two verdicts.
    let mut checks: String = d
        .checks
        .iter()
        .map(|c| {
            let detail = if c.detail.is_empty() {
                String::new()
            } else {
                format!(" <small>{}</small>", esc(&c.detail))
            };
            format!(
                "<div class=\"row\"><span>{}{detail}</span>{}</div>",
                esc(&c.method.to_ascii_uppercase()),
                pill(&c.result)
            )
        })
        .collect();
    for (label, verdict) in [
        ("SES spam verdict", &d.spam_verdict),
        ("SES virus verdict", &d.virus_verdict),
    ] {
        if !verdict.trim().is_empty() {
            checks.push_str(&format!(
                "<div class=\"row\"><span>{label}</span>{}</div>",
                pill(&verdict.trim().to_ascii_lowercase().replace('_', " "))
            ));
        }
    }
    if checks.is_empty() {
        checks.push_str(
            "<div class=\"none\">No delivery checks in the headers. SES adds them to mail it \
             receives, so this message may have come from somewhere else.</div>",
        );
    }
    // The message: its id, when it was sent and received, and what it's made of.
    let mut parts = Vec::new();
    if d.html.is_some() {
        parts.push("HTML".to_string());
    }
    if d.has_text {
        parts.push("plain text".to_string());
    }
    if !d.attachments.is_empty() {
        parts.push(plural(d.attachments.len(), "attachment"));
    }
    let facts = [
        ("Message-ID", mono(&d.message_id)),
        ("Sent", esc(or_dash(&d.date_raw))),
        ("Received", esc(or_dash(&d.received))),
        (
            "Size and parts",
            format!("{} · {}", human_size(d.size as u64), esc(&parts.join(", "))),
        ),
    ];
    let message: String = facts.iter().map(|(k, v)| fact(k, v)).collect();
    // This copy: where it's stored, when it was opened and by what, and what the page blocks.
    let opened = copy.opened.format(WHEN).unwrap_or_default();
    let this_copy = [
        ("Stored at", mono(copy.location)),
        (
            "Opened",
            format!("{} · re:SES {}", esc(&opened), env!("CARGO_PKG_VERSION")),
        ),
        (
            "Protection",
            if copy.remote {
                "Remote images, styles and fonts load here, so the sender can see you opened it. \
                 Scripts and forms are still blocked."
            } else {
                "Scripts, remote images, fonts and forms are blocked. Links open only when you \
                 click them. Loading remote content lets the sender see you opened it."
            }
            .to_string(),
        ),
    ]
    .iter()
    .map(|(k, v)| fact(k, v))
    .collect::<String>();
    format!(
        "<section class=\"about\"><header><h2>About this message</h2>\
         <span>From its headers, SES and the S3 object</span></header><div class=\"cols\">\
         <div class=\"card\"><div class=\"label\">Delivery checks</div>{checks}</div>\
         <div class=\"card\"><div class=\"label\">The message</div>{message}</div>\
         <div class=\"card\"><div class=\"label\">This copy</div>{this_copy}</div>\
         </div></section>"
    )
}

/// A check's result as a pill: its word, an icon, and a colour, so it never rests on hue alone.
fn pill(result: &str) -> String {
    let (class, icon) = match result {
        "pass" => ("pass", ICON_PASS),
        "fail" | "permerror" | "hardfail" => ("fail", ICON_FAIL),
        _ => ("caution", ICON_CAUTION),
    };
    format!("<span class=\"pill {class}\">{icon}{}</span>", esc(result))
}

/// Reply and Reply all as `mailto:` links. Reply goes to Reply-To when the message has one,
/// else to From, with "Re: " added once and the Message-ID as In-Reply-To for mail apps that
/// thread on it. Reply all copies To and Cc. Nobody to reply to means no links.
fn reply_links(d: &Details) -> String {
    let addresses = |list: &[crate::mail::Mailbox]| -> Vec<String> {
        list.iter()
            .map(|m| m.address.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect()
    };
    let mut target = addresses(&d.reply_to);
    if target.is_empty() {
        target = addresses(&d.from);
    }
    if target.is_empty() {
        return String::new();
    }
    let subject = d.subject.trim();
    let subject = if subject.len() >= 3 && subject[..3].eq_ignore_ascii_case("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    };
    let to = target
        .iter()
        .map(|a| utf8_percent_encode(a, ADDRESS).to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut tail = format!("subject={}", utf8_percent_encode(&subject, QUERY));
    if !d.message_id.trim().is_empty() {
        tail.push_str(&format!(
            "&In-Reply-To={}",
            utf8_percent_encode(d.message_id.trim(), QUERY)
        ));
    }
    // Everyone else who got it, once each, and not the people the reply already goes to.
    let mut cc: Vec<String> = Vec::new();
    for a in addresses(&d.to).into_iter().chain(addresses(&d.cc)) {
        let seen = |x: &String| x.eq_ignore_ascii_case(&a);
        if !target.iter().any(seen) && !cc.iter().any(seen) {
            cc.push(a);
        }
    }
    let reply = format!("mailto:{to}?{tail}");
    let mut links = format!(
        "<div class=\"actions\"><a href=\"{}\">{ICON_REPLY}Reply in Mail</a>",
        esc(&reply)
    );
    if !cc.is_empty() {
        let joined = cc.join(",");
        let cc = utf8_percent_encode(&joined, QUERY);
        links.push_str(&format!(
            "<a href=\"{}\">Reply all</a>",
            esc(&format!("mailto:{to}?cc={cc}&{tail}"))
        ));
    }
    links.push_str("</div>");
    links
}

/// Up to two initials for the avatar: from the name's first words, else the address.
fn initials(name: &str, address: &str) -> String {
    let from_name: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if !from_name.is_empty() {
        return from_name;
    }
    address
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_string(), |c| c.to_uppercase().collect())
}

/// A mailbox list as a reader prints it, "Name <address>" joined by commas, escaped.
fn mailbox_list(list: &[crate::mail::Mailbox]) -> String {
    list.iter()
        .map(|m| match (m.name.is_empty(), m.address.is_empty()) {
            (false, false) => format!("{} <{}>", m.name, m.address),
            (false, true) => m.name.clone(),
            _ => m.address.clone(),
        })
        .map(|s| esc(&s))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One header field row: its label and its (already escaped) value.
fn field(label: &str, value: &str) -> String {
    format!("<div class=\"label\">{label}</div><div>{value}</div>")
}

/// One footer fact: a label over its (already escaped) value.
fn fact(label: &str, value: &str) -> String {
    format!("<div class=\"fact\"><span>{label}</span>{value}</div>")
}

/// A value in the mono face, escaped, or a dash when it's empty.
fn mono(value: &str) -> String {
    format!("<div class=\"mono\">{}</div>", esc(or_dash(value)))
}

/// `value`, or a dash standing in for nothing.
fn or_dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value.trim()
    }
}

/// "1 attachment", "2 attachments".
fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// The date's own offset, as the sender wrote it.
fn offset_label(date: OffsetDateTime) -> String {
    let (h, m, _) = date.offset().as_hms();
    format!(
        "{}{:02}{:02} in the sender's time",
        if date.offset().is_negative() {
            '-'
        } else {
            '+'
        },
        h.abs(),
        m.abs()
    )
}

/// The canonical SVG without its XML prolog, ready to inline.
fn svg(source: &str) -> &str {
    source.find("<svg").map_or(source, |at| &source[at..])
}

/// `text` escaped for HTML text and double-quoted attributes.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// I write the reader page for `details` as a pair in `dir` (#69): the copy that blocks remote content,
/// whose path I return since it's the one to open, and beside it the copy that shows it, each
/// linking to the other by file name. Both are new files only the user can read.
pub(crate) fn write_reader(
    dir: &Path,
    details: &Details,
    location: &str,
    opened: OffsetDateTime,
) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // The blocked copy takes a fresh name, and its twin the same name with -remote on the end, so
    // each can link to the other by file name alone.
    let (mut blocked, blocked_path) = tempfile::Builder::new()
        .prefix("reses-")
        .suffix(".html")
        .tempfile_in(dir)?
        .keep()
        .map_err(|e| e.error)?;
    let stem = blocked_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("reses")
        .to_string();
    let blocked_name = format!("{stem}.html");
    let remote_name = format!("{stem}-remote.html");
    // The twin is new too (never over someone else's file) and private like the first.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut remote = options.open(blocked_path.with_file_name(&remote_name))?;
    let copy = |remote, other| Copy {
        location,
        opened,
        remote,
        other,
    };
    io::Write::write_all(
        &mut remote,
        reader(details, &copy(true, blocked_name.as_str())).as_bytes(),
    )?;
    io::Write::write_all(
        &mut blocked,
        reader(details, &copy(false, remote_name.as_str())).as_bytes(),
    )?;
    Ok(blocked_path)
}

/// I write `contents` to a new file in `dir` ending in `suffix` that only the user can read,
/// and return its path. `dir` is created if it doesn't exist yet, and an earlier file is never
/// overwritten.
pub(crate) fn write(dir: &Path, contents: &[u8], suffix: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let (mut file, path) = tempfile::Builder::new()
        .prefix("reses-")
        .suffix(suffix)
        .tempfile_in(dir)?
        .keep()
        .map_err(|e| e.error)?;
    io::Write::write_all(&mut file, contents)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::mail::{Check, Mailbox};

    /// I build the details of a message that has everything the reader shows.
    fn full() -> Details {
        let mailbox = |name: &str, address: &str| Mailbox {
            name: name.into(),
            address: address.into(),
        };
        let check = |method: &str, result: &str, detail: &str| Check {
            method: method.into(),
            result: result.into(),
            detail: detail.into(),
        };
        Details {
            subject: "Your Q3 numbers are in".into(),
            from: vec![mailbox("Alice Example", "alice@example.com")],
            to: vec![mailbox("Me", "me@example.com")],
            cc: vec![
                mailbox("Bob Builder", "bob@example.org"),
                mailbox("", "carol@example.net"),
            ],
            reply_to: vec![mailbox("", "reports@example.com")],
            bcc: "archive@example.com".into(),
            date: Some(datetime!(2026-09-25 09:30:00 +00:00)),
            date_raw: "Fri, 25 Sep 2026 09:30:00 +0000".into(),
            received: "Fri, 25 Sep 2026 09:30:04 +0000 (UTC)".into(),
            message_id: "<CAF7x2Q9@mail.example.com>".into(),
            attachments: vec![
                ("q3-report.pdf".into(), 421_888),
                ("regions.csv".into(), 900),
            ],
            html: Some("<p>the <b>html</b> version</p>".into()),
            has_text: true,
            checks: vec![
                check("spf", "pass", "alice@example.com"),
                check("dkim", "pass", "example.com"),
                check("dmarc", "fail", "example.com"),
            ],
            spam_verdict: "PASS".into(),
            virus_verdict: "FAIL".into(),
            size: 493_568,
        }
    }

    /// I make the page for `details` as if it was opened from a fixed place at a fixed time.
    fn render(details: &Details) -> String {
        render_as(details, false)
    }

    /// I make one of the pair: the copy that blocks remote content, or the one that shows it.
    fn render_as(details: &Details, remote: bool) -> String {
        reader(
            details,
            &Copy {
                location: "s3://inbox-bucket/mail/0abc123",
                opened: datetime!(2026-09-26 21:14:00 -07:00),
                remote,
                other: if remote {
                    "reses-abc.html"
                } else {
                    "reses-abc-remote.html"
                },
            },
        )
    }

    /// #69: the blocked copy's chip is a link, in the same tab, to the copy that shows remote
    /// content, and says what that costs.
    #[test]
    fn the_blocked_page_offers_to_load_remote_content() {
        let page = render(&full());
        assert!(
            page.contains(
                "<a class=\"pill blocked\" href=\"reses-abc-remote.html\" target=\"_self\""
            ),
            "{page}"
        );
        assert!(page.contains("Load remote content"), "{page}");
        assert!(page.contains("lets the sender see you opened it"), "{page}");
    }

    /// #69: the copy that shows remote content lets images, styles, fonts and media come from the
    /// network, still refuses scripts and form posts, and its chip links back to blocking.
    #[test]
    fn the_remote_page_loads_remote_content_and_nothing_more() {
        let page = render_as(&full(), true);
        let head = page.find("<head>").expect("a head");
        assert!(
            page[head + "<head>".len()..].starts_with(GUARD_REMOTE),
            "{page}"
        );
        for rule in [
            "img-src data: https: http:",
            "style-src 'unsafe-inline' https: http:",
            "font-src data: https: http:",
            "media-src https: http:",
            "form-action 'none'",
            "base-uri 'none'",
        ] {
            assert!(
                GUARD_REMOTE.contains(rule),
                "{rule} missing: {GUARD_REMOTE}"
            );
        }
        assert!(!page.contains("script-src"), "{page}");
        assert!(
            page.contains("<a class=\"pill shown\" href=\"reses-abc.html\" target=\"_self\""),
            "{page}"
        );
        assert!(page.contains("Remote content shown · Block it"), "{page}");
        assert!(page.contains("the sender can see you opened it"), "{page}");
    }

    /// #69: the reader is written as a pair. The blocked copy is the one to open, its twin sits
    /// beside it named `-remote`, both are private, and each links to the other by file name.
    #[test]
    fn write_reader_writes_both_copies_linked_to_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let pages = dir.path().join("pages");
        let blocked = write_reader(
            &pages,
            &full(),
            "s3://inbox-bucket/mail/0abc123",
            datetime!(2026-09-26 21:14:00 -07:00),
        )
        .unwrap();
        let stem = blocked.file_stem().unwrap().to_str().unwrap().to_string();
        let remote = blocked.with_file_name(format!("{stem}-remote.html"));
        let blocked_page = std::fs::read_to_string(&blocked).unwrap();
        let remote_page = std::fs::read_to_string(&remote).expect("the remote copy is written too");
        assert!(
            blocked_page.contains(&format!("href=\"{stem}-remote.html\"")),
            "{blocked_page}"
        );
        assert!(
            remote_page.contains(&format!("href=\"{stem}.html\"")),
            "{remote_page}"
        );
        assert!(blocked_page.contains(GUARD) && !blocked_page.contains(GUARD_REMOTE));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&blocked, &remote] {
                let mode = std::fs::metadata(path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{}", path.display());
            }
        }
    }

    /// The page is a real document whose head opens with the policy, before anything the page
    /// could load.
    #[test]
    fn the_page_opens_with_the_policy() {
        let page = render(&full());
        assert!(page.starts_with("<!doctype html>"), "{page}");
        let head = page.find("<head>").expect("a head");
        assert!(page[head + "<head>".len()..].starts_with(GUARD), "{page}");
        for rule in [
            "default-src 'none'",
            "form-action 'none'",
            "base-uri 'none'",
        ] {
            assert!(page.contains(rule), "{rule} missing");
        }
        assert!(!page.contains("script-src"), "{page}");
    }

    /// The header shows what a mail reader leads with: subject, sender, date, every recipient
    /// line, and the attachments with their sizes.
    #[test]
    fn the_header_shows_every_field() {
        let page = render(&full());
        for text in [
            "Your Q3 numbers are in",
            "Alice Example",
            "alice@example.com",
            ">AE<",
            "Me &lt;me@example.com&gt;",
            "Bob Builder &lt;bob@example.org&gt;",
            "carol@example.net",
            "archive@example.com",
            "reports@example.com",
            "q3-report.pdf",
            "412.0 KiB",
            "regions.csv",
            "900 B",
            "Fri 25 Sep 2026, 09:30",
        ] {
            assert!(page.contains(text), "{text} missing from the header");
        }
    }

    /// Every header value is escaped, so a name or subject can't add markup to the page.
    #[test]
    fn header_values_are_escaped() {
        let mut d = full();
        d.from[0].name = "<img src=x onerror=alert(1)>".into();
        d.subject = "Hi </template><h1>FAKE</h1>".into();
        d.attachments = vec![("\"><script>x</script>.pdf".into(), 1)];
        let page = render(&d);
        assert!(!page.contains("<img src=x"), "{page}");
        assert!(
            page.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "{page}"
        );
        assert!(!page.contains("<h1>FAKE"), "{page}");
        assert!(!page.contains("<script>"), "{page}");
    }

    /// The sender's HTML sits in a shadow root it can't leave: a `</template` in it can't close
    /// the root early, and a refresh or redirect it carries is dropped.
    #[test]
    fn the_message_sits_in_a_shadow_root_it_cant_leave() {
        let mut d = full();
        d.html = Some(
            concat!(
                "<meta http-equiv=\"refresh\" content=\"0;url=https://evil.example/\">",
                "<style>p{color:red}</style><p>body text</p>",
                "</template><div>FAKE HEADER</div></TEMPLATE >",
            )
            .into(),
        );
        let page = render(&d);
        let open = page
            .find("<template shadowrootmode=\"open\">")
            .expect("a shadow root");
        assert_eq!(page.matches("</template>").count(), 1, "{page}");
        let close = page.find("</template>").unwrap();
        let fake = page
            .find("FAKE HEADER")
            .expect("the message text is still shown");
        assert!(
            open < fake && fake < close,
            "the message escaped its root: {page}"
        );
        assert!(page[open..close].contains("<p>body text</p>"), "{page}");
        assert!(
            !page.to_ascii_lowercase().contains("http-equiv=\"refresh\""),
            "{page}"
        );
    }

    /// Reply goes to Reply-To when there is one, else to From, with Re: added once and the
    /// Message-ID for mail apps that thread on it. Reply all adds To and Cc.
    #[test]
    fn reply_links_open_the_mail_app() {
        let page = render(&full());
        assert!(
            page.contains(
                "href=\"mailto:reports@example.com?subject=Re%3A%20Your%20Q3%20numbers%20are%20in\
                 &amp;In-Reply-To=%3CCAF7x2Q9%40mail.example.com%3E\""
            ),
            "{page}"
        );
        assert!(
            page.contains(
                "href=\"mailto:reports@example.com?cc=me%40example.com%2Cbob%40example.org%2Ccarol%40example.net\
                 &amp;subject=Re%3A%20Your%20Q3%20numbers%20are%20in\
                 &amp;In-Reply-To=%3CCAF7x2Q9%40mail.example.com%3E\""
            ),
            "{page}"
        );
        let mut d = full();
        d.reply_to.clear();
        d.subject = "RE: already a reply".into();
        let page = render(&d);
        assert!(
            page.contains("href=\"mailto:alice@example.com?subject=RE%3A%20already%20a%20reply"),
            "{page}"
        );
    }

    /// A message with nobody to reply to gets no reply links at all.
    #[test]
    fn no_sender_means_no_reply_links() {
        let mut d = full();
        d.from.clear();
        d.reply_to.clear();
        assert!(!render(&d).contains("mailto:"));
    }

    /// The footer says what the message's headers, SES and the S3 object say about it: each
    /// check in words, the verdicts, the ids, times, size and parts, and this copy's origin.
    #[test]
    fn the_footer_says_where_the_message_came_from() {
        let page = render(&full());
        for text in [
            "About this message",
            "SPF",
            "alice@example.com",
            "DKIM",
            "DMARC",
            ">fail<",
            "SES spam verdict",
            "SES virus verdict",
            "&lt;CAF7x2Q9@mail.example.com&gt;",
            "Fri, 25 Sep 2026 09:30:04 +0000 (UTC)",
            "482.0 KiB",
            "HTML, plain text, 2 attachments",
            "s3://inbox-bucket/mail/0abc123",
            "Sat 26 Sep 2026, 21:14",
            concat!("re:SES ", env!("CARGO_PKG_VERSION")),
            "nothing on this page can send, reply or delete",
        ] {
            assert!(page.contains(text), "{text} missing from the footer");
        }
        // A pass and a fail differ in their word as well as their colour.
        assert!(page.contains(">pass<"), "{page}");
    }

    /// A message SES didn't check still gets a footer, saying there's nothing to show rather
    /// than inventing passes.
    #[test]
    fn a_message_without_checks_says_so() {
        let mut d = full();
        d.checks.clear();
        d.spam_verdict.clear();
        d.virus_verdict.clear();
        let page = render(&d);
        assert!(page.contains("No delivery checks in the headers"), "{page}");
        assert!(!page.contains(">pass<"), "{page}");
    }

    /// The file is new, ends in the suffix, holds exactly the bytes and only the user can read it.
    #[test]
    fn write_makes_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir.path().join("pages"), b"<b>hi</b>", ".html").unwrap();
        assert_eq!(path.extension().unwrap(), "html");
        assert_eq!(std::fs::read(&path).unwrap(), b"<b>hi</b>");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = write(&dir.path().join("pages"), b"<b>hi</b>", ".html").unwrap();
        assert_ne!(path, again, "a second file overwrote the first");
    }
}
