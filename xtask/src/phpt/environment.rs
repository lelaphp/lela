//! The environment a test's programs get, built in this one place as
//! run-tests.php builds it at the pinned tag: what the runner adds
//! to its own environment once for every test (run-tests.php:215-216,
//! 645-677),
//! then, for each test, a redirect's environment (run-tests.php:2228), the
//! CGI variables emptied (run-tests.php:2000-2008), the test's `--ENV--`
//! (run-tests.php:2009-2018) and `TEST_PHP_EXTRA_ARGS`
//! (run-tests.php:2115); a CGI-shaped test's request is laid over that by
//! the `cgi` module. A later entry of a name stands over an earlier, as a
//! PHP array's does, and an entry left empty is taken out of the
//! environment when the program starts, as `proc_open` takes it out
//! ([`crate::oracle::Invocation::env`]).
//!
//! Upstream's runner starts from its own process's environment; this one
//! starts from the recipe's, which the pin clears and states (`oracle.toml`,
//! `[recipe.env]`), so what differs from upstream's CI is the pin's to say
//! and not this module's.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use super::file::Phpt;
use crate::oracle;

/// The CGI variables run-tests.php empties for every test before it reads
/// `--ENV--` (run-tests.php:2000-2008), so a test's own may set them and
/// no test finds another's; `TZ` among them, so a test's programs run
/// with none unless its `--ENV--` names one.
const RESET: [&str; 8] = [
    "REDIRECT_STATUS",
    "QUERY_STRING",
    "PATH_TRANSLATED",
    "SCRIPT_FILENAME",
    "REQUEST_METHOD",
    "CONTENT_TYPE",
    "CONTENT_LENGTH",
    "TZ",
];

/// A program's environment over the recipe's, in the order run-tests.php
/// assigns it, a later entry of a name standing over an earlier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Environment(Vec<(String, OsString)>);

impl Environment {
    /// What run-tests.php adds to its own environment for every test of a
    /// run, `recipe` being the environment it runs in: `TEMP`, where
    /// `recipe` gives none, as its own `sys_get_temp_dir()` answers
    /// (run-tests.php:215-216, [`temp_dir`]); `SKIP_ONLINE_TESTS`, set as upstream sets it unless `--online`
    /// is asked for, which this runner never is (run-tests.php:645-650);
    /// `SKIP_IO_CAPTURE_TESTS`, set as upstream sets it when its own streams
    /// are not all a terminal (run-tests.php:652-656), since this runner
    /// never hands a test a terminal on a stream it leaves uncaptured; the
    /// binary under test by the names the corpus spawns it by, and the CGI
    /// host beside it, `cgi`, and the debugger host, `phpdbg`, each empty
    /// where there is none, as `get_binary` leaves a host it finds none of
    /// (run-tests.php:667-677, 1003-1019); and `TEST_PHP_SRCDIR`, `root`,
    /// the tree the programs run in, never the pinned one.
    pub(crate) fn of_runner(
        binary: &Path,
        (cgi, phpdbg): (Option<&Path>, Option<&Path>),
        root: &Path,
        recipe: &BTreeMap<String, String>,
    ) -> Environment {
        let mut env = Environment::default();
        if recipe.get("TEMP").is_none_or(php_empty) {
            env.set("TEMP", temp_dir(recipe));
        }
        env.set("SKIP_ONLINE_TESTS", "1");
        env.set("SKIP_IO_CAPTURE_TESTS", "1");
        let named = |path: Option<&Path>| {
            path.map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        for (host, path) in [
            ("TEST_PHP_EXECUTABLE", named(Some(binary))),
            ("TEST_PHP_CGI_EXECUTABLE", named(cgi)),
            ("TEST_PHPDBG_EXECUTABLE", named(phpdbg)),
        ] {
            env.set(&format!("{host}_ESCAPED"), escaped(&path));
            env.set(host, path);
        }
        env.set(oracle::TREE_VARIABLE, root.as_os_str());
        env
    }

    /// One test's environment over the runner's: a redirect's entries
    /// (`redirected`), the CGI variables emptied, the test's own
    /// `--ENV--`, and `TEST_PHP_EXTRA_ARGS`, in run-tests.php's order.
    pub(crate) fn of_test(
        &self,
        redirected: &[(String, OsString)],
        phpt: &Phpt,
        test: &Path,
        extra_args: String,
    ) -> Environment {
        let mut env = self.clone();
        env.extend(redirected.iter().cloned());
        env.reset();
        env.extend(env_of(phpt, test));
        env.set("TEST_PHP_EXTRA_ARGS", extra_args);
        env
    }

    /// The CGI variables emptied, as run-tests.php empties them for every
    /// test before its `--ENV--`.
    pub(crate) fn reset(&mut self) {
        for name in RESET {
            self.set(name, "");
        }
    }

    /// `name` set to `value`, over whatever stood before it.
    pub(crate) fn set(&mut self, name: &str, value: impl Into<OsString>) {
        self.0.push((name.to_owned(), value.into()));
    }

    /// Whether `name` is set to something php's `!empty()` holds true,
    /// what run-tests.php asks before it lays a request variable over a
    /// test's own.
    pub(crate) fn given(&self, name: &str) -> bool {
        self.0
            .iter()
            .rev()
            .find(|(had, _)| had == name)
            .is_some_and(|(_, value)| !php_empty(value))
    }

    /// The entries in the order they were set, what an invocation hands a
    /// program over the recipe's.
    pub(crate) fn entries(&self) -> &[(String, OsString)] {
        &self.0
    }
}

impl Extend<(String, OsString)> for Environment {
    fn extend<I: IntoIterator<Item = (String, OsString)>>(&mut self, entries: I) {
        self.0.extend(entries);
    }
}

impl From<Vec<(String, OsString)>> for Environment {
    fn from(entries: Vec<(String, OsString)>) -> Environment {
        Environment(entries)
    }
}

/// Whether php's `empty()` holds a string true: empty, or `"0"`.
fn php_empty(value: impl AsRef<std::ffi::OsStr>) -> bool {
    let value = value.as_ref();
    value.is_empty() || value == "0"
}

/// What php's `sys_get_temp_dir()` answers in `env` with no
/// `sys_temp_dir`, as `php_get_temporary_directory` finds it
/// (`main/php_open_temporary_file.c` at the tag): `TMPDIR` less one
/// trailing slash where it names one, and the C library's `P_tmpdir`
/// otherwise, `/var/tmp/` on macOS and `/tmp` under glibc and musl.
fn temp_dir(env: &BTreeMap<String, String>) -> String {
    match env.get("TMPDIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => dir.strip_suffix('/').unwrap_or(dir).to_owned(),
        None if cfg!(target_vendor = "apple") => "/var/tmp/".to_owned(),
        None => "/tmp".to_owned(),
    }
}

/// A test's `--ENV--`, one `NAME=value` a line, the value its bytes, with
/// `{PWD}` the test's directory and `{TMP}` the scratch one.
pub(crate) fn env_of(phpt: &Phpt, test: &Path) -> Vec<(String, OsString)> {
    use std::os::unix::ffi::OsStringExt;
    let beside = test
        .parent()
        .map(|dir| dir.as_os_str().as_encoded_bytes().to_vec())
        .unwrap_or_default();
    let body = phpt.section("ENV").unwrap_or_default();
    body.split(|b| *b == b'\n')
        .filter_map(|line| {
            let eq = line.iter().position(|b| *b == b'=')?;
            let name = String::from_utf8_lossy(&line[..eq]).trim().to_owned();
            let value = oracle::replace(line[eq + 1..].trim_ascii(), b"{PWD}", &beside);
            let value = oracle::replace(
                &value,
                b"{TMP}",
                std::env::temp_dir().as_os_str().as_encoded_bytes(),
            );
            (!name.is_empty()).then(|| (name, OsString::from_vec(value)))
        })
        .collect()
}

/// php's `escapeshellarg` on a POSIX host: the word in single quotes,
/// each quote inside closed, escaped and reopened.
pub(super) fn escaped(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<'a>(env: &'a Environment, name: &str) -> Option<&'a std::ffi::OsStr> {
        env.entries()
            .iter()
            .rev()
            .find(|(had, _)| had == name)
            .map(|(_, value)| value.as_os_str())
    }

    #[test]
    fn the_runner_skips_the_online_and_the_capture_tests_as_upstream_does_off_a_terminal() {
        let none = BTreeMap::new();
        let env =
            Environment::of_runner(Path::new("/b/php"), (None, None), Path::new("/tree"), &none);
        assert_eq!(value(&env, "SKIP_ONLINE_TESTS"), Some("1".as_ref()));
        assert_eq!(value(&env, "SKIP_IO_CAPTURE_TESTS"), Some("1".as_ref()));
        assert_eq!(value(&env, "TEST_PHP_EXECUTABLE"), Some("/b/php".as_ref()));
        assert_eq!(
            value(&env, "TEST_PHP_EXECUTABLE_ESCAPED"),
            Some("'/b/php'".as_ref())
        );
        assert_eq!(value(&env, "TEST_PHP_CGI_EXECUTABLE"), Some("".as_ref()));
        assert_eq!(value(&env, "TEST_PHP_SRCDIR"), Some("/tree".as_ref()));
    }

    #[test]
    fn a_test_empties_the_cgi_variables_and_its_own_env_stands_over_them() {
        let runner = Environment::of_runner(
            Path::new("/b/php"),
            (None, None),
            Path::new("/tree"),
            &BTreeMap::new(),
        );
        let text = "--TEST--\nn\n--ENV--\nTZ=GMT\nemptyval=\n--FILE--\nx\n--EXPECT--\n1\n";
        let phpt = Phpt::parse(Path::new("/t/x.phpt"), text.as_bytes()).unwrap();
        let redirected = [("REQUEST_METHOD".to_owned(), OsString::from("GET"))];
        let env = runner.of_test(&redirected, &phpt, Path::new("/t/x.phpt"), " -n".into());
        assert_eq!(value(&env, "TZ"), Some("GMT".as_ref()));
        assert_eq!(
            value(&env, "REQUEST_METHOD"),
            Some("".as_ref()),
            "the reset stands over a redirect's"
        );
        assert_eq!(value(&env, "emptyval"), Some("".as_ref()));
        assert!(!env.given("emptyval") && !env.given("QUERY_STRING"));
        assert_eq!(value(&env, "TEST_PHP_EXTRA_ARGS"), Some(" -n".as_ref()));
        assert_eq!(escaped("/a/it's/php"), r"'/a/it'\''s/php'");
    }

    /// run-tests.php sets `TEMP` in its own environment, which every test
    /// inherits, to what its `sys_get_temp_dir()` answers where its
    /// environment gives none (run-tests.php:215-216): the recipe's
    /// `TMPDIR` less a trailing slash, the C library's `P_tmpdir` where
    /// the recipe names none, and the recipe's own `TEMP` where it names
    /// one.
    #[test]
    fn the_runner_gives_temp_as_its_own_sys_get_temp_dir_answers() {
        let temp = |recipe: &[(&str, &str)]| {
            let recipe = recipe
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect();
            let env =
                Environment::of_runner(Path::new("/b/php"), (None, None), Path::new("/t"), &recipe);
            value(&env, "TEMP").map(|v| v.to_string_lossy().into_owned())
        };
        assert_eq!(
            temp(&[("TMPDIR", "/run/tmp/")]).as_deref(),
            Some("/run/tmp")
        );
        let p_tmpdir = if cfg!(target_vendor = "apple") {
            "/var/tmp/"
        } else {
            "/tmp"
        };
        assert_eq!(temp(&[]).as_deref(), Some(p_tmpdir));
        assert_eq!(temp(&[("TMPDIR", "")]).as_deref(), Some(p_tmpdir));
        assert_eq!(temp(&[("TEMP", "/mine"), ("TMPDIR", "/run/tmp")]), None);
        assert_eq!(temp(&[("TEMP", "0")]).as_deref(), Some(p_tmpdir));
    }
}
