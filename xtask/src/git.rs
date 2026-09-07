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
use toml::value::Datetime;

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
