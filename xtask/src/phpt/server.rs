//! The `MySQL` server the `mysqli` and `pdo_mysql` suites need (#1345, #1366).
//! Without one the oracle skips 412 of `mysqli`'s 449 tests and 161 of
//! `pdo_mysql`'s 167, and a reading of those skips would be a verdict on a
//! server that was not there. So before a run of such a subset the pinned
//! php asks the server the recipe's `PDO_MYSQL_TEST_DSN` names for
//! `SELECT VERSION()`: no answer refuses the run, and the answer is kept on
//! the reading, since `ci/mysql/compose.yaml`'s image tag floats and a shift
//! in the oracle's verdict is then traced to its bump. The runner holds the
//! same line: handed such a subset with no server named, it refuses rather
//! than read every test's `skip` as the subset's verdict.

use std::path::Path;

use anyhow::{Result, bail};

use super::Subset;
use crate::oracle::{Oracle, Program};

/// The corpus directories whose tests need the server.
const SUITES: [&str; 2] = ["ext/mysqli/tests", "ext/pdo_mysql/tests"];

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
    subset
        .dirs
        .iter()
        .any(|dir| SUITES.iter().any(|suite| dir.under == Path::new(suite)))
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
    if seen.exit != Some(0) || said.is_empty() {
        bail!(
            "{scope}: refused, not skipped: the subset needs the MySQL server the recipe's PDO_MYSQL_TEST_DSN names, and it did not answer ({said}); {START}"
        );
    }
    Ok(said)
}
