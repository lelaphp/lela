//! The request leak sensor (#1992) over this extension's wrapper: each
//! request below gives up everything its opens took, so an armed engine
//! ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

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
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
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

/// The bzip2 filters give back to the request's heap everything they
/// took of it, the library's state among it (#2620): removed by hand,
/// ended at their stream's end, or still attached as the request ends,
/// a decompressor mid-stream included.
#[test]
fn the_filters_give_back_the_heap_they_took() {
    let stdout = armed(
        r#"<?php
$s = ''; for ($i = 0; $i < 20000; $i++) $s .= md5($i);
$f = fopen('php://memory', 'w+');
$c = stream_filter_append($f, 'bzip2.compress', STREAM_FILTER_WRITE, ['blocks' => 2]);
fwrite($f, $s);
stream_filter_remove($c);
rewind($f);
$d = stream_filter_append($f, 'bzip2.decompress', STREAM_FILTER_READ, ['small' => true]);
echo stream_get_contents($f) === $s ? 'round' : 'broken', "\n";
stream_filter_remove($d);
$g = fopen('php://memory', 'w+'); fwrite($g, bzcompress($s)); rewind($g);
stream_filter_append($g, 'bzip2.decompress', STREAM_FILTER_READ);
echo strlen(fread($g, 100)), "\n";
$h = fopen('php://memory', 'w+');
stream_filter_append($h, 'bzip2.compress', STREAM_FILTER_WRITE);
fwrite($h, 'left');
"#,
    );
    assert_eq!(stdout, "round\n100\n");
}
