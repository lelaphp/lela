//! The engine's hosts a lane runs a program through (ADR-0008, #1908),
//! beside the pinned php's own (`oracle::Hosts`, `[sapi]` of the pin):
//! the one statement of each host's crate, the binary the phpt lane runs
//! or finds beside the binary under test, as upstream's runner finds
//! php-cgi, php-fpm and phpdbg beside `TEST_PHP_EXECUTABLE`, and the
//! witness lane's build of the same crate. The phpt lane's lookups
//! (`phpt::{cgi,fpm,phpdbg}`), the cargo line that rebuilds a stale
//! binary, the witness lane's builds and what `xtask status --section
//! corpus` hands CI to build all read [`HOSTS`], and none spells a host.

use crate::layout;

/// One host: the crate under `sapi/` that builds it, its binary, and the
/// witness lane's build of it, the same binary with `witness-trace` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EngineHost {
    /// The crate's manifest name, `sapi/cgi`.
    pub(crate) krate: &'static str,
    /// The binary the phpt lane runs a test through.
    pub(crate) binary: &'static str,
    /// The binary the witness lane serves a fence through.
    pub(crate) witness: &'static str,
}

impl EngineHost {
    /// What cargo calls the crate's package, `php_sapi_cgi`.
    pub(crate) fn package(self) -> String {
        layout::package(self.krate)
    }
}

/// php's CLI, the binary under test itself (`crate::carried::BINARY`).
pub(crate) const CLI: EngineHost = EngineHost {
    krate: crate::carried::BINARY,
    binary: "php",
    witness: "php-witness",
};

/// The CGI host, which a CGI-shaped test is run through.
pub(crate) const CGI: EngineHost = EngineHost {
    krate: "sapi/cgi",
    binary: "php-cgi",
    witness: "php-cgi-witness",
};

/// The `FastCGI` process manager, which `sapi/fpm/tests` start.
pub(crate) const FPM: EngineHost = EngineHost {
    krate: "sapi/fpm",
    binary: "php-fpm",
    witness: "php-fpm-witness",
};

/// The debugger host, which a `--PHPDBG--` test is run through.
pub(crate) const PHPDBG: EngineHost = EngineHost {
    krate: "sapi/phpdbg",
    binary: "phpdbg",
    witness: "phpdbg-witness",
};

/// Every host the phpt lane hands a test to, the binary under test first.
pub(crate) const HOSTS: [EngineHost; 4] = [CLI, CGI, FPM, PHPDBG];

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Each host is a crate of the workspace, and its package, its binary
    /// and its witness build are what the crate's Cargo.toml declares. A
    /// `sapi/*` crate the lane hands no test to is not asked to be one.
    #[test]
    fn every_host_is_a_sapi_crate_of_the_workspace_as_its_cargo_toml_declares_it() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for host in HOSTS {
            let text = std::fs::read_to_string(root.join(host.krate).join("Cargo.toml"))
                .unwrap_or_else(|e| panic!("{}: {e}", host.krate));
            let cargo: toml::Table = toml::from_str(&text).unwrap();
            assert_eq!(
                cargo["package"]["name"].as_str(),
                Some(host.package().as_str()),
                "{}",
                host.krate
            );
            let bins: Vec<&str> = cargo["bin"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|bin| bin.get("name").and_then(toml::Value::as_str))
                .collect();
            assert!(bins.contains(&host.binary), "{}: {bins:?}", host.krate);
            assert!(bins.contains(&host.witness), "{}: {bins:?}", host.krate);
        }
    }
}
