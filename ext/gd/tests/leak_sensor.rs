//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// `src` served by an armed engine with this extension alone, what it
/// wrote held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[php_ext_gd::extension()])
        .build();
    let served = engine.serve_capture(src, Capture::default());
    served.assert_clean(expected);
}

/// `imagecreatefrom*()` gives up the bytes it read from the file, the
/// string `_php_stream_copy_to_mem` answers, whether the decoder takes
/// them or refuses them.
#[test]
fn an_image_from_a_file_keeps_no_bytes() {
    served(
        "<?php $u = 'data://text/plain;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7';\n\
         echo imagesx(imagecreatefromgif($u)), \"\\n\";\n\
         echo @imagecreatefrompng($u) === false ? 'refused' : 'taken', \"\\n\";\n",
        "1\nrefused\n",
    );
}
