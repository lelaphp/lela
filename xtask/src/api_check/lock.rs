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
use std::path::Path;

use anyhow::Result;

#[derive(Debug, Clone, Default)]
pub struct Lock {
    /// The leading comment lines, kept when the lock is rewritten.
    pub header: Vec<String>,
    /// Every line, whichever block it stands in: the set a diff compares.
    pub lines: BTreeSet<String>,
    /// The body in the file's order: the lines before any `[name]` marker,
    /// which are every line of a lock without sections, then one block per
    /// marker (#1369).
    pub blocks: Vec<Block>,
}

/// One block of a lock's body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    /// The section a `[name]` marker opens; `None` before the first marker.
    pub name: Option<String>,
    pub lines: BTreeSet<String>,
    /// The lines as the file carries them, in its own order: what
    /// `disorder` judges.
    pub as_written: Vec<String>,
}

impl Block {
    /// A block to render: `lines` under `name`'s marker, or under none.
    pub fn of(name: Option<&str>, lines: BTreeSet<String>) -> Block {
        Block {
            name: name.map(str::to_owned),
            lines,
            as_written: Vec::new(),
        }
    }
}

impl Lock {
    pub fn read(path: &Path) -> Result<Lock> {
        let text = crate::read(path)?;
        Ok(Lock::parse(&text))
    }

    pub fn parse(text: &str) -> Lock {
        let mut lock = Lock::default();
        let mut block = Block::default();
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
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let next = Block::of(Some(name), BTreeSet::new());
                lock.blocks.push(std::mem::replace(&mut block, next));
                continue;
            }
            lock.lines.insert(line.to_owned());
            block.lines.insert(line.to_owned());
            block.as_written.push(line.to_owned());
        }
        lock.blocks.push(block);
        lock
    }

    /// The block a `[name]` marker opens, if the file has one.
    pub fn section(&self, name: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.name.as_deref() == Some(name))
    }

    /// Where the file departs from the one order above, said in the words
    /// a reading prints, or `None` when the body is its own rendering.
    pub fn disorder(&self) -> Option<String> {
        for block in &self.blocks {
            if let Some((written, rendered)) = block
                .as_written
                .iter()
                .zip(&block.lines)
                .find(|(written, rendered)| written != rendered)
            {
                return Some(format!("{written} stands where {rendered} is written"));
            }
        }
        let written: usize = self.blocks.iter().map(|b| b.as_written.len()).sum();
        match written.cmp(&self.lines.len()) {
            Ordering::Greater => Some(format!(
                "{} of its {written} lines stand twice",
                written - self.lines.len(),
            )),
            _ => None,
        }
    }

    /// The file with `blocks` in place of the committed body, header kept:
    /// each block's marker, then its lines.
    pub fn render(&self, blocks: &[Block]) -> String {
        let mut out = String::new();
        for comment in &self.header {
            out.push_str("# ");
            out.push_str(comment);
            out.push('\n');
        }
        for block in blocks {
            if let Some(name) = &block.name {
                out.push('[');
                out.push_str(name);
                out.push_str("]\n");
            }
            for line in &block.lines {
                out.push_str(line);
                out.push('\n');
            }
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
        assert_eq!(lock.blocks[0].as_written, ["b::y", "a::x", "c"]);
        let lines: BTreeSet<String> = ["z".to_owned()].into_iter().collect();
        assert_eq!(lock.render(&[Block::of(None, lines)]), "# one\n# two\nz\n");
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
            sorted.render(&sorted.blocks),
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
