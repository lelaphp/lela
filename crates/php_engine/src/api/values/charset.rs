//! `Z/ext/standard/html.c`'s charsets: `enum entity_charset`, the names
//! `charset_map` takes and `get_next_char`, which reads one character
//! of a charset. The engine's `escape_html` reads a diagnostic in the
//! request's charset with them (`Z/main/main.c:1038-1050`), and
//! ext/standard's escapers and `php_next_utf8_char` read theirs.

/// `enum entity_charset`, in php's order, which the three `CHARSET_*`
/// classes read: UTF-8, the eight single-byte charsets php has tables
/// for, and the five multi-byte ones it reads and passes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Charset {
    Utf8,
    Iso88591,
    Cp1252,
    Iso885915,
    Cp1251,
    Iso88595,
    Cp866,
    MacRoman,
    Koi8r,
    Big5,
    Gb2312,
    Big5Hkscs,
    Sjis,
    EucJp,
}

/// `charset_map`: each name php takes, compared ignoring case.
const CHARSET_MAP: [(&[u8], Charset); 33] = [
    (b"ISO-8859-1", Charset::Iso88591),
    (b"ISO8859-1", Charset::Iso88591),
    (b"ISO-8859-15", Charset::Iso885915),
    (b"ISO8859-15", Charset::Iso885915),
    (b"utf-8", Charset::Utf8),
    (b"cp1252", Charset::Cp1252),
    (b"Windows-1252", Charset::Cp1252),
    (b"1252", Charset::Cp1252),
    (b"BIG5", Charset::Big5),
    (b"950", Charset::Big5),
    (b"GB2312", Charset::Gb2312),
    (b"936", Charset::Gb2312),
    (b"BIG5-HKSCS", Charset::Big5Hkscs),
    (b"Shift_JIS", Charset::Sjis),
    (b"SJIS", Charset::Sjis),
    (b"932", Charset::Sjis),
    (b"SJIS-win", Charset::Sjis),
    (b"CP932", Charset::Sjis),
    (b"EUCJP", Charset::EucJp),
    (b"EUC-JP", Charset::EucJp),
    (b"eucJP-win", Charset::EucJp),
    (b"KOI8-R", Charset::Koi8r),
    (b"koi8-ru", Charset::Koi8r),
    (b"koi8r", Charset::Koi8r),
    (b"cp1251", Charset::Cp1251),
    (b"Windows-1251", Charset::Cp1251),
    (b"win-1251", Charset::Cp1251),
    (b"iso8859-5", Charset::Iso88595),
    (b"iso-8859-5", Charset::Iso88595),
    (b"cp866", Charset::Cp866),
    (b"866", Charset::Cp866),
    (b"ibm866", Charset::Cp866),
    (b"MacRoman", Charset::MacRoman),
];

/// One test of a byte of a double-byte charset ([`Charset::next_char`]).
type Test = fn(u32) -> bool;

impl Charset {
    /// The charset `charset_map` names `name` by, ignoring case, as
    /// `determine_charset` looks it up; `None` for a name it lacks,
    /// which php takes as UTF-8, behind a warning where it is not quiet.
    #[must_use]
    pub fn named(name: &[u8]) -> Option<Charset> {
        CHARSET_MAP
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|&(_, charset)| charset)
    }

    /// `CHARSET_UNICODE_COMPAT`: its code units are code points.
    #[must_use]
    pub fn unicode_compat(self) -> bool {
        self <= Charset::Iso88591
    }

    /// `CHARSET_SINGLE_BYTE`.
    #[must_use]
    pub fn single_byte(self) -> bool {
        self > Charset::Utf8 && self < Charset::Big5
    }

    /// `CHARSET_PARTIAL_SUPPORT`: read, and escaped with the basic
    /// entities alone.
    #[must_use]
    pub fn partial(self) -> bool {
        self >= Charset::Big5
    }

    /// `get_next_char(charset, str, str_len, cursor, status)`
    /// (`Z/ext/standard/html.c:89`): the character at `pos`, a code
    /// point in UTF-8 and the code unit's bytes as one number in the
    /// rest, or `None` for an ill-formed one, and the offset past what
    /// was read. A single-byte charset reads every byte as a character.
    #[must_use]
    pub fn next_char(self, s: &[u8], pos: usize) -> (Option<u32>, usize) {
        let avail = s.len().saturating_sub(pos);
        let at = |i: usize| u32::from(s.get(pos + i).copied().unwrap_or(0));
        let c = at(0);
        let two = || (c << 8) | at(1);
        // A double-byte charset's four tests: whether a byte leads a
        // pair, whether one trails it, whether a byte that fails to trail
        // is read again as the next character's, and whether a byte that
        // leads none is a character of its own.
        let big5_trail: Test = |n| (0x40..=0x7E).contains(&n) || (0xA1..=0xFE).contains(&n);
        let gb_lead: Test = |c| c != 0x8E && c != 0x8F && c != 0xA0 && c != 0xFF;
        let sjis_lead: Test = |c| c != 0x80 && c != 0xA0 && c < 0xFD;
        let (first, trail, again, single): (Test, Test, Test, Test) = match self {
            Charset::Utf8 => return next_utf8_char(s, pos),
            Charset::EucJp => return next_euc_jp_char(s, pos),
            Charset::Big5 => (
                |c| (0x81..=0xFE).contains(&c),
                big5_trail,
                |_| true,
                |_| true,
            ),
            Charset::Big5Hkscs => (
                |c| (0x81..=0xFE).contains(&c),
                big5_trail,
                |n| n != 0x80 && n != 0xFF,
                |_| true,
            ),
            Charset::Gb2312 => (
                |c| (0xA1..=0xFE).contains(&c),
                |c| (0xA1..=0xFE).contains(&c),
                gb_lead,
                gb_lead,
            ),
            Charset::Sjis => (
                |c| (0x81..=0x9F).contains(&c) || (0xE0..=0xFC).contains(&c),
                |n| (0x40..0xFD).contains(&n) && n != 0x7F,
                sjis_lead,
                |c| c < 0x80 || (0xA1..=0xDF).contains(&c),
            ),
            _ => return (Some(c), pos + 1),
        };
        if !first(c) {
            return (single(c).then_some(c), pos + 1);
        }
        if avail < 2 {
            return (None, pos + 1);
        }
        let next = at(1);
        if trail(next) {
            (Some(two()), pos + 2)
        } else if again(next) {
            (None, pos + 1)
        } else {
            (None, pos + 2)
        }
    }
}

/// `get_next_char`'s `cs_eucjp` arm: a two-byte kanji, a half-width
/// katakana after `SS2`, a three-byte one after `SS3`, or a byte of its
/// own other than `0xA0` and `0xFF`.
fn next_euc_jp_char(s: &[u8], pos: usize) -> (Option<u32>, usize) {
    let avail = s.len().saturating_sub(pos);
    let at = |i: usize| u32::from(s.get(pos + i).copied().unwrap_or(0));
    let c = at(0);
    let edge = |next: u32| if next != 0xA0 && next != 0xFF { 1 } else { 2 };
    if (0xA1..=0xFE).contains(&c) {
        if avail < 2 {
            return (None, pos + 1);
        }
        let next = at(1);
        if (0xA1..=0xFE).contains(&next) {
            (Some((c << 8) | next), pos + 2)
        } else {
            (None, pos + edge(next))
        }
    } else if c == 0x8E {
        if avail < 2 {
            return (None, pos + 1);
        }
        let next = at(1);
        if (0xA1..=0xDF).contains(&next) {
            (Some((c << 8) | next), pos + 2)
        } else {
            (None, pos + edge(next))
        }
    } else if c == 0x8F {
        let kanji = |c: u32| (0xA1..=0xFE).contains(&c);
        if avail < 3 || !kanji(at(1)) || !kanji(at(2)) {
            let skip = if avail < 2 || (at(1) != 0xA0 && at(1) != 0xFF) {
                1
            } else if avail < 3 || (at(2) != 0xA0 && at(2) != 0xFF) {
                2
            } else {
                3
            };
            return (None, pos + skip);
        }
        (Some((c << 16) | (at(1) << 8) | at(2)), pos + 3)
    } else if c != 0xA0 && c != 0xFF {
        (Some(c), pos + 1)
    } else {
        (None, pos + 1)
    }
}

/// `get_next_char`'s `cs_utf_8` arm, what `php_next_utf8_char` answers:
/// the code point at `pos` and the offset after it, or `None` and the
/// offset past the bytes UTR #36's second strategy takes as the
/// ill-formed sequence: a non-initial byte that is a lead or encodes a
/// character of its own is left for the next read.
fn next_utf8_char(s: &[u8], pos: usize) -> (Option<u32>, usize) {
    let avail = s.len().saturating_sub(pos);
    let lead = |c: u8| c < 0x80 || (0xc2..=0xf4).contains(&c);
    let trail = |c: u8| (0x80..=0xbf).contains(&c);
    let b = |i: usize| s.get(pos + i).copied().unwrap_or(0);
    let c = b(0);
    if c < 0x80 {
        return (Some(u32::from(c)), pos + 1);
    }
    if c < 0xc2 {
        return (None, pos + 1);
    }
    if c < 0xe0 {
        if avail < 2 {
            return (None, pos + 1);
        }
        if !trail(b(1)) {
            return (None, pos + if lead(b(1)) { 1 } else { 2 });
        }
        let cp = ((u32::from(c) & 0x1f) << 6) | (u32::from(b(1)) & 0x3f);
        if cp < 0x80 {
            return (None, pos + 2);
        }
        return (Some(cp), pos + 2);
    }
    if c < 0xf0 {
        if avail < 3 || !trail(b(1)) || !trail(b(2)) {
            let skip = if avail < 2 || lead(b(1)) {
                1
            } else if avail < 3 || lead(b(2)) {
                2
            } else {
                3
            };
            return (None, pos + skip);
        }
        let cp = ((u32::from(c) & 0x0f) << 12)
            | ((u32::from(b(1)) & 0x3f) << 6)
            | (u32::from(b(2)) & 0x3f);
        if cp < 0x800 || (0xd800..=0xdfff).contains(&cp) {
            return (None, pos + 3);
        }
        return (Some(cp), pos + 3);
    }
    if c < 0xf5 {
        if avail < 4 || !trail(b(1)) || !trail(b(2)) || !trail(b(3)) {
            let skip = if avail < 2 || lead(b(1)) {
                1
            } else if avail < 3 || lead(b(2)) {
                2
            } else if avail < 4 || lead(b(3)) {
                3
            } else {
                4
            };
            return (None, pos + skip);
        }
        let cp = ((u32::from(c) & 0x07) << 18)
            | ((u32::from(b(1)) & 0x3f) << 12)
            | ((u32::from(b(2)) & 0x3f) << 6)
            | (u32::from(b(3)) & 0x3f);
        if !(0x10000..=0x0010_ffff).contains(&cp) {
            return (None, pos + 4);
        }
        return (Some(cp), pos + 4);
    }
    (None, pos + 1)
}

#[cfg(test)]
mod tests {
    use super::Charset;

    #[test]
    fn a_name_is_looked_up_ignoring_case() {
        assert_eq!(Charset::named(b"windows-1251"), Some(Charset::Cp1251));
        assert_eq!(Charset::named(b"UTF-8"), Some(Charset::Utf8));
        assert_eq!(Charset::named(b"utf8"), None);
    }

    #[test]
    fn utf_8_leaves_a_lead_byte_for_the_next_read() {
        // `\xF2` leads a four-byte sequence `\xE5` cannot continue, and
        // `\xE5` is a lead of its own with nothing after it.
        assert_eq!(Charset::Utf8.next_char(b"\xF2\xE5", 0), (None, 1));
        assert_eq!(Charset::Utf8.next_char(b"\xF2\xE5", 1), (None, 2));
        assert_eq!(Charset::Utf8.next_char("é".as_bytes(), 0), (Some(0xE9), 2));
    }

    #[test]
    fn a_single_byte_charset_reads_every_byte() {
        assert_eq!(Charset::Cp1251.next_char(b"\xF2", 0), (Some(0xF2), 1));
    }

    #[test]
    fn shift_jis_reads_a_lead_and_its_trail_as_one_character() {
        assert_eq!(Charset::Sjis.next_char(b"\x82\xA0", 0), (Some(0x82A0), 2));
        assert_eq!(Charset::Sjis.next_char(b"\x82", 0), (None, 1));
    }
}
