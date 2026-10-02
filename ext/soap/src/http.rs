//! `php_http.c`: the headers a client's credentials and the `http.header`
//! option of its stream add, which the WSDL load and the SOAP request
//! share.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpObject, Resource, ValueRef};
use php_ext_standard::{base64, stream_ctx_option};

use crate::shared::{ClientSlot, client_slot};

/// `Authorization: Basic` or `Proxy-Authorization: Basic` over `login`
/// and an optional password.
fn basic(soap_headers: &mut Vec<u8>, header: &[u8], login: &[u8], password: Option<&[u8]>) {
    let mut auth = login.to_vec();
    auth.push(b':');
    if let Some(p) = password {
        auth.extend_from_slice(p);
    }
    soap_headers.extend_from_slice(header);
    soap_headers.extend_from_slice(b": Basic ");
    soap_headers.extend_from_slice(&base64::encode(&auth));
    soap_headers.extend_from_slice(b"\r\n");
}

/// `proxy_authentication`: the client's `proxy_login` and
/// `proxy_password` as `Proxy-Authorization: Basic`, where it has a
/// login.
pub(crate) fn proxy_authentication(this: PhpObject, soap_headers: &mut Vec<u8>) -> bool {
    let login = client_slot(this, ClientSlot::ProxyLogin);
    let Some(login) = login.as_str_bytes() else {
        return false;
    };
    let password = client_slot(this, ClientSlot::ProxyPassword);
    basic(
        soap_headers,
        b"Proxy-Authorization",
        login,
        password.as_str_bytes(),
    );
    true
}

/// `basic_authentication`: the client's `login` and `password` as
/// `Authorization: Basic`, where it has a login and does not use digest.
pub(crate) fn basic_authentication(this: PhpObject, soap_headers: &mut Vec<u8>) -> bool {
    let login = client_slot(this, ClientSlot::Login);
    let digest = client_slot(this, ClientSlot::UseDigest);
    let Some(login) = login.as_str_bytes() else {
        return false;
    };
    if matches!(digest.as_ref(), ValueRef::Bool(true)) {
        return false;
    }
    let password = client_slot(this, ClientSlot::Password);
    basic(
        soap_headers,
        b"Authorization",
        login,
        password.as_str_bytes(),
    );
    true
}

/// `http_context_add_header`: each `name: value` line of `s` appended,
/// but the headers the client writes itself.
fn http_context_add_header(
    s: &[u8],
    has_authorization: bool,
    has_proxy_authorization: bool,
    has_cookies: bool,
    soap_headers: &mut Vec<u8>,
) {
    let mut at = 0;
    while at < s.len() {
        while at < s.len() && matches!(s[at], b' ' | b'\t' | b'\r' | b'\n') {
            at += 1;
        }
        let start = at;
        let mut p = at;
        let mut name_len: Option<usize> = None;
        while p < s.len() {
            match s[p] {
                b':' => {
                    if name_len.is_none() {
                        name_len = Some(p - start);
                    }
                    break;
                }
                b' ' | b'\t' if name_len.is_none() => name_len = Some(p - start),
                b'\r' | b'\n' => break,
                _ => {}
            }
            p += 1;
        }
        if p < s.len() && s[p] == b':' {
            while p < s.len() && s[p] != b'\r' && s[p] != b'\n' {
                p += 1;
            }
            let name = &s[start..start + name_len.unwrap_or(0)];
            let is = |n: &[u8]| name.eq_ignore_ascii_case(n);
            let skipped = is(b"host")
                || is(b"connection")
                || is(b"user-agent")
                || is(b"content-length")
                || is(b"content-type")
                || (has_cookies && is(b"cookie"))
                || (has_authorization && is(b"authorization"))
                || (has_proxy_authorization && is(b"proxy-authorization"));
            if !skipped {
                soap_headers.extend_from_slice(&s[start..p]);
                soap_headers.extend_from_slice(b"\r\n");
            }
        }
        at = if p < s.len() { p + 1 } else { p };
    }
}

/// `http_context_headers`: the `http.header` option of `context`, a
/// string or an array of strings, through [`http_context_add_header`].
pub(crate) fn http_context_headers(
    req: &mut Request,
    context: Option<Resource>,
    has_authorization: bool,
    has_proxy_authorization: bool,
    has_cookies: bool,
    soap_headers: &mut Vec<u8>,
) -> Result<(), Thrown> {
    let Some(context) = context else {
        return Ok(());
    };
    let Some(header) = stream_ctx_option(req, context, b"http", b"header")? else {
        return Ok(());
    };
    let flags = (has_authorization, has_proxy_authorization, has_cookies);
    match header.as_ref() {
        ValueRef::Str(s) if !s.is_empty() => {
            http_context_add_header(s.as_bytes(), flags.0, flags.1, flags.2, soap_headers);
        }
        ValueRef::Array(a) => {
            for (_, value) in a.snapshot() {
                if let ValueRef::Str(s) = value.as_ref()
                    && !s.is_empty()
                {
                    http_context_add_header(s.as_bytes(), flags.0, flags.1, flags.2, soap_headers);
                }
            }
        }
        _ => {}
    }
    Ok(())
}
