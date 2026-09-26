//! What happens to a file reses has just written from a message: an attachment, or a decoded
//! message's text.
//!
//! On macOS I give it the `com.apple.quarantine` attribute a browser or Mail would, so
//! Gatekeeper asks before anything in it runs: an attachment came from a stranger. Setting it
//! can fail (a filesystem without extended attributes, say), and that must never cost the user
//! the file, so it only ever adds a note. Everywhere else there's nothing to set.
//!
//! The saved names can differ from the ones shown in the message (a clash becomes name-1.pdf),
//! so the status line lists what was actually written.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::text::escape;

/// The quarantine value: flags 0081 (downloaded, not yet opened), the time in hex seconds, and
/// the agent that wrote it, in the format LaunchServices uses.
pub fn quarantine_value(unix_secs: u64) -> String {
    format!("0081;{unix_secs:08x};reses;")
}

/// Mark `path` as downloaded, on macOS. A no-op that succeeds everywhere else.
pub fn quarantine(path: &Path) -> io::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    set_quarantine(path, &quarantine_value(now))
}

#[cfg(target_os = "macos")]
fn set_quarantine(path: &Path, value: &str) -> io::Result<()> {
    rustix::fs::setxattr(
        path,
        "com.apple.quarantine",
        value.as_bytes(),
        rustix::fs::XattrFlags::empty(),
    )
    .map_err(io::Error::from)
}

#[cfg(not(target_os = "macos"))]
fn set_quarantine(path: &Path, value: &str) -> io::Result<()> {
    let _ = (path, value);
    Ok(())
}

/// Quarantine every file in `paths`, and say how it went: None when every one was marked, or
/// a short note naming the first failure otherwise. The files stay saved either way.
pub fn quarantine_all(paths: &[PathBuf]) -> Option<String> {
    let failed: Vec<(PathBuf, io::Error)> = paths
        .iter()
        .filter_map(|p| quarantine(p).err().map(|e| (p.clone(), e)))
        .collect();
    let (first, e) = failed.first()?;
    Some(format!(
        "couldn't mark {} of them as downloaded ({}: {e})",
        failed.len(),
        escape(&first.display().to_string())
    ))
}

/// The saved files' names for the status line, escaped, as written on disk.
pub fn names(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| {
            let name = p.file_name().map_or_else(
                || p.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            escape(&name).into_owned()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quarantine_value_has_the_launchservices_shape() {
        assert_eq!(quarantine_value(0), "0081;00000000;reses;");
        assert_eq!(quarantine_value(0x66f4_1c00), "0081;66f41c00;reses;");
    }

    #[test]
    fn quarantining_never_fails_a_save_it_cannot_mark() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.pdf");
        std::fs::write(&file, b"x").unwrap();
        // A file that's gone can't take an attribute: that's a note, not an error, and on
        // anything but macOS there's nothing to set at all.
        let missing = dir.path().join("gone.pdf");
        let note = quarantine_all(&[file.clone(), missing]);
        if cfg!(target_os = "macos") {
            assert!(note.unwrap().contains("gone.pdf"));
        } else {
            assert_eq!(note, None);
        }
        assert!(file.exists());
    }

    #[test]
    fn names_are_the_ones_on_disk_escaped() {
        let paths = [
            PathBuf::from("/tmp/dl/note-1.txt"),
            PathBuf::from("/tmp/dl/evil\u{1b}]52;c;eA==\u{7}.pdf"),
        ];
        assert_eq!(names(&paths), "note-1.txt, evil\\x1b]52;c;eA==\\x07.pdf");
    }
}
