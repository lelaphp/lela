//! What a reading records about the moment it was taken: the git tree hash
//! of its scope's sources, the commit, and the time. The tree hash is what
//! makes a reading fresh or stale without a turn counter (harness-design.md
//! §4.2): it covers the working tree, tracked and untracked files alike, so
//! an unstaged edit changes it.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use toml::value::{Datetime, Offset, Time};

#[derive(Debug, Clone)]
pub struct Stamp {
    pub tree: String,
    pub commit: String,
    pub at: Datetime,
}

impl Stamp {
    /// Stamp the sources under `scope` (relative to `root`) now.
    pub fn take(root: &Path, scope: &Path) -> Result<Stamp> {
        Ok(Stamp {
            tree: tree_hash(root, scope)?,
            commit: head_commit(root)?,
            at: now()?,
        })
    }
}

/// The short id of `HEAD`, read from the repository's files; `git
/// rev-parse` when the layout is one this reader does not know. A stamp is
/// taken at every denial, so it spawns git once (`ls-files`) and not four
/// times.
pub fn head_commit(root: &Path) -> Result<String> {
    match read_head(root) {
        Some(id) => Ok(id),
        None => Ok(git(root, &["rev-parse", "--short", "HEAD"], None)?
            .trim()
            .to_owned()),
    }
}

/// `HEAD` through `.git` (a directory, or a worktree's file naming one),
/// a symbolic ref through `refs/` and then `packed-refs`, shortened to the
/// seven characters `rev-parse --short` prints on a small repository.
fn read_head(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let line = fs::read_to_string(&dot_git).ok()?;
        let target = line.strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            root.join(target)
        }
    };
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => git_dir.join(rel.trim()),
        Err(_) => git_dir.clone(),
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let full = match head.strip_prefix("ref:") {
        None => head.to_owned(),
        Some(name) => {
            let name = name.trim();
            match fs::read_to_string(common.join(name)) {
                Ok(id) => id.trim().to_owned(),
                Err(_) => fs::read_to_string(common.join("packed-refs"))
                    .ok()?
                    .lines()
                    .find_map(|l| l.strip_suffix(name).map(|id| id.trim().to_owned()))?,
            }
        }
    };
    (full.len() == 40 && full.chars().all(|c| c.is_ascii_hexdigit())).then(|| full[..7].to_owned())
}

/// Every file under `scope` (relative to `root`) that git does not ignore,
/// tracked or not, as paths relative to `root`, sorted. What an instrument
/// means by "the sources" of a scope; `tree_hash` is one hash over them.
pub fn files(root: &Path, scope: &Path) -> Result<Vec<String>> {
    let scope = scope.to_str().context("the scope path is not UTF-8")?;
    let listing = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            scope,
        ],
        None,
    )?;
    let mut paths: Vec<String> = listing
        .split('\0')
        .filter(|p| !p.is_empty() && root.join(p).is_file())
        .map(str::to_owned)
        .collect();
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// One object id over every file under `scope` that git does not ignore,
/// tracked or not, with its content as it is on disk: the blob id of each
/// file, then the blob id of the sorted `<id> <path>` listing, which is
/// what `git hash-object` would print for the same bytes.
pub fn tree_hash(root: &Path, scope: &Path) -> Result<String> {
    let mut manifest = String::new();
    for path in files(root, scope)? {
        let file = root.join(&path);
        let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        manifest.push_str(&blob_id(&bytes));
        manifest.push(' ');
        manifest.push_str(&path);
        manifest.push('\n');
    }
    Ok(blob_id(manifest.as_bytes()))
}

/// The id git gives a blob: SHA-1 over `blob <len>\0<content>`.
pub fn blob_id(content: &[u8]) -> String {
    let mut hash = sha1_smol::Sha1::new();
    hash.update(format!("blob {}\0", content.len()).as_bytes());
    hash.update(content);
    hash.digest().to_string()
}

/// Run git in `root` and return its stdout; a non-zero exit is an error
/// carrying git's stderr.
pub(crate) fn git(root: &Path, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).context("feeding git")?;
    }
    let out = child.wait_with_output().context("waiting for git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("git wrote something that is not UTF-8")
}

/// The current time in UTC as a TOML datetime, to the second.
pub fn now() -> Result<Datetime> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("the clock is before 1970")?
        .as_secs();
    let (date, time) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(i64::try_from(date).context("the clock is past the calendar")?);
    let text = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    );
    text.parse()
        .with_context(|| format!("{text} is not a datetime"))
}

/// Seconds since 1970-01-01 of a reading's `at`, which `now` writes in UTC
/// to the second; what a window such as the fourteen days of the silent
/// query (harness-design.md §7) is measured in.
pub fn epoch_seconds(at: &Datetime) -> Result<i64> {
    let date = at.date.with_context(|| format!("{at} has no date"))?;
    let time = at.time.unwrap_or(Time {
        hour: 0,
        minute: 0,
        second: 0,
        nanosecond: 0,
    });
    let days = days_from_civil(
        i64::from(date.year),
        i64::from(date.month),
        i64::from(date.day),
    );
    let seconds =
        i64::from(time.hour) * 3600 + i64::from(time.minute) * 60 + i64::from(time.second);
    let offset = at.offset.map_or(0, |o| match o {
        Offset::Z => 0,
        Offset::Custom { minutes } => i64::from(minutes) * 60,
    });
    Ok(days * 86_400 + seconds - offset)
}

/// A proleptic Gregorian date to days since 1970-01-01 (Howard Hinnant's
/// `days_from_civil`), the inverse of `civil_from_days`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The merge base of `HEAD` and `other`: what `done` measures the changes
/// since when no `--base` is given (harness-design.md §4.5). `main` first,
/// then `origin/main` for a checkout that has only the remote's branch.
pub fn merge_base(root: &Path, other: &str) -> Result<String> {
    let remote = format!("origin/{other}");
    let mut last = None;
    for name in [other, remote.as_str()] {
        match git(root, &["merge-base", "HEAD", name], None) {
            Ok(id) => return Ok(id.trim().to_owned()),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no merge base with {other}")))
}

/// One file the working tree differs in from a base commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Relative to the root.
    pub path: String,
    /// The file is gone from the working tree.
    pub deleted: bool,
}

/// Every file that differs between `base` and the working tree, committed
/// since the base or not, plus every untracked file git does not ignore;
/// sorted by path. A rename is its deletion and its addition.
pub fn changed_since(root: &Path, base: &str) -> Result<Vec<Change>> {
    let listing = git(
        root,
        &["diff", "--name-status", "-z", "--no-renames", base, "--"],
        None,
    )?;
    let mut fields = listing.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        out.push(Change {
            path: path.to_owned(),
            deleted: status.starts_with('D'),
        });
    }
    let untracked = git(
        root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        None,
    )?;
    out.extend(
        untracked
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|p| Change {
                path: p.to_owned(),
                deleted: false,
            }),
    );
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup();
    Ok(out)
}

/// Every file the index differs in from `HEAD`: what a commit would take,
/// and what pre-commit judges (harness-design.md §6). Sorted by path; a
/// rename is its deletion and its addition.
pub fn staged(root: &Path) -> Result<Vec<Change>> {
    let listing = git(
        root,
        &[
            "diff",
            "--cached",
            "--name-status",
            "-z",
            "--no-renames",
            "--",
        ],
        None,
    )?;
    let mut fields = listing.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        out.push(Change {
            path: path.to_owned(),
            deleted: status.starts_with('D'),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The content of `path` in the index: `show(root, "", path)`, since git
/// spells the index as an empty revision.
pub fn staged_content(root: &Path, path: &str) -> Result<Option<String>> {
    show(root, "", path)
}

/// The content of `path` at `rev`, or `None` when the commit has no such
/// file.
pub fn show(root: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    match git(root, &["show", &format!("{rev}:{path}")], None) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.to_string().contains("does not exist") => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn blob_ids_are_git_object_ids() {
        assert_eq!(
            super::blob_id(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            super::blob_id(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_703), (2026, 9, 7));
    }
}
