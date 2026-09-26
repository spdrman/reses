//! Scrolling a decoded message without wrapping all of it.
//!
//! A stored message can be tens of megabytes. Wrapping the whole text on the UI thread every
//! time the terminal changes width froze the screen, so I keep the scroll position as a source
//! line plus a row inside it, and wrap a source line only when a screenful needs it. Opening,
//! resizing and jumping to either end each cost about one screen of wrapping.

use std::collections::HashMap;

use super::wrap_line;

/// A position in the wrapped text: source line, then row within that line's wrapped rows.
type Pos = (usize, usize);

pub(super) struct Pager {
    /// Where each source line starts in the text, found once per text.
    starts: Vec<usize>,
    /// Wrapped rows of the source lines visited so far, at `width`.
    cache: HashMap<usize, Vec<String>>,
    width: usize,
    /// The row at the top of the screen.
    top: Pos,
    /// Rows the last render showed, for PageUp and PageDown.
    page: usize,
}

impl Pager {
    /// A pager over `text`, at the top.
    pub(super) fn new(text: &str) -> Self {
        Self {
            starts: line_starts(text),
            cache: HashMap::new(),
            width: 0,
            top: (0, 0),
            page: 1,
        }
    }

    /// Switch to another text (the HTML part, say), back at the top.
    pub(super) fn reset(&mut self, text: &str) {
        *self = Self::new(text);
    }

    /// Rows the last render showed.
    pub(super) fn page(&self) -> usize {
        self.page
    }

    /// Source line `i` without its line ending.
    fn line<'t>(&self, text: &'t str, i: usize) -> &'t str {
        let start = self.starts[i];
        let end = self.starts.get(i + 1).map_or(text.len(), |&next| next - 1);
        let line = &text[start..end];
        let line = line.strip_suffix('\n').unwrap_or(line);
        line.strip_suffix('\r').unwrap_or(line)
    }

    /// Line `i`'s wrapped rows, wrapping it the first time it's asked for.
    fn rows(&mut self, text: &str, i: usize) -> &[String] {
        if !self.cache.contains_key(&i) {
            let rows = wrap_line(self.line(text, i), self.width);
            self.cache.insert(i, rows);
        }
        &self.cache[&i]
    }

    /// `n` rows further on from `pos`, stopping at the last row.
    fn advance(&mut self, text: &str, (mut line, mut row): Pos, mut n: usize) -> Pos {
        while n > 0 {
            if row + 1 < self.rows(text, line).len() {
                row += 1;
            } else if line + 1 < self.starts.len() {
                line += 1;
                row = 0;
            } else {
                break;
            }
            n -= 1;
        }
        (line, row)
    }

    /// `n` rows back from `pos`, stopping at the first row.
    fn retreat(&mut self, text: &str, (mut line, mut row): Pos, mut n: usize) -> Pos {
        while n > 0 {
            if row > 0 {
                row -= 1;
            } else if line > 0 {
                line -= 1;
                row = self.rows(text, line).len() - 1;
            } else {
                break;
            }
            n -= 1;
        }
        (line, row)
    }

    /// The furthest the top can go: a full screen above the last row.
    fn max_top(&mut self, text: &str) -> Pos {
        let Some(last_line) = self.starts.len().checked_sub(1) else {
            return (0, 0);
        };
        let last = (last_line, self.rows(text, last_line).len() - 1);
        self.retreat(text, last, self.page.saturating_sub(1))
    }

    /// Scroll down `n` rows, never past a full last screen.
    pub(super) fn down(&mut self, text: &str, n: usize) {
        let moved = self.advance(text, self.top, n);
        self.top = moved.min(self.max_top(text));
    }

    /// Scroll up `n` rows.
    pub(super) fn up(&mut self, text: &str, n: usize) {
        self.top = self.retreat(text, self.top, n);
    }

    /// I jump back to the first row of the first line.
    pub(super) fn home(&mut self) {
        self.top = (0, 0);
    }

    /// I jump to the last screenful, so the final row sits at the bottom rather than the top.
    pub(super) fn end(&mut self, text: &str) {
        self.top = self.max_top(text);
    }

    /// The rows for a `width` by `height` screen, from the top row down.
    ///
    /// A new width drops the wrapped rows and keeps the top on the same source line, so a
    /// resize doesn't jump somewhere else in the message.
    pub(super) fn screen(&mut self, text: &str, width: usize, height: usize) -> Vec<String> {
        if width != self.width {
            self.width = width;
            self.cache.clear();
            self.top.1 = 0;
        }
        self.page = height.max(1);
        if self.starts.is_empty() {
            return Vec::new();
        }
        // A taller screen than last time can leave the top below the last full screen.
        self.top = self.top.min(self.max_top(text));
        let mut out = Vec::with_capacity(self.page);
        let mut pos = self.top;
        loop {
            out.push(self.rows(text, pos.0)[pos.1].clone());
            let next = self.advance(text, pos, 1);
            if out.len() == self.page || next == pos {
                break;
            }
            pos = next;
        }
        out
    }
}

/// Where each line of `text` starts, the way `str::lines` splits it: a final newline doesn't
/// start another, empty, line.
fn line_starts(text: &str) -> Vec<usize> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' && i + 1 < text.len() {
            starts.push(i + 1);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I check my line index splits text exactly the way `str::lines` does, CRLF and trailing
    /// newlines included, since the pager relies on the two agreeing.
    #[test]
    fn line_starts_split_like_str_lines() {
        for text in [
            "",
            "a",
            "a\n",
            "a\nb",
            "a\r\nb\r\n",
            "\n\nx",
            "one\n\ntwo\n",
        ] {
            let pager = Pager::new(text);
            let got: Vec<&str> = (0..pager.starts.len())
                .map(|i| pager.line(text, i))
                .collect();
            let want: Vec<&str> = text.lines().collect();
            assert_eq!(got, want, "{text:?}");
        }
    }

    /// I check lazily wrapping one screen gives the same rows as wrapping the whole text up front,
    /// and that scrolling a row at a time visits each row once.
    #[test]
    fn the_screen_matches_wrapping_everything_up_front() {
        let text = "short\nthis line is long enough to wrap over several rows at ten\n\nend";
        let whole: Vec<String> = text.lines().flat_map(|l| wrap_line(l, 10)).collect();
        let mut pager = Pager::new(text);
        assert_eq!(pager.screen(text, 10, 100), whole);
        // Scrolling one row at a time visits each row once, and stops at a full last screen.
        let mut pager = Pager::new(text);
        let mut tops = vec![pager.screen(text, 10, 3)[0].clone()];
        for _ in 0..20 {
            pager.down(text, 1);
            tops.push(pager.screen(text, 10, 3)[0].clone());
        }
        tops.dedup();
        assert_eq!(tops, whole[..whole.len() - 2]);
    }
}
