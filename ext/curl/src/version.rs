//! `curl_version()` and `PHP_MINFO_FUNCTION(curl)`: what the linked
//! libcurl says it is, read from `curl_version_info`.

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray};

use crate::constants as c;
use crate::ffi::{Version, version};
use crate::info::{Assoc, list};

/// The features `curl_version()` and the info table name, in php's
/// order, with the bit each is.
const FEATURES: [(&str, i64); 29] = [
    ("AsynchDNS", c::CURL_VERSION_ASYNCHDNS),
    ("CharConv", c::CURL_VERSION_CONV),
    ("Debug", c::CURL_VERSION_DEBUG),
    ("GSS-Negotiate", c::CURL_VERSION_GSSNEGOTIATE),
    ("IDN", c::CURL_VERSION_IDN),
    ("IPv6", c::CURL_VERSION_IPV6),
    ("krb4", c::CURL_VERSION_KERBEROS4),
    ("Largefile", c::CURL_VERSION_LARGEFILE),
    ("libz", c::CURL_VERSION_LIBZ),
    ("NTLM", c::CURL_VERSION_NTLM),
    ("NTLMWB", c::CURL_VERSION_NTLM_WB),
    ("SPNEGO", c::CURL_VERSION_SPNEGO),
    ("SSL", c::CURL_VERSION_SSL),
    ("SSPI", c::CURL_VERSION_SSPI),
    ("TLS-SRP", c::CURL_VERSION_TLSAUTH_SRP),
    ("HTTP2", c::CURL_VERSION_HTTP2),
    ("GSSAPI", c::CURL_VERSION_GSSAPI),
    ("KERBEROS5", c::CURL_VERSION_KERBEROS5),
    ("UNIX_SOCKETS", c::CURL_VERSION_UNIX_SOCKETS),
    ("PSL", c::CURL_VERSION_PSL),
    ("HTTPS_PROXY", c::CURL_VERSION_HTTPS_PROXY),
    ("MULTI_SSL", c::CURL_VERSION_MULTI_SSL),
    ("BROTLI", c::CURL_VERSION_BROTLI),
    ("ALTSVC", c::CURL_VERSION_ALTSVC),
    ("HTTP3", c::CURL_VERSION_HTTP3),
    ("UNICODE", c::CURL_VERSION_UNICODE),
    ("ZSTD", c::CURL_VERSION_ZSTD),
    ("HSTS", c::CURL_VERSION_HSTS),
    ("GSASL", c::CURL_VERSION_GSASL),
];

/// `CAAS`: a string, `""` for none.
fn add_str(
    req: &mut Request,
    out: &mut Assoc,
    key: &str,
    text: Option<&[u8]>,
) -> Result<(), Thrown> {
    out.add_str(req, key, text.unwrap_or_default())
}

fn array(req: &mut Request, d: &Version) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = Assoc::new(req)?;
    out.add(req, "version_number", Owner::int(d.version_num))?;
    out.add(req, "age", Owner::int(d.age))?;
    out.add(req, "features", Owner::int(d.features))?;
    let mut features = Assoc::new(req)?;
    for (name, bit) in FEATURES {
        features.add(req, name, Owner::bool(d.features & bit != 0))?;
    }
    out.add(req, "feature_list", features.value())?;
    out.add(req, "ssl_version_number", Owner::int(d.ssl_version_num))?;
    add_str(req, &mut out, "version", Some(&d.version))?;
    add_str(req, &mut out, "host", Some(&d.host))?;
    add_str(req, &mut out, "ssl_version", d.ssl_version.as_deref())?;
    add_str(req, &mut out, "libz_version", d.libz_version.as_deref())?;
    let protocols = list(req, &d.protocols)?;
    out.add(req, "protocols", protocols)?;
    if d.age >= 1 {
        add_str(req, &mut out, "ares", d.ares.as_deref())?;
        out.add(req, "ares_num", Owner::int(d.ares_num))?;
    }
    if d.age >= 2 {
        add_str(req, &mut out, "libidn", d.libidn.as_deref())?;
    }
    if d.age >= 3 {
        out.add(req, "iconv_ver_num", Owner::int(d.iconv_ver_num))?;
        add_str(req, &mut out, "libssh_version", d.libssh_version.as_deref())?;
    }
    if d.age >= 4 {
        out.add(req, "brotli_ver_num", Owner::int(d.brotli_ver_num))?;
        add_str(req, &mut out, "brotli_version", d.brotli_version.as_deref())?;
    }
    Ok(out.0)
}

/// `curl_version(): array|false`.
///
/// ```php
/// $v = curl_version();
/// var_dump(array_keys($v), is_array($v['feature_list']), in_array('file', $v['protocols']), $v['version_number'] >= 0x080000);
/// ```
/// ```expect
/// array(17) {
///   [0]=>
///   string(14) "version_number"
///   [1]=>
///   string(3) "age"
///   [2]=>
///   string(8) "features"
///   [3]=>
///   string(12) "feature_list"
///   [4]=>
///   string(18) "ssl_version_number"
///   [5]=>
///   string(7) "version"
///   [6]=>
///   string(4) "host"
///   [7]=>
///   string(11) "ssl_version"
///   [8]=>
///   string(12) "libz_version"
///   [9]=>
///   string(9) "protocols"
///   [10]=>
///   string(4) "ares"
///   [11]=>
///   string(8) "ares_num"
///   [12]=>
///   string(6) "libidn"
///   [13]=>
///   string(13) "iconv_ver_num"
///   [14]=>
///   string(14) "libssh_version"
///   [15]=>
///   string(14) "brotli_ver_num"
///   [16]=>
///   string(14) "brotli_version"
/// }
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn curl_version(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    match version() {
        Some(d) => array(req, &d).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `PHP_MINFO_FUNCTION(curl)`: the version, each feature, the protocols
/// and the libraries libcurl names, then the ini entry.
///
/// ```php
/// ob_start();
/// (new ReflectionExtension('curl'))->info();
/// $info = ob_get_clean();
/// echo implode("\n", array_slice(explode("\n", $info), 0, 6)), "\n";
/// var_dump(str_contains($info, "\nProtocols => "), str_contains($info, "curl.cainfo => no value => no value"));
/// ```
/// ```expect
///
/// curl
///
/// cURL support => enabled
/// cURL Information => %s
/// Age => %d
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn curl_info(req: &mut Request) {
    let Some(d) = version() else {
        return;
    };
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let mut rows: Vec<Vec<String>> = vec![
        vec!["cURL support".into(), "enabled".into()],
        vec!["cURL Information".into(), text(&d.version)],
        vec!["Age".into(), d.age.to_string()],
    ];
    if d.features != 0 {
        rows.push(vec!["Features".into()]);
        for (name, bit) in FEATURES {
            let on = if d.features & bit != 0 { "Yes" } else { "No" };
            rows.push(vec![name.into(), on.into()]);
        }
    }
    let protocols: Vec<String> = d.protocols.iter().map(|p| text(p)).collect();
    rows.push(vec!["Protocols".into(), protocols.join(", ")]);
    rows.push(vec!["Host".into(), text(&d.host)]);
    let named = [
        ("SSL Version", &d.ssl_version),
        ("ZLib Version", &d.libz_version),
        ("ZLib Version", &d.ares),
        ("libIDN Version", &d.libidn),
        ("libSSH Version", &d.libssh_version),
    ];
    for (label, value) in named {
        if let Some(value) = value {
            rows.push(vec![label.into(), text(value)]);
        }
    }
    let borrowed: Vec<Vec<&str>> = rows
        .iter()
        .map(|row| row.iter().map(String::as_str).collect())
        .collect();
    let cells: Vec<&[&str]> = borrowed.iter().map(Vec::as_slice).collect();
    req.extensions_mut().info_table(&cells);
    req.ini_mut().info_ini_entries(&[crate::CAINFO]);
}
