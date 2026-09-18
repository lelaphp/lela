//! A lock file (design §4.5): comment lines, then one item per line. The
//! comments are a person's; the lines are the instrument's.
//!
//! **One order** (#403). A lock's body is what `render` writes from the
//! set it parsed: the lines ascending by `Ord` on their bytes, which is
//! `BTreeSet`'s own, each line once. `render` writes that order and
//! `disorder` reads it back, so the two are one statement in one place
//! and what `--write` writes is what a clean check accepts. Held as a
//! set alone, the check passed over a file `--write` then rewrote, and
//! `crates/php_heap/heap.lock` carried a swapped pair of lines into
//! every commit whose `--write` was taken for another seam.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Default)]
pub struct Lock {
    /// The leading comment lines, kept when the lock is rewritten.
    pub header: Vec<String>,
    pub lines: BTreeSet<String>,
    /// The lines as the file carries them, in the file's own order: what
    /// `disorder` judges. `lines` is the set a diff compares.
    pub as_written: Vec<String>,
}

impl Lock {
    pub fn read(path: &Path) -> Result<Lock> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Lock::parse(&text))
    }

    pub fn parse(text: &str) -> Lock {
        let mut lock = Lock::default();
        let mut in_header = true;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(comment) = line.strip_prefix('#') {
                if in_header {
                    lock.header.push(comment.trim().to_owned());
                }
                continue;
            }
            in_header = false;
            lock.lines.insert(line.to_owned());
            lock.as_written.push(line.to_owned());
        }
        lock
    }

    /// Where the file departs from the one order above, said in the words
    /// a reading prints, or `None` when the body is its own rendering.
    pub fn disorder(&self) -> Option<String> {
        if let Some((written, rendered)) = self
            .as_written
            .iter()
            .zip(&self.lines)
            .find(|(written, rendered)| written != rendered)
        {
            return Some(format!("{written} stands where {rendered} is written"));
        }
        match self.as_written.len().cmp(&self.lines.len()) {
            Ordering::Greater => Some(format!(
                "{} of its {} lines stand twice",
                self.as_written.len() - self.lines.len(),
                self.as_written.len()
            )),
            _ => None,
        }
    }

    /// The file with `lines` in place of the committed ones, header kept.
    pub fn render(&self, lines: &BTreeSet<String>) -> String {
        let mut out = String::new();
        for comment in &self.header {
            out.push_str("# ");
            out.push_str(comment);
            out.push('\n');
        }
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// What differs between the committed lines and the regenerated ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// Items with no line: added without a lock line.
    pub missing: Vec<String>,
    /// Lines that name no item any more.
    pub stale: Vec<String>,
}

impl Diff {
    pub fn between(committed: &BTreeSet<String>, generated: &BTreeSet<String>) -> Diff {
        Diff {
            missing: generated.difference(committed).cloned().collect(),
            stale: committed.difference(generated).cloned().collect(),
        }
    }
}

/// A list for a message: at most `limit` names, then a count of the rest.
pub fn list(names: &[String], limit: usize) -> String {
    if names.len() <= limit {
        names.join(", ")
    } else {
        format!(
            "{}, and {} more",
            names[..limit].join(", "),
            names.len() - limit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_keeps_its_header_and_sorts_its_lines() {
        let lock = Lock::parse("# one\n# two\n\nb::y\na::x\n# not a header\nc\n");
        assert_eq!(lock.header, ["one", "two"]);
        assert_eq!(lock.lines.iter().collect::<Vec<_>>(), ["a::x", "b::y", "c"]);
        assert_eq!(lock.as_written, ["b::y", "a::x", "c"]);
        let lines: BTreeSet<String> = ["z".to_owned()].into_iter().collect();
        assert_eq!(lock.render(&lines), "# one\n# two\nz\n");
    }

    /// The one order, read both ways (#403): a file `render` would write
    /// unchanged has no disorder, one with a pair swapped names the line
    /// that stands and the line written in its place, and one with a line
    /// twice says so. `Arr::live_from` before `Arr::len` is the pair
    /// `crates/php_heap/heap.lock` carried.
    #[test]
    fn a_lock_that_is_not_its_own_rendering_says_where_it_departs() {
        let sorted = Lock::parse("# a header\nArr::len\nArr::live_from\n");
        assert_eq!(sorted.disorder(), None);
        assert_eq!(
            sorted.render(&sorted.lines),
            "# a header\nArr::len\nArr::live_from\n"
        );

        let swapped = Lock::parse("Arr::live_from\nArr::len\n");
        assert_eq!(
            swapped.disorder().as_deref(),
            Some("Arr::live_from stands where Arr::len is written")
        );

        let twice = Lock::parse("Arr::len\nArr::live_from\nArr::live_from\n");
        assert_eq!(
            twice.disorder().as_deref(),
            Some("1 of its 3 lines stand twice")
        );
    }

    #[test]
    fn a_diff_names_what_is_missing_and_what_is_stale() {
        let committed: BTreeSet<String> = ["a", "b"].map(str::to_owned).into_iter().collect();
        let generated: BTreeSet<String> = ["b", "c"].map(str::to_owned).into_iter().collect();
        let diff = Diff::between(&committed, &generated);
        assert_eq!(diff.missing, ["c"]);
        assert_eq!(diff.stale, ["a"]);
        assert_eq!(Diff::between(&committed, &committed), Diff::default());
    }
}
