//! `Z/ext/standard/dns.c` as the darwin build has it (`HAVE_DNS_SEARCH`:
//! the `dns_search` of `<dns.h>`): `gethostname`, `gethostbyaddr`,
//! `gethostbyname`, `gethostbynamel`, `dns_check_record` and
//! `checkdnsrr`, `dns_get_record`, `dns_get_mx` and `getmxrr`, and the
//! `DNS_*` constants. The answer is parsed as `php_parserr` parses it,
//! into records whose keys are in php's order.
//!
//! The natives, as the stub declares them.
//!
//! ```php
//! foreach (['gethostbyname', 'checkdnsrr', 'dns_get_record', 'getmxrr'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function gethostbyname ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $hostname ]
//!   }
//!   - Return [ string ]
//! }
//! Function [ <internal:standard> function checkdnsrr ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $hostname ]
//!     Parameter #1 [ <optional> string $type = "MX" ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function dns_get_record ] {
//!
//!   - Parameters [5] {
//!     Parameter #0 [ <required> string $hostname ]
//!     Parameter #1 [ <optional> int $type = DNS_ANY ]
//!     Parameter #2 [ <optional> &$authoritative_name_servers = null ]
//!     Parameter #3 [ <optional> &$additional_records = null ]
//!     Parameter #4 [ <optional> bool $raw = false ]
//!   }
//!   - Return [ array|false ]
//! }
//! Function [ <internal:standard> function getmxrr ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> string $hostname ]
//!     Parameter #1 [ <required> &$hosts ]
//!     Parameter #2 [ <optional> &$weights = null ]
//!   }
//!   - Return [ bool ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::{CStr, CString};

use php_ext::abi::{OrFalse, Passed, Path, Ref, Untyped};
use php_ext::attr::{php_const, php_function};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::{arg_value_error, ffi};

use constants::{
    DNS_A, DNS_A6, DNS_AAAA, DNS_ALL, DNS_ANY, DNS_CAA, DNS_CNAME, DNS_HINFO, DNS_MX, DNS_NAPTR,
    DNS_NS, DNS_PTR, DNS_SOA, DNS_SRV, DNS_TXT,
};

/// An untyped by-reference out-parameter, `&$hosts`.
type Out = Ref<Untyped<Value>>;

/// The `DNS_*` constants, in the stub's order.
///
/// ```php
/// var_dump(DNS_A, DNS_NS, DNS_CNAME, DNS_SOA, DNS_PTR, DNS_HINFO, DNS_CAA, DNS_MX, DNS_TXT, DNS_SRV, DNS_NAPTR, DNS_AAAA, DNS_A6, DNS_ANY, DNS_ALL);
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(16)
/// int(32)
/// int(2048)
/// int(4096)
/// int(8192)
/// int(16384)
/// int(32768)
/// int(33554432)
/// int(67108864)
/// int(134217728)
/// int(16777216)
/// int(268435456)
/// int(251721779)
/// ```
#[php_const]
pub(crate) mod constants {
    /// `PHP_DNS_A`, `Z/ext/standard/php_dns.h:60`.
    pub(crate) const DNS_A: i64 = 0x0000_0001;
    /// `PHP_DNS_NS`, `Z/ext/standard/php_dns.h:61`.
    pub(crate) const DNS_NS: i64 = 0x0000_0002;
    /// `PHP_DNS_CNAME`, `Z/ext/standard/php_dns.h:62`.
    pub(crate) const DNS_CNAME: i64 = 0x0000_0010;
    /// `PHP_DNS_SOA`, `Z/ext/standard/php_dns.h:63`.
    pub(crate) const DNS_SOA: i64 = 0x0000_0020;
    /// `PHP_DNS_PTR`, `Z/ext/standard/php_dns.h:64`.
    pub(crate) const DNS_PTR: i64 = 0x0000_0800;
    /// `PHP_DNS_HINFO`, `Z/ext/standard/php_dns.h:65`.
    pub(crate) const DNS_HINFO: i64 = 0x0000_1000;
    /// `PHP_DNS_CAA`, `Z/ext/standard/php_dns.h:67`.
    pub(crate) const DNS_CAA: i64 = 0x0000_2000;
    /// `PHP_DNS_MX`, `Z/ext/standard/php_dns.h:69`.
    pub(crate) const DNS_MX: i64 = 0x0000_4000;
    /// `PHP_DNS_TXT`, `Z/ext/standard/php_dns.h:70`.
    pub(crate) const DNS_TXT: i64 = 0x0000_8000;
    /// `PHP_DNS_SRV`, `Z/ext/standard/php_dns.h:72`.
    pub(crate) const DNS_SRV: i64 = 0x0200_0000;
    /// `PHP_DNS_NAPTR`, `Z/ext/standard/php_dns.h:73`.
    pub(crate) const DNS_NAPTR: i64 = 0x0400_0000;
    /// `PHP_DNS_AAAA`, `Z/ext/standard/php_dns.h:74`.
    pub(crate) const DNS_AAAA: i64 = 0x0800_0000;
    /// `PHP_DNS_A6`, `Z/ext/standard/php_dns.h:71`.
    pub(crate) const DNS_A6: i64 = 0x0100_0000;
    /// `PHP_DNS_ANY`, `Z/ext/standard/php_dns.h:75`.
    pub(crate) const DNS_ANY: i64 = 0x1000_0000;
    /// `PHP_DNS_ALL`, `Z/ext/standard/php_dns.h:82`.
    pub(crate) const DNS_ALL: i64 = DNS_A
        | DNS_NS
        | DNS_CNAME
        | DNS_SOA
        | DNS_PTR
        | DNS_HINFO
        | DNS_CAA
        | DNS_MX
        | DNS_TXT
        | DNS_A6
        | DNS_SRV
        | DNS_NAPTR
        | DNS_AAAA;
}

/// `php_error_docref` of one [`Warning`].
fn warn(req: &mut Request, warning: &Warning) {
    docref(req, Level::Warning, &warning.1);
}

/// A string value of the request's.
fn text(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `Z_PARAM_PATH` as a C string: the check has refused a NUL.
fn c_path(path: Path) -> CString {
    CString::new(path.0.as_bytes()).unwrap_or_default()
}

/// The `ValueError` of one [`DnsError`]; `arg_value_error` adds the
/// `must `.
fn value_error(req: &mut Request, error: DnsError) -> Thrown {
    let tail = error
        .message()
        .strip_prefix("must ")
        .unwrap_or(error.message());
    arg_value_error(req, error.arg_num(), tail)
}

/// `gethostname(): string|false`, `PHP_FUNCTION(gethostname)`
/// (`Z/ext/standard/dns.c:125`): the host's name, which is its own, so
/// the witness pins its shape.
///
/// ```php
/// var_dump(is_string(gethostname()), gethostname() === gethostname());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn gethostname(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match lookup_hostname() {
        Ok(name) => Ok(OrFalse::Value(PhpString::new(req, &name)?)),
        Err(warning) => {
            warn(req, &warning);
            Ok(OrFalse::False)
        }
    }
}

/// `gethostbyaddr(string $ip): string|false`,
/// `PHP_FUNCTION(gethostbyaddr)` (`Z/ext/standard/dns.c:146`): the name
/// `getnameinfo(3)` gives the address, the address itself where it gives
/// none.
///
/// ```php
/// var_dump(gethostbyaddr('127.0.0.1'), gethostbyaddr('nope'));
/// try { gethostbyaddr("1\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: gethostbyaddr(): Address is not a valid IPv4 or IPv6 address in %s on line 2
/// string(9) "localhost"
/// bool(false)
/// gethostbyaddr(): Argument #1 ($ip) must not contain any null bytes
/// ```
#[php_function]
fn gethostbyaddr(req: &mut Request, ip: Path) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match lookup_addr(&c_path(ip)) {
        Ok(name) => Ok(OrFalse::Value(PhpString::new(req, &name)?)),
        Err(warning) => {
            warn(req, &warning);
            Ok(OrFalse::False)
        }
    }
}

/// `gethostbyname(string $hostname): string`,
/// `PHP_FUNCTION(gethostbyname)` (`Z/ext/standard/dns.c:219`): the first
/// IPv4 address `getaddrinfo(3)` finds, the name itself where it finds
/// none.
///
/// ```php
/// var_dump(gethostbyname('localhost'), gethostbyname('127.0.0.2'), gethostbyname('no-such-host.invalid'));
/// var_dump(strlen(gethostbyname(str_repeat('a', 256))));
/// ```
/// ```expect
/// string(9) "127.0.0.1"
/// string(9) "127.0.0.2"
/// string(20) "no-such-host.invalid"
///
/// Warning: gethostbyname(): Host name cannot be longer than 255 characters in %s on line 3
/// int(256)
/// ```
#[php_function]
fn gethostbyname(req: &mut Request, hostname: Path) -> Result<Owner<PhpString>, Thrown> {
    let (answer, warning) = lookup_name(&c_path(hostname));
    if let Some(warning) = warning {
        warn(req, &warning);
    }
    PhpString::new(req, &answer)
}

/// `gethostbynamel(string $hostname): array|false`,
/// `PHP_FUNCTION(gethostbynamel)` (`Z/ext/standard/dns.c:245`): every
/// IPv4 address `getaddrinfo(3)` finds.
///
/// ```php
/// var_dump(gethostbynamel('127.0.0.1'), gethostbynamel('no-such-host.invalid'), gethostbynamel(str_repeat('a', 256)));
/// ```
/// ```expect
/// Warning: gethostbynamel(): Host name cannot be longer than 255 characters in %s on line 2
/// array(1) {
///   [0]=>
///   string(9) "127.0.0.1"
/// }
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn gethostbynamel(req: &mut Request, hostname: Path) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let (list, warning) = lookup_names(&c_path(hostname));
    if let Some(warning) = warning {
        warn(req, &warning);
    }
    let Some(list) = list else {
        return Ok(OrFalse::False);
    };
    let mut array = PhpArray::new(req)?;
    for address in list {
        let v = text(req, &address)?;
        array.append(req, v)?;
    }
    Ok(OrFalse::Value(array))
}

/// `dns_check_record(string $hostname, string $type = "MX"): bool` and
/// its alias `checkdnsrr`, `PHP_FUNCTION(dns_check_record)`
/// (`Z/ext/standard/dns.c:367`): whether a search for the type finds an
/// answer.
///
/// ```php
/// foreach (['', 'x'] as $h) { try { var_dump(checkdnsrr($h, 'BOGUS')); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// try { dns_check_record('x', 'nope'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(dns_check_record('no-such-host.invalid', 'a'));
/// ```
/// ```expect
/// checkdnsrr(): Argument #1 ($hostname) must not be empty
/// checkdnsrr(): Argument #2 ($type) must be a valid DNS record type
/// dns_check_record(): Argument #2 ($type) must be a valid DNS record type
/// bool(false)
/// ```
#[php_function(aliases(checkdnsrr))]
fn dns_check_record(
    req: &mut Request,
    hostname: Path,
    #[php(default = "MX")] r#type: Passed<PhpString>,
) -> Result<bool, Thrown> {
    let rectype = match &r#type {
        Passed::Given(rectype) => Some(rectype.as_bytes()),
        Passed::Omitted => None,
    };
    check_record(&c_path(hostname), rectype).map_err(|error| value_error(req, error))
}

/// One record as php's array of it.
fn record_array(req: &mut Request, record: Record) -> Result<Owner<Value>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for (name, field) in record {
        let value = match field {
            Field::Int(n) => Owner::int(n),
            Field::Str(s) => text(req, &s)?,
            Field::List(entries) => {
                let mut list = PhpArray::new(req)?;
                for entry in entries {
                    let v = text(req, &entry)?;
                    list.append(req, v)?;
                }
                list.into_value()
            }
        };
        array.set_str(req, name.as_bytes(), value)?;
    }
    Ok(array.into_value())
}

/// A list of records as php's array of them.
fn records_array(req: &mut Request, records: Vec<Record>) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for record in records {
        let v = record_array(req, record)?;
        array.append(req, v)?;
    }
    Ok(array)
}

/// `zend_try_array_init` of each by-reference argument passed.
fn empty_refs(req: &mut Request, targets: &[&Out]) -> Result<(), Thrown> {
    for target in targets {
        if target.is_given() {
            let empty = PhpArray::new(req)?;
            target.set(req, Untyped(empty.into_value()))?;
        }
    }
    Ok(())
}

/// `dns_get_record(string $hostname, int $type = DNS_ANY,
/// &$authoritative_name_servers = null, &$additional_records = null,
/// bool $raw = false): array|false`, `PHP_FUNCTION(dns_get_record)`
/// (`Z/ext/standard/dns.c:808`): each record of each type asked for, as
/// `php_parserr` reads it. What a name resolves to is the network's, so
/// the witness pins the checks.
///
/// ```php
/// $a = 'kept';
/// try { dns_get_record('x', 1 << 30, $a); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($a);
/// try { dns_get_record('x', 0, $n, $m, true); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { dns_get_record("x\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// dns_get_record(): Argument #2 ($type) must be a DNS_* constant
/// array(0) {
/// }
/// dns_get_record(): Argument #2 ($type) must be between 1 and 65535 when argument #5 ($raw) is true
/// dns_get_record(): Argument #1 ($hostname) must not contain any null bytes
/// ```
#[php_function]
fn dns_get_record(
    req: &mut Request,
    hostname: Path,
    #[php(default = DNS_ANY)] r#type: i64,
    #[php(default = null)] authoritative_name_servers: Out,
    #[php(default = null)] additional_records: Out,
    #[php(default = false)] raw: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let hostname = c_path(hostname);
    empty_refs(req, &[&authoritative_name_servers, &additional_records])?;
    let authns = authoritative_name_servers.is_given();
    let addtl = additional_records.is_given();
    let found = match query_records(&hostname, r#type, authns, addtl, raw) {
        Ok(found) => found,
        Err(error) => return Err(value_error(req, error)),
    };
    for warning in &found.warnings {
        warn(req, warning);
    }
    if authns {
        let v = records_array(req, found.authns)?;
        authoritative_name_servers.set(req, Untyped(v.into_value()))?;
    }
    if addtl {
        let v = records_array(req, found.addtl)?;
        additional_records.set(req, Untyped(v.into_value()))?;
    }
    match found.records {
        Some(records) => records_array(req, records).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `dns_get_mx(string $hostname, &$hosts, &$weights = null): bool` and
/// its alias `getmxrr`, `PHP_FUNCTION(dns_get_mx)`
/// (`Z/ext/standard/dns.c:1048`): the MX hosts of the name and their
/// weights.
///
/// ```php
/// $h = 'kept'; $w = 'kept';
/// var_dump(getmxrr('no-such-host.invalid', $h, $w), $h, $w);
/// ```
/// ```expect
/// bool(false)
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
#[php_function(aliases(getmxrr))]
fn dns_get_mx(
    req: &mut Request,
    hostname: Path,
    hosts: Out,
    #[php(default = null)] weights: Out,
) -> Result<bool, Thrown> {
    let hostname = c_path(hostname);
    empty_refs(req, &[&hosts, &weights])?;
    let with_weights = weights.is_given();
    let found = query_mx(&hostname, with_weights);
    let mut list = PhpArray::new(req)?;
    for host in found.hosts {
        let v = text(req, &host)?;
        list.append(req, v)?;
    }
    hosts.set(req, Untyped(list.into_value()))?;
    if with_weights {
        let mut list = PhpArray::new(req)?;
        for weight in found.weights {
            list.append(req, Owner::int(weight))?;
        }
        weights.set(req, Untyped(list.into_value()))?;
    }
    Ok(found.found)
}

/// `E_WARNING`, `Z/Zend/zend_errors.h:24`.
const E_WARNING: i32 = 2;

/// `PHP_DNS_NUM_TYPES`, `Z/ext/standard/php_dns.h:81`.
const PHP_DNS_NUM_TYPES: i32 = 13;

/// `DNS_T_A`, `Z/ext/standard/dns.c:73`.
const DNS_T_A: u16 = 1;
/// `DNS_T_NS`, `Z/ext/standard/dns.c:76`.
const DNS_T_NS: u16 = 2;
/// `DNS_T_CNAME`, `Z/ext/standard/dns.c:79`.
const DNS_T_CNAME: u16 = 5;
/// `DNS_T_SOA`, `Z/ext/standard/dns.c:82`.
const DNS_T_SOA: u16 = 6;
/// `DNS_T_PTR`, `Z/ext/standard/dns.c:85`.
const DNS_T_PTR: u16 = 12;
/// `DNS_T_HINFO`, `Z/ext/standard/dns.c:88`.
const DNS_T_HINFO: u16 = 13;
/// `DNS_T_MX`, `Z/ext/standard/dns.c:94`.
const DNS_T_MX: u16 = 15;
/// `DNS_T_TXT`, `Z/ext/standard/dns.c:97`.
const DNS_T_TXT: u16 = 16;
/// `DNS_T_AAAA`, `Z/ext/standard/dns.c:100`.
const DNS_T_AAAA: u16 = 28;
/// `DNS_T_SRV`, `Z/ext/standard/dns.c:103`.
const DNS_T_SRV: u16 = 33;
/// `DNS_T_NAPTR`, `Z/ext/standard/dns.c:106`.
const DNS_T_NAPTR: u16 = 35;
/// `DNS_T_A6`, `Z/ext/standard/dns.c:109`.
const DNS_T_A6: u16 = 38;
/// `DNS_T_CAA`, `Z/ext/standard/dns.c:112`.
const DNS_T_CAA: u16 = 257;
/// `DNS_T_ANY`, `Z/ext/standard/dns.c:116`.
const DNS_T_ANY: u16 = 255;

/// `C_IN`, `<arpa/nameser.h>`, as `Z/ext/standard/dns.c:429` passes it.
const C_IN: u32 = 1;
/// `HFIXEDSZ`, `Z/ext/standard/dns.c:326`.
const HFIXEDSZ: usize = 12;
/// `QFIXEDSZ`, `Z/ext/standard/dns.c:330`.
const QFIXEDSZ: usize = 4;
/// `MAXHOSTNAMELEN` as `Z/ext/standard/dns.c:334` redefines it.
const MAXHOSTNAMELEN: usize = 1024;
/// `sizeof(querybuf)`, `Z/ext/standard/dns.c:340`.
const QUERYBUF: usize = 65536;
/// `MAXFQDNLEN`, `Z/main/php_network.h:365`.
const MAXFQDNLEN: usize = 255;
/// `HOST_NAME_MAX` as `Z/ext/standard/dns.c:66` defines it on darwin.
const HOST_NAME_MAX: usize = 255;

/// `HOST_NOT_FOUND`, `<netdb.h>`, tested at `Z/ext/standard/dns.c:968`.
const HOST_NOT_FOUND: i32 = 1;
/// `TRY_AGAIN`, `<netdb.h>`, tested at `Z/ext/standard/dns.c:975`.
const TRY_AGAIN: i32 = 2;
/// `NO_RECOVERY`, `<netdb.h>`, tested at `Z/ext/standard/dns.c:971`.
const NO_RECOVERY: i32 = 3;
/// `NO_DATA`, `<netdb.h>`, tested at `Z/ext/standard/dns.c:967`.
const NO_DATA: i32 = 4;

/// One value of a record: `add_assoc_long`, `add_assoc_string[l]`, or the
/// `entries` list of a TXT record (`Z/ext/standard/dns.c:584`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    /// A php int, `add_assoc_long` (`Z/ext/standard/dns.c:491`).
    Int(i64),
    /// A php string, `add_assoc_stringl` (`Z/ext/standard/dns.c:496`).
    Str(Vec<u8>),
    /// A php list of strings, the TXT `entries` (`Z/ext/standard/dns.c:604`).
    List(Vec<Vec<u8>>),
}

/// One record as `php_parserr` builds it (`Z/ext/standard/dns.c:487`),
/// keys in php's order.
type Record = Vec<(&'static str, Field)>;

/// One warning of `php_error_docref` (`Z/main/main.c:1239`): its level and
/// text, without the `func(): ` php prefixes.
type Warning = (i32, Vec<u8>);

/// The `ValueError`s the natives throw. Each is the
/// `zend_argument_value_error` form (`Z/Zend/zend_API.c:440`): the engine prefixes
/// `func(): Argument #N ($name) ` to [`DnsError::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DnsError {
    /// `zend_argument_must_not_be_empty_error(1)`, `Z/ext/standard/dns.c:391`.
    EmptyHostname,
    /// `Z/ext/standard/dns.c:410`.
    BadRecordType,
    /// `Z/ext/standard/dns.c:855`.
    NotDnsConstant,
    /// `Z/ext/standard/dns.c:860`.
    RawTypeRange,
}

impl DnsError {
    /// The `arg_num` of `zend_argument_value_error`, `Z/Zend/zend_API.c:440`.
    pub(super) fn arg_num(self) -> u32 {
        match self {
            DnsError::EmptyHostname => 1,
            DnsError::BadRecordType | DnsError::NotDnsConstant | DnsError::RawTypeRange => 2,
        }
    }

    /// The `format` of `zend_argument_value_error`, `Z/Zend/zend_API.c:440`:
    /// the text after `Argument #N ($name) `.
    pub(super) fn message(self) -> &'static str {
        match self {
            DnsError::EmptyHostname => "must not be empty",
            DnsError::BadRecordType => "must be a valid DNS record type",
            DnsError::NotDnsConstant => "must be a DNS_* constant",
            DnsError::RawTypeRange => "must be between 1 and 65535 when argument #5 ($raw) is true",
        }
    }
}

/// `GETSHORT` at `at` (`Z/ext/standard/dns.c:468`); a byte past the buffer
/// reads 0.
fn get16(buf: &[u8], at: usize) -> u16 {
    let hi = buf.get(at).copied().unwrap_or(0);
    let lo = buf.get(at.saturating_add(1)).copied().unwrap_or(0);
    u16::from_be_bytes([hi, lo])
}

/// `GETLONG` at `at` (`Z/ext/standard/dns.c:470`); a byte past the buffer
/// reads 0.
fn get32(buf: &[u8], at: usize) -> u32 {
    (u32::from(get16(buf, at)) << 16) | u32::from(get16(buf, at.saturating_add(2)))
}

/// The answer php parses (`Z/ext/standard/dns.c:825`): the whole `querybuf`
/// and the offset of `end`.
struct Answer<'a> {
    /// `answer.qb2`, all 65536 bytes.
    buf: &'a [u8],
    /// `end - answer.qb2`: the length `dns_search` gave.
    end: usize,
}

impl Answer<'_> {
    /// `answer.qb2 .. end`, the span `dn_expand` and `dn_skipname` see
    /// (`Z/ext/standard/dns.c:987`).
    fn msg(&self) -> &[u8] {
        self.buf.get(..self.end).unwrap_or(self.buf)
    }

    /// `CHECKCP(n)`, `Z/ext/standard/dns.c:442`: whether `cp + n <= end`.
    fn fits(&self, cp: usize, n: usize) -> bool {
        cp.saturating_add(n) <= self.end
    }

    /// `dn_expand(answer->qb2, end, cp, name, (sizeof name) - 2)`,
    /// `Z/ext/standard/dns.c:461`.
    fn expand(&self, cp: usize) -> Option<(usize, Vec<u8>)> {
        ffi::dn_expand(self.msg(), cp, MAXHOSTNAMELEN - 2)
    }

    /// `n` bytes at `cp`, a span `CHECKCP` has proven, as
    /// `add_assoc_stringl` copies them (`Z/ext/standard/dns.c:543`).
    fn bytes(&self, cp: usize, n: usize) -> Vec<u8> {
        self.buf
            .get(cp..cp.saturating_add(n))
            .unwrap_or_default()
            .to_vec()
    }

    /// `*cp & 0xFF` (`Z/ext/standard/dns.c:540`); a byte past the buffer
    /// reads 0.
    fn byte(&self, cp: usize) -> u8 {
        self.buf.get(cp).copied().unwrap_or(0)
    }
}

/// How the type switch of `php_parserr` ends, `Z/ext/standard/dns.c:501`.
enum Step {
    /// `return cp` with the record kept.
    At(usize),
    /// `return NULL`: the record built so far is kept and the section stops.
    Stop,
    /// The `default:` case: the record is dropped, `cp += dlen`.
    Skip(usize),
}

/// The text php builds for an IPv6 address, `Z/ext/standard/dns.c:634`:
/// the `tp`, `have_v6_break` and `in_v6_break` of `php_parserr`.
#[derive(Default)]
struct V6Text {
    /// The bytes before `tp`.
    text: Vec<u8>,
    /// `have_v6_break`.
    have_break: bool,
    /// `in_v6_break`.
    in_break: bool,
}

impl V6Text {
    /// One 16-bit group, `Z/ext/standard/dns.c:638`.
    fn group(&mut self, s: u16) {
        if s != 0 {
            if !self.text.is_empty() {
                self.in_break = false;
                self.text.push(b':');
            }
            self.text.extend_from_slice(format!("{s:x}").as_bytes());
        } else {
            self.zero();
        }
    }

    /// A zero group, `Z/ext/standard/dns.c:646`.
    fn zero(&mut self) {
        if !self.have_break {
            self.have_break = true;
            self.in_break = true;
            self.text.push(b':');
        } else if !self.in_break {
            self.text.extend_from_slice(b":0");
        }
    }

    /// The close of `Z/ext/standard/dns.c:659`.
    fn finish(mut self) -> Vec<u8> {
        if self.have_break && self.in_break {
            self.text.push(b':');
        }
        self.text
    }
}

/// `php_parserr`, `Z/ext/standard/dns.c:449`: one resource record at `cp`.
/// The first value is the `cp` php returns (`None` for `NULL`); the second
/// is the record, set where php leaves `subarray` an array.
fn php_parserr(
    ans: &Answer,
    cp: usize,
    type_to_fetch: i64,
    store: bool,
    raw: bool,
) -> (Option<usize>, Option<Record>) {
    let Some((n, name)) = ans.expand(cp) else {
        return (None, None);
    };
    let mut cp = cp.saturating_add(n);
    if !ans.fits(cp, 10) {
        return (None, None);
    }
    let ty = get16(ans.buf, cp);
    let ttl = get32(ans.buf, cp + 4);
    let dlen = usize::from(get16(ans.buf, cp + 8));
    cp += 10;
    if !ans.fits(cp, dlen) {
        return (None, None);
    }
    if dlen == 0 {
        return (None, None);
    }
    if type_to_fetch != i64::from(DNS_T_ANY) && i64::from(ty) != type_to_fetch {
        return (Some(cp + dlen), None);
    }
    if !store {
        return (Some(cp + dlen), None);
    }
    let mut rec: Record = vec![
        ("host", Field::Str(name)),
        ("class", Field::Str(b"IN".to_vec())),
        ("ttl", Field::Int(i64::from(ttl))),
    ];
    if raw {
        rec.push(("type", Field::Int(i64::from(ty))));
        rec.push(("data", Field::Str(ans.bytes(cp, dlen))));
        return (Some(cp + dlen), Some(rec));
    }
    match rdata(ans, cp, ty, dlen, &mut rec) {
        Step::At(cp) => (Some(cp), Some(rec)),
        Step::Stop => (None, Some(rec)),
        Step::Skip(cp) => (Some(cp), None),
    }
}

/// A string php reads as a length byte and that many bytes
/// (`Z/ext/standard/dns.c:539`): `None` where a `CHECKCP` fails.
fn char_string(ans: &Answer, cp: &mut usize) -> Option<Vec<u8>> {
    if !ans.fits(*cp, 1) {
        return None;
    }
    let n = usize::from(ans.byte(*cp));
    *cp += 1;
    if !ans.fits(*cp, n) {
        return None;
    }
    let s = ans.bytes(*cp, n);
    *cp += n;
    Some(s)
}

/// `add_assoc_stringl`, as `Z/ext/standard/dns.c:543` calls it.
fn push_str(rec: &mut Record, key: &'static str, v: &[u8]) {
    rec.push((key, Field::Str(v.to_vec())));
}

/// `add_assoc_long`, as `Z/ext/standard/dns.c:513` calls it.
fn push_int(rec: &mut Record, key: &'static str, v: impl Into<i64>) {
    rec.push((key, Field::Int(v.into())));
}

/// The type switch of `php_parserr`, `Z/ext/standard/dns.c:501`.
fn rdata(ans: &Answer, mut cp: usize, ty: u16, dlen: usize, rec: &mut Record) -> Step {
    match ty {
        DNS_T_A => {
            if !ans.fits(cp, 4) {
                return Step::Stop;
            }
            push_str(rec, "type", b"A");
            let ip = format!(
                "{}.{}.{}.{}",
                ans.byte(cp),
                ans.byte(cp + 1),
                ans.byte(cp + 2),
                ans.byte(cp + 3)
            );
            push_str(rec, "ip", ip.as_bytes());
            Step::At(cp + dlen)
        }
        DNS_T_MX | DNS_T_CNAME | DNS_T_NS | DNS_T_PTR => {
            if ty == DNS_T_MX {
                if !ans.fits(cp, 2) {
                    return Step::Stop;
                }
                push_str(rec, "type", b"MX");
                push_int(rec, "pri", get16(ans.buf, cp));
                cp += 2;
            }
            let name: &[u8] = match ty {
                DNS_T_CNAME => b"CNAME",
                DNS_T_NS => b"NS",
                DNS_T_PTR => b"PTR",
                _ => b"",
            };
            if !name.is_empty() {
                push_str(rec, "type", name);
            }
            let Some((n, target)) = ans.expand(cp) else {
                return Step::Stop;
            };
            rec.push(("target", Field::Str(target)));
            Step::At(cp + n)
        }
        DNS_T_HINFO => {
            push_str(rec, "type", b"HINFO");
            let Some(cpu) = char_string(ans, &mut cp) else {
                return Step::Stop;
            };
            push_str(rec, "cpu", &cpu);
            let Some(os) = char_string(ans, &mut cp) else {
                return Step::Stop;
            };
            push_str(rec, "os", &os);
            Step::At(cp)
        }
        DNS_T_CAA => caa(ans, cp, dlen, rec),
        DNS_T_TXT => {
            push_str(rec, "type", b"TXT");
            let (txt, entries) = txt(ans, cp, dlen);
            rec.push(("txt", Field::Str(txt)));
            rec.push(("entries", Field::List(entries)));
            Step::At(cp + dlen)
        }
        DNS_T_SOA => soa(ans, cp, rec),
        DNS_T_AAAA => {
            if !ans.fits(cp, 16) {
                return Step::Stop;
            }
            let mut v6 = V6Text::default();
            for _ in 0..8 {
                v6.group(get16(ans.buf, cp));
                cp += 2;
            }
            push_str(rec, "type", b"AAAA");
            rec.push(("ipv6", Field::Str(v6.finish())));
            Step::At(cp)
        }
        DNS_T_A6 => a6(ans, cp, dlen, rec),
        DNS_T_SRV => {
            if !ans.fits(cp, 6) {
                return Step::Stop;
            }
            push_str(rec, "type", b"SRV");
            push_int(rec, "pri", get16(ans.buf, cp));
            push_int(rec, "weight", get16(ans.buf, cp + 2));
            push_int(rec, "port", get16(ans.buf, cp + 4));
            cp += 6;
            let Some((n, target)) = ans.expand(cp) else {
                return Step::Stop;
            };
            rec.push(("target", Field::Str(target)));
            Step::At(cp + n)
        }
        DNS_T_NAPTR => naptr(ans, cp, rec),
        _ => Step::Skip(cp + dlen),
    }
}

/// The CAA case of `php_parserr`, `Z/ext/standard/dns.c:552`.
fn caa(ans: &Answer, mut cp: usize, dlen: usize, rec: &mut Record) -> Step {
    push_str(rec, "type", b"CAA");
    if !ans.fits(cp, 1) {
        return Step::Stop;
    }
    push_int(rec, "flags", ans.byte(cp));
    cp += 1;
    if !ans.fits(cp, 1) {
        return Step::Stop;
    }
    let n = usize::from(ans.byte(cp));
    cp += 1;
    if !ans.fits(cp, n) {
        return Step::Stop;
    }
    push_str(rec, "tag", &ans.bytes(cp, n));
    cp += n;
    if dlen < n + 2 {
        return Step::Stop;
    }
    let n = dlen - n - 2;
    if !ans.fits(cp, n) {
        return Step::Stop;
    }
    push_str(rec, "value", &ans.bytes(cp, n));
    Step::At(cp + n)
}

/// The TXT case of `php_parserr`, `Z/ext/standard/dns.c:575`: the joined
/// text and its `entries`.
fn txt(ans: &Answer, cp: usize, dlen: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut joined = Vec::with_capacity(dlen);
    let mut entries = Vec::new();
    let mut l1 = 0usize;
    while l1 < dlen {
        let mut n = usize::from(ans.byte(cp + l1));
        if l1 + n >= dlen {
            // Invalid chunk length, truncate
            n = dlen - (l1 + 1);
        }
        if n != 0 {
            let chunk = ans.bytes(cp + l1 + 1, n);
            joined.extend_from_slice(&chunk);
            entries.push(chunk);
        }
        l1 = l1 + n + 1;
    }
    (joined, entries)
}

/// The SOA case of `php_parserr`, `Z/ext/standard/dns.c:607`.
fn soa(ans: &Answer, mut cp: usize, rec: &mut Record) -> Step {
    push_str(rec, "type", b"SOA");
    for key in ["mname", "rname"] {
        let Some((n, name)) = ans.expand(cp) else {
            return Step::Stop;
        };
        cp += n;
        rec.push((key, Field::Str(name)));
    }
    if !ans.fits(cp, 5 * 4) {
        return Step::Stop;
    }
    for key in ["serial", "refresh", "retry", "expire", "minimum-ttl"] {
        push_int(rec, key, get32(ans.buf, cp));
        cp += 4;
    }
    Step::At(cp)
}

/// The A6 case of `php_parserr`, `Z/ext/standard/dns.c:667`.
fn a6(ans: &Answer, mut cp: usize, dlen: usize, rec: &mut Record) -> Step {
    let p = cp;
    push_str(rec, "type", b"A6");
    if !ans.fits(cp, 1) {
        return Step::Stop;
    }
    let n = ans.byte(cp);
    cp += 1;
    push_int(rec, "masklen", n);
    let mut v6 = V6Text::default();
    if n > 15 {
        v6.have_break = true;
        v6.in_break = true;
        v6.text.push(b':');
    }
    if n % 16 > 8 {
        // Partial short. php reads this byte without CHECKCP.
        if ans.byte(cp) != 0 {
            if !v6.text.is_empty() {
                v6.in_break = false;
                v6.text.push(b':');
            }
            // php's snprintf of the byte here does not move `tp`, so every
            // later write lands over it and the closing NUL cuts it: the
            // digits never show.
        } else {
            v6.zero();
        }
        cp += 1;
    }
    for _ in (usize::from(n) + 8) / 16..8 {
        if !ans.fits(cp, 2) {
            return Step::Stop;
        }
        v6.group(get16(ans.buf, cp));
        cp += 2;
    }
    rec.push(("ipv6", Field::Str(v6.finish())));
    if cp < p + dlen {
        let Some((n, chain)) = ans.expand(cp) else {
            return Step::Stop;
        };
        cp += n;
        rec.push(("chain", Field::Str(chain)));
    }
    Step::At(cp)
}

/// The NAPTR case of `php_parserr`, `Z/ext/standard/dns.c:760`.
fn naptr(ans: &Answer, mut cp: usize, rec: &mut Record) -> Step {
    if !ans.fits(cp, 4) {
        return Step::Stop;
    }
    push_str(rec, "type", b"NAPTR");
    push_int(rec, "order", get16(ans.buf, cp));
    push_int(rec, "pref", get16(ans.buf, cp + 2));
    cp += 4;
    for key in ["flags", "services", "regex"] {
        let Some(s) = char_string(ans, &mut cp) else {
            return Step::Stop;
        };
        push_str(rec, key, &s);
    }
    let Some((n, name)) = ans.expand(cp) else {
        return Step::Stop;
    };
    rec.push(("replacement", Field::Str(name)));
    Step::At(cp + n)
}

/// What `dns_get_record` (`Z/ext/standard/dns.c:808`) gives back: its return value (`None` for `false`)
/// and what it left in the two by-reference arrays.
#[derive(Debug, Default)]
struct DnsRecords {
    /// The return value; `None` is `false`.
    pub(super) records: Option<Vec<Record>>,
    /// `$authoritative_name_servers`, filled only where it was passed.
    pub(super) authns: Vec<Record>,
    /// `$additional_records`, filled only where it was passed.
    pub(super) addtl: Vec<Record>,
    /// The warnings, in order.
    pub(super) warnings: Vec<Warning>,
}

/// The `type_to_fetch` of step `ty` of the loop in `dns_get_record`
/// (`Z/ext/standard/dns.c:890`), for steps 0 to 12.
fn type_of_step(ty: i32, type_param: i64) -> i64 {
    let (bit, t) = match ty {
        0 => (DNS_A, DNS_T_A),
        1 => (DNS_NS, DNS_T_NS),
        2 => (DNS_CNAME, DNS_T_CNAME),
        3 => (DNS_SOA, DNS_T_SOA),
        4 => (DNS_PTR, DNS_T_PTR),
        5 => (DNS_HINFO, DNS_T_HINFO),
        6 => (DNS_MX, DNS_T_MX),
        7 => (DNS_TXT, DNS_T_TXT),
        8 => (DNS_AAAA, DNS_T_AAAA),
        9 => (DNS_SRV, DNS_T_SRV),
        10 => (DNS_NAPTR, DNS_T_NAPTR),
        11 => (DNS_A6, DNS_T_A6),
        12 => (DNS_CAA, DNS_T_CAA),
        _ => return i64::from(DNS_T_ANY),
    };
    if type_param & bit != 0 {
        i64::from(t)
    } else {
        0
    }
}

/// `PHP_FUNCTION(dns_get_record)`, `Z/ext/standard/dns.c:808`, after
/// parsing: `authns` and `addtl` say whether the two references were passed.
/// The engine sets each passed reference to an empty array before calling,
/// as `zend_try_array_init` does before the type is checked, and assigns
/// [`DnsRecords::authns`] and [`DnsRecords::addtl`] to them after, also
/// when the return value is `false`.
fn query_records(
    hostname: &CStr,
    type_param: i64,
    authns: bool,
    addtl: bool,
    raw: bool,
) -> Result<DnsRecords, DnsError> {
    if !raw {
        if (type_param & !DNS_ALL) != 0 && type_param != DNS_ANY {
            return Err(DnsError::NotDnsConstant);
        }
    } else if !(1..=0xFFFF).contains(&type_param) {
        return Err(DnsError::RawTypeRange);
    }

    let mut out = DnsRecords {
        records: Some(Vec::new()),
        ..DnsRecords::default()
    };
    let mut answer = vec![0u8; QUERYBUF];
    let mut from = ffi::DnsFrom::new();
    let mut store_results = true;
    let mut first_query = true;
    let mut ty: i32 = if raw {
        -1
    } else if type_param == DNS_ANY {
        PHP_DNS_NUM_TYPES + 1
    } else {
        0
    };
    let limit = if addtl {
        PHP_DNS_NUM_TYPES + 2
    } else {
        PHP_DNS_NUM_TYPES
    };

    while ty < limit || first_query {
        first_query = false;
        let type_to_fetch = match ty {
            -1 => {
                // skip over the rest and go directly to additional records
                ty = PHP_DNS_NUM_TYPES - 1;
                type_param
            }
            PHP_DNS_NUM_TYPES => {
                store_results = false;
                ty += 1;
                continue;
            }
            _ => type_of_step(ty, type_param),
        };
        ty += 1;
        if type_to_fetch == 0 {
            continue;
        }
        let query = Query {
            hostname,
            type_to_fetch,
            store_results,
            refs: Refs { authns, addtl },
            raw,
        };
        if !query.fetch(&mut answer, &mut from, &mut out) {
            out.records = None;
            return Ok(out);
        }
    }
    Ok(out)
}

/// Which by-reference arrays `dns_get_record` was given,
/// `Z/ext/standard/dns.c:840`.
#[derive(Clone, Copy)]
struct Refs {
    /// `authns != NULL`.
    authns: bool,
    /// `addtl != NULL`.
    addtl: bool,
}

/// One pass of the loop body of `dns_get_record`, `Z/ext/standard/dns.c:944`.
struct Query<'a> {
    /// `hostname`.
    hostname: &'a CStr,
    /// `type_to_fetch`, nonzero.
    type_to_fetch: i64,
    /// `store_results`.
    store_results: bool,
    /// Which by-reference arrays were passed.
    refs: Refs,
    /// `raw`.
    raw: bool,
}

impl Query<'_> {
    /// `Z/ext/standard/dns.c:945`: queries and parses one answer into `out`; `false` where php destroys
    /// the return value and gives `false`.
    fn fetch(&self, answer: &mut [u8], from: &mut ffi::DnsFrom, out: &mut DnsRecords) -> bool {
        let Some(handle) = ffi::DnsHandle::open() else {
            return false;
        };
        // The raw form checks 1..=0xFFFF and the other steps give a DNS_T_*.
        let ty = u32::try_from(self.type_to_fetch).unwrap_or(0);
        let n = match handle.search(self.hostname, C_IN, ty, answer, from) {
            Ok(n) => n,
            Err(dns_errno) => {
                drop(handle);
                let text: &[u8] = match dns_errno {
                    NO_DATA | HOST_NOT_FOUND => return true,
                    NO_RECOVERY => b"An unexpected server failure occurred.",
                    TRY_AGAIN => b"A temporary server error occurred.",
                    _ => b"DNS Query failed",
                };
                out.warnings.push((E_WARNING, text.to_vec()));
                return false;
            }
        };
        let ans = Answer {
            buf: answer,
            end: n,
        };
        self.parse(&ans, out)
    }

    /// The parse of one answer, `Z/ext/standard/dns.c:986`: `false` where
    /// php gives `false`.
    fn parse(&self, ans: &Answer, out: &mut DnsRecords) -> bool {
        let mut qd = get16(ans.buf, 4);
        let mut an = get16(ans.buf, 6);
        let mut ns = get16(ans.buf, 8);
        let mut ar = get16(ans.buf, 10);

        // Skip QD entries, they're only used by dn_expand later on
        let mut cp = Some(HFIXEDSZ);
        let mut at = HFIXEDSZ;
        while qd > 0 {
            qd -= 1;
            let Ok(n) = usize::try_from(ffi::dn_skipname(ans.msg(), at)) else {
                out.warnings
                    .push((E_WARNING, b"Unable to parse DNS data received".to_vec()));
                return false;
            };
            at = at.saturating_add(n + QFIXEDSZ);
            cp = Some(at);
        }

        // YAY! Our real answers!
        while an > 0 {
            an -= 1;
            let Some(at) = cp.filter(|&c| c < ans.end) else {
                break;
            };
            let (next, rec) =
                php_parserr(ans, at, self.type_to_fetch, self.store_results, self.raw);
            cp = next;
            if let Some(rec) = rec
                && self.store_results
                && let Some(records) = out.records.as_mut()
            {
                records.push(rec);
            }
        }

        if self.refs.authns || self.refs.addtl {
            // List of Authoritative Name Servers
            while ns > 0 {
                ns -= 1;
                let Some(at) = cp.filter(|&c| c < ans.end) else {
                    break;
                };
                let any = i64::from(DNS_T_ANY);
                let (next, rec) = php_parserr(ans, at, any, self.refs.authns, self.raw);
                cp = next;
                out.authns.extend(rec);
            }
        }

        if self.refs.addtl {
            // Additional records associated with authoritative name servers
            while ar > 0 {
                ar -= 1;
                let Some(at) = cp.filter(|&c| c < ans.end) else {
                    break;
                };
                let (next, rec) = php_parserr(ans, at, i64::from(DNS_T_ANY), true, self.raw);
                cp = next;
                out.addtl.extend(rec);
            }
        }
        true
    }
}

/// `PHP_FUNCTION(dns_check_record)`, `Z/ext/standard/dns.c:367`, and its
/// alias `checkdnsrr`, after parsing: `rectype` is `None` where the second
/// value was not passed.
fn check_record(hostname: &CStr, rectype: Option<&[u8]>) -> Result<bool, DnsError> {
    if hostname.is_empty() {
        return Err(DnsError::EmptyHostname);
    }
    let mut ty = DNS_T_MX;
    if let Some(rectype) = rectype {
        /// The type names of `Z/ext/standard/dns.c:396`.
        const NAMES: [(&[u8], u16); 13] = [
            (b"A", DNS_T_A),
            (b"NS", DNS_T_NS),
            (b"MX", DNS_T_MX),
            (b"PTR", DNS_T_PTR),
            (b"ANY", DNS_T_ANY),
            (b"SOA", DNS_T_SOA),
            (b"CAA", DNS_T_CAA),
            (b"TXT", DNS_T_TXT),
            (b"CNAME", DNS_T_CNAME),
            (b"AAAA", DNS_T_AAAA),
            (b"SRV", DNS_T_SRV),
            (b"NAPTR", DNS_T_NAPTR),
            (b"A6", DNS_T_A6),
        ];
        let Some(&(_, t)) = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(rectype)) else {
            return Err(DnsError::BadRecordType);
        };
        ty = t;
    }

    let Some(handle) = ffi::DnsHandle::open() else {
        return Ok(false);
    };
    let mut answer = vec![0u8; QUERYBUF];
    let mut from = ffi::DnsFrom::new();
    let found = handle.search(hostname, C_IN, u32::from(ty), &mut answer, &mut from);
    drop(handle);
    if found.is_err() {
        return Ok(false);
    }
    Ok(get16(&answer, 6) != 0)
}

/// What `dns_get_mx` (`Z/ext/standard/dns.c:1048`) gives back: its return value and what it left in the
/// two by-reference arrays.
#[derive(Debug, Default)]
struct MxHosts {
    /// The return value.
    pub(super) found: bool,
    /// `$hosts`.
    pub(super) hosts: Vec<Vec<u8>>,
    /// `$weights`, filled only where it was passed.
    pub(super) weights: Vec<i64>,
}

/// `PHP_FUNCTION(dns_get_mx)`, `Z/ext/standard/dns.c:1048`, and its alias
/// `getmxrr`, after parsing. The engine sets `$hosts` (and `$weights` where
/// passed) to an empty array before calling, as `zend_try_array_init`
/// does, and assigns the lists after, also when [`MxHosts::found`] is
/// `false`: php keeps the hosts it added before a parse failure.
fn query_mx(hostname: &CStr, weights: bool) -> MxHosts {
    let mut out = MxHosts::default();
    let Some(handle) = ffi::DnsHandle::open() else {
        return out;
    };
    let mut answer = vec![0u8; QUERYBUF];
    let mut from = ffi::DnsFrom::new();
    let Ok(n) = handle.search(hostname, C_IN, u32::from(DNS_T_MX), &mut answer, &mut from) else {
        return out;
    };
    drop(handle);
    let ans = Answer {
        buf: &answer,
        end: n,
    };
    if mx_list(&ans, weights, &mut out) {
        out.found = !out.hosts.is_empty();
    }
    out
}

/// The parse loop of `dns_get_mx`, `Z/ext/standard/dns.c:1107`: `false`
/// where php gives `false` early.
fn mx_list(ans: &Answer, weights: bool, out: &mut MxHosts) -> bool {
    let mut cp = HFIXEDSZ;
    for _ in 0..get16(ans.buf, 4) {
        let Ok(i) = usize::try_from(ffi::dn_skipname(ans.msg(), cp)) else {
            return false;
        };
        cp = cp.saturating_add(i + QFIXEDSZ);
    }
    let mut count = i32::from(get16(ans.buf, 6));
    loop {
        count -= 1;
        if count < 0 || cp >= ans.end {
            break;
        }
        let Ok(i) = usize::try_from(ffi::dn_skipname(ans.msg(), cp)) else {
            return false;
        };
        cp = cp.saturating_add(i);
        let ty = get16(ans.buf, cp);
        cp = cp.saturating_add(2 + 2 + 4);
        let i = usize::from(get16(ans.buf, cp));
        cp = cp.saturating_add(2);
        if ty != DNS_T_MX {
            cp = cp.saturating_add(i);
            continue;
        }
        let weight = get16(ans.buf, cp);
        cp = cp.saturating_add(2);
        let Some((i, name)) = ffi::dn_expand(ans.msg(), cp, MAXHOSTNAMELEN - 1) else {
            return false;
        };
        cp = cp.saturating_add(i);
        out.hosts.push(name);
        if weights {
            out.weights.push(i64::from(weight));
        }
    }
    true
}

/// `PHP_FUNCTION(gethostname)`, `Z/ext/standard/dns.c:125`: the name, or
/// the warning of a `false`.
fn lookup_hostname() -> Result<Vec<u8>, Warning> {
    ffi::gethostname(HOST_NAME_MAX + 1).map_err(|(errno, text)| {
        let mut w = format!("Unable to fetch host [{errno}]: ").into_bytes();
        w.extend_from_slice(&text);
        (E_WARNING, w)
    })
}

/// `PHP_FUNCTION(gethostbyaddr)`, `Z/ext/standard/dns.c:146`: the name, or
/// the warning of a `false`.
fn lookup_addr(ip: &CStr) -> Result<Vec<u8>, Warning> {
    php_gethostbyaddr(ip).ok_or_else(|| {
        (
            E_WARNING,
            b"Address is not a valid IPv4 or IPv6 address".to_vec(),
        )
    })
}

/// `php_gethostbyaddr`, `Z/ext/standard/dns.c:172`: `None` for an address
/// that is neither IPv6 nor IPv4; the address itself where no name is found.
fn php_gethostbyaddr(ip: &CStr) -> Option<Vec<u8>> {
    if let Some(addr) = ffi::inet_pton6(ip) {
        return Some(ffi::name_of_v6(addr).unwrap_or_else(|| ip.to_bytes().to_vec()));
    }
    if let Some(addr) = ffi::inet_pton4(ip) {
        return Some(ffi::name_of_v4(addr).unwrap_or_else(|| ip.to_bytes().to_vec()));
    }
    None
}

/// `PHP_FUNCTION(gethostbyname)`, `Z/ext/standard/dns.c:219`: the string it
/// gives, and the warning raised before it, if any.
fn lookup_name(hostname: &CStr) -> (Vec<u8>, Option<Warning>) {
    let bytes = hostname.to_bytes();
    if bytes.len() > MAXFQDNLEN {
        // name too long, protect from CVE-2015-0235
        return (bytes.to_vec(), Some(too_long()));
    }
    (php_gethostbyname(hostname), None)
}

/// The warning of `Z/ext/standard/dns.c:231`.
fn too_long() -> Warning {
    let text = format!("Host name cannot be longer than {MAXFQDNLEN} characters");
    (E_WARNING, text.into_bytes())
}

/// Dotted quad text, as the `inet_ntop(AF_INET, ...)` of
/// `Z/ext/standard/dns.c:314` writes it.
fn dotted(a: [u8; 4]) -> Vec<u8> {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]).into_bytes()
}

/// `php_gethostbyname`, `Z/ext/standard/dns.c:293`: the first IPv4 address,
/// or the name where there is none. php's `inet_ntop` failure, the `NULL`
/// that becomes the `Host name to ip failed` warning, cannot happen for a
/// 16-byte buffer and is left out.
fn php_gethostbyname(name: &CStr) -> Vec<u8> {
    match ffi::ipv4_addresses(name).as_deref() {
        Some([first, ..]) => dotted(*first),
        _ => name.to_bytes().to_vec(),
    }
}

/// `PHP_FUNCTION(gethostbynamel)`, `Z/ext/standard/dns.c:245`: the list,
/// `None` for `false`, and the warning raised, if any. As in
/// [`php_gethostbyname`], the `inet_ntop` warning cannot happen.
fn lookup_names(hostname: &CStr) -> (Option<Vec<Vec<u8>>>, Option<Warning>) {
    if hostname.to_bytes().len() > MAXFQDNLEN {
        // name too long, protect from CVE-2015-0235
        return (None, Some(too_long()));
    }
    let list = ffi::ipv4_addresses(hostname).map(|l| l.into_iter().map(dotted).collect());
    (list, None)
}
