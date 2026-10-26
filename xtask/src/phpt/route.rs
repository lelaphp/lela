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

/// Route the tests of `subsets` under the pinned tree at `source`: each
/// variant row's gains every test of the corpus that names an extension
/// of its route, and every other subset leaves those of its own. Nothing
/// routes where the main oracle is not on this machine, nor to a variant
/// whose php is not, since neither listing is there to read.
pub(crate) fn route(
    manifest: &Manifest,
    oracles: &Oracles<'_>,
    carried: &Carried,
    source: &Path,
    subsets: &mut [Subset],
) -> Result<()> {
    let routes = routes(manifest, oracles)?;
    apply(&routes, manifest, carried, source, subsets)
}

/// [`route`] by `routes`, derived.
pub(crate) fn apply(
    routes: &[Route<'_>],
    manifest: &Manifest,
    carried: &Carried,
    source: &Path,
    subsets: &mut [Subset],
) -> Result<()> {
    if routes.is_empty() {
        return Ok(());
    }
    let mut named = BTreeMap::new();
    for corpus in Subset::corpus(source) {
        for test in corpus.tests()? {
            if let Some((route, phpt)) = routed_by(routes, &test) {
                named.insert(corpus.under(&test), (route, phpt));
            }
        }
    }
    for subset in subsets {
        for (under, (route, phpt)) in &named {
            let own = subset.found_in(&source.join(under)).is_some();
            if route.krate.name == subset.scope && !own {
                let why = unbuilt(manifest, carried, route.krate, under, phpt);
                subset.routed.gained.insert(under.clone(), why);
            } else if route.krate.name != subset.scope && own {
                subset.routed.left.insert(under.clone());
            }
        }
    }
    Ok(())
}

/// Each row naming a variant whose php is here, with the extensions its
/// listing has and the main oracle's has not.
fn routes<'m>(manifest: &'m Manifest, oracles: &Oracles<'_>) -> Result<Vec<Route<'m>>> {
    let listed = |variant: Option<&str>| -> Result<Option<BTreeSet<String>>> {
        let oracle = oracles.of(variant)?;
        let pin = oracle.pin();
        if pin.oracle.absent().is_some() {
            return Ok(None);
        }
        let loaded = adapter::loaded_extensions(oracle, &pin.oracle.path)?;
        Ok(Some(loaded.into_iter().collect()))
    };
    let Some(main) = listed(None)? else {
        return Ok(Vec::new());
    };
    let mut routes = Vec::new();
    for krate in &manifest.crates {
        let Some(variant) = krate.oracle.as_deref() else {
            continue;
        };
        if let Some(loaded) = listed(Some(variant))? {
            routes.extend(Route::of(krate, &loaded, &main));
        }
    }
    Ok(routes)
}

/// The route the test at `path` takes, the first whose extensions its
/// `--EXTENSIONS--` names, with the test read; none where it names none
/// or does not parse. A file without the section is never parsed.
fn routed_by<'r, 'm>(routes: &'r [Route<'m>], path: &Path) -> Option<(&'r Route<'m>, Phpt)> {
    let bytes = std::fs::read(path).ok()?;
    bytes
        .windows(14)
        .any(|w| w == b"--EXTENSIONS--")
        .then_some(())?;
    let phpt = Phpt::load(path).ok()?;
    let names = phpt.extensions();
    let route = routes
        .iter()
        .find(|route| names.iter().any(|name| route.extensions.contains(name)))?;
    Some((route, phpt))
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
    phpt: &Phpt,
) -> Option<Unbuilt> {
    let host = if phpt.is_cgi() {
        Some(CGI)
    } else if phpt.has("PHPDBG") {
        Some(PHPDBG)
    } else {
        under.starts_with(fpm::TESTS).then_some(FPM)
    };
    if let Some(host) = host {
        return Some(Unbuilt::Host(host));
    }
    let named = phpt.extensions();
    let behind = named
        .iter()
        .filter_map(|name| manifest.crate_named(&format!("ext/{name}")))
        .find(|krate| !carried.carries(krate) && krate.feature != row.feature)?;
    behind.feature.clone().map(Unbuilt::Feature)
}
