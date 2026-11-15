//! The `MySQL` server the `mysqli` and `pdo_mysql` suites need.
//! Without one the oracle skips 412 of `mysqli`'s 449 tests and 161 of
//! `pdo_mysql`'s 167, and a reading of those skips would be a verdict on a
//! server that was not there. So before a run of such a subset the pinned
//! php asks the server the recipe's `PDO_MYSQL_TEST_DSN` names for
//! `SELECT VERSION()`: no answer refuses the run, and the answer is kept on
//! the reading, since `ci/mysql/compose.yaml`'s image tag floats and a shift
//! in the oracle's verdict is then traced to its bump. The runner holds the
//! same line: handed such a subset with no server named, it refuses rather
//! than read every test's `skip` as the subset's verdict.
//!
//! **A database of the run's own.** The suites' tests drop and make their
//! tables in the database `MYSQL_TEST_DB` and `PDO_MYSQL_TEST_DSN` name,
//! `test` by default, and the conflict keys hold them apart within a run
//! and, by key, across runs, but a test whose own `--CONFLICTS--` names
//! another key runs beside another run's tests, and two runs in two
//! worktrees read each other's `Table 'test.test' doesn't exist`.
//! So each run of such a subset makes a database of its own on the server,
//! names it to its programs through the variables `connect.inc` and
//! `inc/config.inc` read, as upstream's README asks of a database other
//! than `test`, and drops it when the subset ends ([`Database`]). Its name
//! carries the process that made it, so a run killed before its drop
//! leaves one the next run drops once that process is gone.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};

use super::Subset;
use crate::oracle::{Ended, Oracle, Pin, Program};

/// The corpus directories whose tests need the server: the extensions'
/// subsets as the design derives them, and any directory inside.
const SUITES: [&str; 2] = ["ext/mysqli", "ext/pdo_mysql"];

/// What the pinned php runs to ask: the suites' own defaults where the
/// recipe names none (`ext/pdo_mysql/tests/inc/config.inc`).
const ASK: &str = "$dsn = getenv('PDO_MYSQL_TEST_DSN') ?: 'mysql:host=localhost;dbname=test';
try {
    $db = new PDO($dsn, getenv('PDO_MYSQL_TEST_USER') ?: 'root', getenv('PDO_MYSQL_TEST_PASS') ?: '');
    echo $db->query('SELECT VERSION()')->fetchColumn();
} catch (Throwable $e) {
    echo $e->getMessage();
    exit(1);
}
";

/// How the server is started, said wherever a run is refused for want of it.
pub(crate) const START: &str = "`docker compose -f ci/mysql/compose.yaml up -d --wait` starts it";

/// Whether the subset's tests need the server.
pub(crate) fn needs(subset: &Subset) -> bool {
    subset.dirs.iter().any(|dir| {
        SUITES
            .iter()
            .any(|suite| dir.under.starts_with(Path::new(suite)))
    })
}

/// The server's version where the subset needs one and its tests will run
/// (`runs`), none where either is not so, and the run refused where the
/// server does not answer.
pub(crate) fn served(oracle: &Oracle<'_>, subset: &Subset, runs: bool) -> Result<Option<String>> {
    if !(runs && needs(subset)) {
        return Ok(None);
    }
    version(oracle, &subset.scope).map(Some)
}

/// The server's `SELECT VERSION()`, asked through the pinned php under the
/// recipe, or the run refused with what the connection said.
pub(crate) fn version(oracle: &Oracle<'_>, scope: &str) -> Result<String> {
    let seen = oracle.run(Program::Snippet(ASK), None)?;
    let said = String::from_utf8_lossy(&seen.stdout).trim().to_owned();
    if seen.exit != Ended::Status(0) || said.is_empty() {
        bail!(
            "{scope}: refused, not skipped: the subset needs the MySQL server the recipe's PDO_MYSQL_TEST_DSN names, and it did not answer ({said}); {START}"
        );
    }
    Ok(said)
}

/// What every run's database is named before its process and its number.
const PREFIX: &str = "xtask_phpt_";

/// The pinned php's connection to the server as the recipe names it, under
/// which `$db` runs the statements after it, each failure an exit of 1
/// with the server's message.
const CONNECT: &str = "$dsn = getenv('PDO_MYSQL_TEST_DSN') ?: 'mysql:host=localhost;dbname=test';
try {
    $db = new PDO($dsn, getenv('PDO_MYSQL_TEST_USER') ?: 'root', getenv('PDO_MYSQL_TEST_PASS') ?: '', [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);
";

/// The end of a [`CONNECT`] snippet.
const END: &str = "
} catch (Throwable $e) {
    echo $e->getMessage();
    exit(1);
}
";

/// One run's database on the server, made by [`Database::made`] and
/// dropped when this value drops, through the oracle that made it.
pub(crate) struct Database<'a> {
    oracle: &'a Oracle<'a>,
    pub name: String,
}

impl<'a> Database<'a> {
    /// A database of this run's own, made on the server the recipe names,
    /// after dropping each one an earlier run's process made and left
    /// when that process is gone.
    pub(crate) fn made(oracle: &'a Oracle<'a>, scope: &str) -> Result<Database<'a>> {
        static MADE: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let name = format!("{PREFIX}{pid}_{}", MADE.fetch_add(1, Ordering::Relaxed));
        let listed = said(
            oracle,
            &format!(
                "echo implode(\"\\n\", $db->query(\"SHOW DATABASES LIKE '{}%'\")->fetchAll(PDO::FETCH_COLUMN));",
                PREFIX.replace('_', "\\\\_")
            ),
        )
        .map_err(|why| anyhow::anyhow!("{scope}: listing the runs' databases on the server: {why}"))?;
        let left = left(&listed, crate::sysv::alive);
        let mut statements: Vec<String> = left
            .iter()
            .map(|gone| format!("DROP DATABASE IF EXISTS `{gone}`"))
            .collect();
        statements.push(format!("CREATE DATABASE `{name}`"));
        let code = statements
            .iter()
            .fold(String::new(), |mut code, statement| {
                writeln!(code, "$db->exec('{statement}');").ok();
                code
            });
        said(oracle, &code)
            .map_err(|why| anyhow::anyhow!("{scope}: making the run's database {name}: {why}"))?;
        Ok(Database { oracle, name })
    }

    /// `pin` with this database named to the suites' programs: what
    /// `connect.inc` reads, `MYSQL_TEST_DB`, and what `inc/config.inc` and
    /// `common.phpt` read, `PDO_MYSQL_TEST_DB` and the DSN's `dbname`.
    pub(crate) fn lay_over(&self, pin: &mut Pin) {
        let env = &mut pin.recipe.env;
        let dsn = env
            .get("PDO_MYSQL_TEST_DSN")
            .map_or("mysql:host=localhost;dbname=test", String::as_str);
        let dsn = with_database(dsn, &self.name);
        env.insert("PDO_MYSQL_TEST_DSN".to_owned(), dsn);
        env.insert("PDO_MYSQL_TEST_DB".to_owned(), self.name.clone());
        env.insert("MYSQL_TEST_DB".to_owned(), self.name.clone());
    }
}

impl Drop for Database<'_> {
    fn drop(&mut self) {
        let name = &self.name;
        if let Err(why) = said(
            self.oracle,
            &format!("$db->exec('DROP DATABASE IF EXISTS `{name}`');"),
        ) {
            println!(
                "phpt: the run's database {name} is not dropped ({why}); the next run drops it"
            );
        }
    }
}

/// What the pinned php said running `statements` connected to the server,
/// or what went wrong.
fn said(oracle: &Oracle<'_>, statements: &str) -> std::result::Result<String, String> {
    let code = format!("{CONNECT}{statements}{END}");
    let seen = oracle
        .run(Program::Snippet(&code), None)
        .map_err(|e| format!("{e:#}"))?;
    let said = String::from_utf8_lossy(&seen.stdout).trim().to_owned();
    if seen.exit == Ended::Status(0) {
        Ok(said)
    } else {
        Err(said)
    }
}

/// The runs' databases of `listed`, one name a line, whose process
/// `alive` says is gone; a name that is no run's is never among them.
pub(crate) fn left(listed: &str, alive: impl Fn(u32) -> bool) -> Vec<String> {
    listed
        .lines()
        .map(str::trim)
        .filter(|name| {
            name.strip_prefix(PREFIX)
                .and_then(|rest| rest.split_once('_'))
                .and_then(|(pid, n)| {
                    n.bytes()
                        .all(|b| b.is_ascii_digit())
                        .then_some(pid)?
                        .parse()
                        .ok()
                })
                .is_some_and(|pid| !alive(pid))
        })
        .map(str::to_owned)
        .collect()
}

/// `dsn` naming database `name`: its `dbname` replaced, or added where it
/// names none.
pub(crate) fn with_database(dsn: &str, name: &str) -> String {
    let (driver, rest) = dsn.split_once(':').unwrap_or(("mysql", dsn));
    let mut parts: Vec<String> = rest
        .split(';')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, _)) if key.trim() == "dbname" => format!("dbname={name}"),
            _ => part.to_owned(),
        })
        .collect();
    if !parts.iter().any(|part| part.starts_with("dbname=")) {
        parts.push(format!("dbname={name}"));
    }
    format!("{driver}:{}", parts.join(";"))
}
