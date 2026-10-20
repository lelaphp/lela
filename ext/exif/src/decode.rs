//! The three `zend_multibyte` calls `exif.c` makes: fetch an encoding
//! by name, convert between two, and parse an ini entry's list. php
//! answers them from the functions the loaded mbstring hands
//! `zend_multibyte_set_functions`, and with Zend's dummies where none
//! did, under which nothing is fetched and every list parses; the
//! engine's [`Extensions::encodings`] and [`Extensions::parse_encoding_list`]
//! are both.

use php_ext::request::Request;

/// `zend_multibyte_encoding_converter` from `from` to `to`, each fetched
/// by name with `zend_multibyte_fetch_encoding`, `None` where either
/// does not fetch or nothing converts: the caller keeps the bytes as
/// they are.
pub(crate) fn convert(req: &mut Request, text: &[u8], to: &[u8], from: &[u8]) -> Option<Vec<u8>> {
    let functions = req.extensions_mut().encodings();
    let (to, from) = (functions.fetch(to)?, functions.fetch(from)?);
    functions.convert(text, to, from)
}

/// `zend_multibyte_parse_encoding_list` succeeds for `names`, the
/// provider's warning raised where it does not.
pub(crate) fn parses(req: &mut Request, names: &[u8]) -> bool {
    req.extensions_mut().parse_encoding_list(names).is_some()
}

/// `USE_MBSTRING`: whether mbstring is loaded.
pub(crate) fn provided(req: &Request) -> bool {
    req.extensions().named(b"mbstring").is_some()
}
