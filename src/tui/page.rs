//! A message's HTML part as a page for the browser.
//!
//! `h` on the message screen shows the HTML part the way it was meant to be seen, in the
//! default browser. The HTML is untrusted: it's whatever the sender wrote, and senders put
//! tracking pixels, remote fonts and sometimes scripts in mail. So before it goes near a browser
//! I put a Content-Security-Policy at the top of the page. The policy blocks every fetch and
//! script, and allows only inline styles and `data:` images and fonts, which is what makes a
//! message look right without phoning home. A policy the message carries itself can only
//! tighten that, since browsers apply every policy on a page. Links still open when clicked,
//! because following one is the reader's choice.
//!
//! The page is written to its own file (readable only by the user) and kept there, because the
//! browser reads it after reses has moved on. It lives in the system temp dir, which the OS
//! clears.

use std::io;
use std::path::{Path, PathBuf};

/// What I put in front of every page: the charset, since the HTML part was decoded to UTF-8,
/// and the policy that stops the page fetching or running anything.
pub(crate) const GUARD: &str = "";

/// I return `html` with [`GUARD`] in front of it, after a leading doctype if there is one, so
/// the page doesn't drop into quirks mode.
pub(crate) fn guarded(html: &str) -> String {
    html.to_string()
}

/// I write `html`, guarded, to a new `.html` file in `dir` that only the user can read, and
/// return its path. `dir` is created if it doesn't exist yet.
pub(crate) fn write(dir: &Path, html: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let (mut file, path) = tempfile::Builder::new()
        .prefix("reses-")
        .suffix(".html")
        .tempfile_in(dir)?
        .keep()
        .map_err(|e| e.error)?;
    io::Write::write_all(&mut file, guarded(html).as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy comes first and shuts off every fetch, script and form post, leaving only
    /// inline styles and `data:` images and fonts.
    #[test]
    fn the_page_starts_with_a_policy_that_blocks_everything_remote() {
        let page = guarded("<p>hi <img src=\"https://tracker.example/p.gif\"></p>");
        assert!(page.starts_with("<meta charset=\"utf-8\">"), "{page}");
        for rule in [
            "default-src 'none'",
            "img-src data:",
            "style-src 'unsafe-inline'",
            "font-src data:",
            "form-action 'none'",
            "base-uri 'none'",
        ] {
            assert!(page.contains(rule), "{rule} missing: {page}");
        }
        assert!(!page.contains("script-src"), "{page}");
        assert!(page.ends_with("<p>hi <img src=\"https://tracker.example/p.gif\"></p>"));
    }

    /// A doctype stays the very first thing on the page, whatever its case, with the guard
    /// right after it.
    #[test]
    fn a_doctype_stays_first() {
        let page = guarded("<!DOCTYPE html>\n<html><body>x</body></html>");
        assert!(page.starts_with("<!DOCTYPE html>"), "{page}");
        assert!(page[15..].starts_with(GUARD), "{page}");
        let page = guarded("  <!doctype html><p>x</p>");
        assert!(page.starts_with("  <!doctype html>"), "{page}");
        assert!(page.contains("<!doctype html><meta charset"), "{page}");
    }

    /// The file is new, ends in `.html`, holds the guarded page and only the user can read it.
    #[test]
    fn write_makes_a_private_html_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir.path().join("pages"), "<b>hi</b>").unwrap();
        assert_eq!(path.extension().unwrap(), "html");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), guarded("<b>hi</b>"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = write(&dir.path().join("pages"), "<b>hi</b>").unwrap();
        assert_ne!(path, again, "a second page overwrote the first");
    }
}
