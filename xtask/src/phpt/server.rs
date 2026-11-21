//! The `MySQL` server the `mysqli` and `pdo_mysql` suites need.
//! Without one the oracle skips 412 of `mysqli`'s 449 tests and 161 of
//! `pdo_mysql`'s 167, and a reading of those skips would be a verdict on a
//! server that was not there. So once before a run that holds such a
//! subset, and before the tree is copied, the pinned php asks the server
//! the recipe's `PDO_MYSQL_TEST_DSN` names for `SELECT VERSION()`: no
//! answer refuses those subsets by name and the rest run, and the answer
//! is kept on the reading with where it came from, since
//! `ci/mysql/compose.yaml`'s image tag floats and a shift in the oracle's
//! verdict is then traced to its bump. The caller may say where the server
//! is ([`Address`]). The runner holds the
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

use anyhow::Result;

use super::Subset;
use crate::oracle::{Ended, Oracle, Pin, Program};

/// The corpus directories whose tests need the server: the extensions'
/// subsets as the design derives them, and any directory inside.
const SUITES: [&str; 2] = ["ext/mysqli", "ext/pdo_mysql"];

/// The recipe's DSN, which every program connecting to the server reads.
const DSN: &str = "PDO_MYSQL_TEST_DSN";

/// What the pinned php runs to ask the server the recipe's DSN names.
const ASK: &str = "try {
    $db = new PDO(getenv('PDO_MYSQL_TEST_DSN'), getenv('PDO_MYSQL_TEST_USER') ?: 'root', getenv('PDO_MYSQL_TEST_PASS') ?: '');
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

/// The server's answer where the subset needs one and its tests will run
/// (`runs`), none where either is not so, and the run refused where the
/// server does not answer.
pub(crate) fn served(oracle: &Oracle<'_>, subset: &Subset, runs: bool) -> Result<Option<String>> {
    if !(runs && needs(subset)) {
        return Ok(None);
    }
    version(oracle, &subset.scope).map(Some)
}

/// [`answer`], or `scope` refused with what the connection said.
pub(crate) fn version(oracle: &Oracle<'_>, scope: &str) -> Result<String> {
    answer(oracle).map_err(|why| anyhow::anyhow!("{scope}: refused, not skipped: {why}"))
}

/// The server's `SELECT VERSION()` and where it answered, asked through
/// the pinned php under the recipe, which a reading names it by; or why
/// it did not answer, naming where it was asked and how it is started.
pub(crate) fn answer(oracle: &Oracle<'_>) -> std::result::Result<String, String> {
    let at = at(oracle.pin().recipe.env.get(DSN).map_or("", String::as_str));
    let seen = oracle
        .run(Program::Snippet(ASK), None)
        .map_err(|why| format!("asking the MySQL server at {at}: {why:#}"))?;
    let said = String::from_utf8_lossy(&seen.stdout).trim().to_owned();
    if seen.exit != Ended::Status(0) || said.is_empty() {
        return Err(format!(
            "the subset needs the MySQL server at {at}, and it did not answer ({said}); {START}"
        ));
    }
    Ok(format!("{said} at {at}"))
}

/// The server `dsn` names, by its `host` and `port` parts as the suites
/// connect to it, or the DSN whole where it names no host.
fn at(dsn: &str) -> String {
    let part = |key: &str| {
        let rest = dsn.split_once(':').map_or(dsn, |(_, rest)| rest);
        (rest.split(';'))
            .filter_map(|part| part.split_once('='))
            .find(|(name, _)| name.trim() == key)
            .map(|(_, value)| value.trim().to_owned())
    };
    match (part("host"), part("port")) {
        (Some(host), Some(at)) => format!("{host}:{at}"),
        (Some(host), None) => host,
        (None, _) => format!("`{DSN}={dsn}`"),
    }
}

/// Where the caller says the server is: php-src's own `MYSQL_TEST_HOST`
/// and `MYSQL_TEST_PORT`, read from xtask's environment and laid over a
/// copy of the recipe at run time, as a run's database is, so
/// `oracle.toml` is never written and a reading's digest stays the pin's.
/// Neither set, the recipe's address stands.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Address {
    host: Option<String>,
    port: Option<u16>,
}

impl Address {
    /// The address xtask's environment names.
    pub(crate) fn asked() -> Result<Address> {
        Address::read(|name| std::env::var(name).ok())
    }

    /// The address `var` names, an empty value as none; a `MYSQL_TEST_PORT`
    /// that is no number of one is refused.
    pub(crate) fn read(var: impl Fn(&str) -> Option<String>) -> Result<Address> {
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        let port = set("MYSQL_TEST_PORT")
            .map(|value| {
                value.parse().map_err(|_| {
                    anyhow::anyhow!("MYSQL_TEST_PORT={value} names no TCP port of the MySQL server")
                })
            })
            .transpose()?;
        Ok(Address {
            host: set("MYSQL_TEST_HOST"),
            port,
        })
    }

    /// `pin` with this address named to every program: the two variables
    /// `connect.inc` reads, the two `inc/config.inc` reads beside them, and
    /// the DSN's `host` and `port`, derived where the recipe names no DSN.
    pub(crate) fn lay_over(&self, pin: &mut Pin) {
        let env = &mut pin.recipe.env;
        let parts = [
            ("host", self.host.clone()),
            ("port", self.port.map(|port| port.to_string())),
        ];
        for (key, value) in parts {
            let Some(value) = value else { continue };
            let upper = key.to_uppercase();
            env.insert(format!("MYSQL_TEST_{upper}"), value.clone());
            env.insert(format!("PDO_MYSQL_TEST_{upper}"), value.clone());
            let dsn = env.get(DSN).map_or("mysql:", String::as_str);
            env.insert(DSN.to_owned(), with_part(dsn, key, &value));
        }
    }
}

/// What every run's database is named before its process and its number.
const PREFIX: &str = "xtask_phpt_";

/// The pinned php's connection to the server as the recipe names it, under
/// which `$db` runs the statements after it, each failure an exit of 1
/// with the server's message.
const CONNECT: &str = "try {
    $db = new PDO(getenv('PDO_MYSQL_TEST_DSN'), getenv('PDO_MYSQL_TEST_USER') ?: 'root', getenv('PDO_MYSQL_TEST_PASS') ?: '', [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);
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
        let dsn = env.get(DSN).map_or("mysql:", String::as_str);
        env.insert(DSN.to_owned(), with_part(dsn, "dbname", &self.name));
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

/// `dsn` with its `key` part `value`: replaced, or added where it names
/// none, as a run's database is its `dbname` and a caller's address its
/// `host` and `port`.
pub(crate) fn with_part(dsn: &str, key: &str, value: &str) -> String {
    let (driver, rest) = dsn.split_once(':').unwrap_or(("mysql", dsn));
    let mut named = false;
    let mut parts: Vec<String> = rest
        .split(';')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((name, _)) if name.trim() == key => {
                named = true;
                format!("{key}={value}")
            }
            _ => part.to_owned(),
        })
        .collect();
    if !named {
        parts.push(format!("{key}={value}"));
    }
    format!("{driver}:{}", parts.join(";"))
}
