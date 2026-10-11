//! `xtask fpm-compare` (#2349): the oracle for what PHP code sees under
//! makala. Each request of the HTTP-served subset is served by a makala of
//! its own over HTTP and by a pool of its own of the pinned php-fpm under
//! the pin's `[fastcgi]` mapping (#2334), the same client the `request
//! makala` witnesses are served by (`witness::makala`), and the two
//! responses are held to each other as a client of nginx in front of
//! php-fpm reads them: the status code, each header line PHP set in order,
//! and the body. One reading, at makala's crate.
//!
//! **The differences known before a request is sent**, each handled in
//! the one place this list names (#2349, from #2347's witnesses):
//!
//! 1. *A header name's case.* makala's HTTP stack writes every name in
//!    lowercase (#2570), so both responses are read with their names
//!    lowercased, and without the server's own `date` and `connection`
//!    lines, by `witness::makala`'s rendering, the witnesses' too.
//! 2. *The ports.* `REMOTE_PORT` and `SERVER_PORT` are each connection's
//!    own, so php-fpm is handed makala's two through the mapping's
//!    `$remote_port` and `$server_port`, and they agree by construction.
//! 3. *`SERVER_SOFTWARE`.* makala names itself `makala/<version>` (#2335)
//!    where the mapping's nginx names itself `nginx/<version>`, so makala's
//!    name is replaced in its response by the mapping's, [`software`].
//! 4. *An uploaded file's temporary name*, random per request, read as one
//!    token on both sides (`super::uploads_named_alike`), as the scratch
//!    directory each side's program stands in is by the recipe's own
//!    normalisation, the two directories of one length.
//! 5. *The order of header lines across names.* hyper's `HeaderMap`
//!    keeps every value of a name together at the name's first place, so
//!    makala cannot interleave two lines of one name with another's, as
//!    php's header list may (#2640). Both responses are read with their
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
//! a side whose answer moves between its two runs (a time, a page that
//! renders the build) makes the entry unstable, named in the message and
//! counted apart, since no comparison of one run of each can hold it; and
//! two runs that differ alike are a difference, named by its test and its
//! first differing line. A request either side could not be served is a
//! bork. A difference an accepted `makala:` row names is accepted,
//! counted apart and named under its row, and a row whose test agrees is
//! named stale (`super::held`, #2629). The reading passes with no other
//! difference and no bork, and is a skip where nothing was compared; a
//! difference that stands is filed as a ticket, and its acceptance is a
//! row, a person's.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, anyhow};

use super::{Consulted, Entry, Held, SCOPE};
use crate::divergence::Registry;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Oracle, Promise, diff, normalise_in};
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
    Unstable,
    Bork(String),
}

/// `xtask fpm-compare`: the subset of the pinned tree, or the tests of it
/// `only` names by their paths, compared; one reading, a skip where this
/// machine has neither the tree nor the pinned php-fpm.
pub(crate) fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Reading> {
    let snapshot = Snapshot::disk(root)?;
    let krate = (manifest.crate_named(SCOPE)).ok_or_else(|| anyhow!("no [[crate]] {SCOPE}"))?;
    let mut reading = Reading::for_crate(INSTRUMENT, krate, &snapshot)?.under(&manifest.pin);
    let pin = &manifest.pin;
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
    let (mut entries, left) = super::subset(&tree)?;
    if !only.is_empty() {
        entries.retain(|entry| only.contains(&entry.id));
    }
    let host = super::makala(root)?;
    let came: Vec<(String, Came)> = entries
        .iter()
        .map(|entry| {
            let came = compared(&oracle, &fpm, &host, entry)
                .unwrap_or_else(|why| Came::Bork(format!("{why:#}")));
            println!("{} {}", word(&came), entry.id);
            if let Came::Differs(diff) = &came {
                let changed = diff.lines().filter(|line| line.starts_with(['-', '+']));
                for line in changed.take(20) {
                    println!("  {line}");
                }
            }
            (entry.id.clone(), came)
        })
        .collect();
    tally(&mut reading, &came, left, &Registry::load(root)?)?;
    Ok(reading)
}

/// One entry served by both and held: the same, a difference, unstable.
fn compared(oracle: &Oracle<'_>, fpm: &Path, host: &Path, entry: &Entry) -> Result<Came> {
    let recipe = &oracle.pin().recipe;
    // Two directories of one length, so a `string(n)` of a path is the
    // same number on both sides.
    let own = oracle.tree("fpm-compare")?;
    let (by_makala, by_fpm) = (own.join("m"), own.join("f"));
    for dir in [&by_makala, &by_fpm] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join(&recipe.script_name), &entry.program)?;
    }
    let nginx = &oracle.pin().fastcgi()?.nginx;
    let by_makala = || -> Result<(Vec<u8>, Ports)> {
        let answered = makala::by_makala(recipe, host, &by_makala, &entry.http, &entry.ini, &[])?;
        let rendered = names_folded(&makala::from_http(&answered.response));
        let rendered = super::uploads_named_alike(&software(&rendered, &answered.software, nginx));
        Ok((
            normalise_in(&recipe.normalise, &by_makala, rendered),
            answered.ports,
        ))
    };
    let by_php_fpm = |ports: Ports| -> Result<Vec<u8>> {
        let (stdout, _) = makala::by_fpm(oracle, fpm, &by_fpm, &entry.http, &entry.ini, ports)?;
        let rendered = super::uploads_named_alike(&names_folded(&makala::from_cgi(&stdout)));
        Ok(normalise_in(&recipe.normalise, &by_fpm, rendered))
    };
    let (shown, ports) = by_makala()?;
    let wanted = by_php_fpm(ports)?;
    if wanted == shown {
        return Ok(Came::Same);
    }
    // Once more each: a pair that then agrees crossed a second's edge or
    // the like between the two, and a side whose answer moves is unstable.
    let (again, ports) = by_makala()?;
    let wanted_again = by_php_fpm(ports)?;
    Ok(if wanted_again == again {
        Came::Same
    } else if wanted_again != wanted || again != shown {
        Came::Unstable
    } else {
        Came::Differs(diff(&wanted, &shown))
    })
}

/// makala's response with its own `SERVER_SOFTWARE`, `makala/<version>`,
/// replaced by the mapping's, `nginx/<version>`: known difference 3.
fn software(response: &[u8], own: &str, nginx: &str) -> Vec<u8> {
    let theirs = format!("nginx/{nginx}");
    let (mut out, mut rest) = (Vec::new(), response);
    while let Some(at) = (!own.is_empty())
        .then(|| rest.windows(own.len()).position(|w| w == own.as_bytes()))
        .flatten()
    {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(theirs.as_bytes());
        rest = &rest[at + own.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// A rendered response with its header lines in the order of their names,
/// the lines of one name in the order they came: known difference 5, the
/// order across names folded as RFC 9110 §5.3 makes it insignificant, and
/// held within a name, where it is not (#2640, #2643).
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
        Came::Unstable => "unstable",
        Came::Bork(_) => "bork",
    }
}

/// The reading's numbers, status and message from what each entry came
/// to, each entry held to the `makala:` rows of `registry`: a difference a
/// row accepts is counted apart from the failures and named under its row,
/// and a row whose test now agrees is named stale.
fn tally(
    reading: &mut Reading,
    came: &[(String, Came)],
    left: usize,
    registry: &Registry,
) -> Result<()> {
    let mut consulted = Consulted::default();
    // What stands once the rows are read: each unaccepted difference,
    // and the unstable and borked entries, which no row touches.
    let standing: Vec<(&str, &Came)> = (came.iter())
        .filter(|(id, c)| match c {
            Came::Same => {
                consulted.hold(registry, id, true);
                false
            }
            Came::Differs(_) => consulted.hold(registry, id, false) == Held::Differs,
            Came::Unstable | Came::Bork(_) => true,
        })
        .map(|(id, c)| (id.as_str(), c))
        .collect();
    let count = |kind: fn(&Came) -> bool| standing.iter().filter(|(_, c)| kind(c)).count();
    let (differs, unstable, borked) = (
        count(|c| matches!(c, Came::Differs(_))),
        count(|c| matches!(c, Came::Unstable)),
        count(|c| matches!(c, Came::Bork(_))),
    );
    let same = came.iter().filter(|(_, c)| matches!(c, Came::Same)).count();
    for (key, n) in [
        ("entries", came.len()),
        ("same", same),
        ("differs", differs),
        ("unstable", unstable),
        ("borked", borked),
        ("left_out", left),
    ] {
        reading.count(key, n as u64)?;
    }
    let rows = consulted.counted(reading)?;
    reading.status = match (came.len(), differs + borked) {
        (0, _) => Status::Skip,
        (_, 0) => Status::Pass,
        _ => Status::Fail,
    };
    let mut message = format!(
        "{same} of {} requests answered alike by makala and the pinned php-fpm; {differs} differ, {unstable} unstable, {borked} borked; {left} CGI-shaped tests left out",
        came.len()
    );
    let named: Vec<String> = (standing.iter())
        .filter_map(|(id, c)| match c {
            Came::Differs(diff) => Some(format!("{id} ({})", first_line(diff))),
            Came::Bork(why) => Some(format!("{id} ({why})")),
            Came::Unstable => Some(format!("{id} (unstable)")),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// makala's own name is the mapping's nginx in what is compared, and a
    /// response that does not carry it is kept as it came.
    #[test]
    fn makala_s_server_software_reads_as_the_mapping_s_nginx() {
        let shown = software(b"200\n\nmakala/0.0.0 on", "makala/0.0.0", "1.30.5");
        assert_eq!(shown, b"200\n\nnginx/1.30.5 on");
        assert_eq!(software(b"200\n\nx", "makala/0.0.0", "1.30.5"), b"200\n\nx");
    }

    /// The reading fails on a difference or a bork, names each with its
    /// first differing line, and passes when every entry is the same or
    /// unstable.
    #[test]
    fn a_difference_or_a_bork_fails_the_reading_and_is_named() {
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        let came = vec![
            ("a.phpt".to_owned(), Came::Same),
            ("b.phpt".to_owned(), Came::Differs("-x / +y".to_owned())),
            ("c.phpt".to_owned(), Came::Unstable),
        ];
        tally(&mut reading, &came, 4, &Registry::default()).unwrap();
        assert_eq!(reading.status, Status::Fail);
        assert!(
            reading.message.contains("b.phpt (-x / +y)"),
            "{}",
            reading.message
        );
        assert!(reading.message.contains("4 CGI-shaped tests left out"));
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        tally(&mut reading, &came[..1], 0, &Registry::default()).unwrap();
        assert_eq!(reading.status, Status::Pass);
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        tally(&mut reading, &[], 0, &Registry::default()).unwrap();
        assert_eq!(reading.status, Status::Skip, "nothing compared is no pass");
        assert_eq!(first_line(&diff(b"200\n\na\nb", b"200\n\na\nc")), "-b / +c");
    }

    /// The fixture's registry (`xtask/fixtures/makala`): one accepted row
    /// naming 004.phpt and covering 007.phpt, none for 011.phpt.
    pub(crate) fn fixture() -> Registry {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/makala");
        Registry::parse(&std::fs::read_to_string(dir.join("divergence.toml")).unwrap()).unwrap()
    }

    /// The negative control: a difference no `makala:` row names fails the
    /// reading, one the row names is accepted, counted apart and named
    /// under the row, and a test the row covers that now agrees names the
    /// row stale without failing the reading.
    #[test]
    fn a_difference_no_makala_row_names_fails_and_one_a_row_names_is_accepted() {
        let registry = fixture();
        let differs = || Came::Differs("-a\n+b\n".to_owned());
        let came = vec![
            ("ext/filter/tests/004.phpt".to_owned(), differs()),
            ("ext/filter/tests/007.phpt".to_owned(), Came::Same),
            ("ext/filter/tests/011.phpt".to_owned(), differs()),
        ];
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        tally(&mut reading, &came, 0, &registry).unwrap();
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
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        tally(&mut reading, &came[..2], 0, &registry).unwrap();
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
