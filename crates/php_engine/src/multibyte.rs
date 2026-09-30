//! `zend.multibyte`'s engine half (design §4.9, §6.1): the three entries
//! of `Z/Zend/zend.c` that say whether and how a file is read in its own
//! encoding, and the encoding functions bound to the request a file
//! compiles for: Zend's dummies (`Z/Zend/zend_multibyte.c:25-80`) until an
//! extension can hand the engine its own. What the scanner does with them
//! is the compiler's ([`php_compile::multibyte`]).

use php_compile::multibyte::{Dummy, Encoding, Encodings, Multibyte};

use crate::api::request::{Request, ini_truth};

/// The functions bound to one request, with what the scanner reads
/// beside them: what the compiler's [`Multibyte`] borrows.
pub(crate) struct Bound {
    encodings: Box<dyn Encodings>,
    detect_unicode: bool,
    listed: Vec<Encoding>,
}

impl Bound {
    /// What the compiler is handed.
    pub(crate) fn settings(&self) -> Multibyte<'_> {
        Multibyte {
            encodings: &*self.encodings,
            detect_unicode: self.detect_unicode,
            listed: &self.listed,
        }
    }
}

/// The functions bound to a request: Zend's dummies, since no
/// extension sets any yet (`zend_multibyte_get_functions()` is none).
fn bind() -> Box<dyn Encodings> {
    Box::new(Dummy)
}

/// What a file compiled for `req` is read under where `multibyte`, the
/// `zend.multibyte` the compile reads, is on: the functions bound to the
/// request, `zend.detect_unicode` and `CG(script_encoding_list)`, the
/// request's where it set one and the engine's where it did not.
pub(crate) fn bound(req: &mut Request, multibyte: bool) -> Option<Bound> {
    if !multibyte {
        return None;
    }
    let detect_unicode = ini_truth(req.ini_get(b"zend.detect_unicode"));
    let listed = req
        .script_encodings
        .clone()
        .unwrap_or_else(|| req.shared.script_encodings.clone());
    Some(Bound {
        encodings: bind(),
        detect_unicode,
        listed,
    })
}

/// `OnUpdateScriptEncoding` (`Z/Zend/zend.c:144-153`) for `value`:
/// refused while `zend.multibyte` is off; taken as it stands where no
/// extension set the functions; otherwise the list the functions read of
/// it (`zend_multibyte_set_script_encoding_by_string`), refused where a
/// name names no encoding, with the provider's warning, or where it
/// names none at all. The list taken, `None` for a refusal.
///
/// ```php
/// var_dump(ini_get('zend.multibyte'), ini_get('zend.script_encoding'), ini_get('zend.detect_unicode'));
/// var_dump(ini_set('zend.multibyte', '1'), ini_set('zend.script_encoding', 'UTF-8'), ini_set('zend.detect_unicode', '0'), ini_get('zend.script_encoding'));
/// ```
/// ```expect
/// string(1) "0"
/// string(0) ""
/// string(1) "1"
/// bool(false)
/// bool(false)
/// string(1) "1"
/// string(0) ""
/// ```
pub(crate) fn script_encoding(req: &mut Request, _value: &[u8]) -> Option<Vec<Encoding>> {
    if !ini_truth(req.ini_get(b"zend.multibyte")) {
        return None;
    }
    Some(Vec::new())
}

/// `zend.script_encoding` as the engine starts: the entry registered
/// under `OnUpdateScriptEncoding`, which refuses a value while
/// `zend.multibyte` is off and so leaves it at its default, then read
/// again once the functions are set (`zend_multibyte_set_functions`),
/// where a name that names no encoding is the provider's warning. The
/// list, and whether the entry was refused.
pub(crate) fn startup(req: &mut Request) -> (Vec<Encoding>, Startup) {
    let value = req
        .ini_get(b"zend.script_encoding")
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    if value.is_empty() {
        return (Vec::new(), Startup::Taken);
    }
    if !ini_truth(req.ini_get(b"zend.multibyte")) {
        return (Vec::new(), Startup::Refused);
    }
    let listed = script_encoding(req, &value).unwrap_or_default();
    (listed, Startup::Taken)
}

/// What the startup did with the host's `zend.script_encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Startup {
    Taken,
    /// Refused, so the entry keeps its default.
    Refused,
}
