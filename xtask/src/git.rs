//! What a reading records about the moment it was taken: the git tree hash
//! of its scope's sources, the commit, and the time. The tree hash is what
//! makes a reading fresh or stale without a turn counter (harness-design.md
//! §4.2): it covers the working tree, tracked and untracked files alike, so
//! an unstaged edit changes it.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use toml::value::Datetime;

use crate::reading::Reading;

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
            commit: git(root, &["rev-parse", "--short", "HEAD"], None)?
                .trim()
                .to_owned(),
            at: now()?,
        })
    }

    pub fn apply(&self, reading: &mut Reading) {
        reading.tree = self.tree.clone();
        reading.commit = self.commit.clone();
        reading.at = self.at;
    }
}

/// One object id over every file under `scope` that git does not ignore,
/// tracked or not, with its content as it is on disk.
pub fn tree_hash(root: &Path, scope: &Path) -> Result<String> {
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
    let mut paths: Vec<&str> = listing
        .split('\0')
        .filter(|p| !p.is_empty() && root.join(p).is_file())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    let stdin = paths.join("\n");
    let hashes = git(root, &["hash-object", "--stdin-paths"], Some(&stdin))?;
    let mut manifest = String::new();
    for (path, hash) in paths.iter().zip(hashes.lines()) {
        manifest.push_str(hash);
        manifest.push(' ');
        manifest.push_str(path);
        manifest.push('\n');
    }
    Ok(git(root, &["hash-object", "--stdin"], Some(&manifest))?
        .trim()
        .to_owned())
}

fn git(root: &Path, args: &[&str], stdin: Option<&str>) -> Result<String> {
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
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_703), (2026, 9, 7));
    }
}
