//! `xtask fpm-compare`: the oracle for what PHP code sees under
//! makala. Each request of the HTTP-served subset is served by a makala of
//! its own over HTTP and by a pool of its own of the pinned php-fpm under
//! the pin's `[fastcgi]` mapping, the same client the `request
//! makala` witnesses are served by (`witness::makala`), and the two
//! responses are held to each other as a client of nginx in front of
//! php-fpm reads them: the status code, each header line PHP set, in
//! order within its name, and the body. One reading, at makala's crate.
//!
//! **The differences known before a request is sent**, each handled in
//! the one place this list names, and known from makala's witnesses:
//!
//! 1. *A header name's case.* makala's HTTP stack writes every name in
//!    lowercase, so both responses are read with their names
//!    lowercased, and without the server's own `date` and `connection`
//!    lines, by `witness::makala`'s rendering, the witnesses' too.
//! 2. *The ports.* `REMOTE_PORT` and `SERVER_PORT` are each connection's
//!    own, so php-fpm is handed makala's two through the mapping's
//!    `$remote_port` and `$server_port`, and they agree by construction.
//! 3. *`SERVER_SOFTWARE`.* makala names itself `makala/<version>` where
//!    the mapping's nginx names itself `nginx/<version>`, so makala's name
//!    is replaced in its response by the mapping's, [`software`].
//! 4. *What php makes at random per request*, an uploaded file's temporary
//!    name and a session id it made, each read as one token on
//!    both sides (`Entry::randoms_named_alike`), as the scratch directory
//!    each side's program stands in is by the recipe's own normalisation,
//!    the two directories of one length, and named as `{PWD}` in the
//!    entry's ini (`Entry::ini_in`).
//! 5. *The order of header lines across names.* hyper's `HeaderMap`
//!    keeps every value of a name together at the name's first place, so
//!    makala cannot interleave two lines of one name with another's, as
//!    php's header list may. Both responses are read with their
//!    lines in the order of their names, the lines of one name in the
//!    order they came, [`names_folded`]: RFC 9110 §5.3 makes the order
//!    across names insignificant and the order within one significant.
//! 6. *What php-fpm registers that makala does not:* `FCGI_ROLE`, and
//!    `USER` and `HOME` under `clear_env = yes`, the pool's setting here,
//!    which also keeps the process's environment out of `$_SERVER`. Not
//!    normalised: a response that shows them differs, and is counted as a
//!    difference to be filed.
//!
//! **What a difference is.** Responses that differ after the first five
//! are served once more by each side. A pair that then agrees is the
//! same, the first having crossed a second's edge between the two runs;
//! a pair that differs again is a difference, named by its test and the
//! first line the second pair differs at, whether or not a side's answer
//! moved between its two runs. A page that renders a time beside what
//! the binary is (`phpinfo()`) moves on both sides and differs on both
//! runs, and was once counted apart as unstable and passed; a side that
//! moves is no reason to hold nothing, so it is a difference, and a
//! by-design one is a row. A request either side could not be
//! served is a bork. A difference an accepted `makala:` row names is accepted,
//! counted apart and named under its row, and a row whose test agrees is
//! named stale (`super::held`). The reading passes with no other
//! difference and no bork, and is a skip where nothing was compared; a
//! difference that stands is filed as a ticket, and its acceptance is a
//! row, a person's.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, anyhow};

use super::{Consulted, Entry, Held, SCOPE};
use crate::divergence::Registry;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Invocation, Oracle, Program, Promise, diff, hosts::CGI, normalise_in};
use crate::phpt::adapter::{SKIPIF_SHOWS, Said, system_with_timeout};
use crate::phpt::environment::Environment;
use crate::reading::{Reading, Status};
use crate::witness::makala::{self, Ports};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "fpm-compare";

/// How many differences the message names, the rest counted.
const NAMED: usize = 12;

/// What one entry came to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Came {
    Same,
    /// The line diff, php-fpm's as `-` and makala's as `+`.
    Differs(String),
    Bork(String),
}

/// `xtask fpm-compare`: the subset of the pinned tree, or the tests of it
/// `only` names by their paths, compared; one reading, a skip where this
/// machine has neither the tree nor the pinned php-fpm.
pub(crate) fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Reading> {
    let pin = manifest.pin()?;
    let snapshot = Snapshot::disk(root)?;
    let krate = (manifest.crate_named(SCOPE)).ok_or_else(|| anyhow!("no [[crate]] {SCOPE}"))?;
    let mut reading = Reading::for_crate(INSTRUMENT, krate, &snapshot)?.under(pin);
    let oracle = pin.oracle();
    let fpm = Promise::Oracle.or_skip(oracle.asserted().and_then(|()| {
        (pin.sapi.fpm.as_ref().map(|fpm| fpm.path.clone()))
            .ok_or_else(|| anyhow!("oracle.toml's [sapi] names no php-fpm"))
    }))?;
    let tree = Promise::Tree.or_skip(pin.source.tree())?;
    let (Some(fpm), Some(tree)) = (fpm, tree) else {
        reading.message = format!(
            "{INSTRUMENT}: this machine has not both the pinned php-fpm and the pinned php-src, so nothing was compared"
        );
        return Ok(reading);
    };
    let (mut entries, mut left) = super::subset(&tree)?;
    narrowed(&mut entries, &mut left, only);
    let host = super::makala(root)?;
    reading.judged(&host);
    let registry = Registry::load(root)?;
    let mut ledger = Ledger::new(&registry);
    for entry in &entries {
        if let Some(reason) = skipped(&oracle, &tree, entry)? {
            println!("skip {} ({reason})", entry.id);
            left.push(entry.id.clone());
            continue;
        }
        let came = compared(&oracle, &fpm, &host.path, entry)
            .unwrap_or_else(|why| Came::Bork(format!("{why:#}")));
        println!("{}", ledger.hold(&entry.id, came));
    }
    ledger.tally(&mut reading, left.len())?;
    Ok(reading)
}

/// The corpus and the tests it left out narrowed to the tests `only`
/// names, where it names any, so a narrowed run counts as left out only
/// the tests it was asked for.
fn narrowed(entries: &mut Vec<Entry>, left: &mut Vec<String>, only: &[String]) {
    if !only.is_empty() {
        entries.retain(|entry| only.contains(&entry.id));
        left.retain(|test| only.contains(test));
    }
}

/// Why upstream's runner, run over the pinned build, skips the entry, or
/// what its `--SKIPIF--` wrote where upstream borks the test and runs
/// nothing, its words read by the phpt lane's rule (`adapter::Said`);
/// none for an entry with no section or one the section does not skip.
/// Every entry is CGI-shaped, so the section runs as run-tests.php runs
/// such a test's: through the pinned php-cgi with `-C -q` and
/// `SKIPIF_SHOWS` (run-tests.php:1897-1901, 2133-2134), so a section
/// that skips under the CLI alone runs here, and in the runner's
/// environment over the recipe's, the pinned php as `TEST_PHP_EXECUTABLE`
/// with its hosts beside it and the CGI variables emptied
/// (run-tests.php:667-677, 2000-2008), so a section that looks for
/// php-cgi there finds it. The section is written to a directory of its
/// own, so the pinned tree at `tree` is never written to, and runs in the
/// test's directory there, so its `include('skipif.inc')` finds the file
/// beside the test through the include path's `.`; it runs without the
/// test's `--ENV--`, so a section that reads `__DIR__` or the test's own
/// environment may answer otherwise than in the phpt lane.
fn skipped(oracle: &Oracle<'_>, tree: &Path, entry: &Entry) -> Result<Option<String>> {
    let Some(body) = &entry.skipif else {
        return Ok(None);
    };
    let pin = oracle.pin();
    let cgi = oracle.pinned_host(CGI)?;
    let phpdbg = (pin.sapi.phpdbg.as_ref()).map(|host| host.path.as_path());
    let recipe = pin.recipe.environment()?;
    let mut env = Environment::of_runner(&pin.oracle.path, (Some(&cgi), phpdbg), tree, &recipe);
    env.reset();
    let own = oracle.tree("fpm-compare-skipif")?;
    let program = own.write("skipif.php", body)?;
    let flags = ["-C", "-q"].map(String::from);
    let shown = SKIPIF_SHOWS.map(String::from);
    let ran = system_with_timeout(
        oracle,
        &Invocation {
            ini: &shown,
            flags: &flags,
            env: env.entries(),
            cwd: tree.join(&entry.id).parent(),
            ..Invocation::new(&cgi, Program::Beside(&program))
        },
    )?;
    Ok(match Said::ran(&ran) {
        Said::Skip(why) => Some(why),
        Said::Bork(said) => Some(format!("invalid output from SKIPIF: {said}")),
        Said::XFail(_) | Said::Warn(_) | Said::Run => None,
    })
}

/// One entry served by both and held: the same or a difference.
fn compared(oracle: &Oracle<'_>, fpm: &Path, host: &Path, entry: &Entry) -> Result<Came> {
    let recipe = &oracle.pin().recipe;
    // Two directories of one length, so a `string(n)` of a path is the
    // same number on both sides.
    let own = oracle.tree("fpm-compare")?;
    let (by_makala, by_fpm) = (own.join("m"), own.join("f"));
    for dir in [&by_makala, &by_fpm] {
        crate::make_dir(dir)?;
        std::fs::write(dir.join(&entry.script), &entry.program)?;
    }
    let nginx = &oracle.pin().fastcgi()?.nginx;
    let (in_makala, in_fpm) = (by_makala.join(&entry.script), by_fpm.join(&entry.script));
    // Each side's `{PWD}` the document root it serves, which both read
    // through the directory's real path.
    let (makala_ini, fpm_ini) = (
        entry.ini_in(&by_makala.canonicalize()?),
        entry.ini_in(&by_fpm.canonicalize()?),
    );
    let by_makala = || -> Result<(Vec<u8>, Ports)> {
        let answered = makala::by_makala(recipe, host, &in_makala, &entry.http, &makala_ini, &[])?;
        let rendered = names_folded(&makala::from_http(&answered.response));
        let rendered = entry.randoms_named_alike(&software(&rendered, &answered.software, nginx));
        Ok((
            normalise_in(&recipe.normalise, &by_makala, rendered),
            answered.ports,
        ))
    };
    let by_php_fpm = |ports: Ports| -> Result<Vec<u8>> {
        let (stdout, _) = makala::by_fpm(oracle, fpm, &in_fpm, &entry.http, &fpm_ini, ports)?;
        let rendered = entry.randoms_named_alike(&names_folded(&makala::from_cgi(&stdout)));
        Ok(normalise_in(&recipe.normalise, &by_fpm, rendered))
    };
    let pair = || -> Result<[Vec<u8>; 2]> {
        let (shown, ports) = by_makala()?;
        Ok([by_php_fpm(ports)?, shown])
    };
    held_twice(pair)
}

/// What an entry came to over `pair`, php-fpm's answer and makala's, asked
/// once and again where the first two differ: the same where either pair
/// agrees, and else a difference, the second pair's diff, whether or not
/// a side's answer moved between its two runs.
fn held_twice(mut pair: impl FnMut() -> Result<[Vec<u8>; 2]>) -> Result<Came> {
    let [mut wanted, mut shown] = pair()?;
    if wanted != shown {
        [wanted, shown] = pair()?;
    }
    Ok(if wanted == shown {
        Came::Same
    } else {
        Came::Differs(diff(&wanted, &shown))
    })
}

/// makala's response with its own `SERVER_SOFTWARE`, `makala/<version>`,
/// replaced by the mapping's, `nginx/<version>`: known difference 3.
fn software(response: &[u8], own: &str, nginx: &str) -> Vec<u8> {
    if own.is_empty() {
        return response.to_vec();
    }
    let nginx = format!("nginx/{nginx}");
    crate::oracle::replace(response, own.as_bytes(), nginx.as_bytes())
}

/// A rendered response with its header lines in the order of their names,
/// the lines of one name in the order they came: known difference 5, the
/// order across names folded as RFC 9110 §5.3 makes it insignificant, and
/// held within a name, where it is not.
fn names_folded(rendered: &[u8]) -> Vec<u8> {
    let Some(end) = rendered.windows(2).position(|w| w == b"\n\n") else {
        return rendered.to_vec();
    };
    let mut lines: Vec<&[u8]> = rendered[..end].split(|b| *b == b'\n').collect();
    if let Some((_, fields)) = lines.split_first_mut() {
        fields.sort_by_key(|line| line.split(|b| *b == b':').next());
    }
    let mut out = lines.join(&b'\n');
    out.extend_from_slice(&rendered[end..]);
    out
}

/// The first line a diff takes out and the first it puts in, in that
/// order, either left out where the diff has none.
pub(super) fn first_line(diff: &str) -> String {
    let first = |mark: char| diff.lines().find(|line| line.starts_with(mark));
    let lines: Vec<&str> = [first('-'), first('+')].into_iter().flatten().collect();
    lines.join(" / ")
}

/// The word one entry's line on stdout starts with.
fn word(came: &Came) -> &'static str {
    match came {
        Came::Same => "same",
        Came::Differs(_) => "differs",
        Came::Bork(_) => "bork",
    }
}

/// A run's entries, each held to the `makala:` rows as it comes: the one
/// place a row is read, for the line an entry prints and for the reading
/// alike.
struct Ledger<'r> {
    registry: &'r Registry,
    consulted: Consulted<'r>,
    /// Each entry, what it came to, and what the rows made of it: none
    /// for a borked entry, which no row touches.
    held: Vec<(String, Came, Option<Held<'r>>)>,
}

impl<'r> Ledger<'r> {
    fn new(registry: &'r Registry) -> Ledger<'r> {
        Ledger {
            registry,
            consulted: Consulted::default(),
            held: Vec::new(),
        }
    }

    /// `came`, what the entry `id` came to, held to the rows, and the line
    /// it prints: its word and its test, with an unaccepted difference's
    /// changed lines after, or `accepted <test> (<row>)` for a difference
    /// a row accepts.
    fn hold(&mut self, id: &str, came: Came) -> String {
        let held = match &came {
            Came::Same => Some(self.consulted.hold(self.registry, id, true)),
            Came::Differs(_) => Some(self.consulted.hold(self.registry, id, false)),
            Came::Bork(_) => None,
        };
        let mut line = match held {
            Some(Held::Accepted(row)) => format!("accepted {id} ({})", row.id),
            _ => format!("{} {id}", word(&came)),
        };
        if let (Came::Differs(diff), Some(Held::Differs)) = (&came, held) {
            let changed = diff.lines().filter(|line| line.starts_with(['-', '+']));
            for changed in changed.take(20) {
                write!(line, "\n  {changed}").ok();
            }
        }
        self.held.push((id.to_owned(), came, held));
        line
    }

    /// The reading's numbers, status and message from what each entry
    /// came to, `left` the tests the run left out: a difference a row
    /// accepts is counted apart from the failures and named under its row,
    /// and a row whose test now agrees is named stale.
    fn tally(self, reading: &mut Reading, left: usize) -> Result<()> {
        let came = &self.held;
        let same = came.iter().filter(|(_, c, _)| *c == Came::Same).count();
        // What stands once the rows are read: an unaccepted difference,
        // and a borked entry.
        let standing: Vec<(&str, &Came)> = (came.iter())
            .filter(|(_, _, held)| held.is_none_or(|held| held == Held::Differs))
            .map(|(id, c, _)| (id.as_str(), c))
            .collect();
        let count = |kind: fn(&Came) -> bool| standing.iter().filter(|(_, c)| kind(c)).count();
        let differs = count(|c| matches!(c, Came::Differs(_)));
        let borked = count(|c| matches!(c, Came::Bork(_)));
        for (key, n) in [
            ("entries", came.len()),
            ("same", same),
            ("differs", differs),
            ("borked", borked),
            ("left_out", left),
        ] {
            reading.count(key, n as u64)?;
        }
        let rows = self.consulted.counted(reading)?;
        reading.status = match (came.len(), differs + borked) {
            (0, _) => Status::Skip,
            (_, 0) => Status::Pass,
            _ => Status::Fail,
        };
        let mut message = format!(
            "{same} of {} requests answered alike by makala and the pinned php-fpm; {differs} differ, {borked} borked; {left} CGI-shaped tests left out",
            came.len()
        );
        let named: Vec<String> = (standing.iter())
            .filter_map(|(id, c)| match c {
                Came::Differs(diff) => Some(format!("{id} ({})", first_line(diff))),
                Came::Bork(why) => Some(format!("{id} ({why})")),
                Came::Same => None,
            })
            .collect();
        if !named.is_empty() {
            message.push_str(": ");
            message.push_str(&named[..named.len().min(NAMED)].join("; "));
            if named.len() > NAMED {
                write!(message, "; and {} more", named.len() - NAMED).ok();
            }
        }
        if let Some(rows) = rows {
            write!(message, "; {rows}").ok();
        }
        reading.message = message;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reading a run whose entries came to `came` gives, each held to
    /// `registry` through one ledger, `left` tests left out.
    fn tallied(came: &[(String, Came)], left: usize, registry: &Registry) -> Reading {
        let mut ledger = Ledger::new(registry);
        for (id, came) in came {
            ledger.hold(id, came.clone());
        }
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        ledger.tally(&mut reading, left).unwrap();
        reading
    }

    /// A `--SKIPIF--` runs as upstream's runs a CGI-shaped test's, through
    /// php-cgi in the runner's environment: ext/soap's skip under the CLI
    /// alone and sapi/cgi's `skipif.inc`, which looks for php-cgi beside
    /// `TEST_PHP_EXECUTABLE`, each run the test, and a skip still skips.
    #[test]
    fn a_skipif_runs_through_php_cgi_in_the_runner_s_environment() {
        let manifest = Manifest::load(&crate::built_root()).unwrap();
        let pin = manifest.pin().unwrap();
        let file = pin.oracle.file().map(Path::to_path_buf);
        let (Some(_), Some(tree)) = (
            Promise::Oracle.or_skip(file).unwrap(),
            Promise::Tree.or_skip(pin.source.tree()).unwrap(),
        ) else {
            return;
        };
        let entry = |id: &str, skipif: &str| Entry {
            id: id.to_owned(),
            program: Vec::new(),
            script: "a.php".to_owned(),
            skipif: Some(skipif.as_bytes().to_vec()),
            http: crate::witness::makala::Http::get(),
            ini: Vec::new(),
        };
        let oracle = pin.oracle();
        let soap = "<?php if (php_sapi_name()=='cli') echo 'skip';";
        let cgi = "<?php include \"skipif.inc\"; ?>";
        for (id, skipif, wanted) in [
            ("ext/soap/tests/server029.phpt", soap, None),
            ("sapi/cgi/tests/bug61605.phpt", cgi, None),
            (
                "ext/soap/tests/a.phpt",
                "<?php echo 'skip because';",
                Some("because"),
            ),
        ] {
            let came = skipped(&oracle, &tree, &entry(id, skipif)).unwrap();
            assert_eq!(came.as_deref(), wanted, "{id}");
        }
    }

    /// A narrowed run counts as left out only the tests it asked for that
    /// the corpus leaves out, and compares only those it asked for; a run
    /// that names none keeps the whole corpus.
    #[test]
    fn a_narrowed_run_counts_only_the_asked_for_tests_left_out() {
        let entry = |id: &str| Entry {
            id: id.to_owned(),
            program: Vec::new(),
            script: "a.php".to_owned(),
            skipif: None,
            http: crate::witness::makala::Http::get(),
            ini: Vec::new(),
        };
        let corpus = || {
            (
                vec![entry("a.phpt"), entry("b.phpt")],
                vec!["c.phpt".to_owned(), "d.phpt".to_owned()],
            )
        };
        let (mut entries, mut left) = corpus();
        narrowed(
            &mut entries,
            &mut left,
            &["b.phpt".to_owned(), "d.phpt".to_owned()],
        );
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!((ids, left), (vec!["b.phpt"], vec!["d.phpt".to_owned()]));
        let (mut entries, mut left) = corpus();
        narrowed(&mut entries, &mut left, &["a.phpt".to_owned()]);
        assert_eq!(
            (entries.len(), left.len()),
            (1, 0),
            "a compared test leaves none out"
        );
        let (mut entries, mut left) = corpus();
        narrowed(&mut entries, &mut left, &[]);
        assert_eq!((entries.len(), left.len()), (2, 2));
    }

    /// The line a request prints is the rows' word on it, over the fixture
    /// (`xtask/fixtures/makala`): a difference the row accepts prints
    /// `accepted` and the row with no diff, one no row names prints
    /// `differs` and its changed lines.
    #[test]
    fn a_difference_a_row_accepts_prints_accepted_and_its_row_and_no_diff() {
        let registry = crate::makala::fixture();
        let mut ledger = Ledger::new(&registry);
        let differs = || Came::Differs("-a\n+b\n".to_owned());
        assert_eq!(
            ledger.hold("ext/filter/tests/004.phpt", differs()),
            "accepted ext/filter/tests/004.phpt (makala:ext/filter/tests/004.phpt)"
        );
        assert_eq!(
            ledger.hold("ext/filter/tests/011.phpt", differs()),
            "differs ext/filter/tests/011.phpt\n  -a\n  +b"
        );
        assert_eq!(
            ledger.hold("ext/filter/tests/007.phpt", Came::Same),
            "same ext/filter/tests/007.phpt"
        );
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        ledger.tally(&mut reading, 0).unwrap();
        assert_eq!(
            reading.numbers.get("accepted").copied(),
            Some(crate::reading::Number::count(1))
        );
    }

    /// makala's own name is the mapping's nginx in what is compared, and a
    /// response that does not carry it is kept as it came.
    #[test]
    fn makala_s_server_software_reads_as_the_mapping_s_nginx() {
        let shown = software(b"200\n\nmakala/0.0.0 on", "makala/0.0.0", "1.30.5");
        assert_eq!(shown, b"200\n\nnginx/1.30.5 on");
        assert_eq!(software(b"200\n\nx", "makala/0.0.0", "1.30.5"), b"200\n\nx");
    }

    /// The reading fails on a difference or a bork, names each with its
    /// first differing line, and passes when every entry is the same.
    #[test]
    fn a_difference_or_a_bork_fails_the_reading_and_is_named() {
        let came = vec![
            ("a.phpt".to_owned(), Came::Same),
            ("b.phpt".to_owned(), Came::Differs("-x / +y".to_owned())),
            ("c.phpt".to_owned(), Came::Bork("refused".to_owned())),
        ];
        let reading = tallied(&came, 4, &Registry::default());
        assert_eq!(reading.status, Status::Fail);
        assert!(
            reading.message.contains("b.phpt (-x / +y)"),
            "{}",
            reading.message
        );
        assert!(reading.message.contains("4 CGI-shaped tests left out"));
        assert!(reading.message.contains("1 differ, 1 borked"));
        let reading = tallied(&came[..1], 0, &Registry::default());
        assert_eq!(reading.status, Status::Pass);
        let reading = tallied(&[], 0, &Registry::default());
        assert_eq!(reading.status, Status::Skip, "nothing compared is no pass");
        assert_eq!(first_line(&diff(b"200\n\na\nb", b"200\n\na\nc")), "-b / +c");
    }

    /// A pair that agrees, first or once more, is the same; one that
    /// differs twice is a difference named by its second diff, though a
    /// side's answer moved between its runs, as `phpinfo()`'s page does
    /// (`ext/standard/tests/general_functions/phpinfo2.phpt`).
    #[test]
    fn a_pair_that_differs_twice_is_a_difference_though_a_side_moved() {
        let asked = |pairs: &[[&[u8]; 2]]| {
            let mut pairs = pairs.iter().map(|pair| pair.map(<[u8]>::to_vec));
            held_twice(|| pairs.next().ok_or_else(|| anyhow!("asked a third time"))).unwrap()
        };
        assert_eq!(asked(&[[b"a", b"a"]]), Came::Same);
        assert_eq!(asked(&[[b"a", b"b"], [b"c", b"c"]]), Came::Same);
        let moved = asked(&[
            [b"200\n\nt=1", b"200\n\nt=2"],
            [b"200\n\nt=3", b"200\n\nt=4"],
        ]);
        assert_eq!(moved, Came::Differs(diff(b"200\n\nt=3", b"200\n\nt=4")));
    }

    /// The negative control: a difference no `makala:` row names fails the
    /// reading, one the row names is accepted, counted apart and named
    /// under the row, and a test the row covers that now agrees names the
    /// row stale without failing the reading.
    #[test]
    fn a_difference_no_makala_row_names_fails_and_one_a_row_names_is_accepted() {
        let registry = crate::makala::fixture();
        let differs = || Came::Differs("-a\n+b\n".to_owned());
        let came = vec![
            ("ext/filter/tests/004.phpt".to_owned(), differs()),
            ("ext/filter/tests/007.phpt".to_owned(), Came::Same),
            ("ext/filter/tests/011.phpt".to_owned(), differs()),
        ];
        let reading = tallied(&came, 0, &registry);
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        let said = &reading.message;
        assert!(
            said.contains("1 differ,") && said.contains("011.phpt (-a / +b)"),
            "{said}"
        );
        assert!(
            said.contains("1 differs as a makala: row accepts: makala:ext/filter/tests/004.phpt"),
            "{said}"
        );
        assert!(said.contains("stale, the test now alike: ext/filter/tests/007.phpt under makala:ext/filter/tests/004.phpt"), "{said}");
        let reading = tallied(&came[..2], 0, &registry);
        assert_eq!(reading.status, Status::Pass, "{}", reading.message);
        let count = |key: &str| reading.numbers.get(key).copied();
        assert_eq!(count("accepted"), Some(crate::reading::Number::count(1)));
        assert_eq!(count("differs"), Some(crate::reading::Number::count(0)));
    }

    /// Header lines are held in order within a name and folded across
    /// names: php's `Vary`, `Content-Encoding`, `Vary` reads as makala's
    /// grouping (`ext/zlib/tests/bug51269.phpt`), two names reordered read
    /// alike, and two lines of one name swapped still differ.
    #[test]
    fn header_order_is_held_within_a_name_and_folded_across_names() {
        let php = names_folded(
            b"200\nvary: Cookie\ncontent-encoding: gzip\nvary: Accept-Encoding\n\nfoo\n\nbar",
        );
        let makala = names_folded(
            b"200\nvary: Cookie\nvary: Accept-Encoding\ncontent-encoding: gzip\n\nfoo\n\nbar",
        );
        assert_eq!(
            String::from_utf8_lossy(&php),
            String::from_utf8_lossy(&makala)
        );
        assert!(
            php.ends_with(b"\n\nfoo\n\nbar"),
            "the body is kept as it came"
        );
        let reordered = names_folded(b"200\nx-a: 1\nx-b: 2\n\n");
        assert_eq!(reordered, names_folded(b"200\nx-b: 2\nx-a: 1\n\n"));
        let swapped = names_folded(b"200\nvary: Accept-Encoding\nvary: Cookie\n\n");
        assert_ne!(
            swapped,
            names_folded(b"200\nvary: Cookie\nvary: Accept-Encoding\n\n")
        );
        assert_eq!(names_folded(b"200\n\nbody"), b"200\n\nbody");
    }
}
