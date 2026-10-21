//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

mod support;

use php_ext::test::{Capture, Files};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = support::engine();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// `exif_read_data` lays a section's values into an array of their own
/// only where the section is one, `add_assoc_image_info`'s
/// `array_init(&tmpi)` under `sub_array`, and otherwise into the
/// answer, making no array it does not hand on (#2100).
#[test]
fn a_section_laid_into_the_answer_makes_no_array_of_its_own() {
    let files = Files::new("exif-leak", &[]);
    let path = files.path().join("section.tiff");
    let tiff: &[u8] = b"MM\0\x2A\0\0\0\x08\0\x03\x01\x0F\0\x02\0\0\0\x04ACM\0\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\x82\x98\0\x02\0\0\0\x04A\0B\0\0\0\0\0";
    files.write("section.tiff", tiff);
    let stdout = armed(&format!(
        "<?php $r = exif_read_data('{}'); echo $r['MimeType'], ' ', $r['Make'], ' ', $r['COMPUTED']['Copyright'], \"\\n\";",
        path.display()
    ));
    assert_eq!(stdout, "image/tiff ACM A, B\n");
}
