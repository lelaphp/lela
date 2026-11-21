//! `xtask hosts --profile <name>`, the command that builds the hosts an
//! instrument judges and prints where each one is, one a line: the hosts
//! of [`crate::oracle::hosts::HOSTS`], the list the phpt lane hands its
//! tests to, built by the one cargo line [`profile::build_hosts`] runs, in
//! a profile of the workspace's own ([`Profile`]). A host added to that
//! list is built here, and someone running an instrument over hosts they
//! built copies no list out of the source. A command and not an
//! instrument: it measures nothing and writes no reading, but it builds, so
//! it takes a seat as the building instruments do ([`answer`]).
//!
//! `xtask hosts --list` is the same list read and not built: one JSON
//! object a line per host, its binary, crate, package and where a release
//! ships it ([`crate::oracle::hosts::Ships`]), and with `--profile` the
//! path the build would leave. `dist/` reads it, so a release's tarballs
//! and images keep no list of hosts of their own. It builds nothing and
//! takes no seat.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::oracle::hosts::{EngineHost, HOSTS};
use crate::oracle::profile::{self, Profile};

/// The command as it was asked: with `--list`, [`list`], seatless; else
/// [`command`] in the profile named, in a seat taken first, as the
/// building instruments take theirs.
pub(crate) fn answer(root: &Path, profile: Option<Profile>, list: bool) -> Result<()> {
    match (list, profile) {
        (true, profile) => self::list(root, profile),
        (false, Some(profile)) => {
            let _seat = crate::seat::Seats::of_env().take("hosts");
            command(root, profile)
        }
        (false, None) => bail!("hosts: --profile names the profile to build in"),
    }
}

/// Every host built in `root` in `profile`, and each one's path printed,
/// in the list's order, the binary under test first; refused, printing
/// nothing, where cargo left one of them unbuilt.
pub(crate) fn command(root: &Path, profile: Profile) -> Result<()> {
    let built: Vec<_> = profile::build_hosts(root, profile)?
        .into_iter()
        .map(profile::found)
        .collect::<Result<_>>()?;
    for path in built {
        println!("{}", path.display());
    }
    Ok(())
}

/// Every host printed as a [`Line`], in the list's order, each naming the
/// path a build in `profile` would leave where one is named; nothing built.
pub(crate) fn list(root: &Path, profile: Option<Profile>) -> Result<()> {
    let target = profile::target(root);
    for host in HOSTS {
        let path = profile.map(|profile| profile.binary(&target, host.binary));
        println!("{}", serde_json::to_string(&Line::of(host, path))?);
    }
    Ok(())
}

/// One line of `--list`, its keys printed in this order.
#[derive(Serialize)]
struct Line {
    binary: &'static str,
    #[serde(rename = "crate")]
    krate: &'static str,
    package: String,
    tarball: bool,
    images: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
}

impl Line {
    /// `host`'s line, naming `path` where a profile was named.
    fn of(host: EngineHost, path: Option<PathBuf>) -> Line {
        Line {
            binary: host.binary,
            krate: host.krate,
            package: host.package(),
            tarball: host.ships.tarball,
            images: host.ships.images.iter().map(|image| image.name()).collect(),
            path,
        }
    }
}
