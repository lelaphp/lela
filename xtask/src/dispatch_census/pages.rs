//! The bench's pages, served from the rig. The pages and
//! the recipe they are served by are the declaration's
//! ([`crate::workloads::Pages`]); the rig, the site that serves them, is
//! kept off the repository, and where it is on this machine is the
//! description [`RIG_VARIABLE`] names, as the pinned php-src is the tree
//! `TEST_PHP_SRCDIR` names.
//!
//! **The description.** A TOML file outside the repository, whose paths
//! are each relative to `rig`, and `rig` to the file's own directory where
//! it is not absolute: `copy`, what of the rig the census copies, each to
//! the same place in a scratch tree of its own, so the rig is never
//! written; `root`, the document root in that copy; `snapshot`, the
//! database each page starts on; `database`, where in the copy the
//! snapshot is copied before each page; and `host`, the address the site
//! answers to, which each request names as its `Host` wherever the copy
//! is served from, since of the four pages `/` and `/?p=1` answer 301 to
//! a `Host` other than the site's own, while `/?s=hello` and
//! `/wp-login.php` answer 200 under either.
//!
//! **A page.** Each page is served by a server of its own, the counting
//! build's built-in server under `-n` and the declared settings, with the
//! copy's root its document root, over a fresh copy of the snapshot: its
//! declared warm-up requests first, then the one counted, whose counts
//! are the lines its request logged before the server logged its
//! connection's end. A page answers 200, or its counts are out of the
//! vote.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use super::Census;
use crate::scratch::Tree;
use crate::server::Server;
use crate::workloads::{Page, Pages};

/// The environment variable that names the rig's description on this
/// machine, a leading `~/` or `$HOME/` read as the home directory, as
/// `TEST_PHP_SRCDIR` is read: stated here and nowhere else.
pub(crate) const RIG_VARIABLE: &str = "XTASK_RIG";

/// How long the server is given to listen, and each request to be
/// answered and logged, on the counting build.
const TIMEOUT: Duration = Duration::from_mins(2);

/// The description as the file spells it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Described {
    rig: PathBuf,
    copy: Vec<PathBuf>,
    root: PathBuf,
    snapshot: PathBuf,
    database: PathBuf,
    host: String,
}

/// The rig as its description says it, each path resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rig {
    /// The rig's directory.
    dir: PathBuf,
    /// What of it is copied, relative to it and to the copy.
    copy: Vec<PathBuf>,
    /// The document root, relative to the copy.
    root: PathBuf,
    /// The database each page starts on.
    snapshot: PathBuf,
    /// Where in the copy the snapshot is copied.
    database: PathBuf,
    /// The address the site answers to, each request's `Host`.
    host: String,
}

/// Where the rig is on this machine, or why it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Located {
    Here(Rig),
    /// No rig is described here, and why: its pages are a skip.
    Absent(String),
}

/// The rig [`RIG_VARIABLE`] describes: absent where the variable is unset
/// or names no file, and refused where the file is no description or the
/// rig it describes is not there.
pub(crate) fn located() -> Result<Located> {
    let Some(named) = std::env::var_os(RIG_VARIABLE).filter(|v| !v.is_empty()) else {
        return Ok(Located::Absent(format!(
            "{RIG_VARIABLE} is unset, so no rig is described on this machine"
        )));
    };
    let file = crate::oracle::at_home(Path::new(&named));
    if !file.is_file() {
        return Ok(Located::Absent(format!(
            "{RIG_VARIABLE} names {}, which is no file",
            file.display()
        )));
    }
    let dir = file.parent().unwrap_or(Path::new("."));
    let rig = Rig::parse(&crate::read(&file)?, dir)
        .with_context(|| format!("{} is no description of a rig", file.display()))?;
    ensure!(
        rig.dir.is_dir(),
        "{} describes the rig at {}, which is no directory",
        file.display(),
        rig.dir.display()
    );
    Ok(Located::Here(rig))
}

impl Rig {
    /// `text` as a description in the directory `dir`: refused where a
    /// path of the copy is absolute or leaves it, or the root or the
    /// database is outside what is copied.
    pub(crate) fn parse(text: &str, dir: &Path) -> Result<Rig> {
        let Described {
            rig,
            copy,
            root,
            snapshot,
            database,
            host,
        } = toml::from_str(text)?;
        let named = |c: char| c.is_ascii_alphanumeric() || ".-:[]".contains(c);
        ensure!(
            !host.is_empty() && host.chars().all(named),
            "the host {host:?} is no address a request names"
        );
        let inside = |path: &Path, what: &str| -> Result<()> {
            let down = path.components().all(|c| matches!(c, Component::Normal(_)));
            ensure!(
                down && !path.as_os_str().is_empty(),
                "the {what} {} is no path inside the rig",
                path.display()
            );
            Ok(())
        };
        ensure!(!copy.is_empty(), "it copies nothing of the rig");
        for path in &copy {
            inside(path, "copied path")?;
        }
        for (path, what) in [(&root, "root"), (&database, "database")] {
            inside(path, what)?;
            ensure!(
                copy.iter().any(|copied| path.starts_with(copied)),
                "the {what} {} is in nothing it copies",
                path.display()
            );
        }
        let dir = dir.join(crate::oracle::at_home(&rig));
        Ok(Rig {
            snapshot: dir.join(snapshot),
            dir,
            copy,
            root,
            database,
            host,
        })
    }

    /// The rig copied into a scratch tree of its own, each path with
    /// `cp -c -R`, a clone-on-write copy where the volume supports one,
    /// and `cp -R` where it does not.
    pub(crate) fn copied(&self) -> Result<Copied> {
        ensure!(
            self.snapshot.is_file(),
            "the rig's snapshot {} is no file",
            self.snapshot.display()
        );
        let tree = Tree::new("dispatch-census-rig")?;
        for path in &self.copy {
            let to = tree.path().join(path);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            cloned(&self.dir.join(path), &to)?;
        }
        Ok(Copied {
            host: self.host.clone(),
            root: tree.path().join(&self.root),
            database: tree.path().join(&self.database),
            snapshot: self.snapshot.clone(),
            tree,
        })
    }
}

/// `from` copied to `to`, cloned where the volume can.
fn cloned(from: &Path, to: &Path) -> Result<()> {
    let mut said = String::new();
    for flags in [&["-c", "-R"][..], &["-R"]] {
        let out = Command::new("cp")
            .args(flags)
            .arg(from)
            .arg(to)
            .stdin(Stdio::null())
            .output()
            .context("running cp")?;
        if out.status.success() {
            return Ok(());
        }
        String::from_utf8_lossy(&out.stderr)
            .trim()
            .clone_into(&mut said);
        std::fs::remove_dir_all(to)
            .or_else(|_| std::fs::remove_file(to))
            .ok();
    }
    bail!("copying {} failed: {said}", from.display())
}

/// The rig's copy, taken away when the census is done with it.
pub(crate) struct Copied {
    host: String,
    root: PathBuf,
    database: PathBuf,
    snapshot: PathBuf,
    tree: Tree,
}

impl Copied {
    /// `page` served by `engine` from this copy as `pages` declares it,
    /// over a fresh copy of the snapshot: the counts of its counted
    /// request, and the status it answered.
    pub(crate) fn count(&self, engine: &Path, pages: &Pages, page: &Page) -> Result<(Census, u16)> {
        std::fs::copy(&self.snapshot, &self.database).with_context(|| {
            format!(
                "copying the snapshot over {} in {}",
                self.database.display(),
                self.tree.path().display()
            )
        })?;
        let command = || -> Result<Command> {
            let mut server = Command::new(engine);
            server.arg("-n");
            for ini in &pages.ini {
                server.arg("-d").arg(ini);
            }
            server.current_dir(&self.root).stdout(Stdio::null());
            Ok(server)
        };
        let server = Server::start(command, &self.root, TIMEOUT)?;
        for _ in 0..pages.warm {
            server.get_as(&self.host, &page.path, TIMEOUT)?;
            server.logged(TIMEOUT)?;
        }
        let (status, _) = server.get_as(&self.host, &page.path, TIMEOUT)?;
        let logged = server.logged(TIMEOUT)?.join("\n");
        Ok((Census::read(logged.as_bytes())?, status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::Promise;
    use crate::workloads::{ANSWERS, Role};

    const DESCRIBED: &str = r#"
rig = "site-rig"
copy = ["docs", "beside.txt"]
root = "docs"
snapshot = "state/start.db"
database = "docs/data/live.db"
host = "127.0.0.1:8099"
"#;

    /// A description's paths are the rig's, the rig's the file's
    /// directory's; one of the copy that leaves it, and a root or a
    /// database in nothing copied, are refused, naming it.
    #[test]
    fn a_description_resolves_each_path_against_the_rig() {
        let rig = Rig::parse(DESCRIBED, Path::new("/m")).unwrap();
        assert_eq!(
            rig,
            Rig {
                dir: PathBuf::from("/m/site-rig"),
                copy: vec![PathBuf::from("docs"), PathBuf::from("beside.txt")],
                root: PathBuf::from("docs"),
                snapshot: PathBuf::from("/m/site-rig/state/start.db"),
                database: PathBuf::from("docs/data/live.db"),
                host: "127.0.0.1:8099".to_owned(),
            }
        );
        let absolute = DESCRIBED.replace("rig = \"site-rig\"", "rig = \"/r\"");
        let absolute = Rig::parse(&absolute, Path::new("/m")).unwrap();
        assert_eq!(absolute.dir, PathBuf::from("/r"));
        let refused = |from: &str, to: &str| {
            let text = DESCRIBED.replace(from, to);
            format!("{:#}", Rig::parse(&text, Path::new("/m")).unwrap_err())
        };
        let cases = [
            (
                "\"beside.txt\"",
                "\"../out\"",
                "the copied path ../out is no path inside the rig",
            ),
            (
                "\"beside.txt\"",
                "\"/abs\"",
                "the copied path /abs is no path inside the rig",
            ),
            (
                "root = \"docs\"",
                "root = \"elsewhere\"",
                "the root elsewhere is in nothing it copies",
            ),
            (
                "database = \"docs/data/live.db\"",
                "database = \"live.db\"",
                "the database live.db is in nothing it copies",
            ),
            ("snapshot", "snap", "unknown field `snap`"),
            (
                "host = \"127.0.0.1:8099\"",
                "host = \"a b\"",
                "the host \"a b\" is no address a request names",
            ),
        ];
        for (from, to, said) in cases {
            let why = refused(from, to);
            assert!(why.contains(said), "{to}: {why}");
        }
    }

    /// Each page is served on a server of its own over a fresh copy of the
    /// snapshot, its warm-up requests before the counted one, and only the
    /// counted request's lines are its counts: the fixture rig's page
    /// appends to its database and logs the length it read, so the
    /// counted request of each page reads the snapshot's five bytes and
    /// the three the warm-ups appended, and the rig is never written; it
    /// answers 200 only to the host the description names.
    /// Served by the oracle's built-in server, which writes no counts of
    /// its own.
    #[test]
    fn each_page_is_counted_on_its_fourth_request_over_a_fresh_database() {
        let manifest = crate::manifest::Manifest::load(&crate::package_dir().join("..")).unwrap();
        let pin = manifest.pin().unwrap();
        let file = pin.oracle.file().map(Path::to_path_buf);
        let Some(php) = Promise::Oracle.or_skip(file).unwrap() else {
            return;
        };
        let fixture = crate::package_dir().join("fixtures/rig");
        let rig = Rig::parse(&crate::read(&fixture.join("rig.toml")).unwrap(), &fixture).unwrap();
        let pages = Pages {
            first_read: 1,
            about: String::new(),
            role: Role::Bench,
            warm: 3,
            ini: vec!["memory_limit=64M".to_owned()],
            page: Vec::new(),
        };
        let copied = rig.copied().unwrap();
        for (name, path) in [("front", "/"), ("query", "/?p=1")] {
            let page = Page {
                name: name.to_owned(),
                path: path.to_owned(),
            };
            let (census, status) = copied.count(&php, &pages, &page).unwrap();
            assert_eq!(status, ANSWERS, "{name}");
            assert_eq!(census.shares(), [("Read8", 100.0)], "{name}");
        }
        let kept = crate::read(&fixture.join("start.txt")).unwrap();
        assert_eq!(kept, "fresh");
        assert!(!fixture.join("site/live.txt").exists());
    }

    /// A page asked for under a `Host` other than the site's own answers
    /// 301, as the fixture rig's page does, so the census's reading of it
    /// fails, its counts kept and out of the vote, which names it as a
    /// declared run that did not vote and is a skip.
    /// Served by the oracle's built-in server, as above.
    #[test]
    fn a_page_asked_for_under_another_host_answers_301_and_stays_out_of_the_vote() {
        use super::super::{Counted, Ends, INSTRUMENT, Source, VOTE, Vote, served, tallied};
        use crate::reading::{Reading, Status};
        let manifest = crate::manifest::Manifest::load(&crate::package_dir().join("..")).unwrap();
        let pin = manifest.pin().unwrap();
        let file = pin.oracle.file().map(Path::to_path_buf);
        let Some(php) = Promise::Oracle.or_skip(file).unwrap() else {
            return;
        };
        let fixture = crate::package_dir().join("fixtures/rig");
        let described = crate::read(&fixture.join("rig.toml")).unwrap();
        let elsewhere = described.replace("127.0.0.1:8099", "127.0.0.1:8098");
        assert_ne!(described, elsewhere);
        let rig = Rig::parse(&elsewhere, &fixture).unwrap();
        let page = Page {
            name: "front".to_owned(),
            path: "/".to_owned(),
        };
        let pages = Pages {
            first_read: 1,
            about: String::new(),
            role: Role::Bench,
            warm: 3,
            ini: vec!["memory_limit=64M".to_owned()],
            page: vec![page.clone()],
        };
        let counted = Counted {
            scope: "pages/front".to_owned(),
            source: Source::Page(page.clone()),
            args: Vec::new(),
            ends: Ends::Answers(ANSWERS),
            role: Some(pages.role),
        };
        let copied = rig.copied().unwrap();
        let counts = served(&copied, &php, (&pages, &page), counted.ends);
        let mut reading = Reading::new(INSTRUMENT, &counted.scope);
        let mut vote = Vote::default();
        tallied(&counted, counts, &mut reading, &mut vote);
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        assert_eq!(reading.int("oplines"), Some(1), "{}", reading.message);
        assert!(
            reading.message.ends_with(
                "; the page answered 301 where it should answer 200, so its counts are out of the vote"
            ),
            "{}",
            reading.message
        );
        let mut said = Reading::new(INSTRUMENT, VOTE);
        vote.written_into(&std::collections::BTreeSet::new(), &mut said)
            .unwrap();
        assert_eq!(said.status, Status::Skip, "{}", said.message);
        assert_eq!(said.int("voters"), Some(0));
        assert_eq!(said.int("voted"), Some(0));
        assert!(
            said.message.contains("did not vote: pages/front (fail);"),
            "{}",
            said.message
        );
    }
}
