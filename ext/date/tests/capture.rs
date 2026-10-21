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
    assert_eq!(served.stdout_text(), "bool(true)\n");
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
        .ini("html_errors", "0")
        .ini("date.timezone", "foo")
        .build();
    let served = engine.serve_capture(
        "<?php var_dump(ini_get('date.timezone'), date_default_timezone_get());",
        Capture::default(),
    );
    assert_eq!(
        served.stdout_text(),
        "\nWarning: PHP Startup: Invalid date.timezone value 'foo', using 'UTC' instead in Unknown on line 0\nstring(3) \"UTC\"\nstring(3) \"UTC\"\n"
    );
}

/// A subclass's declared properties are keyed by their mangled names,
/// `\0*\0p` and `\0I\0a`, as interned strings, as php's `prop_info->name`
/// is one, so no reader of the class's rows, `get_object_vars()`, the
/// `(array)` cast, `__serialize` or a dumper, leaves one held: armed, the
/// request ends with no block of its heap held (#2072).
#[cfg(debug_assertions)]
#[test]
fn a_subclass_s_mangled_property_names_are_left_held_by_no_reader() {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .ini("date.timezone", "UTC")
        .build();
    let served = engine.serve_capture(
        r#"<?php
class P extends DatePeriod { protected $prot = 2; }
$p = P::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D');
$n = 0; foreach (get_object_vars($p) as $_) $n++; foreach ((array) $p as $_) $n++; echo $n, "\n";
class D extends DateTime { protected $p = 2; private $q = 3; }
$d = new D('2000-01-01');
$n = 0; foreach (get_object_vars($d) as $_) $n++; foreach ((array) $d as $_) $n++; foreach ($d->__serialize() as $_) $n++; echo $n, "\n";
"#,
        Capture::default(),
    );
    served.assert_clean("16\n10\n");
}
