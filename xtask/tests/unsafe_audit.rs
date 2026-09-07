//! `xtask unsafe-audit` over the tracked workspace (design §4.1).

use std::path::Path;

use xtask::manifest::Manifest;
use xtask::reading::Status;
use xtask::unsafe_audit;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

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
    assert_eq!(xtask.numbers["sites"], 0.into());
    assert_eq!(xtask.numbers["outside"], 0.into());
    assert!(!xtask.tree.is_empty(), "stamped");
}
