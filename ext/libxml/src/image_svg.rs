//! `php-src/ext/libxml/image_svg.c`: the SVG reader libxml's `MINIT` hands
//! `standard` with `php_image_register_handler`, here a registered
//! [`ImageHandler`] that `getimagesize` asks of a file every fixed format
//! refused.

use php_ext_standard::{ImageHandler, ImageSize};

use crate::ffi::Reader;

/// `svg_image_handler`.
pub(crate) const SVG: ImageHandler = ImageHandler {
    mime_type: "image/svg+xml",
    extension: ".svg",
    const_name: "IMAGETYPE_SVG",
    read,
};

/// `php_libxml_svg_image_handle`: bytes that start with `<`, read with
/// libxml's pull reader to their first element, which must be an `svg`
/// (in any case) with a `width` and a `height` that are each digits and
/// then letters; the size is their leading numbers, the units their
/// letters.
///
/// ```php
/// var_dump(IMAGETYPE_SVG, IMAGETYPE_COUNT, image_type_to_mime_type(IMAGETYPE_SVG), image_type_to_extension(IMAGETYPE_SVG), image_type_to_extension(IMAGETYPE_SVG, false));
/// var_dump(getimagesizefromstring('<svg width="12" height="3px"/>'));
/// var_dump(getimagesizefromstring("<?xml version=\"1.0\"?>\n<x:SVG xmlns:x=\"urn:x\" width='4cm' height='8mm'><g/></x:SVG>"));
/// var_dump(getimagesizefromstring('<svg width="1e" height="1"/>'), getimagesizefromstring('<svg width="1"/>'), getimagesizefromstring('<g width="1" height="1"/>'), getimagesizefromstring(' <svg width="1" height="1"/>'));
/// ```
/// ```expect
/// int(21)
/// int(22)
/// string(13) "image/svg+xml"
/// string(4) ".svg"
/// string(3) "svg"
/// array(7) {
///   [0]=>
///   int(12)
///   [1]=>
///   int(3)
///   [2]=>
///   int(21)
///   [3]=>
///   string(21) "width="12" height="3""
///   ["mime"]=>
///   string(13) "image/svg+xml"
///   ["width_unit"]=>
///   string(2) "px"
///   ["height_unit"]=>
///   string(2) "px"
/// }
/// array(6) {
///   [0]=>
///   int(4)
///   [1]=>
///   int(8)
///   [2]=>
///   int(21)
///   ["mime"]=>
///   string(13) "image/svg+xml"
///   ["width_unit"]=>
///   string(2) "cm"
///   ["height_unit"]=>
///   string(2) "mm"
/// }
/// array(6) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(1)
///   [2]=>
///   int(21)
///   ["mime"]=>
///   string(13) "image/svg+xml"
///   ["width_unit"]=>
///   string(1) "e"
///   ["height_unit"]=>
///   string(2) "px"
/// }
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn read(bytes: &[u8]) -> Option<ImageSize> {
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let element = Reader::first_element(bytes)?;
    if !element.name?.eq_ignore_ascii_case(b"svg") {
        return None;
    }
    let (width, width_unit) = dimension(&element.width?)?;
    let (height, height_unit) = dimension(&element.height?)?;
    Some(ImageSize {
        width,
        height,
        width_unit,
        height_unit,
    })
}

/// `php_libxml_parse_dimension`, then `ZEND_STRTOL(input, NULL, 10)` into
/// an `unsigned int`: a digit first, digits up to the first letter, and
/// letters to the end; the number, and the letters where there are any.
fn dimension(input: &[u8]) -> Option<(u32, Option<Vec<u8>>)> {
    if !input.first()?.is_ascii_digit() {
        return None;
    }
    let digits = input.iter().take_while(|b| b.is_ascii_digit()).count();
    let unit = &input[digits..];
    if !unit.iter().all(u8::is_ascii_alphabetic) {
        return None;
    }
    // `strtol` saturates at `LONG_MAX`, and the `unsigned int` keeps its low
    // 32 bits.
    let number = input[..digits].iter().fold(0_i64, |n, d| {
        n.saturating_mul(10).saturating_add(i64::from(d - b'0'))
    });
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let number = number as u32;
    Some((number, (!unit.is_empty()).then(|| unit.to_vec())))
}
