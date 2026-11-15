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

/// A sibling's debug callable that calls `curl_multi_get_handles()`
/// during the collection that frees the multi's `CurlHandle` objects is
/// given the one the collection has destroyed, as php lists it, and
/// `curl_getinfo` on it ends the request with the `InternalError` of a
/// method served on an object without its payload, status 255, as on
/// every destroyed `CurlHandle`. php's answer there is not held, so no
/// witness can pin it; this holds the engine's side of
/// `trace:curl-multi-get-handles-lists-collected-handle`.
#[test]
fn a_curl_object_the_collection_destroyed_listed_to_a_sibling_ends_the_request() {
    let engine = Engine::builder()
        .extensions(&[php_ext_curl::extension()])
        .ini("html_errors", "0")
        .build();
    let served = engine.serve_capture(
        r#"<?php
$collecting = false;
$m = curl_multi_init();
$a = curl_init('http://127.0.0.1:1/');
$b = curl_init('http://127.0.0.1:1/');
foreach ([$a, $b] as $h) {
    curl_setopt($h, CURLOPT_VERBOSE, true);
    curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) use ($m, &$collecting) {
        static $acted = false;
        if (!$collecting || $acted) return;
        $acted = true;
        foreach (curl_multi_get_handles($m) as $k => $h) {
            echo "$k: "; var_dump(curl_getinfo($h, CURLINFO_EFFECTIVE_URL), curl_getinfo($h, CURLINFO_PRIMARY_PORT), curl_errno($h));
        }
    });
    curl_multi_add_handle($m, $h);
}
curl_multi_exec($m, $running);
unset($a, $b, $h, $m);
$collecting = true;
var_dump(gc_collect_cycles());
echo "Done\n";
"#,
        Capture::default(),
    );
    assert_eq!(
        served.stdout_text(),
        "0: string(19) \"http://127.0.0.1:1/\"\nint(-1)\nint(0)\n\
         1: string(19) \"http://127.0.0.1:1/\"\nint(-1)\nint(0)\n\
         0: \nFatal error: InternalError: a method served on an object without its payload in Standard input code on line 13\n\
         Stack trace:\n#0 Standard input code(13): curl_getinfo(Object(CurlHandle), 1048577)\n\
         #1 [internal function]: {closure:Standard input code:8}(Object(CurlHandle), 0, 'closing connect...')\n\
         #2 Standard input code(21): gc_collect_cycles()\n#3 {main}\n"
    );
    assert_eq!(served.exit_code, 255);
}
