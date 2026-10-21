//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
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
            php_ext_zlib::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// `gzfile` keeps each line it reads only in the array it returns
/// (#2086).
#[test]
fn a_line_gzfile_reads_is_given_up() {
    let stdout = armed(
        r#"<?php
$f = sys_get_temp_dir() . '/leak-sensor-gzfile-' . getmypid() . '.gz';
file_put_contents($f, gzencode("one\ntwo\n"));
echo implode('|', gzfile($f)); unlink($f);
"#,
    );
    assert_eq!(stdout, "one\n|two\n");
}

/// An include through `compress.zlib://` keeps nothing of the open, the
/// name its inner open set among it, whether the file opens or not, and
/// the included file is named by its real path, as php names it (#2119).
#[test]
fn an_include_through_the_wrapper_gives_up_the_path_it_opened() {
    let stdout = armed(
        r#"<?php
$f = sys_get_temp_dir() . '/leak-sensor-include-' . getmypid() . '.php';
file_put_contents($f, gzencode('<?php echo __FILE__ === realpath($GLOBALS["f"]) ? "file" : __FILE__, "\n";'));
include 'compress.zlib://' . $f;
var_dump(@include 'compress.zlib://' . $f . '.missing');
echo in_array(realpath($f), get_included_files(), true) ? 'named' : implode(',', get_included_files()), "\n";
unlink($f);
"#,
    );
    assert_eq!(stdout, "file\nbool(false)\nnamed\n");
}

/// A persistent stream's filter keeps zlib's state on the system
/// allocator, not the request's heap, since it outlives the request with
/// the stream: the request ends with nothing of zlib's held.
#[test]
fn a_persistent_streams_filter_holds_nothing_of_the_request() {
    let stdout = armed(
        r#"<?php
$s = stream_socket_server('tcp://127.0.0.1:0');
$a = pfsockopen(...explode(':', stream_socket_get_name($s, false)));
stream_filter_append($a, 'zlib.deflate', STREAM_FILTER_WRITE);
fwrite($a, str_repeat('x', 100));
echo "end\n";
"#,
    );
    assert_eq!(stdout, "end\n");
}
