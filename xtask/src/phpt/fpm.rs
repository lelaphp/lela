//! A test of `sapi/fpm/tests`, run as upstream's `FPM\Tester` finds its
//! php-fpm (`sapi/fpm/tests/tester.inc:226-257` at the tag): beside the
//! binary under test, `<prefix>/fpm/php-fpm` or `<prefix>/sbin/php-fpm`,
//! and else at `dirname(__DIR__)/php-fpm`, the build tree's
//! `sapi/fpm/php-fpm`. The pinned php finds the keg's php-fpm, which is
//! the one `[sapi]` pins, by the first; the engine's binary stands in
//! `target`, where neither is, so the runner puts the engine's own
//! `php-fpm`, `sapi/fpm`'s binary (ADR-0008), where the third looks: a
//! link beside the subset's copy of the tests. Where none was built, or
//! the pin names none, nothing is linked and the tests skip in upstream's
//! words, `php-fpm binary not found`.

use std::path::Path;

use anyhow::{Context, Result};

use super::Subset;
use crate::oracle::hosts::FPM;

/// Where the tests stand in the tree, the directory `FPM\Tester` looks
/// beside.
const TESTS: &str = "sapi/fpm/tests";

/// `host` linked as `php-fpm` beside each directory of `subset` that is
/// `sapi/fpm/tests`, where `FPM\Tester::findExecutable` looks last; a link
/// standing there from before replaced, and none left where there is no
/// host.
pub(crate) fn place(subset: &Subset, host: Option<&Path>) -> Result<()> {
    for place in subset
        .dirs
        .iter()
        .filter(|place| place.under == Path::new(TESTS))
    {
        let Some(parent) = place.dir.parent() else {
            continue;
        };
        let link = parent.join(FPM.binary);
        crate::remove(&link)?;
        if let Some(host) = host {
            std::os::unix::fs::symlink(host, &link)
                .with_context(|| format!("linking {} to {}", link.display(), host.display()))?;
        }
    }
    Ok(())
}
