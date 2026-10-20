//! `php_encoding.c`'s `to_xml_datetime_ex` and the seven XML Schema date
//! and time types written through it: a timestamp by the platform's
//! local time, as `php_localtime_r` and `strftime` read it, a string as it
//! is, and a `DateTimeInterface` by php's `date()` format.

use std::fmt::Write as _;

use php_ext::call::lookup_class;
use php_ext::request::{Request, Zone};
use php_ext::values::{Value, ValueRef};
use php_ext_date::php_format_date_obj;
use php_ext_libxml::ffi::Node;

use super::{Enc, bogus, find_zval_null, set_ns_and_type};
use crate::model::Use;
use crate::{Res, soap_error};

/// `struct tm`, what `strftime` reads here.
struct Tm {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    gmtoff: i64,
}

/// `php_localtime_r`: none where the year leaves `tm_year`'s `int`.
fn localtime(zone: &Zone, t: i64) -> Option<Tm> {
    let gmtoff = zone.offset(t);
    let (year, month, day) = zone.date(t);
    i32::try_from(year.checked_sub(1900)?).ok()?;
    let of_day = t.saturating_add(gmtoff).rem_euclid(86_400);
    Some(Tm {
        year,
        month,
        day,
        hour: of_day / 3600,
        min: of_day % 3600 / 60,
        sec: of_day % 60,
        gmtoff,
    })
}

/// `strftime`'s `%Y` as the BSD C library's `_yconv` writes it: the
/// century and the year of it, two digits each.
fn year(out: &mut String, tm_year: i64) {
    let (a, b) = (tm_year, 1900i64);
    let mut trail = a % 100 + b % 100;
    let mut lead = a / 100 + b / 100 + trail / 100;
    trail %= 100;
    if trail < 0 && lead > 0 {
        trail += 100;
        lead -= 1;
    } else if lead < 0 && trail > 0 {
        trail -= 100;
        lead += 1;
    }
    if lead == 0 && trail < 0 {
        out.push_str("-0");
    } else {
        write!(out, "{lead:02}").ok();
    }
    write!(out, "{:02}", trail.abs()).ok();
}

/// `strftime` over the conversions the seven formats use.
fn strftime(format: &str, tm: &Tm) -> String {
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => year(&mut out, tm.year - 1900),
            Some('m') => {
                write!(out, "{:02}", tm.month).ok();
            }
            Some('d') => {
                write!(out, "{:02}", tm.day).ok();
            }
            Some('H') => {
                write!(out, "{:02}", tm.hour).ok();
            }
            Some('M') => {
                write!(out, "{:02}", tm.min).ok();
            }
            Some('S') => {
                write!(out, "{:02}", tm.sec).ok();
            }
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// The zone suffix: `Z` for UTC, else `+hh:mm` or `-hh:mm`.
fn zone_suffix(gmtoff: i64) -> String {
    let sign = if gmtoff < 0 { '-' } else { '+' };
    let s = format!(
        "{sign}{:02}:{:02}",
        (gmtoff / 3600).abs(),
        (gmtoff % 3600 / 60).abs()
    );
    if s == "+00:00" { "Z".into() } else { s }
}

/// `to_xml_datetime_ex`: a `DateTimeInterface` through ext/date's
/// `php_format_date_obj`, past a subclass's `format()`, and one no
/// constructor set is `Invalid DateTimeInterface`.
///
/// ```php
/// class C extends SoapClient { function __doRequest($r, $l, $a, $v, $o = false, ?string $u = null): string { preg_match('/<param0[^>]*>(.*)<\/param0>/', $r, $m); echo $m[1], "\n"; return ''; } }
/// class D extends DateTimeImmutable { function format(string $f): string { return 'overridden'; } }
/// $c = new C(null, ['location' => 'http://x/', 'uri' => 'urn:x']);
/// $c->f(new SoapVar(new D('2001-02-03 04:05:06', new DateTimeZone('+01:00')), XSD_DATETIME));
/// try { $c->f(new SoapVar((new ReflectionClass('DateTime'))->newInstanceWithoutConstructor(), XSD_TIME)); } catch (SoapFault $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 2001-02-03T04:05:06.000000+01:00
/// SOAP-ERROR: Encoding: Invalid DateTimeInterface
/// ```
fn to_xml_datetime_ex(
    req: &mut Request,
    e: &Enc,
    data: Value,
    format: &str,
    ext_date_format: &[u8],
    style: Use,
    parent: Node,
) -> Res<Node> {
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    match data.as_ref() {
        ValueRef::Int(t) => {
            let Some(tm) = localtime(&Zone::of(req), t) else {
                return Err(soap_error(format_args!("Encoding: Invalid timestamp {t}")));
            };
            let mut buf = strftime(format, &tm);
            buf.push_str(&zone_suffix(tm.gmtoff));
            ret.set_content(buf.as_bytes());
        }
        ValueRef::Str(s) => ret.set_content_len(s.as_bytes()),
        ValueRef::Object(o) => {
            if let Some(iface) = lookup_class(req, b"DateTimeInterface")?
                && o.instance_of(req, iface)
            {
                match php_format_date_obj(req, ext_date_format, o) {
                    Some(b) => ret.set_content_len(&b),
                    None => return Err(soap_error("Encoding: Invalid DateTimeInterface")),
                }
            }
        }
        _ => {}
    }
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `to_xml_datetime`.
pub(super) fn to_xml_datetime(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(
        req,
        e,
        data,
        "%Y-%m-%dT%H:%M:%S",
        b"Y-m-d\\TH:i:s.up",
        style,
        parent,
    )
}

/// `to_xml_time`.
pub(super) fn to_xml_time(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(req, e, data, "%H:%M:%S", b"H:i:s.up", style, parent)
}

/// `to_xml_date`.
pub(super) fn to_xml_date(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(req, e, data, "%Y-%m-%d", b"Y-m-dp", style, parent)
}

/// `to_xml_gyearmonth`.
pub(super) fn to_xml_gyearmonth(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(req, e, data, "%Y-%m", b"Y-mp", style, parent)
}

/// `to_xml_gyear`.
pub(super) fn to_xml_gyear(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(req, e, data, "%Y", b"Yp", style, parent)
}

/// `to_xml_gmonthday`.
pub(super) fn to_xml_gmonthday(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(req, e, data, "--%m-%d", b"--m-dp", style, parent)
}

/// `to_xml_gday`.
pub(super) fn to_xml_gday(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(req, e, data, "---%d", b"---dp", style, parent)
}

/// `to_xml_gmonth`.
pub(super) fn to_xml_gmonth(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    to_xml_datetime_ex(req, e, data, "--%m--", b"--m--p", style, parent)
}

#[cfg(test)]
mod tests {
    use super::{Tm, strftime, zone_suffix};

    #[test]
    fn formats_as_strftime() {
        let tm = Tm {
            year: 2004,
            month: 3,
            day: 7,
            hour: 5,
            min: 6,
            sec: 9,
            gmtoff: 0,
        };
        assert_eq!(strftime("%Y-%m-%dT%H:%M:%S", &tm), "2004-03-07T05:06:09");
        assert_eq!(strftime("--%m--", &tm), "--03--");
        let early = Tm { year: 5, ..tm };
        assert_eq!(strftime("%Y", &early), "0005");
        assert_eq!(zone_suffix(0), "Z");
        assert_eq!(zone_suffix(3600), "+01:00");
        assert_eq!(zone_suffix(-34_200), "-09:30");
    }
}
