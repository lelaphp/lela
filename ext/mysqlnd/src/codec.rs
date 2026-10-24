//! `mysqlnd_ps_codec.c`'s fetch half and `php_mysqlnd_rowp_read_binary_protocol`:
//! a binary row's NULL bitmap and each column's packed value decoded into
//! the value php makes of it, and the `BIT` column both protocols read
//! with `ps_fetch_from_1_to_8_bytes`.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString, Value};

use crate::consts::{FieldType, NULL_LENGTH, column};
use crate::result::{Field, Row};
use crate::stats::{Stat, Stats};
use crate::wire::Body;

/// What `ps_fetch_from_1_to_8_bytes` makes: an integer, or the text php
/// writes where it would not fit or is zero-filled.
pub(crate) enum Bits {
    Long(i64),
    Text(String),
}

/// `ps_fetch_from_1_to_8_bytes` over `bytes`, `byte_count` of them: a
/// `BIT` column's big-endian bits, otherwise little-endian; an unsigned
/// value past `i64` or zero-filled as text.
fn from_1_to_8_bytes(field: &Field, bytes: &[u8]) -> Bits {
    let is_bit = field.field_type() == FieldType::Bit;
    let n = bytes.len();
    if field.flags & column::UNSIGNED != 0 {
        let uval = if is_bit || (5..=7).contains(&n) {
            // `bit_uint*korr`: big-endian.
            bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
        } else {
            bytes
                .iter()
                .rev()
                .fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
        };
        if field.flags & column::ZEROFILL != 0 {
            let width = usize::try_from(field.length).unwrap_or(0);
            return Bits::Text(format!("{uval:0width$}"));
        }
        return match i64::try_from(uval) {
            Ok(n) => Bits::Long(n),
            Err(_) => Bits::Text(uval.to_string()),
        };
    }
    let at = |i: usize| bytes.get(i).copied().unwrap_or(0);
    let lval = match n {
        8 => i64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])),
        4 => i64::from(i32::from_le_bytes([at(0), at(1), at(2), at(3)])),
        // `sint3korr`: the third byte's top bit carried into the fourth.
        3 => i64::from(i32::from_le_bytes([
            at(0),
            at(1),
            at(2),
            if at(2) & 0x80 != 0 { 0xFF } else { 0 },
        ])),
        2 => i64::from(i16::from_le_bytes([at(0), at(1)])),
        1 => i64::from(i8::from_le_bytes([at(0)])),
        _ => 0,
    };
    Bits::Long(lval)
}

/// A `BIT` column of the text protocol: its bytes as
/// `ps_fetch_from_1_to_8_bytes` reads them with no packed length.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// $db->exec('CREATE TEMPORARY TABLE bits (b BIT(10), c BIT(1))');
/// $db->exec("INSERT INTO bits VALUES (b'1000000001', b'1')");
/// var_dump($db->query('SELECT b, c FROM bits')->fetch(PDO::FETCH_NUM));
/// ```
/// ```expect mysql
/// array(2) {
///   [0]=>
///   int(513)
///   [1]=>
///   int(1)
/// }
/// ```
pub(crate) fn bit_value(field: &Field, bytes: &[u8]) -> Bits {
    from_1_to_8_bytes(field, bytes.get(..bytes.len().min(8)).unwrap_or_default())
}

/// `php_mysqlnd_rowp_read_binary_protocol`: the status byte, the NULL
/// bitmap with its two reserved bits, then each column by its type's
/// fetch function, the over-read warned and the row given up.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_EMULATE_PREPARES => false]);
/// $st = $db->prepare("SELECT 1, -2, 'a', NULL, 1.5, CAST(0.1 AS FLOAT), CAST('2026-10-01 12:34:56' AS DATETIME)");
/// $st->execute();
/// var_dump($st->fetch(PDO::FETCH_NUM));
/// ```
/// ```expect mysql
/// array(7) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(-2)
///   [2]=>
///   string(1) "a"
///   [3]=>
///   NULL
///   [4]=>
///   string(3) "1.5"
///   [5]=>
///   float(0.1)
///   [6]=>
///   string(19) "2026-10-01 12:34:56"
/// }
/// ```
pub(crate) fn decode_binary(
    req: &mut Request,
    stats: &mut Stats,
    bytes: &[u8],
    fields: &[Field],
    mut row: Row,
) -> Result<Result<Row, ()>, Thrown> {
    let count = fields.len();
    let nulls = bytes.get(1..1 + (count + 9) / 8).unwrap_or_default();
    let mut p = Body::new(bytes);
    p.skip(1 + (count + 9) / 8);
    for (i, field) in fields.iter().enumerate() {
        let bit = i + 2;
        let is_null = nulls
            .get(bit / 8)
            .is_some_and(|b| b & (1 << (bit % 8)) != 0);
        let start = p.position();
        let value = if is_null {
            stats.inc(Stat::BinaryTypeFetchedNull);
            Owner::null()
        } else {
            if start >= bytes.len() {
                docref(
                    req,
                    Level::Warning,
                    "Malformed server packet. No packet space left for the field",
                );
                return Ok(give_up(req, row));
            }
            let left = bytes.len() - start;
            let value = match fetch(req, field, &mut p, left) {
                Ok(Some(value)) => value,
                Ok(None) => return Ok(give_up(req, row)),
                Err(thrown) => {
                    release(req, row);
                    return Err(thrown);
                }
            };
            stats.inc(binary_stat(field.field_type()));
            value
        };
        let size = match value.as_ref() {
            php_ext::values::ValueRef::Str(s) => s.len(),
            _ => p.position() - start,
        };
        stats.add(Stat::BytesReceivedPureDataPs, size as u64);
        row.push(value);
    }
    Ok(Ok(row))
}

/// The values made before a column failed, given back.
fn give_up(req: &mut Request, row: Row) -> Result<Row, ()> {
    release(req, row);
    Err(())
}

/// A row's values given up, `zval_ptr_dtor` over each.
fn release(req: &mut Request, row: Row) {
    for value in row {
        value.give_up(req);
    }
}

/// A column's text as the string php makes of it, one owner.
fn text(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `ps_fetch_over_read_error`.
fn over_read(req: &mut Request) {
    docref(
        req,
        Level::Warning,
        "Malformed server packet. Field length pointing after the end of packet",
    );
}

/// `mysqlnd_ps_fetch_functions[type].func` for one column, with `left`
/// bytes of the packet past its start (php's `pack_len` argument). Each
/// kind reads its bytes where they stand in the row's packet:
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_EMULATE_PREPARES => false]);
/// $db->exec('CREATE TEMPORARY TABLE kinds (t TINYINT, s SMALLINT, y YEAR, m MEDIUMINT, b BIGINT UNSIGNED, d DOUBLE, tm TIME(3), dt DATE, bt BIT(12), v VARBINARY(8))');
/// $db->exec("INSERT INTO kinds VALUES (-7, -300, 2026, -70000, 18446744073709551615, 2.5, '-30:01:02.5', '2026-10-24', b'101000000001', 'ab\\0c'), (NULL, 1, NULL, 2, 3, NULL, '00:00:00', NULL, b'0', '')");
/// $st = $db->prepare('SELECT * FROM kinds');
/// $st->execute();
/// echo json_encode($st->fetchAll(PDO::FETCH_NUM)), "\n";
/// ```
/// ```expect mysql
/// [[-7,-300,"2026",-70000,"18446744073709551615",2.5,"-30:01:02.500","2026-10-24",2561,"ab\u0000c"],[null,1,null,2,3,null,"00:00:00.000",null,0,""]]
/// ```
fn fetch<'a>(
    req: &mut Request,
    field: &Field,
    p: &mut Body<'a>,
    left: usize,
) -> Result<Option<Owner<Value>>, Thrown> {
    let fixed = |req: &mut Request, p: &mut Body<'a>, n: usize| -> Option<&'a [u8]> {
        if n > left {
            over_read(req);
            return None;
        }
        Some(p.take(n))
    };
    let variable = |req: &mut Request, p: &mut Body<'a>| -> Option<&'a [u8]> {
        let before = p.position();
        let len = p.field_length();
        let length_len = p.position() - before;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len as u64 == NULL_LENGTH || length_len > left || len > left - length_len {
            over_read(req);
            return None;
        }
        Some(p.take(len))
    };
    let long = |bits: Bits, req: &mut Request| -> Result<Owner<Value>, Thrown> {
        match bits {
            Bits::Long(n) => Ok(Owner::int(n)),
            Bits::Text(s) => text(req, s.as_bytes()),
        }
    };
    let ty = field.field_type();
    Ok(Some(match ty {
        FieldType::Null => Owner::null(),
        FieldType::Tiny => match fixed(req, p, 1) {
            Some(b) => long(from_1_to_8_bytes(field, b), req)?,
            None => return Ok(None),
        },
        FieldType::Short | FieldType::Year => match fixed(req, p, 2) {
            Some(b) => long(from_1_to_8_bytes(field, b), req)?,
            None => return Ok(None),
        },
        FieldType::Int24 | FieldType::Long => match fixed(req, p, 4) {
            Some(b) => long(from_1_to_8_bytes(field, b), req)?,
            None => return Ok(None),
        },
        FieldType::LongLong => match fixed(req, p, 8) {
            Some(b) => long(from_1_to_8_bytes(field, b), req)?,
            None => return Ok(None),
        },
        FieldType::Float => match fixed(req, p, 4) {
            Some(b) => {
                let f = f32::from_le_bytes(b.try_into().unwrap_or([0; 4]));
                let decimals = if field.decimals >= 31 {
                    None
                } else {
                    Some(field.decimals as usize)
                };
                Owner::float(float_to_double(f, decimals))
            }
            None => return Ok(None),
        },
        FieldType::Double => match fixed(req, p, 8) {
            Some(b) => {
                let d = f64::from_le_bytes(b.try_into().unwrap_or([0; 8]));
                Owner::float(d)
            }
            None => return Ok(None),
        },
        FieldType::Time => match variable(req, p) {
            Some(b) => text(req, time(field, b).as_bytes())?,
            None => return Ok(None),
        },
        FieldType::Date => match variable(req, p) {
            Some(b) => text(req, date(b).as_bytes())?,
            None => return Ok(None),
        },
        FieldType::DateTime | FieldType::Timestamp => match variable(req, p) {
            Some(b) => text(req, datetime(field, b).as_bytes())?,
            None => return Ok(None),
        },
        // `ps_fetch_bit`: an over-read is warned once, by `variable`.
        FieldType::Bit => match variable(req, p) {
            Some(b) if b.len() <= 8 => long(from_1_to_8_bytes(field, b), req)?,
            Some(_) => {
                over_read(req);
                return Ok(None);
            }
            None => return Ok(None),
        },
        _ => match variable(req, p) {
            Some(b) => text(req, b)?,
            None => return Ok(None),
        },
    }))
}

/// `mysql_float_to_double`: the float printed at its precision (`%.*F`
/// with the column's decimals, or `zend_gcvt` to `FLT_DIG` digits) and
/// read back, so `0.1` stays `0.1`.
fn float_to_double(f: f32, decimals: Option<usize>) -> f64 {
    let text = match decimals {
        Some(d) => format!("{:.*}", d, f64::from(f)),
        None => format!("{:.5e}", f64::from(f)),
    };
    text.parse().unwrap_or(f64::from(f))
}

/// The microseconds a value with `decimals` shows, `%0*u` of
/// `second_part / 10^(6 - decimals)`.
fn fraction(decimals: u32, second_part: u32) -> String {
    if decimals > 0 && decimals < 7 {
        let shown = second_part / 10u32.pow(6 - decimals);
        format!(".{shown:0width$}", width = decimals as usize)
    } else {
        String::new()
    }
}

/// `ps_fetch_time`: the days folded into the hours, as php's `unsigned
/// int` holds them.
fn time(field: &Field, b: &[u8]) -> String {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let (neg, hour, minute, second, part) = if b.is_empty() {
        (false, 0u32, 0, 0, 0)
    } else {
        let day = u32::from_le_bytes([at(1), at(2), at(3), at(4)]);
        let part = if b.len() > 8 {
            u32::from_le_bytes([at(8), at(9), at(10), at(11)])
        } else {
            0
        };
        let hour = u32::from(at(5)).wrapping_add(day.wrapping_mul(24));
        (at(0) != 0, hour, at(6), at(7), part)
    };
    format!(
        "{}{hour:02}:{minute:02}:{second:02}{}",
        if neg { "-" } else { "" },
        fraction(field.decimals, part)
    )
}

/// `ps_fetch_date`.
fn date(b: &[u8]) -> String {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let (year, month, day) = if b.is_empty() {
        (0, 0, 0)
    } else {
        (u16::from_le_bytes([at(0), at(1)]), at(2), at(3))
    };
    format!("{year:04}-{month:02}-{day:02}")
}

/// `ps_fetch_datetime`.
fn datetime(field: &Field, b: &[u8]) -> String {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let (year, month, day) = if b.is_empty() {
        (0, 0, 0)
    } else {
        (u16::from_le_bytes([at(0), at(1)]), at(2), at(3))
    };
    let (hour, minute, second) = if b.len() > 4 {
        (at(4), at(5), at(6))
    } else {
        (0, 0, 0)
    };
    let part = if b.len() > 7 {
        u32::from_le_bytes([at(7), at(8), at(9), at(10)])
    } else {
        0
    };
    format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{}",
        fraction(field.decimals, part)
    )
}

/// `STAT_BINARY_TYPE_FETCHED_*` for a column's type.
fn binary_stat(ty: FieldType) -> Stat {
    match ty {
        FieldType::Decimal | FieldType::NewDecimal => Stat::BinaryTypeFetchedDecimal,
        FieldType::Tiny => Stat::BinaryTypeFetchedInt8,
        FieldType::Short => Stat::BinaryTypeFetchedInt16,
        FieldType::Long => Stat::BinaryTypeFetchedInt32,
        FieldType::Float => Stat::BinaryTypeFetchedFloat,
        FieldType::Double => Stat::BinaryTypeFetchedDouble,
        FieldType::Null => Stat::BinaryTypeFetchedNull,
        FieldType::Timestamp => Stat::BinaryTypeFetchedTimestamp,
        FieldType::LongLong => Stat::BinaryTypeFetchedInt64,
        FieldType::Int24 => Stat::BinaryTypeFetchedInt24,
        FieldType::Date | FieldType::NewDate => Stat::BinaryTypeFetchedDate,
        FieldType::Time => Stat::BinaryTypeFetchedTime,
        FieldType::DateTime => Stat::BinaryTypeFetchedDatetime,
        FieldType::Year => Stat::BinaryTypeFetchedYear,
        FieldType::VarChar | FieldType::VarString | FieldType::String => {
            Stat::BinaryTypeFetchedString
        }
        FieldType::Bit => Stat::BinaryTypeFetchedBit,
        FieldType::Enum => Stat::BinaryTypeFetchedEnum,
        FieldType::Set => Stat::BinaryTypeFetchedSet,
        FieldType::TinyBlob
        | FieldType::MediumBlob
        | FieldType::LongBlob
        | FieldType::Blob
        | FieldType::Vector => Stat::BinaryTypeFetchedBlob,
        FieldType::Geometry => Stat::BinaryTypeFetchedGeometry,
        FieldType::Json | FieldType::Other(_) => Stat::BinaryTypeFetchedOther,
    }
}
