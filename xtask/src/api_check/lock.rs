//! A lock file (design §4.5): comment lines, then one item per line. The
//! comments are a person's; the lines are the instrument's.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Default)]
pub struct Lock {
    /// The leading comment lines, kept when the lock is rewritten.
    pub header: Vec<String>,
    pub lines: BTreeSet<String>,
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
        }
        lock
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
        let lines: BTreeSet<String> = ["z".to_owned()].into_iter().collect();
        assert_eq!(lock.render(&lines), "# one\n# two\nz\n");
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
