//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_filter::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('filter'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// `php_zval_filter_recursive` asks no stack limit in php and faults
/// past the thread's end; here a nest deeper than the test harness's
/// 2 MiB thread is refused at its floor with the catchable `Error`.
#[test]
fn a_filtered_nest_deeper_than_the_thread_is_refused_at_its_floor() {
    let engine = Engine::builder()
        .extensions(&[php_ext_filter::extension()])
        .build();
    let source = "<?php\n$a = []; for ($i = 0; $i < 200000; $i++) { $a = [$a]; }\ntry { filter_var($a, FILTER_DEFAULT, FILTER_REQUIRE_ARRAY); } catch (Error $e) { echo $e->getMessage(); }";
    let served = engine.serve_capture(source, Capture::default());
    let stdout = served.stdout_text();
    assert!(
        stdout.starts_with("Maximum call stack size of ")
            && stdout.ends_with(" reached. Infinite recursion?"),
        "{stdout}"
    );
}
