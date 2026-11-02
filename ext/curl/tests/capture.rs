//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_curl::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('curl'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// A `CurlHandle` its debug callable keeps past the collection that frees
/// it is destroyed: `var_dump` prints it as a `CurlHandle` of no
/// properties, and `curl_getinfo` on it ends the request with the
/// `InternalError` of a method served on an object without its payload,
/// status 255. This holds the engine's side of
/// `trace:curl-handle-kept-past-collection`, as `free_obj`'s fence does.
/// The program is the fence's, with Zend's `strncmp` for ext/standard's
/// `str_starts_with`, which an engine of this extension alone does not
/// declare.
#[test]
fn a_curl_object_its_debug_callable_keeps_past_a_collection_is_destroyed() {
    let engine = Engine::builder()
        .extensions(&[php_ext_curl::extension()])
        .ini("html_errors", "0")
        .build();
    let served = engine.serve_capture(
        r#"<?php
$m = curl_multi_init();
$h = curl_init('http://127.0.0.1:1/');
curl_setopt($h, CURLOPT_VERBOSE, 1);
curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $t, $d) use ($m) {
    if ($t == CURLINFO_TEXT && strncmp($d, 'closing', 7) == 0) { echo "debug: $d"; $GLOBALS['keep'] = $ch; }
});
curl_multi_add_handle($m, $h);
curl_multi_exec($m, $r);
unset($m, $h);
var_dump(gc_collect_cycles());
var_dump($keep);
var_dump(curl_getinfo($keep, CURLINFO_EFFECTIVE_URL));
echo "end\n";
"#,
        Capture::default(),
    );
    assert_eq!(
        served.stdout_text(),
        "debug: closing connection #0\nint(3)\nobject(CurlHandle)#2 (0) {\n}\n\n\
         Fatal error: InternalError: a method served on an object without its payload in Standard input code on line 13\n\
         Stack trace:\n#0 Standard input code(13): curl_getinfo(Object(CurlHandle), 1048577)\n#1 {main}\n"
    );
    assert_eq!(served.exit_code, 255);
}
