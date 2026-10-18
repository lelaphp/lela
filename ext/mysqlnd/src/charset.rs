//! `mysqlnd_charset.c`: the character sets mysqlnd knows by number and by
//! name, each with php's multibyte rules, and the two escapes
//! `real_escape_string` reaches through them. The table is php's, row for
//! row and in its order, so a lookup by name answers the row php answers
//! first (`latin1` is `latin1_swedish_ci`, number 8); the multibyte checks
//! are php's byte for byte, and so is the escape loop, down to how a lead
//! byte is widened before php's `mb_charlen` reads it.

/// `MYSQLND_UTF8_MB3_DEFAULT_ID`.
pub(crate) const UTF8_MB3_DEFAULT_ID: u32 = 33;
/// `MYSQLND_UTF8_MB4_DEFAULT_ID`.
pub(crate) const UTF8_MB4_DEFAULT_ID: u32 = 45;

/// `enum mysqlnd_encoding_charlen` and `enum mysqlnd_encoding_valid`, one
/// variant per encoding: every row of php's table pairs a `mysqlnd_mbcharlen_*`
/// with the `check_mb_*` of the same encoding, so the pair is one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mb {
    /// `mysqlnd_mbcharlen_null`, `check_null`: a single-byte set, whose
    /// `lowest_mb_byte` of `0x100` no byte reaches.
    Null,
    Big5,
    Ujis,
    Sjis,
    Euckr,
    Gb2312,
    Gbk,
    /// `mysqlnd_mbcharlen_utf8mb3`, `check_mb_utf8mb3_valid`.
    Utf8mb3,
    Ucs2,
    Eucjpms,
    /// `mysqlnd_mbcharlen_utf8`, `check_mb_utf8_valid`: four bytes at most.
    Utf8,
    Utf16,
    Utf32,
    Cp932,
    /// `mysqlnd_mbcharlen_gb18030`, `my_ismbchar_gb18030`.
    Gb18030,
}

/// The byte at `i`, or the NUL php's `zend_string` ends with where a check
/// reads one past what it was given (`check_mb_ujis` reads `start[1]`
/// unbounded).
fn at(bytes: &[u8], i: usize) -> u8 {
    bytes.get(i).copied().unwrap_or(0)
}

fn is_cont(b: u8) -> bool {
    (b ^ 0x80) < 0x40
}

/// `check_mb_utf8mb3_sequence` (`four` false) and `check_mb_utf8_sequence`
/// (`four` true): the length of the sequence at the start, `0` for none.
fn utf8_sequence(s: &[u8], four: bool) -> u32 {
    let Some(&c) = s.first() else {
        return 0;
    };
    let (b1, b2, b3) = (at(s, 1), at(s, 2), at(s, 3));
    if c < 0x80 {
        1
    } else if c < 0xC2 {
        0
    } else if c < 0xE0 {
        if s.len() < 2 || !is_cont(b1) { 0 } else { 2 }
    } else if c < 0xF0 {
        if s.len() < 3 || !(is_cont(b1) && is_cont(b2) && (c >= 0xE1 || b1 >= 0xA0)) {
            0
        } else {
            3
        }
    } else if four && c < 0xF5 {
        let valid = is_cont(b1)
            && is_cont(b2)
            && is_cont(b3)
            && (c >= 0xF1 || b1 >= 0x90)
            && (c <= 0xF3 || b1 <= 0x8F);
        if s.len() < 4 || !valid { 0 } else { 4 }
    } else {
        0
    }
}

fn big5_head(c: u8) -> bool {
    (0xA1..=0xF9).contains(&c)
}
fn big5_tail(c: u8) -> bool {
    (0x40..=0x7E).contains(&c) || (0xA1..=0xFE).contains(&c)
}
/// `valid_cp932head` and `valid_sjis_head`, which are one rule.
fn sjis_head(c: u8) -> bool {
    (0x81..=0x9F).contains(&c) || (0xE0..=0xFC).contains(&c)
}
/// `valid_cp932tail` and `valid_sjis_tail`.
fn sjis_tail(c: u8) -> bool {
    (0x40..=0x7E).contains(&c) || (0x80..=0xFC).contains(&c)
}
/// `valid_euckr`, `valid_ujis`, `valid_eucjpms`, `valid_gb2312_tail`.
fn a1_fe(c: u8) -> bool {
    (0xA1..=0xFE).contains(&c)
}
/// `valid_ujis_kata`, `valid_eucjpms_kata`.
fn kata(c: u8) -> bool {
    (0xA1..=0xDF).contains(&c)
}
fn gb2312_head(c: u8) -> bool {
    (0xA1..=0xF7).contains(&c)
}
/// `valid_gbk_head` and `is_gb18030_odd`.
fn gbk_head(c: u8) -> bool {
    (0x81..=0xFE).contains(&c)
}
/// `valid_gbk_tail` and `is_gb18030_even_2`.
fn gbk_tail(c: u8) -> bool {
    (0x40..=0x7E).contains(&c) || (0x80..=0xFE).contains(&c)
}
fn gb18030_even_4(c: u8) -> bool {
    (0x30..=0x39).contains(&c)
}
fn utf16_high(c: u8) -> bool {
    c & 0xFC == 0xD8
}
fn utf16_low(c: u8) -> bool {
    c & 0xFC == 0xDC
}

impl Mb {
    /// `mysqlnd_mbcharlen_dispatch`: how long php takes a character whose
    /// lead is `c` to be. php hands it a `char` widened to `unsigned int`,
    /// so a lead byte of `0x80` and above arrives sign-extended
    /// ([`widened`]); the encodings that mask it read the byte, the UTF-8
    /// and GB18030 ones read the wide value and answer `0`.
    pub(crate) fn charlen(self, c: u32) -> u32 {
        // The low byte, `(zend_uchar) c` and `c & 0xFF` in php's macros.
        #[allow(clippy::cast_possible_truncation)]
        let b = c as u8;
        match self {
            Mb::Null => 0,
            Mb::Big5 => 1 + u32::from(big5_head(b)),
            Mb::Sjis | Mb::Cp932 => 1 + u32::from(sjis_head(b)),
            Mb::Euckr => 1 + u32::from(a1_fe(b)),
            Mb::Gb2312 => 1 + u32::from(gb2312_head(b)),
            Mb::Gbk => 1 + u32::from(gbk_head(b)),
            Mb::Ucs2 => 2,
            Mb::Utf32 => 4,
            Mb::Utf16 => {
                if utf16_high(b) {
                    4
                } else {
                    2
                }
            }
            Mb::Ujis | Mb::Eucjpms => {
                if a1_fe(b) || b == 0x8E {
                    2
                } else if b == 0x8F {
                    3
                } else {
                    1
                }
            }
            Mb::Utf8mb3 | Mb::Utf8 => match c {
                0..=0x7F => 1,
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 if self == Mb::Utf8 => 4,
                // `0x80..=0xC1` is no lead byte, nor is anything wider.
                _ => 0,
            },
            Mb::Gb18030 => {
                if c <= 0xFF {
                    u32::from(!gbk_head(b))
                } else if c > 0xFFFF {
                    0
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    let lead = (c >> 8) as u8;
                    if !gbk_head(lead) {
                        0
                    } else if gbk_tail(b) {
                        2
                    } else if gb18030_even_4(b) {
                        4
                    } else {
                        0
                    }
                }
            }
        }
    }

    /// `mysqlnd_mbvalid_dispatch`: the length of the valid multibyte
    /// character at the start of `s` (php's `start` to `end`), `0` where
    /// there is none. `ucs2` and `utf32` answer their width whatever is left,
    /// as php's do.
    pub(crate) fn valid(self, s: &[u8]) -> u32 {
        let Some(&c) = s.first() else {
            return 0;
        };
        let (b1, b2, b3) = (at(s, 1), at(s, 2), at(s, 3));
        let n = s.len();
        match self {
            Mb::Null => 0,
            Mb::Big5 => 2 * u32::from(big5_head(c) && n > 1 && big5_tail(b1)),
            Mb::Sjis | Mb::Cp932 => 2 * u32::from(sjis_head(c) && n > 1 && sjis_tail(b1)),
            Mb::Euckr => 2 * u32::from(n > 1 && c >= 0x80 && a1_fe(b1)),
            Mb::Gb2312 => 2 * u32::from(gb2312_head(c) && n > 1 && a1_fe(b1)),
            Mb::Gbk => 2 * u32::from(gbk_head(c) && n > 1 && gbk_tail(b1)),
            Mb::Ucs2 => 2,
            Mb::Utf32 => 4,
            Mb::Utf8mb3 | Mb::Utf8 => match utf8_sequence(s, self == Mb::Utf8) {
                len if len > 1 => len,
                _ => 0,
            },
            Mb::Ujis => {
                if c < 0x80 {
                    0
                } else if (a1_fe(c) && a1_fe(b1)) || (c == 0x8E && kata(b1)) {
                    2
                } else if c == 0x8F && n > 2 && a1_fe(b1) && a1_fe(b2) {
                    3
                } else {
                    0
                }
            }
            Mb::Eucjpms => {
                if c < 0x80 {
                    0
                } else if (a1_fe(c) && n > 1 && a1_fe(b1)) || (c == 0x8E && n > 1 && kata(b1)) {
                    2
                } else if c == 0x8F && n > 2 && a1_fe(b1) && a1_fe(b2) {
                    3
                } else {
                    0
                }
            }
            Mb::Utf16 => {
                if n < 2 || utf16_low(c) {
                    0
                } else if utf16_high(c) {
                    4 * u32::from(n >= 4 && utf16_low(b2))
                } else {
                    2
                }
            }
            Mb::Gb18030 => {
                if n <= 1 || !gbk_head(c) {
                    0
                } else if gbk_tail(b1) {
                    2
                } else if n > 3 && gb18030_even_4(b1) && gbk_head(b2) && gb18030_even_4(b3) {
                    4
                } else {
                    0
                }
            }
        }
    }
}

/// A byte as php's `char` reaches an `unsigned int` parameter: signed, then
/// widened, so `0xC3` is `0xFFFF_FFC3`.
fn widened(b: u8) -> u32 {
    // php's `char` is signed on the platforms it is built for.
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    let wide = i32::from(b as i8) as u32;
    wide
}

/// `MYSQLND_CHARSET`: one row of php's table.
#[derive(Debug)]
pub(crate) struct Charset {
    pub(crate) nr: u32,
    pub(crate) name: &'static str,
    pub(crate) collation: &'static str,
    pub(crate) char_minlen: u32,
    pub(crate) char_maxlen: u32,
    pub(crate) comment: &'static str,
    /// `mb_charlen` and `mb_valid`.
    pub(crate) mb: Mb,
    /// `lowest_mb_byte`: the first byte that may open a multibyte
    /// character, `0x100` for a single-byte set.
    pub(crate) lowest_mb_byte: u32,
}

/// One row of php's table, for the table below.
const fn row(
    nr: u32,
    name: &'static str,
    collation: &'static str,
    len: (u32, u32),
    comment: &'static str,
    mb: Mb,
) -> Charset {
    let lowest_mb_byte = match mb {
        Mb::Null => 0x100,
        Mb::Big5 | Mb::Gb2312 => 0xA1,
        Mb::Ucs2 | Mb::Utf16 | Mb::Utf32 | Mb::Gb18030 => 0x00,
        _ => 0x80,
    };
    Charset {
        nr,
        name,
        collation,
        char_minlen: len.0,
        char_maxlen: len.1,
        comment,
        mb,
        lowest_mb_byte,
    }
}

/// A single-byte row.
const fn single(nr: u32, name: &'static str, collation: &'static str) -> Charset {
    row(nr, name, collation, (1, 1), "", Mb::Null)
}

const UTF8: &str = "UTF-8 Unicode";
const UTF32: &str = "UTF-32 Unicode";

/// `mysqlnd_charsets`, in php's order, its terminating row left out.
// One row per line, as php writes its table.
#[rustfmt::skip]
static CHARSETS: [Charset; 246] = [
    row(1, "big5", "big5_chinese_ci", (1, 2), "", Mb::Big5),
    single(3, "dec8", "dec8_swedish_ci"),
    single(4, "cp850", "cp850_general_ci"),
    single(6, "hp8", "hp8_english_ci"),
    single(7, "koi8r", "koi8r_general_ci"),
    single(8, "latin1", "latin1_swedish_ci"),
    single(5, "latin1", "latin1_german1_ci"),
    single(9, "latin2", "latin2_general_ci"),
    single(2, "latin2", "latin2_czech_cs"),
    single(10, "swe7", "swe7_swedish_ci"),
    single(11, "ascii", "ascii_general_ci"),
    row(12, "ujis", "ujis_japanese_ci", (1, 3), "", Mb::Ujis),
    row(13, "sjis", "sjis_japanese_ci", (1, 2), "", Mb::Sjis),
    single(16, "hebrew", "hebrew_general_ci"),
    row(17, "filename", "filename", (1, 5), "", Mb::Null),
    single(18, "tis620", "tis620_thai_ci"),
    row(19, "euckr", "euckr_korean_ci", (1, 2), "", Mb::Euckr),
    single(21, "latin2", "latin2_hungarian_ci"),
    single(27, "latin2", "latin2_croatian_ci"),
    single(22, "koi8u", "koi8u_general_ci"),
    row(24, "gb2312", "gb2312_chinese_ci", (1, 2), "", Mb::Gb2312),
    single(25, "greek", "greek_general_ci"),
    single(26, "cp1250", "cp1250_general_ci"),
    row(28, "gbk", "gbk_chinese_ci", (1, 2), "", Mb::Gbk),
    single(30, "latin5", "latin5_turkish_ci"),
    single(31, "latin1", "latin1_german2_ci"),
    single(15, "latin1", "latin1_danish_ci"),
    single(32, "armscii8", "armscii8_general_ci"),
    row(33, "utf8", "utf8_general_ci", (1, 3), UTF8, Mb::Utf8mb3),
    row(35, "ucs2", "ucs2_general_ci", (2, 2), "UCS-2 Unicode", Mb::Ucs2),
    single(36, "cp866", "cp866_general_ci"),
    single(37, "keybcs2", "keybcs2_general_ci"),
    single(38, "macce", "macce_general_ci"),
    single(39, "macroman", "macroman_general_ci"),
    single(40, "cp852", "cp852_general_ci"),
    single(41, "latin7", "latin7_general_ci"),
    single(20, "latin7", "latin7_estonian_cs"),
    single(57, "cp1256", "cp1256_general_ci"),
    single(59, "cp1257", "cp1257_general_ci"),
    single(63, "binary", "binary"),
    row(97, "eucjpms", "eucjpms_japanese_ci", (1, 3), "", Mb::Eucjpms),
    single(29, "cp1257", "cp1257_lithuanian_ci"),
    single(31, "latin1", "latin1_german2_ci"),
    single(34, "cp1250", "cp1250_czech_cs"),
    single(42, "latin7", "latin7_general_cs"),
    single(43, "macce", "macce_bin"),
    single(44, "cp1250", "cp1250_croatian_ci"),
    row(45, "utf8mb4", "utf8mb4_general_ci", (1, 4), UTF8, Mb::Utf8),
    row(46, "utf8mb4", "utf8mb4_bin", (1, 4), UTF8, Mb::Utf8),
    single(47, "latin1", "latin1_bin"),
    single(48, "latin1", "latin1_general_ci"),
    single(49, "latin1", "latin1_general_cs"),
    single(51, "cp1251", "cp1251_general_ci"),
    single(14, "cp1251", "cp1251_bulgarian_ci"),
    single(23, "cp1251", "cp1251_ukrainian_ci"),
    single(50, "cp1251", "cp1251_bin"),
    single(52, "cp1251", "cp1251_general_cs"),
    single(53, "macroman", "macroman_bin"),
    row(54, "utf16", "utf16_general_ci", (2, 4), "UTF-16 Unicode", Mb::Utf16),
    row(55, "utf16", "utf16_bin", (2, 4), "UTF-16 Unicode", Mb::Utf16),
    row(56, "utf16le", "utf16le_general_ci", (2, 4), "UTF-16LE Unicode", Mb::Utf16),
    single(58, "cp1257", "cp1257_bin"),
    row(60, "utf32", "utf32_general_ci", (4, 4), UTF32, Mb::Utf32),
    row(61, "utf32", "utf32_bin", (4, 4), UTF32, Mb::Utf32),
    row(62, "utf16le", "utf16le_bin", (2, 4), "UTF-16LE Unicode", Mb::Utf16),
    single(64, "armscii8", "armscii8_bin"),
    single(65, "ascii", "ascii_bin"),
    single(66, "cp1250", "cp1250_bin"),
    single(67, "cp1256", "cp1256_bin"),
    single(68, "cp866", "cp866_bin"),
    single(69, "dec8", "dec8_bin"),
    single(70, "greek", "greek_bin"),
    single(71, "hebrew", "hebrew_bin"),
    single(72, "hp8", "hp8_bin"),
    single(73, "keybcs2", "keybcs2_bin"),
    single(74, "koi8r", "koi8r_bin"),
    single(75, "koi8u", "koi8u_bin"),
    single(77, "latin2", "latin2_bin"),
    single(78, "latin5", "latin5_bin"),
    single(79, "latin7", "latin7_bin"),
    single(80, "cp850", "cp850_bin"),
    single(81, "cp852", "cp852_bin"),
    single(82, "swe7", "swe7_bin"),
    row(83, "utf8", "utf8_bin", (1, 3), UTF8, Mb::Utf8mb3),
    row(84, "big5", "big5_bin", (1, 2), "", Mb::Big5),
    row(85, "euckr", "euckr_bin", (1, 2), "", Mb::Euckr),
    row(86, "gb2312", "gb2312_bin", (1, 2), "", Mb::Gb2312),
    row(87, "gbk", "gbk_bin", (1, 2), "", Mb::Gbk),
    row(88, "sjis", "sjis_bin", (1, 2), "", Mb::Sjis),
    single(89, "tis620", "tis620_bin"),
    row(90, "ucs2", "ucs2_bin", (2, 2), "UCS-2 Unicode", Mb::Ucs2),
    row(91, "ujis", "ujis_bin", (1, 3), "", Mb::Ujis),
    single(92, "geostd8", "geostd8_general_ci"),
    single(93, "geostd8", "geostd8_bin"),
    single(94, "latin1", "latin1_spanish_ci"),
    row(95, "cp932", "cp932_japanese_ci", (1, 2), "", Mb::Cp932),
    row(96, "cp932", "cp932_bin", (1, 2), "", Mb::Cp932),
    row(97, "eucjpms", "eucjpms_japanese_ci", (1, 3), "", Mb::Eucjpms),
    row(98, "eucjpms", "eucjpms_bin", (1, 3), "", Mb::Eucjpms),
    single(99, "cp1250", "cp1250_polish_ci"),
    row(128, "ucs2", "ucs2_unicode_ci", (2, 2), "", Mb::Ucs2),
    row(129, "ucs2", "ucs2_icelandic_ci", (2, 2), "", Mb::Ucs2),
    row(130, "ucs2", "ucs2_latvian_ci", (2, 2), "", Mb::Ucs2),
    row(131, "ucs2", "ucs2_romanian_ci", (2, 2), "", Mb::Ucs2),
    row(132, "ucs2", "ucs2_slovenian_ci", (2, 2), "", Mb::Ucs2),
    row(133, "ucs2", "ucs2_polish_ci", (2, 2), "", Mb::Ucs2),
    row(134, "ucs2", "ucs2_estonian_ci", (2, 2), "", Mb::Ucs2),
    row(135, "ucs2", "ucs2_spanish_ci", (2, 2), "", Mb::Ucs2),
    row(136, "ucs2", "ucs2_swedish_ci", (2, 2), "", Mb::Ucs2),
    row(137, "ucs2", "ucs2_turkish_ci", (2, 2), "", Mb::Ucs2),
    row(138, "ucs2", "ucs2_czech_ci", (2, 2), "", Mb::Ucs2),
    row(139, "ucs2", "ucs2_danish_ci", (2, 2), "", Mb::Ucs2),
    row(140, "ucs2", "ucs2_lithuanian_ci", (2, 2), "", Mb::Ucs2),
    row(141, "ucs2", "ucs2_slovak_ci", (2, 2), "", Mb::Ucs2),
    row(142, "ucs2", "ucs2_spanish2_ci", (2, 2), "", Mb::Ucs2),
    row(143, "ucs2", "ucs2_roman_ci", (2, 2), "", Mb::Ucs2),
    row(144, "ucs2", "ucs2_persian_ci", (2, 2), "", Mb::Ucs2),
    row(145, "ucs2", "ucs2_esperanto_ci", (2, 2), "", Mb::Ucs2),
    row(146, "ucs2", "ucs2_hungarian_ci", (2, 2), "", Mb::Ucs2),
    row(147, "ucs2", "ucs2_sinhala_ci", (2, 2), "", Mb::Ucs2),
    row(148, "ucs2", "ucs2_german2_ci", (2, 2), "", Mb::Ucs2),
    row(149, "ucs2", "ucs2_croatian_ci", (2, 2), "", Mb::Ucs2),
    row(150, "ucs2", "ucs2_unicode_520_ci", (2, 2), "", Mb::Ucs2),
    row(151, "ucs2", "ucs2_vietnamese_ci", (2, 2), "", Mb::Ucs2),
    row(160, "utf32", "utf32_unicode_ci", (4, 4), UTF32, Mb::Utf32),
    row(161, "utf32", "utf32_icelandic_ci", (4, 4), UTF32, Mb::Utf32),
    row(162, "utf32", "utf32_latvian_ci", (4, 4), UTF32, Mb::Utf32),
    row(163, "utf32", "utf32_romanian_ci", (4, 4), UTF32, Mb::Utf32),
    row(164, "utf32", "utf32_slovenian_ci", (4, 4), UTF32, Mb::Utf32),
    row(165, "utf32", "utf32_polish_ci", (4, 4), UTF32, Mb::Utf32),
    row(166, "utf32", "utf32_estonian_ci", (4, 4), UTF32, Mb::Utf32),
    row(167, "utf32", "utf32_spanish_ci", (4, 4), UTF32, Mb::Utf32),
    row(168, "utf32", "utf32_swedish_ci", (4, 4), UTF32, Mb::Utf32),
    row(169, "utf32", "utf32_turkish_ci", (4, 4), UTF32, Mb::Utf32),
    row(170, "utf32", "utf32_czech_ci", (4, 4), UTF32, Mb::Utf32),
    row(171, "utf32", "utf32_danish_ci", (4, 4), UTF32, Mb::Utf32),
    row(172, "utf32", "utf32_lithuanian_ci", (4, 4), UTF32, Mb::Utf32),
    row(173, "utf32", "utf32_slovak_ci", (4, 4), UTF32, Mb::Utf32),
    row(174, "utf32", "utf32_spanish2_ci", (4, 4), UTF32, Mb::Utf32),
    row(175, "utf32", "utf32_roman_ci", (4, 4), UTF32, Mb::Utf32),
    row(176, "utf32", "utf32_persian_ci", (4, 4), UTF32, Mb::Utf32),
    row(177, "utf32", "utf32_esperanto_ci", (4, 4), UTF32, Mb::Utf32),
    row(178, "utf32", "utf32_hungarian_ci", (4, 4), UTF32, Mb::Utf32),
    row(179, "utf32", "utf32_sinhala_ci", (4, 4), UTF32, Mb::Utf32),
    row(180, "utf32", "utf32_german2_ci", (4, 4), UTF32, Mb::Utf32),
    row(181, "utf32", "utf32_croatian_ci", (4, 4), UTF32, Mb::Utf32),
    row(182, "utf32", "utf32_unicode_520_ci", (4, 4), UTF32, Mb::Utf32),
    row(183, "utf32", "utf32_vietnamese_ci", (4, 4), UTF32, Mb::Utf32),
    row(192, "utf8", "utf8_unicode_ci", (1, 3), "", Mb::Utf8mb3),
    row(193, "utf8", "utf8_icelandic_ci", (1, 3), "", Mb::Utf8mb3),
    row(194, "utf8", "utf8_latvian_ci", (1, 3), "", Mb::Utf8mb3),
    row(195, "utf8", "utf8_romanian_ci", (1, 3), "", Mb::Utf8mb3),
    row(196, "utf8", "utf8_slovenian_ci", (1, 3), "", Mb::Utf8mb3),
    row(197, "utf8", "utf8_polish_ci", (1, 3), "", Mb::Utf8mb3),
    row(198, "utf8", "utf8_estonian_ci", (1, 3), "", Mb::Utf8mb3),
    row(199, "utf8", "utf8_spanish_ci", (1, 3), "", Mb::Utf8mb3),
    row(200, "utf8", "utf8_swedish_ci", (1, 3), "", Mb::Utf8mb3),
    row(201, "utf8", "utf8_turkish_ci", (1, 3), "", Mb::Utf8mb3),
    row(202, "utf8", "utf8_czech_ci", (1, 3), "", Mb::Utf8mb3),
    row(203, "utf8", "utf8_danish_ci", (1, 3), "", Mb::Utf8mb3),
    row(204, "utf8", "utf8_lithuanian_ci", (1, 3), "", Mb::Utf8mb3),
    row(205, "utf8", "utf8_slovak_ci", (1, 3), "", Mb::Utf8mb3),
    row(206, "utf8", "utf8_spanish2_ci", (1, 3), "", Mb::Utf8mb3),
    row(207, "utf8", "utf8_roman_ci", (1, 3), "", Mb::Utf8mb3),
    row(208, "utf8", "utf8_persian_ci", (1, 3), "", Mb::Utf8mb3),
    row(209, "utf8", "utf8_esperanto_ci", (1, 3), "", Mb::Utf8mb3),
    row(210, "utf8", "utf8_hungarian_ci", (1, 3), "", Mb::Utf8mb3),
    row(211, "utf8", "utf8_sinhala_ci", (1, 3), "", Mb::Utf8mb3),
    row(212, "utf8", "utf8_german2_ci", (1, 3), "", Mb::Utf8mb3),
    row(213, "utf8", "utf8_croatian_ci", (1, 3), "", Mb::Utf8mb3),
    row(214, "utf8", "utf8_unicode_520_ci", (1, 3), "", Mb::Utf8mb3),
    row(215, "utf8", "utf8_vietnamese_ci", (1, 3), "", Mb::Utf8mb3),
    row(224, "utf8mb4", "utf8mb4_unicode_ci", (1, 4), "", Mb::Utf8),
    row(225, "utf8mb4", "utf8mb4_icelandic_ci", (1, 4), "", Mb::Utf8),
    row(226, "utf8mb4", "utf8mb4_latvian_ci", (1, 4), "", Mb::Utf8),
    row(227, "utf8mb4", "utf8mb4_romanian_ci", (1, 4), "", Mb::Utf8),
    row(228, "utf8mb4", "utf8mb4_slovenian_ci", (1, 4), "", Mb::Utf8),
    row(229, "utf8mb4", "utf8mb4_polish_ci", (1, 4), "", Mb::Utf8),
    row(230, "utf8mb4", "utf8mb4_estonian_ci", (1, 4), "", Mb::Utf8),
    row(231, "utf8mb4", "utf8mb4_spanish_ci", (1, 4), "", Mb::Utf8),
    row(232, "utf8mb4", "utf8mb4_swedish_ci", (1, 4), "", Mb::Utf8),
    row(233, "utf8mb4", "utf8mb4_turkish_ci", (1, 4), "", Mb::Utf8),
    row(234, "utf8mb4", "utf8mb4_czech_ci", (1, 4), "", Mb::Utf8),
    row(235, "utf8mb4", "utf8mb4_danish_ci", (1, 4), "", Mb::Utf8),
    row(236, "utf8mb4", "utf8mb4_lithuanian_ci", (1, 4), "", Mb::Utf8),
    row(237, "utf8mb4", "utf8mb4_slovak_ci", (1, 4), "", Mb::Utf8),
    row(238, "utf8mb4", "utf8mb4_spanish2_ci", (1, 4), "", Mb::Utf8),
    row(239, "utf8mb4", "utf8mb4_roman_ci", (1, 4), "", Mb::Utf8),
    row(240, "utf8mb4", "utf8mb4_persian_ci", (1, 4), "", Mb::Utf8),
    row(241, "utf8mb4", "utf8mb4_esperanto_ci", (1, 4), "", Mb::Utf8),
    row(242, "utf8mb4", "utf8mb4_hungarian_ci", (1, 4), "", Mb::Utf8),
    row(243, "utf8mb4", "utf8mb4_sinhala_ci", (1, 4), "", Mb::Utf8),
    row(244, "utf8mb4", "utf8mb4_german2_ci", (1, 4), "", Mb::Utf8),
    row(245, "utf8mb4", "utf8mb4_croatian_ci", (1, 4), "", Mb::Utf8),
    row(246, "utf8mb4", "utf8mb4_unicode_520_ci", (1, 4), "", Mb::Utf8),
    row(247, "utf8mb4", "utf8mb4_vietnamese_ci", (1, 4), "", Mb::Utf8),
    row(248, "gb18030", "gb18030_chinese_ci", (1, 4), "", Mb::Gb18030),
    row(249, "gb18030", "gb18030_bin", (1, 4), "", Mb::Gb18030),
    // php gives the `utf8` row 254 the four-byte rules of `utf8mb4`.
    row(254, "utf8", "utf8_general_cs", (1, 3), "", Mb::Utf8),
    row(255, "utf8mb4", "utf8mb4_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(256, "utf8mb4", "utf8mb4_de_pb_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(257, "utf8mb4", "utf8mb4_is_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(258, "utf8mb4", "utf8mb4_lv_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(259, "utf8mb4", "utf8mb4_ro_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(260, "utf8mb4", "utf8mb4_sl_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(261, "utf8mb4", "utf8mb4_pl_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(262, "utf8mb4", "utf8mb4_et_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(263, "utf8mb4", "utf8mb4_es_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(264, "utf8mb4", "utf8mb4_sv_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(265, "utf8mb4", "utf8mb4_tr_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(266, "utf8mb4", "utf8mb4_cs_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(267, "utf8mb4", "utf8mb4_da_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(268, "utf8mb4", "utf8mb4_lt_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(269, "utf8mb4", "utf8mb4_sk_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(270, "utf8mb4", "utf8mb4_es_trad_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(271, "utf8mb4", "utf8mb4_la_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(272, "utf8mb4", "utf8mb4_fa_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(273, "utf8mb4", "utf8mb4_eo_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(274, "utf8mb4", "utf8mb4_hu_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(275, "utf8mb4", "utf8mb4_hr_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(276, "utf8mb4", "utf8mb4_si_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(277, "utf8mb4", "utf8mb4_vi_0900_ai_ci", (1, 4), "", Mb::Utf8),
    row(278, "utf8mb4", "utf8mb4_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(279, "utf8mb4", "utf8mb4_de_pb_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(280, "utf8mb4", "utf8mb4_is_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(281, "utf8mb4", "utf8mb4_lv_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(282, "utf8mb4", "utf8mb4_ro_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(283, "utf8mb4", "utf8mb4_sl_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(284, "utf8mb4", "utf8mb4_pl_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(285, "utf8mb4", "utf8mb4_et_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(286, "utf8mb4", "utf8mb4_es_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(287, "utf8mb4", "utf8mb4_sv_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(288, "utf8mb4", "utf8mb4_tr_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(289, "utf8mb4", "utf8mb4_cs_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(290, "utf8mb4", "utf8mb4_da_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(291, "utf8mb4", "utf8mb4_lt_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(292, "utf8mb4", "utf8mb4_sk_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(293, "utf8mb4", "utf8mb4_es_trad_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(294, "utf8mb4", "utf8mb4_la_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(295, "utf8mb4", "utf8mb4_fa_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(296, "utf8mb4", "utf8mb4_eo_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(297, "utf8mb4", "utf8mb4_hu_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(298, "utf8mb4", "utf8mb4_hr_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(299, "utf8mb4", "utf8mb4_si_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(300, "utf8mb4", "utf8mb4_vi_0900_as_cs", (1, 4), "", Mb::Utf8),
    row(303, "utf8mb4", "utf8mb4_ja_0900_as_cs", (1, 4), "", Mb::Utf8),
];

/// `mysqlnd_find_charset_nr`: the first row of the number, none for `0`
/// and for a number php's table has no row for.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// var_dump($db->query('SELECT @@character_set_client, @@collation_connection')->fetch(PDO::FETCH_NUM));
/// ```
/// ```expect mysql
/// array(2) {
///   [0]=>
///   string(7) "utf8mb4"
///   [1]=>
///   string(18) "utf8mb4_0900_ai_ci"
/// }
/// ```
pub(crate) fn by_nr(nr: u32) -> Option<&'static Charset> {
    CHARSETS.iter().find(|c| c.nr == nr)
}

/// `mysqlnd_find_charset_name`: the first row whose name equals `name`
/// as `strcasecmp` compares them, `name` read up to its first NUL as php's
/// C string ends there.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test;charset=LATIN1', 'root', '');
/// var_dump($db->query('SELECT @@character_set_client')->fetchColumn());
/// try { new PDO('mysql:host=127.0.0.1;dbname=test;charset=klingon', 'root', ''); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// string(6) "latin1"
/// SQLSTATE[HY000] [2019] Unknown character set
/// ```
pub(crate) fn by_name(name: &[u8]) -> Option<&'static Charset> {
    let name = php_ext::coerce::c_str(name);
    CHARSETS
        .iter()
        .find(|c| c.name.as_bytes().eq_ignore_ascii_case(name))
}

/// The valid multibyte character at `rest`'s start under `cset`: its
/// length, or what is left of `rest` where that is shorter.
fn multibyte(cset: &Charset, rest: &[u8]) -> usize {
    let first = rest.first().copied().map_or(0, u32::from);
    if first < cset.lowest_mb_byte {
        return 0;
    }
    (cset.mb.valid(rest) as usize).min(rest.len())
}

/// `mysqlnd_cset_escape_quotes`, the escape of a connection whose server
/// status has `NO_BACKSLASH_ESCAPES`: each `'` doubled, a valid multibyte
/// character copied whole, every other byte as it is.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// $db->exec("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'");
/// var_dump($db->quote("it's a\\b"), $db->query('SELECT ' . $db->quote("it's a\\b"))->fetchColumn());
/// ```
/// ```expect mysql
/// string(11) "'it''s a\b'"
/// string(8) "it's a\b"
/// ```
pub(crate) fn escape_quotes(cset: &Charset, from: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(from.len() * 2);
    let mut i = 0;
    while let Some(rest) = from.get(i..).filter(|r| !r.is_empty()) {
        let len = multibyte(cset, rest);
        if len > 0 {
            out.extend_from_slice(rest.get(..len).unwrap_or(rest));
            i += len;
            continue;
        }
        let b = rest.first().copied().unwrap_or(0);
        if b == b'\'' {
            out.extend_from_slice(b"''");
        } else {
            out.push(b);
        }
        i += 1;
    }
    out
}

/// `mysqlnd_cset_escape_slashes`, `real_escape_string`'s escape: a valid
/// multibyte character copied whole; a byte from the set's multibyte range
/// that opens no valid character, but whose `mb_charlen` is more than one,
/// escaped with a backslash before it; NUL, `\n`, `\r`, `\\`, `'`, `"` and
/// `\032` escaped as `\0`, `\n`, `\r`, `\\`, `\'`, `\"` and `\Z`.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// $s = "a'b\"c\\d\0e\nf\rg\x1Ah";
/// var_dump($db->quote($s), $db->query('SELECT ' . $db->quote($s))->fetchColumn() === $s);
/// ```
/// ```expect mysql
/// string(24) "'a\'b\"c\\d\0e\nf\rg\Zh'"
/// bool(true)
/// ```
pub(crate) fn escape_slashes(cset: &Charset, from: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(from.len() * 2);
    let mut i = 0;
    while let Some(rest) = from.get(i..).filter(|r| !r.is_empty()) {
        let b = rest.first().copied().unwrap_or(0);
        let mut esc = 0u8;
        if u32::from(b) >= cset.lowest_mb_byte {
            let len = multibyte(cset, rest);
            if len > 0 {
                out.extend_from_slice(rest.get(..len).unwrap_or(rest));
                i += len;
                continue;
            } else if cset.mb.charlen(widened(b)) > 1 {
                esc = b;
            }
        }
        if esc == 0 {
            esc = match b {
                0 => b'0',
                b'\n' => b'n',
                b'\r' => b'r',
                b'\\' | b'\'' | b'"' => b,
                0x1A => b'Z',
                _ => 0,
            };
        }
        if esc == 0 {
            out.push(b);
        } else {
            out.extend_from_slice(&[b'\\', esc]);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    static ASCII: Charset = single(11, "ascii", "ascii_general_ci");

    fn named(name: &str) -> &'static Charset {
        by_name(name.as_bytes()).unwrap_or(&ASCII)
    }

    #[test]
    fn rows_are_found_by_number_and_by_name_as_php_finds_them() {
        assert_eq!(by_nr(8).map(|c| c.collation), Some("latin1_swedish_ci"));
        assert_eq!(
            by_nr(45).map(|c| (c.name, c.char_maxlen, c.comment)),
            Some(("utf8mb4", 4, "UTF-8 Unicode"))
        );
        assert_eq!(by_nr(97).map(|c| c.collation), Some("eucjpms_japanese_ci"));
        assert!(by_nr(0).is_none() && by_nr(302).is_none());
        assert_eq!(by_name(b"LATIN1").map(|c| c.nr), Some(8));
        assert_eq!(by_name(b"utf8").map(|c| c.nr), Some(UTF8_MB3_DEFAULT_ID));
        assert_eq!(
            by_name(b"utf8mb4\0x").map(|c| c.nr),
            Some(UTF8_MB4_DEFAULT_ID)
        );
        assert!(by_name(b"klingon").is_none());
        assert_eq!(by_nr(254).map(|c| c.mb), Some(Mb::Utf8));
        assert_eq!(by_nr(1).map(|c| c.lowest_mb_byte), Some(0xA1));
        assert_eq!(by_nr(11).map(|c| c.lowest_mb_byte), Some(0x100));
    }

    #[test]
    fn the_ascii_specials_are_escaped() {
        let latin1 = named("latin1");
        assert_eq!(
            escape_slashes(latin1, b"a\0b\n\r\\'\"\x1Az\xE9"),
            b"a\\0b\\n\\r\\\\\\'\\\"\\Zz\xE9".to_vec()
        );
        assert_eq!(
            escape_quotes(latin1, b"it's \\ \"x\""),
            b"it''s \\ \"x\"".to_vec()
        );
    }

    #[test]
    fn a_multibyte_tail_that_is_a_backslash_passes_through() {
        // GBK's U+7E8D and Shift-JIS's U+8868 end in 0x5C.
        assert_eq!(
            escape_slashes(named("gbk"), b"\xBF\x5C'"),
            b"\xBF\x5C\\'".to_vec()
        );
        assert_eq!(
            escape_slashes(named("sjis"), b"\x95\x5C"),
            b"\x95\x5C".to_vec()
        );
        // Under latin1 the same bytes are two characters, the second escaped.
        assert_eq!(
            escape_slashes(named("latin1"), b"\xBF\x5C"),
            b"\xBF\\\x5C".to_vec()
        );
        // A GBK lead with no tail is escaped itself, as its charlen is 2.
        assert_eq!(escape_slashes(named("gbk"), b"\xBF"), b"\\\xBF".to_vec());
    }

    #[test]
    fn utf8mb4_takes_four_byte_sequences_and_utf8_does_not() {
        let mb4 = named("utf8mb4");
        assert_eq!(mb4.mb.valid(b"\xF0\x9F\x98\x80"), 4);
        assert_eq!(mb4.mb.valid(b"\xF4\x90\x80\x80"), 0);
        assert_eq!(named("utf8").mb.valid(b"\xF0\x9F\x98\x80"), 0);
        assert_eq!(
            escape_slashes(mb4, b"\xF0\x9F\x98\x80'"),
            b"\xF0\x9F\x98\x80\\'".to_vec()
        );
        // A stray lead byte is widened as php's `char` is, so its charlen is
        // 0 and it is copied as it is.
        assert_eq!(escape_slashes(mb4, b"\xC3'"), b"\xC3\\'".to_vec());
    }

    #[test]
    fn the_fixed_width_sets_copy_their_width_and_no_further() {
        let ucs2 = named("ucs2");
        assert_eq!(escape_slashes(ucs2, b"\x00'"), b"\x00'".to_vec());
        assert_eq!(escape_slashes(ucs2, b"\x00'\x00"), b"\x00'\x00".to_vec());
        assert_eq!(named("utf16").mb.valid(b"\xD8\x00\xDC\x00"), 4);
        assert_eq!(named("gb18030").mb.valid(b"\x81\x30\x81\x30"), 4);
    }
}
