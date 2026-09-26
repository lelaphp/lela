//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('date'));", sink);
    assert_eq!(served.stdout, b"bool(true)\n");
}

/// `OnUpdate_date_timezone` at `ZEND_INI_STAGE_STARTUP`: a zone the
/// database has not, set as the engine starts (`-d date.timezone=foo`),
/// is refused with php's startup warning and leaves the entry at its
/// default, which is what the oracle answers under the same `-d`. A
/// witness sets no startup ini, so the fence cannot hold this.
#[test]
fn a_startup_timezone_the_database_has_not_leaves_the_default() {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .ini("date.timezone", "foo")
        .build();
    let served = engine.serve_capture(
        "<?php var_dump(ini_get('date.timezone'), date_default_timezone_get());",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "\nWarning: PHP Startup: Invalid date.timezone value 'foo', using 'UTC' instead in Unknown on line 0\nstring(3) \"UTC\"\nstring(3) \"UTC\"\n"
    );
}
