//! `xtask unsafe-audit` over the tracked workspace (design §4.1).

use std::path::Path;

use xtask::manifest::Manifest;
use xtask::reading::Status;
use xtask::unsafe_audit;

mod common {
    pub mod repo;
}
use common::repo::Repo;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here builds the manifest it judges.
mod tracked {
    use super::*;

    #[test]
    fn the_tracked_workspace_has_no_unsafe_outside_the_homes() {
        let manifest = Manifest::load(workspace()).unwrap();
        let readings = unsafe_audit::run(workspace(), &manifest).unwrap();
        assert_eq!(
            readings.len(),
            manifest.crates.len(),
            "one reading per crate"
        );
        for reading in &readings {
            assert_eq!(reading.rule, "unsafe.homes");
            assert_eq!(reading.adr, "0006", "the ADR that decided the homes");
            assert_ne!(reading.status, Status::Fail, "{reading:#?}");
        }
        let xtask = readings.iter().find(|r| r.scope == "xtask").unwrap();
        assert_eq!(xtask.status, Status::Pass);
        // The bench reader's four, in its home `xtask::bench::spent::read`
        // (#3062's ruling); on another platform the module is not compiled,
        // but its source is read all the same.
        assert_eq!(xtask.numbers["sites"], 4.into());
        assert_eq!(xtask.numbers["outside"], 0.into());
        assert!(!xtask.key().is_empty(), "stamped");
    }

    /// A host's `unsafe` lives under `sapi/*/src/ffi/` and nowhere else
    /// (ADR-0008), under the home #1249 proposes on a manifest that does not
    /// carry it yet, which this adds and adds nothing once it does: `sapi/fpm`
    /// with its block in `src/ffi/`
    /// and `#![deny(unsafe_code)]` at its root passes, and `sapi/phpdbg` with
    /// one in `src/lib.rs` fails naming the line.
    #[test]
    fn a_host_s_unsafe_is_at_home_under_its_ffi_directory_alone() {
        let tree = xtask::scratch::Tree::new("unsafe-audit-hosts").unwrap();
        let write = |rel: &str, text: &str| {
            let path = tree.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "sapi/fpm/Cargo.toml",
            "[package]\nname = \"php_sapi_fpm\"\n",
        );
        write(
            "sapi/fpm/src/lib.rs",
            "#![deny(unsafe_code)]\n#[allow(unsafe_code)]\nmod ffi;\n",
        );
        write(
            "sapi/fpm/src/ffi/mod.rs",
            "pub fn pid() -> i32 {\n    // SAFETY: the fixture's.\n    unsafe { 0 }\n}\n",
        );
        write(
            "sapi/phpdbg/Cargo.toml",
            "[package]\nname = \"php_sapi_phpdbg\"\n",
        );
        write(
            "sapi/phpdbg/src/lib.rs",
            "#![forbid(unsafe_code)]\npub fn tty() -> i32 {\n    unsafe { 0 }\n}\n",
        );
        Repo::init(&tree).commit("hosts");
        let mut manifest = Manifest::load(workspace()).unwrap();
        manifest.crates = vec![
            xtask::manifest::Crate::named("sapi/fpm"),
            xtask::manifest::Crate::named("sapi/phpdbg"),
        ];
        let homes = manifest
            .rules
            .iter_mut()
            .find(|r| r.id == "unsafe.homes")
            .and_then(|r| r.predicate.allow.as_mut())
            .unwrap();
        if !homes.iter().any(|h| h == "sapi/*/src/ffi/") {
            homes.push("sapi/*/src/ffi/".to_owned());
        }
        let readings = unsafe_audit::run(&tree, &manifest).unwrap();
        let of = |scope: &str| readings.iter().find(|r| r.scope == scope).unwrap();
        assert_eq!(of("sapi/fpm").status, Status::Pass, "{:#?}", of("sapi/fpm"));
        assert_eq!(of("sapi/fpm").numbers["sites"], 1.into());
        let phpdbg = of("sapi/phpdbg");
        assert_eq!(phpdbg.status, Status::Fail, "{phpdbg:#?}");
        assert!(
            phpdbg.message.contains("sapi/phpdbg/src/lib.rs:3"),
            "{}",
            phpdbg.message
        );
    }
}
