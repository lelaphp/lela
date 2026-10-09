//! The request leak sensor (#1992) over this extension's wrapper: each
//! request below gives up everything its opens took, so an armed engine
//! ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension and the
/// standard one it writes files through wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_bz2::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// An include through `compress.bzip2://` keeps nothing of the open,
/// the name it set among it, whether the file opens or not, and the
/// included file is named by the path as written, as php names it
/// (#2119).
#[test]
fn an_include_through_the_wrapper_gives_up_the_path_it_opened() {
    let stdout = armed(
        r#"<?php
$f = sys_get_temp_dir() . '/leak-sensor-include-' . getmypid() . '.php';
file_put_contents($f, bzcompress('<?php echo __FILE__ === $GLOBALS["f"] ? "file" : __FILE__, "\n";'));
include 'compress.bzip2://' . $f;
var_dump(@include 'compress.bzip2://' . $f . '.missing');
echo in_array($f, get_included_files(), true) ? 'named' : implode(',', get_included_files()), "\n";
unlink($f);
"#,
    );
    assert_eq!(stdout, "file\nbool(false)\nnamed\n");
}
