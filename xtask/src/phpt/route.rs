//! Routing (harness-design.md §4.3, #3312): a row naming an oracle
//! variant holds in its subset, beside its own directories, every test of
//! the pinned tree whose `--EXTENSIONS--` names an extension the main
//! oracle does not load and the variant does, each binary's listing read
//! as `php -n -m` under the recipe ([`adapter::loaded_extensions`]). The
//! test leaves the subset it stands in rather than being skipped there by
//! both sides. A routed test the lane cannot build for, needing another
//! feature or a host, is judged [`Judgement::Unbuilt`] on both sides, so
//! it is named outside the target with that reason. Derived, not
//! declared: a declared list fails open, and this fails closed, as
//! Mitchell ruled on #3312.
//!
//! [`Judgement::Unbuilt`]: super::Judgement::Unbuilt

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::file::Phpt;
use super::{Subset, adapter, fpm};
use crate::carried::Carried;
use crate::manifest::{Crate, Manifest};
use crate::oracle::Oracles;
use crate::oracle::hosts::{CGI, EngineHost, FPM, PHPDBG};

/// Why the lane cannot build for a test routed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unbuilt {
    /// It names an extension whose crate is behind a feature that neither
    /// the binary under test nor the row turns on, as `ext/ffi`'s tests
    /// that need `zend_test` name `ffi`.
    Feature(String),
    /// It runs through a host beside php, which the lane builds none of
    /// for the row's feature, as `sapi/fpm/tests` does.
    Host(EngineHost),
}

impl fmt::Display for Unbuilt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unbuilt::Feature(feature) => write!(
                f,
                "routed here, it needs the feature {feature}, which the lane's engine is not built with"
            ),
            Unbuilt::Host(host) => write!(
                f,
                "routed here, it needs the {} host, which the lane does not build",
                host.krate
            ),
        }
    }
}

/// The tests routing moves, by where each stands in the corpus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Routed {
    /// The tests of other directories a variant's subset holds, each with
    /// why the lane cannot build for it, where it cannot.
    pub gained: BTreeMap<PathBuf, Option<Unbuilt>>,
    /// The tests of the subset's own directories it leaves to a variant's.
    pub left: BTreeSet<PathBuf>,
    /// Why a variant's subset could not be routed: its variant unknown to
    /// the pin, or a listing that could not be read. The subset's own
    /// failing reading says it, and no other subset's (harness §4.3).
    pub refused: Option<String>,
}

/// One variant's row and the extensions its variant loads and the main
/// oracle does not.
#[derive(Debug)]
pub(crate) struct Route<'m> {
    krate: &'m Crate,
    extensions: BTreeSet<String>,
}

impl<'m> Route<'m> {
    /// The route of `krate`, whose variant's listing is `variant` and the
    /// main oracle's `main`: none where the main oracle loads all of it.
    pub(crate) fn of(
        krate: &'m Crate,
        variant: &BTreeSet<String>,
        main: &BTreeSet<String>,
    ) -> Option<Route<'m>> {
        let extensions: BTreeSet<String> = variant.difference(main).cloned().collect();
        (!extensions.is_empty()).then_some(Route { krate, extensions })
    }
}

/// What routing reads of one test of the corpus whose `--EXTENSIONS--`
/// names an extension: the names, and the host beside php it runs
/// through, where its sections name one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Named {
    extensions: Vec<String>,
    through: Option<Through>,
}

/// The host beside php a test's sections say it runs through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Through {
    /// `--CGI--`, or a section only php-cgi reads.
    Cgi,
    /// `--PHPDBG--`.
    Phpdbg,
}

impl Named {
    /// The test at `path`, none where it names no extension or does not
    /// parse; a file without the section is never parsed.
    fn of(path: &Path) -> Option<Named> {
        let bytes = std::fs::read(path).ok()?;
        let section = bytes.windows(14).any(|w| w == b"--EXTENSIONS--");
        let phpt = section.then(|| Phpt::load(path).ok())??;
        let through = if phpt.is_cgi() {
            Some(Through::Cgi)
        } else {
            phpt.has("PHPDBG").then_some(Through::Phpdbg)
        };
        Some(Named {
            extensions: phpt.extensions(),
            through,
        })
    }
}

/// Route the tests of `subsets` under the pinned tree at `source`: each
/// variant row's gains every test of the corpus that names an extension
/// of its route, and every other subset leaves those of its own. Nothing
/// routes where the main oracle is not on this machine, nor to a variant
/// whose php is not, since neither listing is there to read. A row whose
/// variant cannot be listed is refused in its own subset ([`Routed`]).
pub(crate) fn route(
    root: &Path,
    manifest: &Manifest,
    oracles: &Oracles<'_>,
    carried: &Carried,
    source: &Path,
    subsets: &mut [Subset],
) -> Result<()> {
    let (routes, refused) = routes(manifest, oracles);
    for (scope, why) in refused {
        if let Some(subset) = subsets.iter_mut().find(|s| s.scope == scope) {
            subset.routed.refused = Some(why);
        }
    }
    if routes.is_empty() {
        return Ok(());
    }
    let named = named(root, &oracles.main().pin().source.commit, source)?;
    apply(&routes, manifest, carried, &named, subsets);
    Ok(())
}

/// The shape [`Named`] is kept in, moved with any change to what a kept
/// file holds or how [`walked`] reads it, so a file another shape wrote
/// is walked again rather than read.
const NAMED_SHAPE: u32 = 1;

/// [`walked`]'s, read once per tree and kept under `root`'s target
/// directory, by the tree's place, the commit it is held to
/// (`Source::tree`) and the shape: a walk reads every one of its 22,000
/// tests, seconds of every run.
fn named(root: &Path, commit: &str, source: &Path) -> Result<BTreeMap<PathBuf, Named>> {
    let said = format!("{commit}\n{}\n{NAMED_SHAPE}", source.display());
    let id = crate::git::blob_id(said.as_bytes());
    let kept = root.join("target/phpt-routing").join(format!("{id}.json"));
    let read = std::fs::read(&kept).ok();
    if let Some(named) = read.and_then(|bytes| serde_json::from_slice(&bytes).ok()) {
        return Ok(named);
    }
    let named = walked(source)?;
    if let Ok(bytes) = serde_json::to_vec(&named) {
        crate::make_parent(&kept)
            .and_then(|()| crate::write(&kept, bytes))
            .ok();
    }
    Ok(named)
}

/// Each test of the pinned tree at `source` that names an extension, by
/// where it stands in the corpus.
pub(crate) fn walked(source: &Path) -> Result<BTreeMap<PathBuf, Named>> {
    let mut named = BTreeMap::new();
    for corpus in Subset::corpus(source) {
        for test in corpus.tests()? {
            if let Some(read) = Named::of(&test) {
                named.insert(corpus.under(&test), read);
            }
        }
    }
    Ok(named)
}

/// [`route`] by `routes`, derived, over the corpus's `named` tests.
pub(crate) fn apply(
    routes: &[Route<'_>],
    manifest: &Manifest,
    carried: &Carried,
    named: &BTreeMap<PathBuf, Named>,
    subsets: &mut [Subset],
) {
    let moved: Vec<(&PathBuf, &Route<'_>, &Named)> = (named.iter())
        .filter_map(|(under, test)| Some((under, routed_by(routes, test)?, test)))
        .collect();
    for subset in subsets {
        for &(under, route, test) in &moved {
            let own = subset.found_in(&subset.root.join(under)).is_some();
            if route.krate.name == subset.scope && !own {
                let why = unbuilt(manifest, carried, route.krate, under, test);
                subset.routed.gained.insert(under.clone(), why);
            } else if route.krate.name != subset.scope && own {
                subset.routed.left.insert(under.clone());
            }
        }
    }
}

/// Each row naming a variant whose php is here, with the extensions its
/// listing has and the main oracle's has not; and each row whose variant
/// could not be listed, by its crate, with why. A main listing that could
/// not be read refuses every variant's row, since none can be routed.
fn routes<'m>(
    manifest: &'m Manifest,
    oracles: &Oracles<'_>,
) -> (Vec<Route<'m>>, Vec<(&'m str, String)>) {
    let listed = |variant: Option<&str>| -> Result<Option<BTreeSet<String>>> {
        let oracle = oracles.of(variant)?;
        let pin = oracle.pin();
        if pin.oracle.absent().is_some() {
            return Ok(None);
        }
        let loaded = adapter::loaded_extensions(oracle, &pin.oracle.path)?;
        Ok(Some(loaded.into_iter().collect()))
    };
    let rows = (manifest.crates.iter()).filter_map(|k| Some((k, k.oracle.as_deref()?)));
    let main = match listed(None) {
        Ok(Some(main)) => main,
        Ok(None) => return (Vec::new(), Vec::new()),
        Err(why) => {
            let why = format!(
                "the main oracle's extensions could not be listed, so nothing is routed: {why:#}"
            );
            return (
                Vec::new(),
                rows.map(|(k, _)| (k.name.as_str(), why.clone())).collect(),
            );
        }
    };
    let (mut routes, mut refused) = (Vec::new(), Vec::new());
    for (krate, variant) in rows {
        match listed(Some(variant)) {
            Ok(loaded) => routes.extend(loaded.and_then(|loaded| Route::of(krate, &loaded, &main))),
            Err(why) => refused.push((
                krate.name.as_str(),
                format!(
                    "its subset could not be routed under the oracle variant {variant}: {why:#}"
                ),
            )),
        }
    }
    (routes, refused)
}

/// The route `test` takes, the first whose extensions it names.
fn routed_by<'r, 'm>(routes: &'r [Route<'m>], test: &Named) -> Option<&'r Route<'m>> {
    let names = &test.extensions;
    (routes.iter()).find(|route| names.iter().any(|name| route.extensions.contains(name)))
}

/// Why the lane of `row` cannot build for the routed test standing at
/// `under`: a host it runs through, or an extension it names whose crate
/// is behind a feature neither `carried`, the binary under test's, nor the
/// row turns on.
fn unbuilt(
    manifest: &Manifest,
    carried: &Carried,
    row: &Crate,
    under: &Path,
    test: &Named,
) -> Option<Unbuilt> {
    let host = match test.through {
        Some(Through::Cgi) => Some(CGI),
        Some(Through::Phpdbg) => Some(PHPDBG),
        None => under.starts_with(fpm::TESTS).then_some(FPM),
    };
    if let Some(host) = host {
        return Some(Unbuilt::Host(host));
    }
    let behind = (test.extensions.iter())
        .filter_map(|name| manifest.crate_named(&format!("ext/{name}")))
        .find(|krate| !carried.carries(krate) && krate.feature != row.feature)?;
    behind.feature.clone().map(Unbuilt::Feature)
}
