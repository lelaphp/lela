//! `xtask crosscheck`, the rendering diff (design §7.1, §4.4; harness
//! §7, ledger 41). php prints the whole of an extension itself: `php --re
//! <name>` constructs a `ReflectionExtension` and prints its
//! `__toString()`, every constant, ini entry, function, class, method,
//! property and the tentative-return row, in php's wording and order. The
//! lane runs the oracle's `--re` under the recipe for each extension the
//! binary under test carries ([`Carried`]), or the one `--ext` names, keeps
//! the oracle's bytes in the store under the extension's name and the oracle's digest,
//! and holds the engine's rendering to them byte for byte through
//! [`oracle::compare`], one reading per extension scoped `ext/<name>`.
//!
//! **The engine half.** The engine's rendering is the same
//! `ReflectionExtension::__toString()` the witness on `ext/reflection`
//! runs, `echo new ReflectionExtension('<name>');` served through the
//! linked adapter, [`oracle::linked`], one fresh engine of `php_bundle`
//! with every extension feature on but `pcntl` (harness §7, revision 43)
//! under the recipe's preamble, ini and normalisation, or, for `pcntl`,
//! which the harness does not link, and for `standard`, which the binary
//! registers its host's natives under ([`HOSTED`]), through the `php`
//! binary the witness lane builds for a fence marked `process` (revision
//! 61) under the same recipe; that `__toString()` is the seam the `php` binary's `--re` flag
//! calls once the hosts exist (design §7.1). A
//! request that ends on a status other than zero rendered nothing, and
//! the engine's own words for why, `ReflectionException: Extension "x"
//! does not exist` or a fatal, are the reading's cause
//! ([`Rendering::Absent`]); one that rendered is held byte for byte
//! ([`Rendering::Bytes`]), and the first differing line is in the
//! message. So the per-extension row is the port's progress meter: ctype
//! reads `pass`, and an extension whose declarations are not the oracle's
//! reads `fail` naming the line. A test hands the lane a directory of
//! renderings standing for the engine's ([`Ask::renderings`]), which is
//! how the fixture under `xtask/fixtures/crosscheck/` is measurable:
//! php's own rendering of `ctype` beside one that differs from it by one
//! byte.
//!
//! **The `--ri` column.** php's fifth flag, `php --ri <name>`, is
//! `php_info_print_module` in text mode (`sapi/cli/php_cli.c` at the
//! tag): the module's name as a one-column table, then its `MINFO` table,
//! the block `phpinfo()` shows per extension and the one thing `--re`
//! does not hold, what the extension says of its own configuration. The
//! lane renders it as a second [`Column`] beside `--re`, the oracle's
//! bytes kept as `<digest>.ri` beside the `.re`, the engine's side `(new
//! ReflectionExtension('<name>'))->info();`, which is the same
//! `php_info_print_module` call (`Request::extension_info`), one reading
//! per extension per column, the `--ri` one scoped `ext/<name>/ri`. An
//! extension without a `MINFO` hook prints php's `Version` row and its
//! ini entries on both sides, so the column grows with the ini system
//! without changing shape.
//!
//! **A host's own module.** `cgi-fcgi` exists only in php-cgi and
//! php-fpm, `cli_server` only in the built-in server, and the oracle's
//! CLI renders neither. `--ext cgi-fcgi` and `--ext cli_server` render
//! the module through its host on both sides, each column's program
//! served as a request by the oracle's host from `[sapi]` (or its `php
//! -S`) and by the engine's witness build of the same host, the
//! response's head let go ([`host`]). The readings stand under the host's
//! row, `sapi/cgi/cgi-fcgi`, `sapi/fpm/cgi-fcgi` and
//! `sapi/cli/cli_server`, each with its `/ri` beside it (#1496).
//!
//! **The load number.** The first line, `Extension [ <persistent>
//! extension #10 ctype version ... ]`, carries the module's load number,
//! which is the oracle's build order and not the bundle's. The pin's
//! `[recipe.normalise] extension_number_token` row (#317) replaces the
//! digits by its token in both streams, the oracle's as its run normalises
//! it and the engine's through [`oracle::normalise`], so a rendering
//! differs only where the extension does; a pin without the row compares
//! the number, which `renumbered.re` under the fixtures holds.

mod host;
#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::carried::Carried;
use crate::divergence::{Kind, Registry};
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{
    self, Ended, Invocation, Observation, Oracle, Oracles, Program, Verdict, linked,
};
use crate::reading::{Reading, Status, Store, listed, plural};
use crate::sources::{self, Outside, Standing};
use crate::tally;
use crate::witness::Binary;
use crate::{git, key};
use host::Host;

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "crosscheck";
/// The two renderings the lane holds an extension to, one reading each:
/// php's `--re` and `--ri` flags, which print one extension and run
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// `php --re <name>`: `ReflectionExtension::__toString()`, every
    /// declaration of the extension.
    Re,
    /// `php --ri <name>`: `php_info_print_module`, the extension's
    /// `MINFO` table as `phpinfo()` shows it.
    Ri,
}

impl Column {
    /// Both columns, in the order a run writes them.
    pub const ALL: [Column; 2] = [Column::Re, Column::Ri];

    /// php's flag.
    pub fn flag(self) -> &'static str {
        match self {
            Column::Re => "--re",
            Column::Ri => "--ri",
        }
    }

    /// The extension of the file the oracle's bytes are kept in under the
    /// store, and of a rendering a test hands the lane: `re` or `ri`.
    pub fn kept(self) -> &'static str {
        match self {
            Column::Re => "re",
            Column::Ri => "ri",
        }
    }

    /// The reading's scope for an extension: the crate's name in the
    /// manifest for `--re`, the reading `done` holds a default extension
    /// to, and `ext/<name>/ri` for `--ri`, so the store keeps both.
    pub(crate) fn scope(self, name: &str) -> String {
        match self {
            Column::Re => format!("ext/{name}"),
            Column::Ri => format!("ext/{name}/ri"),
        }
    }

    /// The program whose first stream is the engine's rendering of
    /// `name` in this column. `php --re <name>` prints `zend_print_zval`
    /// over a `ReflectionExtension` (`sapi/cli/php_cli.c` at the tag),
    /// which is `__toString()`, and `echo` casts the same way; `php --ri
    /// <name>` calls `php_info_print_module`, which is what
    /// `ReflectionExtension::info()` calls. The extension is named as php
    /// names its module, [`oracle::module_name`], and quoted as a PHP
    /// string literal, so a name no extension could have is still a
    /// program, refused by the engine in its own words.
    pub(crate) fn program(self, name: &str) -> String {
        let literal = oracle::module_name(name)
            .replace('\\', "\\\\")
            .replace('\'', "\\'");
        match self {
            Column::Re => format!("echo new ReflectionExtension('{literal}');"),
            Column::Ri => format!("(new ReflectionExtension('{literal}'))->info();"),
        }
    }
}

/// The divergence key of a reading's scope: the registry kind's prefix
/// and the scope less its `ext/`, `crosscheck:ctype/ri` and
/// `crosscheck:sapi/cgi/cgi-fcgi`, so an accepted row under it is what
/// `judge` reads as `Diverged` (harness §4.3).
pub(crate) fn id_of(scope: &str) -> String {
    let bare = scope.strip_prefix("ext/").unwrap_or(scope);
    format!("{}{bare}", Kind::Crosscheck.prefix())
}

/// What one `xtask crosscheck` run was asked for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    /// The extensions, by their crates' names (`ctype`, `opcache`):
    /// `--ext <name>`, repeatable; every extension the binary under test
    /// carries when none is named.
    pub ext: Vec<String>,
    /// A directory of `<name>.re` and `<name>.ri` files standing for the
    /// engine's renderings, what a test hands the lane for the negative
    /// control; a column whose file is absent rendered nothing;
    /// without it the engine's side is the linked engine's own rendering.
    pub renderings: Option<PathBuf>,
    /// A binary standing for `php-linked`, which serves the linked
    /// engine's side: what a test hands the lane to hold a rendering
    /// served through one that dies (#1069); without it, the one built
    /// from the workspace.
    pub linked: Option<PathBuf>,
}

/// The engine's side of one extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rendering {
    /// The engine rendered nothing, and why.
    Absent(String),
    /// What the engine rendered.
    Bytes(Vec<u8>),
}

/// What one extension came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Judgement {
    /// The two renderings are the same bytes.
    Pass,
    /// They differ and an accepted registry row says so.
    Diverged(String),
    /// They differ, or the engine rendered nothing, and the note says
    /// which.
    Fail(String),
}

impl Judgement {
    pub(crate) fn status(&self) -> Status {
        match self {
            Judgement::Pass | Judgement::Diverged(_) => Status::Pass,
            Judgement::Fail(_) => Status::Fail,
        }
    }
}

/// Where the oracle's bytes for `name` in `column` are kept: under the
/// store, the instrument, the extension's `--re` scope and the oracle's
/// digest with the column's extension, `<digest>.re` beside
/// `<digest>.ri`, so a pin bump leaves the old rendering beside the new.
pub fn kept(store: &Store, digest: &str, name: &str, column: Column) -> PathBuf {
    kept_at(store, digest, &Column::Re.scope(name), column)
}

/// [`kept`] under a `--re` scope of any host's, `sapi/cgi/cgi-fcgi`
/// among them.
fn kept_at(store: &Store, digest: &str, scope: &str, column: Column) -> PathBuf {
    store
        .root()
        .join(INSTRUMENT)
        .join(scope)
        .join(format!("{digest}.{}", column.kept()))
}

/// The oracle's rendering written at `path`, [`kept_at`], and the path.
fn keep(path: PathBuf, rendering: &[u8]) -> Result<PathBuf> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&path, rendering)
        .with_context(|| format!("keeping the oracle's rendering at {}", path.display()))?;
    Ok(path)
}

/// The verdict on one extension: the engine's rendering held to the
/// oracle's through the one comparison every lane uses.
pub(crate) fn judge(registry: &Registry, id: &str, oracle: &[u8], engine: &Rendering) -> Judgement {
    match engine {
        Rendering::Absent(why) => Judgement::Fail(format!("the engine rendered nothing: {why}")),
        Rendering::Bytes(bytes) => match oracle::compare(registry, id, oracle, bytes) {
            Verdict::Same => Judgement::Pass,
            Verdict::Diverged(row) => Judgement::Diverged(row.reason.clone()),
            Verdict::Differs(diff) => Judgement::Fail(first_difference(&diff)),
        },
    }
}

/// The note on a rendering that differs: the first differing line by its
/// number in the oracle's rendering, the oracle's lines as `-` and the
/// engine's as `+` up to the next line the two share, and how many lines
/// differ in all. The whole diff of an extension is the size of the
/// extension, `standard`'s a hundred and fifty kilobytes, and a reading is
/// a line a person reads and a row the store keeps; the two files are in
/// the store for the rest.
pub(crate) fn first_difference(diff: &str) -> String {
    let changed = |line: &str| line.starts_with('-') || line.starts_with('+');
    let lines: Vec<&str> = diff.lines().collect();
    let Some(first) = lines.iter().position(|line| changed(line)) else {
        return "the engine's rendering differs from the oracle's in bytes no line shows"
            .to_owned();
    };
    let at = lines[..first]
        .iter()
        .filter(|line| !line.starts_with('+'))
        .count()
        + 1;
    let differ = lines.iter().filter(|line| changed(line)).count();
    let hunk: Vec<&str> = lines[first..]
        .iter()
        .take_while(|line| changed(line))
        .copied()
        .collect();
    let shown = hunk
        .iter()
        .take(HUNK)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let rest = hunk.len().saturating_sub(HUNK);
    let more = if rest == 0 {
        String::new()
    } else {
        format!("\n… and {rest} more line{} of this hunk", plural(rest))
    };
    format!(
        "the engine's rendering differs from the oracle's at line {at}, {differ} line{} in all:\n{shown}{more}",
        plural(differ)
    )
}

/// How many lines of the first hunk the note shows: an extension the
/// engine renders empty is one hunk the size of the oracle's rendering.
const HUNK: usize = 4;

/// The oracle's rendering of `name` in `column`: `php --re <module>` or
/// `php --ri <module>` under the recipe, the module being the name php
/// registers the crate's extension under ([`oracle::module_name`]),
/// through [`Oracle::run_under`] as every lane's oracle side runs. The
/// recipe's program stands beside the flag, since php opens it before it
/// prints, and prints without running it.
pub(crate) fn render(oracle: &Oracle<'_>, name: &str, column: Column) -> Result<Observation> {
    let flags = [
        column.flag().to_owned(),
        oracle::module_name(name).to_owned(),
    ];
    oracle.run_under(&Invocation {
        flags: &flags,
        ..Invocation::new(&oracle.pin().oracle.path, Program::Snippet(""))
    })
}

/// The recipe as the reading names it: the invocation the oracle's bytes
/// came from, and through a host other than the CLI, the host and the
/// program it serves.
pub(crate) fn recipe(manifest: &Manifest, name: &str, host: Host, column: Column) -> String {
    if host != Host::Cli {
        return format!(
            "{} {} serving {}",
            host.invocation(),
            manifest.pin.recipe.args.join(" "),
            column.program(name)
        );
    }
    let module = oracle::module_name(name);
    let module = if module.contains(' ') {
        format!("'{module}'")
    } else {
        module.to_owned()
    };
    format!(
        "php {} {} {module}",
        manifest.pin.recipe.args.join(" "),
        column.flag()
    )
}

/// The extensions a run holds: the ones named, or the manifest's `ext/*`
/// rows the binary under test carries, the bundle's default set and every
/// feature the binary turns on (harness §7, revision 63).
pub(crate) fn extensions(manifest: &Manifest, carried: &Carried, ask: &Ask) -> Vec<String> {
    if !ask.ext.is_empty() {
        return ask.ext.clone();
    }
    manifest
        .crates
        .iter()
        .filter(|krate| carried.carries(krate))
        .filter_map(|krate| krate.name.strip_prefix("ext/"))
        .map(str::to_owned)
        .collect()
}

/// Run what was asked for: one reading per extension per [`Column`],
/// the `--re` one first, written by the caller through `record`, and the
/// oracle's bytes kept in the store.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    // The main oracle, and one for each variant a row names, under which
    // that row's extension is rendered (#1782); each digest is asserted at
    // the first rendering under it and never again.
    let variants = manifest.variants();
    let oracles = Oracles::new(&manifest.pin, &variants);
    let engines = Engines {
        linked: Carried::linked(root)?,
        binary: Binary::of(root),
        server: ask.linked.clone(),
    };
    // The tree before anything renders: what each reading is stamped with.
    let snapshot = Snapshot::disk(root)?;
    let names = extensions(manifest, &Carried::binary(root)?, ask);
    let items: Vec<(&String, Host, Column)> = names
        .iter()
        .flat_map(|name| {
            host::hosts(oracle::module_name(name))
                .iter()
                .flat_map(move |host| Column::ALL.iter().map(move |column| (name, *host, *column)))
        })
        .collect();
    // Each rendering waits on two processes and shares nothing with the
    // next, so the items run side by side, in their order still (#2166).
    tally::each_beside(&items, |&(name, host, column)| {
            let mut reading = reading_of(&snapshot, manifest, name, host, column)?;
            let recipe = recipe(manifest, name, host, column);
            // A row whose crate the linked engine's workspace has not
            // created yet has no rendering to hold, as `budget` and
            // `unsafe-audit` count nothing for it (#64); for a host's own
            // module, the host's row.
            let row = host.krate().map_or_else(|| format!("ext/{name}"), str::to_owned);
            if let Some(krate) = manifest.crates.iter().find(|k| k.name == row)
                && !sources::exists(&crate::built_root(), krate)
            {
                reading.status = Status::Skip;
                reading.message = format!("{}: {}", reading.scope, sources::NOT_YET_CREATED);
                return Ok(reading);
            }
            let oracle = match oracles.of(manifest.variant_of(&row)) {
                Ok(oracle) => oracle,
                Err(unknown) => {
                    reading.status = Status::Fail;
                    reading.message = format!("{}: nothing rendered; {unknown:#}", reading.scope);
                    return Ok(reading);
                }
            };
            let pin = oracle.pin();
            // A reading taken under a variant says so, whatever it came to.
            let told = |mut reading: Reading| {
                if let Some(under) = pin.oracle.read_under() {
                    reading.message = format!("{}; {under}", reading.message);
                }
                Ok(reading)
            };
            if let Some(why) = pin.oracle.absent() {
                reading.status = Status::Skip;
                reading.message = format!("{}: nothing rendered under `{recipe}`; {why}", reading.scope);
                return told(reading);
            }
            oracle.asserted()?;
            let observed = if host == Host::Cli {
                render(oracle, name, column)?
            } else {
                host::oracle_side(oracle, host, name, column)?
            };
            if observed.exit != Ended::Status(0) {
                reading.status = Status::Fail;
                reading.message = format!(
                    "{}: the oracle has no such extension under `{recipe}`; {}",
                    reading.scope,
                    said(&observed)
                );
                return told(reading);
            }
            let path = keep(
                kept_at(store, &pin.oracle.md5, &host.scope(name, Column::Re), column),
                &observed.stdout,
            )?;
            let engine = rendering(manifest, ask, &engines, oracle, name, host, column)?;
            let judgement = judge(
                registry,
                &host.id(name, column),
                &observed.stdout,
                &engine,
            );
            reading.status = judgement.status();
            reading.count("bytes", observed.stdout.len() as u64)?;
            reading.count("lines", observed.stdout.split(|b| *b == b'\n').count() as u64)?;
            let kept = path.strip_prefix(root).unwrap_or(&path).display();
            reading.message = match judgement {
                Judgement::Pass => format!(
                    "{}: the engine's rendering is the oracle's {} bytes under `{recipe}`, kept at {kept}",
                    reading.scope,
                    observed.stdout.len()
                ),
                Judgement::Diverged(reason) => format!(
                    "{}: the engine's rendering differs from the oracle's under `{recipe}` and the registry accepts it: {reason}",
                    reading.scope
                ),
                Judgement::Fail(note) => format!(
                    "{}: the oracle rendered {} bytes under `{recipe}`, kept at {kept}; {note}",
                    reading.scope,
                    observed.stdout.len()
                ),
            };
            told(reading)
    })
    .into_iter()
    .collect()
}

/// What serves the engine's side of one run: the engine linked into this
/// harness, and the `php` binary the witness lane serves a fence marked
/// `process` through, for an extension the linked engine does not carry,
/// which is `pcntl` (harness §8, revision 61), and for [`HOSTED`]. Both
/// answer under the recipe, normalised as the oracle's stream is.
struct Engines {
    /// What the linked engine carries.
    linked: Carried,
    /// The binary, built at the first extension that asks for it.
    binary: Binary,
    /// What stands for `php-linked`, where the run was handed one.
    server: Option<PathBuf>,
}

/// The engine's side for `name` in `column`: the file a test handed the
/// lane, through the recipe's normalisation as the oracle's stream went,
/// or the engine's own rendering of [`Column::program`], served by the
/// linked engine where it carries the extension and by the binary where
/// it does not ([`Engines`]), normalised already. A request that did not
/// end on zero rendered nothing, and the engine's words are why: under
/// the recipe's `display_errors=1` what was thrown and not caught is on
/// the first stream, and the trouble of a serve that did not run, a panic
/// inside or a binary that did not build, is on neither.
fn rendering(
    manifest: &Manifest,
    ask: &Ask,
    engines: &Engines,
    oracle: &Oracle<'_>,
    name: &str,
    host: Host,
    column: Column,
) -> Result<Rendering> {
    let Some(dir) = &ask.renderings else {
        let program = column.program(name);
        let linked = served_linked(manifest, &engines.linked, name);
        let served = if host != Host::Cli {
            host::engine_side(oracle, &engines.binary, host, name, column).map_err(|trouble| {
                format!(
                    "the engine's {} did not serve it: {trouble:#}",
                    host.invocation()
                )
            })
        } else if linked {
            linked::serve_by(
                engines.server.as_deref(),
                &manifest.pin.recipe,
                None,
                &program,
            )
            .map_err(|trouble| format!("the linked engine did not serve it: {trouble:#}"))
        } else {
            engines
                .binary
                .serve(oracle, &program, None, None)
                .map(|(served, _)| served)
                .map_err(|trouble| format!("the php binary did not serve it: {trouble:#}"))
        };
        return Ok(match served {
            Err(why) => Rendering::Absent(why),
            Ok(served) if served.exit != Ended::Status(0) => Rendering::Absent(said(&served)),
            Ok(served) => Rendering::Bytes(served.stdout),
        });
    };
    let path = dir.join(format!("{name}.{}", column.kept()));
    if !path.is_file() {
        return Ok(Rendering::Absent(format!(
            "no rendering at {}",
            path.display()
        )));
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Rendering::Bytes(oracle::normalise(
        &manifest.pin.recipe.normalise,
        bytes,
    )))
}

/// The extension php's `php_module_startup` registers a host's
/// `additional_functions` under (`Z/main/main.c:2359-2366`): the CLI's
/// `dl()` and process title among `standard`'s functions in the oracle's
/// `php --re standard`. The linked engine is built with no host, so its
/// rendering lacks them, and the lane renders it through the `php` binary.
pub(crate) const HOSTED: &str = "standard";

/// Whether the linked engine renders `name` as the binary under test
/// registers it: an extension it carries other than [`HOSTED`], or one the
/// manifest has no row for; anything else the binary serves.
pub(crate) fn served_linked(manifest: &Manifest, linked: &Carried, name: &str) -> bool {
    name != HOSTED
        && manifest
            .crate_named(&Column::Re.scope(name))
            .is_none_or(|krate| linked.carries(krate))
}

/// php's own words for a request that did not end on zero: under the
/// recipe's `display_errors=1` what was thrown and not caught is on the
/// first stream, and a refusal before that on the second; the status
/// closes it.
fn said(observed: &Observation) -> String {
    let words = [&observed.stderr, &observed.stdout]
        .into_iter()
        .find(|stream| !stream.is_empty())
        .map(|stream| String::from_utf8_lossy(stream).trim().to_owned())
        .unwrap_or_default();
    format!("the request ended on {}: {words}", observed.exit)
}

/// The reading of one extension in one column, over the crate's sources
/// where the manifest has a row for it, so both columns are stale
/// together, and over the workspace for a name it has not, which the
/// oracle may still render or refuse.
fn reading_of(
    snapshot: &Snapshot,
    manifest: &Manifest,
    name: &str,
    host: Host,
    column: Column,
) -> Result<Reading> {
    let scope = host.scope(name, column);
    let path = stamped_path(manifest, name, host);
    Ok(Reading::stamped_over(INSTRUMENT, &scope, snapshot, &path)?.under(&manifest.pin))
}

/// The path both columns of `name` through `host` are stamped over: the
/// host's row for a host's own module, the extension's crate for every
/// other, and the workspace for a name the manifest has no row for.
fn stamped_path(manifest: &Manifest, name: &str, host: Host) -> PathBuf {
    let row = host
        .krate()
        .map_or_else(|| Column::Re.scope(name), str::to_owned);
    manifest
        .crate_named(&row)
        .map_or_else(|| PathBuf::from("."), crate::manifest::Crate::path)
}

/// What one rendering reads, as directories of the workspace (#1159).
/// The stamp's key ([`Snapshot::key`] over [`stamped_path`], #1318) covers the
/// extension's crate and its workspace dependency closure: its own
/// declarations, `php_ext`, `php_engine` and what that is built from.
/// The rendering reads three more things the closure does not reach:
/// `ext/reflection`, whose `ReflectionExtension` prints `--re`; the
/// crate the engine's side is served from and the crates it is built
/// from, `php_bundle` for the linked engine ([`served_linked`]) and the
/// host's crate for the binary and a host's own module; and none of the
/// other extensions that crate links, since one extension's rendering
/// prints its own module and the load number every module is normalised
/// out ([`oracle::normalise`]). An extension built on another names it
/// as a dependency, so that one is in its closure already.
#[derive(Debug, Default)]
pub(crate) struct Reads {
    /// The stamp's key's directories: a staged change here moves the
    /// key, so a standing reading over what is staged covers it.
    pub(crate) keyed: Vec<PathBuf>,
    /// What the rendering reads beyond them: a staged change here leaves
    /// the key where it was, so no reading stands for it and the
    /// extension renders again.
    pub(crate) beyond: Vec<PathBuf>,
}

/// The extension whose `ReflectionExtension` prints the `--re` column.
const REFLECTION: &str = "ext/reflection";

impl Reads {
    /// What `name`'s rendering through `host` reads, in the workspace
    /// whose graph is `graph`.
    pub(crate) fn of(
        graph: &key::Graph,
        manifest: &Manifest,
        linked: &Carried,
        name: &str,
        host: Host,
    ) -> Reads {
        let row = |name: &str| {
            manifest
                .crate_named(name)
                .map_or_else(|| crate::layout::path(name), crate::manifest::Crate::path)
        };
        let keyed = graph.paths(&stamped_path(manifest, name, host));
        let served = match host.krate() {
            Some(krate) => krate,
            None if served_linked(manifest, linked, name) => "php_bundle",
            None => "sapi/cli",
        };
        let reflection = graph.paths(&row(REFLECTION));
        let extensions = Path::new("ext");
        let host_side = graph
            .paths(&row(served))
            .into_iter()
            .filter(|p| !p.starts_with(extensions));
        let mut beyond: Vec<PathBuf> = reflection
            .into_iter()
            .chain(host_side)
            .filter(|p| !keyed.contains(p))
            .collect();
        beyond.sort();
        beyond.dedup();
        Reads { keyed, beyond }
    }
}

/// `crosscheck --staged`, a run over what a commit would take (#1159;
/// pre-commit's until revision 140): of the extensions
/// `carried` names, the ones to render again for what is staged. One
/// whose rendering reads no staged file ([`Reads`]) is not rendered,
/// whatever its reading says, since nothing it reads is in the commit;
/// one a staged file under [`Reads::beyond`] reaches is rendered, since
/// its key did not move with it; and one a staged change reaches only
/// through its key is rendered unless both its columns' readings pass
/// over what is staged ([`Standing`]), which is how a run already taken
/// over the commit's content, or a change to an extension's closure that
/// was rendered before it was staged, is relied on and named. A staged
/// file outside every crate reaches renderings as [`sources::Outside`]
/// sorts it (#1738): one every reading reads, such as `oracle.toml` or
/// `docs/divergence.toml`, renders every extension again, and a staged
/// lock reaches, as a staged change to its crate would, each extension
/// whose closure it pins otherwise than `HEAD` ([`sources::lock_moved`]).
pub(crate) fn staged(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    carried: &[String],
) -> Result<Vec<String>> {
    let files: Vec<PathBuf> = git::staged(root)?
        .into_iter()
        .map(|change| PathBuf::from(change.path))
        .collect();
    let graph = key::Graph::of(root)?;
    let linked = Carried::linked(root)?;
    let mut pairs = Vec::new();
    let mut scopes = Vec::new();
    for name in carried {
        for host in host::hosts(oracle::module_name(name)) {
            let path = stamped_path(manifest, name, *host);
            for column in Column::ALL {
                scopes.push((host.scope(name, column), path.clone()));
            }
            pairs.push((name, *host));
        }
    }
    let standing = Standing::for_commit_over(root, manifest, store, INSTRUMENT, &scopes)?;
    let locked = match standing.outside() {
        Outside::Every(file) => {
            println!(
                "{INSTRUMENT}: {file} is staged and every rendering reads it, so every extension renders again"
            );
            return Ok(carried.to_vec());
        }
        Outside::Lock => {
            let paths: Vec<(&str, PathBuf)> = pairs
                .iter()
                .map(|(name, host)| (name.as_str(), stamped_path(manifest, name, *host)))
                .collect();
            let taken = paths
                .iter()
                .map(|(name, path)| (*name, path.as_path(), "HEAD"));
            sources::lock_moved(root, manifest, taken)?
        }
        Outside::Nothing => Vec::new(),
    };
    let reached = |dirs: &[PathBuf]| {
        files
            .iter()
            .find(|f| dirs.iter().any(|d| d == Path::new(".") || f.starts_with(d)))
    };
    let mut render: Vec<String> = Vec::new();
    let mut unkeyed: Vec<&str> = Vec::new();
    let mut first = None;
    for (name, host) in pairs {
        let reads = Reads::of(&graph, manifest, &linked, name, host);
        let stands = Column::ALL
            .iter()
            .all(|column| standing.get(&host.scope(name, *column)).is_some());
        let again = if let Some(file) = reached(&reads.beyond) {
            first = first.or(Some(file));
            unkeyed.push(name);
            true
        } else {
            (reached(&reads.keyed).is_some() || locked.contains(name)) && !stands
        };
        if again && !render.contains(name) {
            render.push(name.clone());
        }
    }
    if let Some(file) = first {
        println!(
            "{INSTRUMENT}: {} is staged and the rendering of {} reads it beyond its key, so no reading stands for it",
            file.display(),
            listed(&unkeyed)
        );
    }
    Ok(render)
}
