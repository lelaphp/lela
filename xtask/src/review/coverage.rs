//! A merge's class and whether its branch was reviewed, as Mitchell ruled
//! in Round 3: the class by the `[review]` table (Q27 (c), Q28 (a)), and
//! what counts as reviewed (Q26 (c)): the branch at an explicit head, with
//! all four of
//!
//! 1. reviewed ranges chaining back to the merge base: the first starting
//!    at a commit of `main`, each next at a commit the one before reached;
//! 2. no commit past the last head but what `main` brought or a merge;
//! 3. no merge past that head carrying a resolution, a tree other than the
//!    one git makes of its parents unaided;
//! 4. every reading taken before the merge's commit time, so a review made
//!    after a merge, as the experiment's of past merges are, stays in the
//!    store and out of coverage.
//!
//! Where a chain reaches a head and something stands past it, the branch
//! is partial, by the count of what stands; where no chain starts on
//! `main`, partial by what no reviewed range holds; where no review reading
//! predates the merge, unreviewed. `worktree land` writes one reading per
//! merge, best effort, after it moves `main` (Round 4, Q35). The ready
//! record is left alone, and no send-back state is kept.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};

use super::{DAYS, Findings, INSTRUMENT, Scope, digest, is_ancestor};
use crate::git::git;
use crate::manifest::{Review, scope_matches};
use crate::reading::{self, Number, Reading, Status, Store};
use crate::track::{MAIN, short};

/// The group kind a range's reading names its first commit under.
const FROM: &str = "from";
/// The group kinds of the review a range's reading records.
const ROLE: &str = "role";
const REVIEWER: &str = "reviewer";
const BUILDER: &str = "builder";
/// The group kind a merge's reading names each class A crate under.
const CRATE: &str = "crate";
/// The group kind a merge's reading names its branch's tip under.
const TIP: &str = "tip";

/// The numbers a merge's reading carries.
const CLASS_A: &str = "class_a";
const UNSAFE_LINES: &str = "unsafe_lines";
const MERGED_AT: &str = "merged_at";
const COVERED: &str = "covered";
const PARTIAL: &str = "partial";
const UNREVIEWED: &str = "unreviewed";
/// The number a range's reading counts its findings under.
const FINDINGS: &str = "findings";

/// What a merge's diff is to review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// Touches a crate the table names, or changes an `unsafe` line.
    A {
        crates: Vec<String>,
        unsafe_lines: usize,
    },
    /// Every other diff, and every diff where the manifest has no table.
    B,
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Class::B => f.write_str("class B"),
            Class::A {
                crates,
                unsafe_lines,
            } => {
                let mut why: Vec<String> = crates.clone();
                if *unsafe_lines > 0 {
                    why.push(format!(
                        "{unsafe_lines} unsafe line{}",
                        reading::plural(*unsafe_lines)
                    ));
                }
                write!(f, "class A ({})", why.join(", "))
            }
        }
    }
}

impl Class {
    /// The class of the diff from `base` to `tip` under `table`: class B
    /// throughout where the manifest has none.
    pub fn of(root: &Path, table: Option<&Review>, base: &str, tip: &str) -> Result<Class> {
        let Some(table) = table else {
            return Ok(Class::B);
        };
        let range = format!("{base}..{tip}");
        let files = git(root, &["diff", "--name-only", "--no-renames", &range])?;
        let crates: Vec<String> = table
            .crates
            .iter()
            .filter(|name| {
                let path = crate::layout::path(name);
                files
                    .lines()
                    .any(|file| scope_matches(&path.to_string_lossy(), Path::new(file)))
            })
            .cloned()
            .collect();
        let unsafe_lines = if table.unsafe_lines {
            let diff = git(root, &["diff", "-U0", "--no-renames", &range, "--", "*.rs"])?;
            changed_unsafe_lines(&diff)
        } else {
            0
        };
        Ok(if crates.is_empty() && unsafe_lines == 0 {
            Class::B
        } else {
            Class::A {
                crates,
                unsafe_lines,
            }
        })
    }

    fn is_a(&self) -> bool {
        matches!(self, Class::A { .. })
    }
}

/// How many lines a unified diff adds or removes that name `unsafe` as a
/// word of code: outside a comment and a string on its line.
pub(crate) fn changed_unsafe_lines(diff: &str) -> usize {
    diff.lines()
        .filter(|l| !l.starts_with("+++") && !l.starts_with("---"))
        .filter_map(|l| l.strip_prefix('+').or_else(|| l.strip_prefix('-')))
        .filter(|l| names_unsafe(l))
        .count()
}

/// Whether one line of Rust names the keyword `unsafe` in its code: the
/// text before a `//` that no string holds, less every string's contents,
/// on a line that does not open or continue a block comment.
fn names_unsafe(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return false;
    }
    let mut code = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                quoted = true;
                code.push(' ');
            }
            '/' if chars.peek() == Some(&'/') => break,
            _ => code.push(c),
        }
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices("unsafe").any(|(at, _)| {
        let before = code[..at].chars().next_back();
        let after = code[at + "unsafe".len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

/// What a branch's reviews came to at its merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// All four conditions hold.
    Covered,
    /// A review predates the merge and the four do not hold; the count is
    /// what stands outside the reviewed ranges, commits and resolved
    /// merges, and is zero where every commit was read and the ranges do
    /// not chain back to the base.
    Partial(usize),
    /// No review reading predates the merge.
    Unreviewed,
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Coverage::Covered => f.write_str("covered"),
            Coverage::Partial(n) => write!(f, "partial, {n} standing outside the reviewed ranges"),
            Coverage::Unreviewed => f.write_str("unreviewed"),
        }
    }
}

/// One review's range, as its reading records it: from a commit to the
/// head it read, and when the reading was taken, in seconds since 1970.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Range {
    pub(crate) from: String,
    pub(crate) head: String,
    pub(crate) at: i64,
}

impl Range {
    /// The reading of the review `file` records over `from..head`.
    pub(crate) fn reading(file: &Findings, from: &str, head: &str) -> Result<Reading> {
        let scope = Scope::Range {
            ticket: file.ticket,
            head: head.to_owned(),
        };
        let key = digest(&[from, head, file.role.trim(), file.reviewer.trim()]);
        let mut reading = Reading::keyed(INSTRUMENT, &scope.to_string(), key)?;
        head.clone_into(&mut reading.commit);
        for (kind, value) in [
            (FROM, from),
            (ROLE, file.role.trim()),
            (REVIEWER, file.reviewer.trim()),
            (BUILDER, file.builder.trim()),
        ] {
            reading.group(&format!("{kind}:{value}"), 1)?;
        }
        let found = file.findings.len();
        reading.count(FINDINGS, found as u64)?;
        reading.status = Status::of(found == 0);
        reading.message = format!(
            "{} read {}..{} of #{} as {}: {found} finding{}",
            file.reviewer.trim(),
            short(from),
            short(head),
            file.ticket,
            file.role.trim(),
            reading::plural(found)
        );
        Ok(reading)
    }

    /// The range a reading records, where it is a range's.
    fn of(reading: &Reading) -> Option<Result<Range>> {
        let Some(Scope::Range { .. }) = Scope::of(&reading.scope) else {
            return None;
        };
        let from = reading
            .groups
            .keys()
            .find_map(|k| k.strip_prefix(FROM)?.strip_prefix(':'));
        Some((|| {
            let from = from.with_context(|| format!("{} names no first commit", reading.scope))?;
            Ok(Range {
                from: from.to_owned(),
                head: reading.commit.clone(),
                at: reading::epoch_seconds(&reading.at)?,
            })
        })())
    }

    /// Every range the store's log holds, in its order; a range's reading
    /// that names no first commit is refused by its scope.
    pub(crate) fn all(store: &Store) -> Result<Vec<Range>> {
        store
            .history(INSTRUMENT)?
            .readings
            .iter()
            .filter_map(Range::of)
            .collect()
    }
}

/// Whether the branch at `tip`, merged at `merged_at` into `main` where
/// it stood at `main`, was covered by `ranges`: the four conditions of this
/// module.
pub(crate) fn judge(
    root: &Path,
    main: &str,
    tip: &str,
    merged_at: i64,
    ranges: &[Range],
) -> Result<Coverage> {
    // Condition 4, and a range of this branch: its head one of the commits
    // the branch brings, each range once.
    let brought: BTreeSet<String> = git(root, &["rev-list", tip, &format!("^{main}")])?
        .lines()
        .map(str::to_owned)
        .collect();
    let mut read: Vec<&Range> = Vec::new();
    for r in ranges {
        let same = |held: &&Range| held.from == r.from && held.head == r.head;
        if r.at < merged_at && brought.contains(&r.head) && !read.iter().any(same) {
            read.push(r);
        }
    }
    if read.is_empty() {
        return Ok(Coverage::Unreviewed);
    }
    // Condition 1: the ranges that chain back to a commit of `main`.
    let mut chained: Vec<&Range> = read
        .iter()
        .copied()
        .filter(|r| is_ancestor(root, &r.from, main))
        .collect();
    loop {
        let next: Vec<&Range> = read
            .iter()
            .copied()
            .filter(|r| !chained.contains(r))
            .filter(|r| chained.iter().any(|c| is_ancestor(root, &r.from, &c.head)))
            .collect();
        if next.is_empty() {
            break;
        }
        chained.extend(next);
    }
    if chained.is_empty() {
        let unread = Standing::of(root, tip, &[main])?;
        let mut left = 0;
        for commit in unread.commits {
            let inside = read.iter().any(|r| {
                is_ancestor(root, &commit.id, &r.head) && !is_ancestor(root, &commit.id, &r.from)
            });
            if !inside && (!commit.merge() || commit.resolved(root)?) {
                left += 1;
            }
        }
        return Ok(Coverage::Partial(left));
    }
    // Conditions 2 and 3, past the chained head that leaves the least.
    let mut least = usize::MAX;
    let heads: BTreeSet<&str> = chained.iter().map(|r| r.head.as_str()).collect();
    for head in heads {
        least = least.min(Standing::of(root, tip, &[main, head])?.count(root)?);
        if least == 0 {
            return Ok(Coverage::Covered);
        }
    }
    Ok(Coverage::Partial(least))
}

/// One commit a branch brings, with its parents.
struct Commit {
    id: String,
    parents: Vec<String>,
}

impl Commit {
    fn merge(&self) -> bool {
        self.parents.len() > 1
    }

    /// Whether a merge carries a resolution: a tree other than the one git
    /// makes of its two parents unaided, a conflict among them, or more
    /// than two parents, which no unaided merge of two makes.
    fn resolved(&self, root: &Path) -> Result<bool> {
        let [first, second] = self.parents.as_slice() else {
            return Ok(true);
        };
        let Ok(made) = git(
            root,
            &["merge-tree", "--write-tree", "--no-messages", first, second],
        ) else {
            return Ok(true);
        };
        let tree = git(root, &["rev-parse", &format!("{}^{{tree}}", self.id)])?;
        Ok(made.lines().next().map(str::trim) != Some(tree.trim()))
    }
}

/// The commits reachable from a tip and from none of the commits it is
/// held against.
struct Standing {
    commits: Vec<Commit>,
}

impl Standing {
    fn of(root: &Path, tip: &str, against: &[&str]) -> Result<Standing> {
        let mut args = vec![
            "rev-list".to_owned(),
            "--parents".to_owned(),
            tip.to_owned(),
        ];
        args.extend(against.iter().map(|a| format!("^{a}")));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let listed = git(root, &args)?;
        let commits = listed
            .lines()
            .filter_map(|line| {
                let mut ids = line.split_whitespace().map(str::to_owned);
                Some(Commit {
                    id: ids.next()?,
                    parents: ids.collect(),
                })
            })
            .collect();
        Ok(Standing { commits })
    }

    /// What stands: each commit that is no merge, and each merge that
    /// carries a resolution.
    fn count(&self, root: &Path) -> Result<usize> {
        let mut n = 0;
        for commit in &self.commits {
            if !commit.merge() || commit.resolved(root)? {
                n += 1;
            }
        }
        Ok(n)
    }
}

/// One merge into `main`, classed and judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Judged {
    commit: String,
    tip: String,
    merged_at: i64,
    class: Class,
    coverage: Coverage,
    /// Whether the manifest had a `[review]` table to class by.
    tabled: bool,
}

impl Judged {
    /// The merge `commit` of `main`, its second parent the branch's tip,
    /// classed by `table` and judged against `ranges`.
    pub(crate) fn judge(
        root: &Path,
        table: Option<&Review>,
        commit: &str,
        ranges: &[Range],
    ) -> Result<Judged> {
        let line = git(root, &["rev-list", "--parents", "-n", "1", commit])?;
        let ids: Vec<&str> = line.split_whitespace().collect();
        let [_, main, tip, ..] = ids.as_slice() else {
            anyhow::bail!(
                "{} is no merge, so no branch was merged by it",
                short(commit)
            );
        };
        let merged_at: i64 = git(root, &["log", "-1", "--format=%ct", commit])?
            .trim()
            .parse()
            .with_context(|| format!("the commit time of {}", short(commit)))?;
        let base = git(root, &["merge-base", main, tip])?;
        let class = Class::of(root, table, base.trim(), tip)?;
        let coverage = judge(root, main, tip, merged_at, ranges)?;
        Ok(Judged {
            commit: commit.to_owned(),
            tip: (*tip).to_owned(),
            merged_at,
            class,
            coverage,
            tabled: table.is_some(),
        })
    }

    /// The merge's reading: a class B merge is skipped, since its coverage
    /// is no one's to read; a class A one passes when covered.
    pub(crate) fn reading(&self) -> Result<Reading> {
        let scope = Scope::Merge {
            commit: self.commit.clone(),
        };
        let mut reading = Reading::keyed(INSTRUMENT, &scope.to_string(), self.commit.clone())?;
        reading.commit.clone_from(&self.commit);
        reading.status = match (&self.class, self.coverage) {
            (Class::B, _) => Status::Skip,
            (Class::A { .. }, coverage) => Status::of(coverage == Coverage::Covered),
        };
        reading.count(CLASS_A, u64::from(self.class.is_a()))?;
        reading.set(MERGED_AT, Number::count(self.merged_at));
        match self.coverage {
            Coverage::Covered => reading.count(COVERED, 1)?,
            Coverage::Partial(n) => reading.count(PARTIAL, n as u64)?,
            Coverage::Unreviewed => reading.count(UNREVIEWED, 1)?,
        }
        if let Class::A {
            crates,
            unsafe_lines,
        } = &self.class
        {
            for name in crates {
                reading.group(&format!("{CRATE}:{name}"), 1)?;
            }
            reading.count(UNSAFE_LINES, *unsafe_lines as u64)?;
        }
        reading.group(&format!("{TIP}:{}", self.tip), 1)?;
        let class = if self.tabled {
            self.class.to_string()
        } else {
            "class B, seams.toml having no `[review]` table".to_owned()
        };
        reading.message = format!(
            "merge {} of {}: {class}, {}",
            short(&self.commit),
            short(&self.tip),
            self.coverage
        );
        Ok(reading)
    }
}

/// What a merge's reading says, as the local facts count it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counted {
    pub(crate) class_a: bool,
    pub(crate) coverage: Coverage,
    pub(crate) merged_at: i64,
}

impl Counted {
    /// The merge a reading judged, where it is a merge's.
    pub(crate) fn of(reading: &Reading) -> Option<Counted> {
        let Some(Scope::Merge { .. }) = Scope::of(&reading.scope) else {
            return None;
        };
        let coverage = if reading.int(COVERED).is_some() {
            Coverage::Covered
        } else if let Some(n) = reading.int(PARTIAL) {
            Coverage::Partial(usize::try_from(n).ok()?)
        } else {
            reading.int(UNREVIEWED)?;
            Coverage::Unreviewed
        };
        Some(Counted {
            class_a: reading.int(CLASS_A)? > 0,
            coverage,
            merged_at: reading.int(MERGED_AT)?,
        })
    }
}

/// The merges of `main`'s first parents in the last [`DAYS`] days that
/// the store holds no reading of, oldest first.
pub(crate) fn unjudged(root: &Path, store: &Store) -> Result<Vec<String>> {
    let since = format!("--since={DAYS} days ago");
    let listed = git(
        root,
        &[
            "rev-list",
            "--first-parent",
            "--merges",
            "--reverse",
            &since,
            MAIN,
        ],
    )?;
    let mut left = Vec::new();
    for commit in listed.lines().map(str::trim).filter(|c| !c.is_empty()) {
        let scope = Scope::Merge {
            commit: commit.to_owned(),
        };
        if store.last_of(INSTRUMENT, &scope.to_string())?.is_none() {
            left.push(commit.to_owned());
        }
    }
    Ok(left)
}
