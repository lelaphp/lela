//! The production server's two lanes beside its witnesses (#2337), each an
//! instrument over `sapi/makala`'s `makala` as `profile::build` builds it,
//! in the lanes' profile, whose debug assertions arm the request leak
//! sensor:
//!
//! - `fpm-compare` ([`compare`], #2349): each request of the corpus served
//!   by makala over HTTP and by the pinned php-fpm under the pin's
//!   `[fastcgi]` mapping, the two responses held to each other. It is the
//!   oracle for what PHP code sees under makala, and runs on request where
//!   the pinned php-fpm is.
//! - `soak` ([`soak`], #2350): many clients sending the corpus to one
//!   makala for a fixed time, each response held to the same request
//!   served alone, with the leak sensor's findings and the growth of the
//!   process's resident memory counted. Its length, its clients and its
//!   lane are #2336's ruling: ten minutes, 64 clients, nightly on
//!   `dax-build`, and a 30-second smoke in makala's own tests.
//!
//! Both are served one corpus, the HTTP-served subset: the tests of the
//! phpt lane's served subset (`phpt::http`, #2348) that one file serves
//! whole, each its `--FILE--`, or the file its `--FILE_EXTERNAL--` names
//! (#2651), written under the name the phpt lane writes it, the one its
//! request names (`frontcontroller21.php`), and the request its sections
//! describe as run-tests.php's CGI translation of them ([`cgi::request`])
//! reads back as HTTP, under its `--INI--`. A test that cleans up after
//! itself, redirects to another test, sends a compressed body or takes
//! arguments is left out, since none of that crosses one request to one
//! file. A test with a `--SKIPIF--` is compared where the pinned php's run
//! of the section does not say skip and left out where it does; the soak,
//! which runs no section, leaves it out. The soak adds the bench workloads
//! committed under `docs/perf/workloads/` (measurement protocol §11), each
//! a `GET` of its own.
//!
//! **What a by-design difference is.** Where makala answers a test
//! otherwise than php's SAPIs by design, an accepted `makala:` row of the
//! registry says so, one row per fact for both of the production server's
//! comparisons, `fpm-compare` and the served phpt lane (#2629, #2639).
//! Each holds a test to the rows through [`held`]: a differing test a row
//! names reads as accepted, counted apart from the failures and named
//! ([`Consulted`]). `fpm-compare` also names a row whose test now agrees
//! stale, so a person retires it; the served lane names none, since a
//! test held to its own expectation may not show what php-fpm shows.

pub(crate) mod bench;
pub(crate) mod compare;
pub(crate) mod soak;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::Command;
use crate::divergence::{Divergence, Kind, Registry};
use crate::manifest::Manifest;
use crate::oracle::hosts::MAKALA;
use crate::oracle::profile;
use crate::phpt::environment::Environment;
use crate::phpt::file::Phpt;
use crate::phpt::{Test, adapter, cgi, http};
use crate::reading::{Reading, plural};
use crate::witness::makala::Http;

/// The crate both instruments' readings are scoped to.
pub(crate) const SCOPE: &str = "sapi/makala";

/// The sections whose test the corpus leaves out, the reasons above.
const LEFT_OUT: [&str; 5] = ["REDIRECTTEST", "CLEAN", "GZIP_POST", "DEFLATE_POST", "ARGS"];

/// Where the bench workloads stand, relative to the workspace root.
const WORKLOADS: &str = "docs/perf/workloads";

/// One request of the corpus: what names it, the whole file it runs and
/// the name it is written under, the request as a client sends it and the
/// ini entries it is served under.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) program: Vec<u8>,
    pub(crate) script: String,
    /// The `--SKIPIF--` the entry is compared only after, where it has one.
    pub(crate) skipif: Option<Vec<u8>>,
    pub(crate) http: Http,
    /// With `{PWD}` kept, since each host serves the program from a
    /// directory of its own: [`Entry::ini_in`] puts it there (#2705).
    pub(crate) ini: Vec<String>,
}

impl Entry {
    /// The ini entries the entry is served under from `dir`, the
    /// directory a host serves its program from, `{PWD}` there as the
    /// phpt lane has the test's own directory there, where it writes the
    /// program (`adapter::with_pwd`).
    pub(crate) fn ini_in(&self, dir: &Path) -> Vec<String> {
        adapter::with_pwd(&self.ini, dir)
    }

    /// `response` to the entry with what php makes at random for one
    /// request read as one token each, so it is held to another response
    /// to the same request without it: an uploaded file's temporary name
    /// and a session id php made, the cookie the entry's `session.name`
    /// names.
    pub(crate) fn randoms_named_alike(&self, response: &[u8]) -> Vec<u8> {
        let name = (self.ini.iter().rev())
            .find_map(|line| line.strip_prefix("session.name="))
            .unwrap_or("PHPSESSID");
        sessions_named_alike(&uploads_named_alike(response), name)
    }
}

/// The reading of the instrument `ask` names: `fpm-compare` or `soak`.
pub(crate) fn run(root: &Path, manifest: &Manifest, ask: Command) -> Result<Reading> {
    match ask {
        Command::FpmCompare { tests } => compare::run(root, manifest, &tests),
        Command::Soak {
            nightly,
            seconds,
            clients,
        } => soak::run(root, manifest, soak::seconds(nightly, seconds), clients),
        _ => anyhow::bail!("{ask:?} is neither fpm-compare nor soak"),
    }
}

/// The tests of the pinned tree at `tree` one file serves whole, in the
/// order of their paths, and the CGI-shaped tests it left out: the
/// served subset's tests (`phpt::http`), which says which tests are
/// CGI-shaped for every lane of the production server.
pub(crate) fn subset(tree: &Path) -> Result<(Vec<Entry>, Vec<String>)> {
    let mut files = http::subset(tree).tests()?;
    files.sort();
    let (mut entries, mut left) = (Vec::new(), Vec::new());
    for path in files {
        let Ok(phpt) = Phpt::load(&path) else {
            continue;
        };
        let file = path.strip_prefix(tree).unwrap_or(&path);
        match entry(&phpt, &path, file) {
            Some(entry) => entries.push(entry),
            None => left.push(file.display().to_string()),
        }
    }
    Ok((entries, left))
}

/// The entry a CGI-shaped test is, or none where it is left out. Its
/// program is `--FILE--`, which holds the external file's bytes where the
/// test names one.
fn entry(phpt: &Phpt, path: &Path, file: &Path) -> Option<Entry> {
    let program = phpt.section("FILE")?;
    if LEFT_OUT.iter().any(|section| phpt.has(section)) {
        return None;
    }
    let written = Test::own(path.to_path_buf()).beside(None);
    let mut env = Environment::from(Vec::new());
    env.reset();
    env.extend(crate::phpt::env_of(phpt, path));
    let request = cgi::request(phpt, path, &env).ok()?;
    Some(Entry {
        id: file.display().to_string(),
        program: program.to_vec(),
        script: written.file_name()?.to_str()?.to_owned(),
        skipif: phpt.filled("SKIPIF").map(<[u8]>::to_vec),
        http: Http::described(&request),
        ini: adapter::ini_before_pwd(phpt).ok()?,
    })
}

/// The bench workloads of the workspace at `root`, each a `GET` with no
/// ini of its own, in the order of their names, written under the recipe's
/// `script_name`, the request the server's bench series has sent since it
/// began (#2351).
pub(crate) fn workloads(root: &Path) -> Result<Vec<Entry>> {
    let script = Manifest::load(root)?.pin.recipe.script_name;
    let dir = root.join(WORKLOADS);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|found| found.ok().map(|found| found.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "php"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            Ok(Entry {
                id: format!("{WORKLOADS}/{name}"),
                program: std::fs::read(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
                script: script.clone(),
                skipif: None,
                http: Http::get(),
                ini: Vec::new(),
            })
        })
        .collect()
}

/// `response` with each temporary copy of an uploaded file named alike:
/// php names one `php` and random characters in `upload_tmp_dir`
/// (`php_open_temporary_fd`), a name no two requests share, so a response
/// that prints one is held to another with the characters as one token.
fn uploads_named_alike(response: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"/php";
    let (mut out, mut at) = (Vec::with_capacity(response.len()), 0);
    while at < response.len() {
        let random = (response[at..].starts_with(NAME))
            .then(|| response[at + NAME.len()..].iter())
            .map_or(0, |rest| {
                rest.take_while(|b| b.is_ascii_alphanumeric()).count()
            });
        if random >= 6 {
            out.extend_from_slice(b"/php%UPLOAD%");
            at += NAME.len() + random;
        } else {
            out.push(response[at]);
            at += 1;
        }
    }
    out
}

/// `response` with each session id php made read as one token wherever
/// it stands: the value of a `set-cookie` line of the session's cookie,
/// `name`, of the shape php makes one at random where a request names
/// none it accepts, 32 characters of `0-9a-f` under the default
/// `session.sid_length` and `session.sid_bits_per_character`
/// (`php_session_create_id`). An id of another shape, one a request sent
/// among them, is kept as it came, so a side that takes it is told.
fn sessions_named_alike(response: &[u8], name: &str) -> Vec<u8> {
    let field = format!("set-cookie: {name}=");
    let head = response
        .split(|b| *b == b'\n')
        .take_while(|line| !line.is_empty());
    let made = head
        .filter_map(|line| line.strip_prefix(field.as_bytes()))
        .filter_map(|value| value.split(|b| *b == b';').next())
        .filter(|id| id.len() == 32 && id.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    let ids: Vec<Vec<u8>> = made.map(<[u8]>::to_vec).collect();
    ids.iter().fold(response.to_vec(), |out, id| {
        crate::oracle::replace(&out, id, b"%SESSION%")
    })
}

/// One test of a comparison held to the `makala:` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held<'r> {
    /// The two sides agree, and no row names the test.
    Alike,
    /// They differ, and no accepted row names the test: a failure.
    Differs,
    /// They differ, and the accepted row names the test.
    Accepted(&'r Divergence),
    /// They agree though the accepted row names the test: the row is stale.
    Stale(&'r Divergence),
}

/// `test`, a path in php-src the two sides answered `alike` or not, held
/// to the registry's `makala:` rows: the one rule both comparisons read.
pub(crate) fn held<'r>(registry: &'r Registry, test: &str, alike: bool) -> Held<'r> {
    match (alike, registry.makala(test)) {
        (true, None) => Held::Alike,
        (false, None) => Held::Differs,
        (false, Some(row)) => Held::Accepted(row),
        (true, Some(row)) => Held::Stale(row),
    }
}

/// What the `makala:` rows made of one run of a comparison: each differing
/// test a row accepted and each test that agreed though a row names it,
/// with the row, in the order the run held them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Consulted<'r> {
    pub(crate) accepted: Vec<(String, &'r Divergence)>,
    pub(crate) stale: Vec<(String, &'r Divergence)>,
}

impl<'r> Consulted<'r> {
    /// One test held through [`held`], what a row made of it kept.
    pub(crate) fn hold(&mut self, registry: &'r Registry, test: &str, alike: bool) -> Held<'r> {
        let found = held(registry, test, alike);
        match found {
            Held::Accepted(row) => self.accepted.push((test.to_owned(), row)),
            Held::Stale(row) => self.stale.push((test.to_owned(), row)),
            Held::Alike | Held::Differs => {}
        }
        found
    }

    /// The reading's `accepted` count, its `stale` one where there are
    /// any, as `predated` is written, and the words that name each test
    /// under its row, none where no row was met.
    pub(crate) fn counted(&self, reading: &mut Reading) -> Result<Option<String>> {
        reading.count("accepted", self.accepted.len() as u64)?;
        if !self.stale.is_empty() {
            reading.count("stale", self.stale.len() as u64)?;
        }
        let named = |held: &[(String, &Divergence)]| -> String {
            let each = held.iter().map(|(test, row)| {
                let own = row.id.as_str().strip_prefix(Kind::Makala.prefix());
                match own {
                    Some(own) if own == test => row.id.to_string(),
                    _ => format!("{test} under {}", row.id),
                }
            });
            each.collect::<Vec<_>>().join(", ")
        };
        let mut words = Vec::new();
        if let n @ 1.. = self.accepted.len() {
            let differ = if n == 1 { "differs" } else { "differ" };
            let said = format!("{n} {differ} as a makala: row accepts");
            words.push(format!("{said}: {}", named(&self.accepted)));
        }
        if let n @ 1.. = self.stale.len() {
            let said = format!("{n} makala: row{} stale, the test now alike", plural(n));
            words.push(format!("{said}: {}", named(&self.stale)));
        }
        Ok((!words.is_empty()).then(|| words.join("; ")))
    }
}

/// makala's binary, built now in the lanes' profile.
pub(crate) fn makala(root: &Path) -> Result<PathBuf> {
    profile::build(root, &MAKALA.package(), MAKALA.binary, None, None)
}

/// The fixture's registry (`xtask/fixtures/makala`): one accepted row
/// naming 004.phpt and covering 007.phpt, none for 011.phpt; the negative
/// control both comparisons are held to.
#[cfg(test)]
pub(crate) fn fixture() -> Registry {
    let dir = crate::package_dir().join("fixtures/makala");
    let text = std::fs::read_to_string(dir.join("divergence.toml")).unwrap_or_default();
    Registry::parse(&text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Tree;

    /// A CGI-shaped test one file serves whole is an entry with its
    /// `--INI--`, a multi-line cookie folded onto one field, and one with a
    /// `--SKIPIF--` carries it; one with a `--CLEAN--` is left out, and one
    /// that is not CGI-shaped is no part of the subset.
    #[test]
    fn the_subset_is_the_cgi_shaped_tests_one_file_serves_whole() {
        let tree = Tree::new("makala-subset").unwrap();
        let test = |sections: &str| {
            format!("--TEST--\nt\n{sections}--FILE--\n<?php echo 1;\n--EXPECT--\n1\n")
        };
        std::fs::create_dir_all(tree.path().join("tests")).unwrap();
        tree.write(
            "tests/a.phpt",
            test("--INI--\nprecision=4\n--COOKIE--\na=1;\nb=2\n"),
        )
        .unwrap();
        tree.write("tests/b.phpt", test("--CLEAN--\n<?php\n--GET--\nx=1\n"))
            .unwrap();
        tree.write("tests/d.phpt", test("--SKIPIF--\n<?php\n--GET--\nx=1\n"))
            .unwrap();
        tree.write("tests/c.phpt", test("")).unwrap();
        let (entries, left) = subset(tree.path()).unwrap();
        assert_eq!(left, ["tests/b.phpt"]);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["tests/a.phpt", "tests/d.phpt"]);
        assert_eq!(entries[0].script, "a.php", "the name the phpt lane writes");
        assert_eq!(entries[1].skipif.as_deref(), Some(&b"<?php\n"[..]));
        assert_eq!(entries[0].ini, ["precision=4"]);
        assert_eq!(entries[0].program, b"<?php echo 1;\n");
        assert_eq!(entries[0].http.field("Cookie"), b"a=1; b=2");
    }

    /// An entry whose `--INI--` names `{PWD}` is served under each host's
    /// own directory there, the one the program is written in, and not
    /// the test's directory in php-src (#2705).
    #[test]
    fn pwd_in_an_entry_s_ini_is_the_directory_each_host_serves() {
        let tree = Tree::new("makala-pwd").unwrap();
        std::fs::create_dir_all(tree.path().join("tests")).unwrap();
        let test = "--TEST--\nt\n--INI--\nphar.cache_list={PWD}/fc21.php\n--GET--\nx=1\n--FILE--\n<?php\n--EXPECT--\n\n";
        tree.write("tests/fc21.phpt", test).unwrap();
        let (entries, _) = subset(tree.path()).unwrap();
        let [entry] = &entries[..] else {
            panic!("{entries:?}")
        };
        let side = |dir: &str| entry.ini_in(Path::new(dir));
        assert_eq!(side("/srv/m"), ["phar.cache_list=/srv/m/fc21.php"]);
        assert_eq!(side("/srv/f"), ["phar.cache_list=/srv/f/fc21.php"]);
        assert_eq!(entry.script, "fc21.php", "the program the list names");
    }

    /// A test whose program is `--FILE_EXTERNAL--` is an entry (#2651):
    /// its program the external file's bytes, written under the name the
    /// phpt lane writes it, the one its request names.
    #[test]
    fn a_test_whose_program_is_an_external_file_is_an_entry_under_its_request_s_name() {
        let tree = Tree::new("makala-external").unwrap();
        std::fs::create_dir_all(tree.path().join("tests/files")).unwrap();
        tree.write("tests/files/fc.phar", b"<?php __HALT_COMPILER(); \x00phar")
            .unwrap();
        let test = "--TEST--\nt\n--ENV--\nSCRIPT_NAME=/fc21.phar.php\nREQUEST_URI=/fc21.phar.php/index.php\nPATH_INFO=/index.php\n--FILE_EXTERNAL--\nfiles/fc.phar\n--EXPECTHEADERS--\nContent-type: text/html\n--EXPECT--\n1\n";
        tree.write("tests/fc21.phar.phpt", test).unwrap();
        let (entries, left) = subset(tree.path()).unwrap();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].program, b"<?php __HALT_COMPILER(); \x00phar");
        assert_eq!(entries[0].script, "fc21.phar.php");
        assert_eq!(
            entries[0].http.target(&entries[0].script),
            b"/fc21.phar.php/index.php"
        );
    }

    /// An uploaded file's temporary name reads as one token, and a path
    /// that only begins alike keeps its name.
    #[test]
    fn an_uploaded_file_s_temporary_name_reads_as_one_token() {
        let named = uploads_named_alike(b"\"/tmp/phpAb12Cd9\" /php/x /phpinfo");
        assert_eq!(named, b"\"/tmp/php%UPLOAD%\" /php/x /phpinfo");
    }

    /// A session id php made reads as one token in its cookie and wherever
    /// the page prints it, under the entry's `session.name` or php's
    /// default; an id a request sent, another cookie of that shape and an
    /// id in the body alone keep their bytes
    /// (`ext/session/tests/rfc1867_sid_only_cookie_2.phpt`, #2706).
    #[test]
    fn a_session_id_php_made_reads_as_one_token() {
        let entry = |ini: &[&str]| Entry {
            id: "t.phpt".to_owned(),
            program: Vec::new(),
            script: "t.php".to_owned(),
            skipif: None,
            http: Http::get(),
            ini: ini.iter().map(|line| (*line).to_owned()).collect(),
        };
        let id = "5a40ac74d9455590d2eda039bedc3cf2";
        let made = format!("200\nset-cookie: PHPSESSID={id}; path=/\n\nstring(32) \"{id}\"");
        let named = entry(&[]).randoms_named_alike(made.as_bytes());
        let alike = "200\nset-cookie: PHPSESSID=%SESSION%; path=/\n\nstring(32) \"%SESSION%\"";
        assert_eq!(String::from_utf8_lossy(&named), alike);
        let other = made.replace("PHPSESSID", "s");
        let named = entry(&["session.name=s"]).randoms_named_alike(other.as_bytes());
        assert_eq!(
            String::from_utf8_lossy(&named),
            alike.replace("PHPSESSID", "s")
        );
        for kept in [
            "200\nset-cookie: PHPSESSID=rfc1867-sid-only-cookie-2-post\n\nx".to_owned(),
            made.replace("PHPSESSID", "other"),
            format!("200\n\nPHPSESSID={id}"),
        ] {
            let named = entry(&[]).randoms_named_alike(kept.as_bytes());
            assert_eq!(String::from_utf8_lossy(&named), kept);
        }
    }
}
