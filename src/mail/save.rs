//! Writing attachments to disk without ever overwriting anything.
//!
//! Each file is opened with `create_new`, so an existing file, a dangling symlink included, is
//! never replaced even if it turns up between two checks. A name that's taken moves on to
//! "stem-1.ext", "stem-2.ext" and so on. The panel found that a message with 32k parts of one name
//! took twenty minutes (N18), because every part retried every suffix from 1; now each name
//! remembers where its suffixes got to, and one save writes at most `SaveReport::MAX` files and
//! says how many it left.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// What one save did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveReport {
    /// The files written, in the order the attachments appear.
    pub saved: Vec<PathBuf>,
    /// Attachments past the cap, which weren't written.
    pub skipped: usize,
    /// Attachments that couldn't be written (a full disk, say), each skipped on its own.
    pub failed: usize,
}

impl SaveReport {
    /// The most attachments one save writes.
    pub const MAX: usize = 1000;
}

/// Longest attachment name I write, in bytes. Filesystems stop at 255, and this leaves room for
/// the "-N" that keeps a name unique.
const MAX_NAME_BYTES: usize = 200;

/// Characters that make a name lie about itself on screen: controls, and the bidi formatting
/// characters that can reorder it ("invoice\u{202e}fdp.exe" shows as "invoiceexe.pdf").
fn is_deceptive(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// The name an attachment is saved under: its last path component, so it can't point outside
/// `dir`, with deceptive characters replaced and the length capped. A leading dot stays.
pub(super) fn safe_file_name(name: &str) -> String {
    let base = name
        .split(['/', '\\'])
        .rfind(|c| !c.is_empty() && *c != ".")
        .unwrap_or("");
    if base.is_empty() || base == ".." {
        return "attachment".into();
    }
    let clean: String = base
        .chars()
        .map(|c| if is_deceptive(c) { '_' } else { c })
        .collect();
    if clean.len() <= MAX_NAME_BYTES {
        return clean;
    }
    // Too long: cut the stem on a character boundary and keep a sensible extension.
    let (stem, suffix) = stem_suffix(&clean);
    let suffix = if suffix.len() <= 32 { suffix } else { "" };
    let stem = if suffix.is_empty() { clean.as_str() } else { stem };
    let mut budget = MAX_NAME_BYTES - suffix.len();
    while !stem.is_char_boundary(budget) {
        budget -= 1;
    }
    format!("{}{suffix}", &stem[..budget])
}

/// pathlib's stem and suffix: the extension is from the last dot, unless that dot starts or
/// ends the name.
fn stem_suffix(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Write one file under the first free name at or after suffix `*next`, moving `*next` past it.
fn write_one(dir: &Path, name: &str, data: &[u8], next: &mut usize) -> io::Result<PathBuf> {
    let (stem, suffix) = stem_suffix(name);
    loop {
        let candidate = if *next == 0 {
            name.to_string()
        } else {
            format!("{stem}-{next}{suffix}")
        };
        *next += 1;
        let target = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(data) {
                    drop(file);
                    // Only the half-written file I just created goes.
                    let _ = std::fs::remove_file(&target);
                    return Err(e);
                }
                return Ok(target);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// Write `(name, bytes)` pairs into `dir` under fresh, safe names, up to `SaveReport::MAX` of
/// them. One that can't be written is skipped so the rest still land; the error only comes back
/// if nothing could be written.
pub(super) fn save_all(dir: &Path, items: Vec<(String, Vec<u8>)>) -> io::Result<SaveReport> {
    let mut report = SaveReport {
        skipped: items.len().saturating_sub(SaveReport::MAX),
        ..SaveReport::default()
    };
    let mut next: HashMap<String, usize> = HashMap::new();
    let mut first_error = None;
    for (name, data) in items.into_iter().take(SaveReport::MAX) {
        let name = safe_file_name(&name);
        let counter = next.entry(name.clone()).or_insert(0);
        match write_one(dir, &name, &data, counter) {
            Ok(path) => report.saved.push(path),
            Err(e) => {
                report.failed += 1;
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) if report.saved.is_empty() => Err(e),
        _ => Ok(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write that fails skips that attachment only, and the error comes back only when
    /// nothing was written at all.
    #[test]
    fn a_failed_write_skips_that_attachment_only() {
        let dir = tempfile::tempdir().unwrap();
        // 300 bytes is past every filesystem's name limit, so this one write fails. It goes in
        // through save_all's items, which would normally already be safe names, to force that.
        let long = "x".repeat(300);
        let mut next = 0;
        assert!(write_one(dir.path(), &long, b"2", &mut next).is_err());
        let report = save_all(
            dir.path(),
            vec![
                ("first.txt".into(), b"1".to_vec()),
                ("third.txt".into(), b"3".to_vec()),
            ],
        )
        .unwrap();
        assert_eq!(report.saved.len(), 2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // When nothing can be written at all, the error comes back.
        let missing = dir.path().join("not-a-dir");
        assert!(save_all(&missing, vec![("a".into(), b"a".to_vec())]).is_err());
    }

    #[test]
    fn file_names() {
        assert_eq!(safe_file_name("../../etc/evil.bin"), "evil.bin");
        assert_eq!(safe_file_name("C:\\x\\y.doc"), "y.doc");
        assert_eq!(safe_file_name("dir/"), "dir");
        assert_eq!(safe_file_name(".."), "attachment");
        assert_eq!(safe_file_name("a\u{202e}b.txt"), "a_b.txt");
        let long = safe_file_name(&format!("{}.pdf", "é".repeat(150)));
        assert!(long.len() <= MAX_NAME_BYTES && long.ends_with(".pdf"), "{long}");
        assert_eq!(stem_suffix("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(stem_suffix(".profile"), (".profile", ""));
        assert_eq!(stem_suffix("trailing."), ("trailing.", ""));
    }

    /// Suffixes carry on per name rather than starting again from 1.
    #[test]
    fn suffixes_carry_on() {
        let dir = tempfile::tempdir().unwrap();
        let items = (0..4).map(|_| ("a.txt".to_string(), b"x".to_vec())).collect();
        let report = save_all(dir.path(), items).unwrap();
        let names: Vec<_> = report
            .saved
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.txt", "a-1.txt", "a-2.txt", "a-3.txt"]);
    }
}
