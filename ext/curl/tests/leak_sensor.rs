//! The request leak sensor (#1992) over the handles the request keeps
//! (ADR-0013, #2317): each request below gives up everything its natives
//! took, so an armed engine ends it without the `InternalError` naming a
//! block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// A file of `body` the test writes for the PHP code to read and removes
/// after, at a path its name makes its own.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str, body: &[u8]) -> Scratch {
        let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        std::fs::write(&path, body).expect("the temporary directory takes a file");
        Scratch(path)
    }

    fn url(&self) -> String {
        format!("file://{}", self.0.display())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// `src` served by an armed engine with this extension, what it wrote
/// held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[php_ext_curl::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(src, Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(String::from_utf8_lossy(&served.stdout), expected);
}

/// `curl_multi_close()` from inside a transfer's write callback gives up
/// the `CurlHandle` the multi held, whose object goes as the callback
/// returns: the transfer holds its `CURL *`, which is cleaned up as the
/// transfer ends, and the multi transfer runs to its end.
#[test]
fn a_handle_freed_by_its_own_write_callback_transfers_to_its_end() {
    let file = Scratch::new("curl-freed-in-callback", &vec![b'x'; 40000]);
    served(
        &format!(
            "<?php $m = curl_multi_init(); $d = curl_init('{}');\n\
             curl_setopt($d, CURLOPT_WRITEFUNCTION, function ($h, $data) use ($m) {{ static $n = 0; if ($n++ === 0) {{ curl_multi_close($m); echo \"closed\\n\"; }} return strlen($data); }});\n\
             curl_multi_add_handle($m, $d); unset($d);\n\
             do {{ $status = curl_multi_exec($m, $active); }} while ($active && $status == CURLM_OK);\n\
             var_dump($status, curl_multi_get_handles($m));\n",
            file.url()
        ),
        "closed\nint(0)\narray(0) {\n}\n",
    );
}

/// A copy of a `CurlHandle` whose `CURLOPT_POSTFIELDS` held a file points at
/// the original's lists and file parts until its own form replaces them,
/// so the original keeps them past setting new ones and past its own
/// end, and the request's end closes and frees them.
#[test]
fn a_copy_s_lists_and_file_parts_outlive_the_original() {
    let file = Scratch::new("curl-copied-form", b"upload body");
    served(
        &format!(
            "<?php $f = '{}';\n\
         $c = curl_init('file:///nonexistent-curl-copy');\n\
         curl_setopt($c, CURLOPT_POSTFIELDS, ['file' => new CURLFile($f), 'x' => 'y']);\n\
         curl_setopt($c, CURLOPT_HTTPHEADER, ['X-A: 1']);\n\
         $d = curl_copy_handle($c); $e = clone $d;\n\
         curl_setopt($c, CURLOPT_POSTFIELDS, ['file' => new CURLFile($f)]);\n\
         curl_setopt($c, CURLOPT_HTTPHEADER, ['X-B: 2']);\n\
         unset($c);\n\
         var_dump(curl_exec($d), curl_errno($d), curl_exec($e)); unset($d, $e);\n",
            file.0.display()
        ),
        "bool(false)\nint(37)\nbool(false)\n",
    );
}

/// A form whose element throws while it is built goes on past the
/// throw, as php's walk goes on with `EG(exception)` set, and is set as
/// the walk ends; the `CURLFile` part's stream closes with the `CurlHandle`
/// and the throwable reaches the `catch` (#2519).
#[test]
fn a_form_whose_element_throws_closes_its_file_parts() {
    let file = Scratch::new("curl-form-throws", b"x");
    served(
        &format!(
            "<?php class T {{ function __toString(): string {{ throw new Exception('no'); }} }}\n\
             class U {{ function __toString(): string {{ echo \"U\\n\"; return 'u'; }} }}\n\
             $c = curl_init('file:///nonexistent-curl-form');\n\
             try {{ curl_setopt($c, CURLOPT_POSTFIELDS, ['a' => new CURLFile('{0}'), 'b' => new T]); }} catch (Exception $e) {{ echo $e->getMessage(), \"\\n\"; }}\n\
             try {{ curl_setopt($c, CURLOPT_POSTFIELDS, ['b' => new T, 'a' => new CURLFile('{0}'), 's' => new CURLStringFile('d', 'p')]); }} catch (Exception $e) {{ echo $e->getMessage(), \"\\n\"; }}\n\
             try {{ curl_setopt($c, CURLOPT_POSTFIELDS, ['b' => new T, 'u' => new U, 'l' => [new U, new T]]); }} catch (Exception $e) {{ echo $e->getMessage(), \"\\n\"; }}\n\
             unset($c); echo \"end\\n\";\n",
            file.0.display()
        ),
        "no\nno\nno\nend\n",
    );
}

/// A `CurlHandle` whose form holds a `CURLFile` freed from inside the
/// write callback of a transfer started from another transfer's
/// callback: the inner transfer holds its `CURL *`, which is cleaned up
/// as that transfer ends, while the outer one still lends the request
/// (#2520).
#[test]
fn a_handle_freed_inside_a_nested_transfer_is_cleaned_up_with_no_request_lent() {
    let body = Scratch::new("curl-nested-body", &vec![b'x'; 40000]);
    let upload = Scratch::new("curl-nested-upload", b"upload body");
    served(
        &format!(
            "<?php $url = '{0}'; $f = '{1}';\n\
             $outer = curl_init($url);\n\
             curl_setopt($outer, CURLOPT_WRITEFUNCTION, function ($h, $data) use ($url, $f) {{\n\
                 static $n = 0;\n\
                 if ($n++ === 0) {{\n\
                     $m = curl_multi_init(); $d = curl_init($url);\n\
                     curl_setopt($d, CURLOPT_POSTFIELDS, ['file' => new CURLFile($f)]);\n\
                     curl_setopt($d, CURLOPT_WRITEFUNCTION, function ($h, $data) use ($m) {{ static $k = 0; if ($k++ === 0) {{ curl_multi_close($m); echo \"closed\\n\"; }} return strlen($data); }});\n\
                     curl_multi_add_handle($m, $d); unset($d);\n\
                     do {{ $status = curl_multi_exec($m, $active); }} while ($active && $status == CURLM_OK);\n\
                     echo \"inner \", $status, \"\\n\";\n\
                 }}\n\
                 return strlen($data);\n\
             }});\n\
             var_dump(curl_exec($outer));\n",
            body.url(),
            upload.0.display()
        ),
        "closed\ninner 0\nbool(true)\n",
    );
}
