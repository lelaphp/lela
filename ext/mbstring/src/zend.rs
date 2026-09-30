//! `mbstring.c`'s side of `zend.multibyte` (`Z/ext/mbstring/mbstring.c:425-499`):
//! the functions `PHP_MINIT_FUNCTION(mbstring)` hands the engine with
//! `zend_multibyte_set_functions`, answered from libmbfl and the
//! request's settings as they stand when the engine asks.

use crate::convert::{convert, guess};
use crate::ffi::mbfl::Encoding;
use crate::settings::{Substitute, parse_list, settings};
use php_ext::abi::{self, Encodings, Multibyte, Unicode};
use php_ext::request::Request;

/// `MBFL_ENCTYPE_GL_UNSAFE`: an encoding whose bytes past the first may
/// be ASCII's, which the lexer would read as its own.
const GL_UNSAFE: u32 = 0x4000;
/// `MBFL_ENCTYPE_WCS2` and `MBFL_ENCTYPE_WCS4`: two and four bytes a
/// character.
const WCS2: u32 = 0x2;
const WCS4: u32 = 0x4;

/// The functions bound to one request: `MBSTRG(internal_encoding)`, the
/// ini's, what the scanner converts into; the current detect order;
/// the current substitute character; the language's `auto` list; and
/// `MBSTRG(http_input_list)` where `mbstring.encoding_translation` is on.
struct Functions {
    internal: Encoding,
    http_input: Option<Vec<Encoding>>,
    detect_order: Vec<Encoding>,
    substitute: Substitute,
    default_detect: &'static [i32],
}

/// What [`php_ext::abi::ExtensionBuilder::encodings`] is handed: the
/// functions as this request's settings stand. The phpt subset's
/// `zend_multibyte-*.phpt` hold what the scanner does with them, since
/// `zend.multibyte` is `PHP_INI_PERDIR` and no snippet turns it on.
pub(crate) fn bind(req: &mut Request) -> Box<dyn Multibyte> {
    let settings = settings(req);
    Box::new(Functions {
        internal: settings.internal,
        http_input: settings
            .encoding_translation
            .then(|| settings.http_input.clone()),
        detect_order: settings.detect_order.clone(),
        substitute: settings.substitute,
        default_detect: settings.default_detect,
    })
}

/// libmbfl's encoding as the engine names it.
fn named(encoding: Encoding) -> abi::Encoding {
    abi::Encoding(u32::try_from(encoding.no()).unwrap_or_default())
}

/// The engine's name for an encoding, as libmbfl's.
fn of(encoding: abi::Encoding) -> Option<Encoding> {
    Encoding::numbered(i32::try_from(encoding.0).ok()?)
}

impl Encodings for Functions {
    /// `php_mb_zend_encoding_fetcher`: `mbfl_name2encoding`.
    fn fetch(&self, name: &[u8]) -> Option<abi::Encoding> {
        Encoding::named(name).map(named)
    }

    fn unicode(&self, which: Unicode) -> abi::Encoding {
        let name: &[u8] = match which {
            Unicode::Utf32Be => b"UTF-32BE",
            Unicode::Utf32Le => b"UTF-32LE",
            Unicode::Utf16Be => b"UTF-16BE",
            Unicode::Utf16Le => b"UTF-16LE",
            Unicode::Utf8 => b"UTF-8",
        };
        Encoding::named(name).map_or(named(Encoding::utf8()), named)
    }

    /// `php_mb_zend_encoding_name_getter`.
    fn name(&self, encoding: abi::Encoding) -> Vec<u8> {
        of(encoding).map(|e| e.name().to_vec()).unwrap_or_default()
    }

    /// `php_mb_zend_encoding_lexer_compatibility_checker`.
    fn lexer_compatible(&self, encoding: abi::Encoding) -> bool {
        of(encoding).is_some_and(|e| e.flag() & GL_UNSAFE == 0)
    }

    /// `php_mb_zend_encoding_detector`: over the current detect order
    /// where no list is given, and never `pass`.
    fn detect(&self, text: &[u8], list: &[abi::Encoding]) -> Option<abi::Encoding> {
        let list: Vec<Encoding> = if list.is_empty() {
            self.detect_order.clone()
        } else {
            list.iter().filter_map(|&e| of(e)).collect()
        };
        if list == [Encoding::pass()] {
            return None;
        }
        guess(&[text], &list, false, false).map(named)
    }

    /// `php_mb_zend_encoding_converter`: `mb_fast_convert` under the
    /// current substitute, which never refuses.
    fn convert(&self, text: &[u8], to: abi::Encoding, from: abi::Encoding) -> Option<Vec<u8>> {
        Some(convert(text, of(from)?, of(to)?, self.substitute).0)
    }

    /// `php_mb_zend_internal_encoding_getter`.
    fn internal(&self) -> Option<abi::Encoding> {
        Some(named(self.internal))
    }

    /// `php_mb_zend_encoding_list_parser`: `php_mb_parse_encoding_list`
    /// as an ini entry reads it (`arg_num` 0), its warning the engine's
    /// to raise.
    fn list(&self, names: &[u8]) -> Result<Vec<abi::Encoding>, Vec<u8>> {
        parse_list(names, self.default_detect)
            .map(|list| list.into_iter().map(named).collect())
            .map_err(|invalid| invalid.0)
    }
}

impl Multibyte for Functions {
    /// `php_mb_encoding_translation` and `php_mb_gpc_get_detect_order`.
    fn http_input(&self) -> Option<Vec<abi::Encoding>> {
        self.http_input
            .as_ref()
            .map(|list| list.iter().map(|&e| named(e)).collect())
    }

    /// `php_mb_mbchar_bytes`: the encoding's `mblen_table`, else its
    /// fixed width, else one.
    fn char_len(&self, encoding: abi::Encoding, lead: u8) -> usize {
        let Some(encoding) = of(encoding) else {
            return 1;
        };
        if let Some(table) = encoding.mblen_table() {
            usize::from(table[usize::from(lead)])
        } else if encoding.flag() & WCS2 != 0 {
            2
        } else if encoding.flag() & WCS4 != 0 {
            4
        } else {
            1
        }
    }
}
