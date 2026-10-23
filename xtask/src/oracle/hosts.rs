//! The engine's hosts a lane runs a program through (ADR-0008, #1908),
//! beside the pinned php's own (`oracle::Sapi`, `[sapi]` of the pin):
//! the one statement of each host's crate, the binary the phpt lane runs
//! or finds beside the binary under test, as upstream's runner finds
//! php-cgi, php-fpm and phpdbg beside `TEST_PHP_EXECUTABLE`, and which
//! the witness lane serves a fence through as well (#2168). The phpt
//! lane's one lookup ([`EngineHost::for_run`]), the cargo line that
//! rebuilds a stale binary, the witness lane's builds and what `xtask status --section
//! corpus` hands CI to build all read [`HOSTS`], and none spells a host.

use std::path::{Path, PathBuf};

use super::Pin;
use crate::layout;

/// One host: the crate under `sapi/` that builds it, and its binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EngineHost {
    /// The crate's manifest name, `sapi/cgi`.
    pub(crate) krate: &'static str,
    /// The binary the phpt lane runs a test through and the witness lane
    /// serves a fence through, with `PHP_WITNESS_TRACE` set (#2168).
    pub(crate) binary: &'static str,
}

impl EngineHost {
    /// What cargo calls the crate's package, `php_sapi_cgi`.
    pub(crate) fn package(self) -> String {
        layout::package(self.krate)
    }

    /// The host of this kind a run hands a test to: the one `[sapi]` pins
    /// where `binary` is the pinned php or the run is the control, and
    /// otherwise the engine's own beside `binary`, which the lane's runner
    /// held to the tree with it (`oracle::Runner::resolve`). `None` where
    /// the pin names none or none is at its path, or none was built beside
    /// `binary`, which upstream's runner answers with a skip
    /// (run-tests.php:1894-1896, 1914; `php-fpm binary not found`).
    pub(crate) fn for_run(self, pin: &Pin, binary: &Path, control: bool) -> Option<PathBuf> {
        let host = if control || binary == pin.oracle.path {
            pin.sapi.of(self).map(|pinned| pinned.path.clone())
        } else {
            Some(binary.with_file_name(self.binary))
        };
        host.filter(|path| path.is_file())
    }
}

/// php's CLI, the binary under test itself (`crate::carried::BINARY`).
pub(crate) const CLI: EngineHost = EngineHost {
    krate: crate::carried::BINARY,
    binary: "php",
};

/// The CGI host, which a CGI-shaped test is run through.
pub(crate) const CGI: EngineHost = EngineHost {
    krate: "sapi/cgi",
    binary: "php-cgi",
};

/// The `FastCGI` process manager, which `sapi/fpm/tests` start.
pub(crate) const FPM: EngineHost = EngineHost {
    krate: "sapi/fpm",
    binary: "php-fpm",
};

/// The debugger host, which a `--PHPDBG--` test is run through.
pub(crate) const PHPDBG: EngineHost = EngineHost {
    krate: "sapi/phpdbg",
    binary: "phpdbg",
};

/// The production server, which a `request makala` fence is served by
/// (#2347) and the phpt lane's served subset is served to over HTTP
/// (`phpt::http`, #2348); its oracle is php-fpm under the pin's
/// `[fastcgi]` mapping, and the served subset's php-cgi, so `[sapi]` pins
/// none of its own.
pub(crate) const MAKALA: EngineHost = EngineHost {
    krate: "sapi/makala",
    binary: "makala",
};

/// Every host the phpt lane hands a test to, the binary under test first.
pub(crate) const HOSTS: [EngineHost; 4] = [CLI, CGI, FPM, PHPDBG];

#[cfg(test)]
mod tests {

    use super::*;
    use crate::oracle::{Host, Sapi};

    /// Each kind reads its own row of `[sapi]`, and the CLI none.
    #[test]
    fn each_kind_reads_its_own_pinned_row() {
        let row = |name: &str| {
            Some(Host {
                path: name.into(),
                md5: String::new(),
            })
        };
        let sapi = Sapi {
            cgi: row("cgi"),
            fpm: row("fpm"),
            phpdbg: row("phpdbg"),
        };
        let read = |kind| sapi.of(kind).map(|host| host.path.display().to_string());
        assert_eq!(read(CGI).as_deref(), Some("cgi"));
        assert_eq!(read(FPM).as_deref(), Some("fpm"));
        assert_eq!(read(PHPDBG).as_deref(), Some("phpdbg"));
        assert_eq!(read(CLI), None);
    }

    /// Each host is a crate of the workspace, and its package and its
    /// binary are what the crate's Cargo.toml declares. A
    /// `sapi/*` crate the lane hands no test to is not asked to be one.
    #[cfg(tracked)]
    #[test]
    fn every_host_is_a_sapi_crate_of_the_workspace_as_its_cargo_toml_declares_it() {
        let root = crate::package_dir().join("..");
        for host in HOSTS.into_iter().chain([MAKALA]) {
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
        }
    }
}
