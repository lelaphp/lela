//! libzip's strings (`zip_string.c`, `zip_utf-8.c`): a name or a comment
//! as its raw bytes and what is known of their encoding, guessed as
//! ASCII, UTF-8 or CP437 and read back as UTF-8 unless the raw bytes are
//! asked for.

use std::borrow::Cow;

/// `ZIP_FL_ENC_RAW`.
pub(crate) const FL_ENC_RAW: u32 = 64;
/// `ZIP_FL_ENC_STRICT`.
pub(crate) const FL_ENC_STRICT: u32 = 128;
/// `ZIP_FL_ENC_UTF_8`.
pub(crate) const FL_ENC_UTF_8: u32 = 2048;
/// `ZIP_FL_ENC_CP437`.
pub(crate) const FL_ENC_CP437: u32 = 4096;
/// `ZIP_FL_ENCODING_ALL`.
const FL_ENCODING_ALL: u32 = FL_ENC_UTF_8 | FL_ENC_CP437;

/// `zip_encoding_type_t`, past `ZIP_ENCODING_UNKNOWN`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Enc {
    Ascii,
    Utf8Known,
    Utf8Guessed,
    Cp437,
}

/// `zip_string_t`: the bytes, and the encoding where it is known rather
/// than guessed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ZStr {
    pub(crate) raw: Vec<u8>,
    pub(crate) known: Option<Enc>,
}

impl ZStr {
    /// `_zip_string_new` with `flags`' encoding: `None` for no bytes, and
    /// `Err` for bytes that are not the encoding the flags name.
    pub(crate) fn new(raw: &[u8], flags: u32) -> Result<Option<ZStr>, ()> {
        if raw.is_empty() {
            return Ok(None);
        }
        let known = match flags & FL_ENCODING_ALL {
            0 => None,
            FL_ENC_UTF_8 if can_be_utf8(raw) => Some(Enc::Utf8Known),
            FL_ENC_CP437 => Some(Enc::Cp437),
            _ => return Err(()),
        };
        Ok(Some(ZStr {
            raw: raw.to_vec(),
            known,
        }))
    }

    /// `_zip_guess_encoding(str, ZIP_ENCODING_UNKNOWN)`.
    pub(crate) fn guess(&self) -> Enc {
        if let Some(enc) = self.known {
            return enc;
        }
        guess(&self.raw)
    }

    /// `_zip_set_name`'s and `zip_file_set_comment`'s promotion: a guessed
    /// UTF-8 string is known to be UTF-8.
    pub(crate) fn promoted(mut self) -> ZStr {
        if self.guess() == Enc::Utf8Guessed {
            self.known = Some(Enc::Utf8Known);
        }
        self
    }
}

/// `_zip_guess_encoding` over bytes whose encoding nothing names.
pub(crate) fn guess(raw: &[u8]) -> Enc {
    let ascii = raw.iter().all(|b| *b < 128);
    let control = raw
        .iter()
        .any(|b| *b < 32 && !matches!(b, b'\r' | b'\n' | b'\t'));
    if ascii {
        if control { Enc::Cp437 } else { Enc::Ascii }
    } else if can_be_utf8(raw) {
        Enc::Utf8Guessed
    } else {
        Enc::Cp437
    }
}

/// The UTF-8 half of `_zip_guess_encoding`: lead bytes of two to four
/// bytes, each followed by its trailing `10xxxxxx` bytes.
fn can_be_utf8(raw: &[u8]) -> bool {
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b < 128 {
            i += 1;
            continue;
        }
        let ulen = if b & 0xe0 == 0xc0 {
            1
        } else if b & 0xf0 == 0xe0 {
            2
        } else if b & 0xf8 == 0xf0 {
            3
        } else {
            return false;
        };
        if i + ulen >= raw.len() {
            return false;
        }
        if raw[i + 1..=i + ulen].iter().any(|c| c & 0xc0 != 0x80) {
            return false;
        }
        i += ulen + 1;
    }
    true
}

/// `_zip_string_get`: the raw bytes under `ZIP_FL_ENC_RAW`, else UTF-8,
/// CP437 converted, and `""` for no string.
pub(crate) fn get(s: Option<&ZStr>, flags: u32) -> Cow<'_, [u8]> {
    let Some(s) = s else {
        return Cow::Borrowed(b"");
    };
    if flags & FL_ENC_RAW == 0 {
        let enc = s.guess();
        let strict = flags & FL_ENC_STRICT != 0 && !matches!(enc, Enc::Ascii | Enc::Utf8Known);
        if strict || enc == Enc::Cp437 {
            return Cow::Owned(cp437_to_utf8(&s.raw));
        }
    }
    Cow::Borrowed(&s.raw)
}

/// `_zip_cp437_to_utf8`: printable ASCII as itself, every other byte
/// through `_cp437_to_unicode`.
fn cp437_to_utf8(raw: &[u8]) -> Vec<u8> {
    let mut out = String::with_capacity(raw.len());
    for b in raw {
        let c = match b {
            0x20..=0x7e => char::from(*b),
            0x00..=0x1f => CP437_LOW.chars().nth(usize::from(*b)).unwrap_or('\u{fffd}'),
            _ => CP437_HIGH
                .chars()
                .nth(usize::from(*b) - 0x7f)
                .unwrap_or('\u{fffd}'),
        };
        out.push(c);
    }
    out.into_bytes()
}

/// `_cp437_to_unicode` from `0x00` to `0x1f`.
const CP437_LOW: &str = "\0☺☻♥♦♣♠•◘○◙♂♀♪♫☼►◄↕‼¶§▬↨↑↓→←∟↔▲▼";

/// `_cp437_to_unicode` from `0x7f` to `0xff`.
const CP437_HIGH: &str = "⌂ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
