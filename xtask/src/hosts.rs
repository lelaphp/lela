//! `xtask hosts --profile <name>`, the command that builds the hosts an
//! instrument judges and prints where each one is, one a line: the hosts
//! of [`crate::oracle::hosts::HOSTS`], the list the phpt lane hands its
//! tests to, built by the one cargo line [`profile::build_hosts`] runs, in
//! a profile of the workspace's own ([`Profile`]). A host added to that
//! list is built here, and someone running an instrument over hosts they
//! built copies no list out of the source. A command and not an
//! instrument: it measures nothing and writes no reading, but it builds, so
//! it takes a seat as the building instruments do
//! ([`crate::seat::of_command`]).

use std::path::Path;

use anyhow::Result;

use crate::oracle::profile::{self, Profile};

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
