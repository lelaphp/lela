//! The arguments openssl.c reads the same way in many natives, each
//! decoded once at the boundary by one function: a path
//! (`php_openssl_check_path_ex`), a certificate
//! (`php_openssl_x509_from_param`, `php_openssl_x509_from_zval`), a
//! signing request (`php_openssl_csr_from_param`), a list of
//! certificates (`php_openssl_array_to_X509_sk`) and the certificates of
//! a file (`php_openssl_load_all_certs_from_file`). The key an argument
//! gives is [`crate::key::key_from`].

use std::ffi::{CStr, CString};
use std::ops::ControlFlow;

use php_ext::abi::{Instance, OrFalse, Union};
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, argument_error, docref, raise};
use php_ext::request::{Request, Resolve};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpString, Value, ValueRef, View};

use crate::csr::OpenSSLCertificateSigningRequest;
use crate::ffi::x509::{CertStack, Csr, read_all_certs};
use crate::ffi::{Bio, Certificate};
use crate::x509::OpenSSLCertificate;
use crate::{State, store_errors};

/// A parameter as its messages name it: one-based position and declared
/// name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Arg {
    pub(crate) position: u32,
    pub(crate) name: &'static str,
}

impl Arg {
    pub(crate) const fn new(position: u32, name: &'static str) -> Arg {
        Arg { position, name }
    }
}

/// Where a path came from, as `php_openssl_check_path_ex`'s `arg_num`,
/// `is_from_array` and `option_name` say it in the messages it raises.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Origin {
    /// A parameter itself.
    Arg(Arg),
    /// An item of an array parameter, under an option or none.
    Item(Arg, Option<&'static str>),
    /// A configuration setting, no parameter at all.
    Setting(&'static str),
}

/// A throwable raised the way php raises one in a function that goes on
/// and returns failure (`zend_argument_error` in
/// `php_openssl_check_path_ex`): it stands, as `EG(exception)` stands,
/// while the native raises what warnings it still raises, and is thrown
/// when the native returns ([`finish`]). A second one while the first
/// stands is given up.
pub(crate) fn defer(req: &mut Request, thrown: Thrown) {
    let state = req.state::<State>();
    if state.thrown.is_none() {
        state.thrown = Some(thrown);
    } else {
        php_ext::raising::clear(req, thrown).ok();
    }
}

/// `EG(exception)` as a native tests it before a warning php skips
/// while a throwable stands.
pub(crate) fn raised(req: &mut Request) -> bool {
    req.state::<State>().thrown.is_some()
}

/// A native's body, and what [`defer`] held thrown when it returns,
/// whatever it answered: php's return with `EG(exception)` set.
pub(crate) fn finish<T: Answer>(
    req: &mut Request,
    body: impl FnOnce(&mut Request) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    let out = body(req);
    match req.state::<State>().thrown.take() {
        Some(thrown) => {
            match out {
                Ok(answer) => answer.discard(req),
                Err(other) => {
                    php_ext::raising::clear(req, other).ok();
                }
            }
            Err(thrown)
        }
        None => out,
    }
}

/// What a native answers through [`finish`], thrown away where the
/// deferred error wins: an owner given up, a scalar dropped.
pub(crate) trait Answer {
    fn discard(self, req: &mut Request);
}

impl Answer for bool {
    fn discard(self, _: &mut Request) {}
}

impl Answer for i64 {
    fn discard(self, _: &mut Request) {}
}

impl<T: Answer> Answer for OrFalse<T> {
    fn discard(self, req: &mut Request) {
        if let OrFalse::Value(answer) = self {
            answer.discard(req);
        }
    }
}

impl<A: Answer, B: Answer> Answer for Union<A, B> {
    fn discard(self, req: &mut Request) {
        match self {
            Union::A(answer) => answer.discard(req),
            Union::B(answer) => answer.discard(req),
        }
    }
}

impl<T: View> Answer for Owner<T> {
    fn discard(self, req: &mut Request) {
        self.give_up(req);
    }
}

/// A byte string as C reads it: up to its first NUL.
pub(crate) fn c_string(bytes: &[u8]) -> CString {
    CString::new(php_ext::coerce::c_str(bytes)).unwrap_or_default()
}

/// `php_openssl_check_path_ex`: the path made absolute, `file://` taken
/// off first where `scheme`, and held to the `open_basedir` guard; the
/// empty path is itself. `None` where it cannot be or is refused, after
/// the warning php raises, and a `ValueError` for a NUL in it where a
/// parameter gave it.
pub(crate) fn check_path(
    req: &mut Request,
    path: &[u8],
    origin: Origin,
    scheme: bool,
) -> Option<CString> {
    if path.is_empty() {
        return Some(CString::default());
    }
    let path = if scheme {
        path.strip_prefix(b"file://")
            .filter(|rest| !rest.is_empty())?
    } else {
        path
    };
    let (fatal, text) = if path.contains(&0) {
        (true, "must not contain any null bytes")
    } else {
        let cwd = req
            .environment()
            .cwd()
            .into_os_string()
            .into_encoded_bytes();
        match req
            .environment_mut()
            .virtual_file_ex(&cwd, path, Resolve::FilePath)
        {
            // The expanded path is held to the `open_basedir` guard,
            // which warns (`php_check_open_basedir`).
            Some(real) => {
                return req
                    .environment_mut()
                    .check_open_basedir(&real, true)
                    .ok()
                    .map(|_| c_string(&real));
            }
            None => (false, "must be a valid file path"),
        }
    };
    let (arg, tail) = match origin {
        Origin::Setting(option) => {
            let message = format!("Path for {option} option {text}");
            docref(req, Level::Warning, &message);
            return None;
        }
        Origin::Arg(arg) => (arg, text.to_owned()),
        Origin::Item(arg, Some(option)) => (arg, format!("option {option} array item {text}")),
        Origin::Item(arg, None) => (arg, format!("array item {text}")),
    };
    if fatal {
        let thrown = argument_error(req, "ValueError", arg.position, &tail);
        defer(req, thrown);
        return None;
    }
    // `php_verror(NULL, arg_name, E_WARNING, ...)`: the parameter's name
    // between the parentheses.
    let message = format!(
        "{}({}): {tail}",
        req.frames().active_function_name(),
        arg.name
    );
    raise(req, Level::Warning, &message);
    None
}

/// `OpenSSLCertificate|string`, `Z_PARAM_OBJ_OF_CLASS_OR_STR`.
pub(crate) type CertParam = Union<Instance<OpenSSLCertificate>, PhpString>;

/// `OpenSSLCertificateSigningRequest|string`.
pub(crate) type CsrParam = Union<Instance<OpenSSLCertificateSigningRequest>, PhpString>;

/// `php_openssl_x509_from_str`: a certificate from a `file://` path (any
/// PEM certificate label) or from the string itself (`CERTIFICATE`
/// alone), what the library said of a failure stored.
pub(crate) fn cert_from_str(req: &mut Request, text: &[u8], origin: Origin) -> Option<Certificate> {
    let cert = if text.len() > 7 && text.starts_with(b"file://") {
        let path = check_path(req, text, origin, true)?;
        let Some(bio) = Bio::file(&path, c"r") else {
            store_errors(req);
            return None;
        };
        Certificate::read_pem(&bio)
    } else {
        let Some(bio) = Bio::reading(text) else {
            store_errors(req);
            return None;
        };
        Certificate::read_pem_certificate(&bio)
    };
    if cert.is_none() {
        store_errors(req);
    }
    cert
}

/// `php_openssl_x509_from_param`: the certificate an object holds, or
/// the one its string names.
pub(crate) fn cert_from_param(
    req: &mut Request,
    param: &CertParam,
    arg: Arg,
) -> Result<Option<Certificate>, Thrown> {
    match param {
        Union::A(object) => Ok(object.payload_mut(req)?.cert.clone()),
        Union::B(text) => Ok(cert_from_str(req, text.as_bytes(), Origin::Arg(arg))),
    }
}

/// `php_openssl_x509_from_zval`: an `OpenSSLCertificate`'s, or the one
/// the value's string names; a string conversion that throws is thrown.
pub(crate) fn cert_from_value(
    req: &mut Request,
    value: &Value,
    origin: Origin,
) -> Result<Option<Certificate>, Thrown> {
    if let ValueRef::Object(object) = value.as_ref()
        && let Some(held) = object.native::<crate::x509::Cert>(req)
    {
        return Ok(held.cert.clone());
    }
    let text = with_string(req, value, <[u8]>::to_vec)?;
    Ok(cert_from_str(req, &text, origin))
}

/// `php_openssl_array_to_X509_sk`: the certificates an array lists, or
/// the one certificate a single value is; `None` once one is not a
/// certificate.
pub(crate) fn cert_stack(
    req: &mut Request,
    value: &Value,
    arg: Arg,
    option: Option<&'static str>,
) -> Result<Option<CertStack>, Thrown> {
    let Some(mut stack) = CertStack::new() else {
        return Ok(None);
    };
    // One certificate onto the stack: `false` once it is not one, or the
    // stack refused it.
    let mut pushed = |req: &mut Request, item: &Value, origin: Origin| -> Result<bool, Thrown> {
        let Some(cert) = cert_from_value(req, item, origin)? else {
            return Ok(false);
        };
        if !stack.push(cert) {
            store_errors(req);
            return Ok(false);
        }
        Ok(true)
    };
    let Some(array) = value.as_array().copied() else {
        let pushed = pushed(req, value, Origin::Arg(arg))?;
        return Ok(pushed.then_some(stack));
    };
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(zcerts), zcertval)`
    // (`Z/ext/openssl/openssl_backend_common.c:872`), no owner of the
    // table taken: the argument holds it while a certificate's string
    // runs `__toString`.
    let refused = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let item = row.value().deref();
        if pushed(req, &item, Origin::Item(arg, option))? {
            Ok(ControlFlow::Continue(()))
        } else {
            Ok(ControlFlow::Break(()))
        }
    })?;
    if refused.is_some() {
        return Ok(None);
    }
    Ok(Some(stack))
}

/// `php_openssl_csr_from_str`: a request from a `file://` path or from
/// the string itself.
fn csr_from_str(req: &mut Request, text: &[u8], arg: Arg) -> Option<Csr> {
    let bio = if text.len() > 7 && text.starts_with(b"file://") {
        let path = check_path(req, text, Origin::Arg(arg), true)?;
        Bio::file(&path, c"r")
    } else {
        Bio::reading(text)
    };
    let Some(bio) = bio else {
        store_errors(req);
        return None;
    };
    let csr = Csr::read_pem(&bio);
    if csr.is_none() {
        store_errors(req);
    }
    csr
}

/// `php_openssl_csr_from_param`: a copy of the request an object holds,
/// or the one its string names.
pub(crate) fn csr_from_param(
    req: &mut Request,
    param: &CsrParam,
    arg: Arg,
) -> Result<Option<Csr>, Thrown> {
    match param {
        Union::A(object) => Ok(object
            .payload_mut(req)?
            .csr
            .as_ref()
            .and_then(Csr::duplicate)),
        Union::B(text) => Ok(csr_from_str(req, text.as_bytes(), arg)),
    }
}

/// `php_openssl_load_all_certs_from_file`: every certificate of a PEM
/// file, after php's warnings for a file that does not open, read, or
/// hold one.
pub(crate) fn certs_from_file(req: &mut Request, file: &[u8], arg: Arg) -> Option<CertStack> {
    let path = check_path(req, file, Origin::Arg(arg), false)?;
    let shown = String::from_utf8_lossy(path.as_bytes()).into_owned();
    let Some(bio) = Bio::file(&path, c"r") else {
        store_errors(req);
        docref(
            req,
            Level::Warning,
            &format!("Error opening the file, {shown}"),
        );
        return None;
    };
    let Some(stack) = read_all_certs(&bio) else {
        store_errors(req);
        docref(
            req,
            Level::Warning,
            &format!("Error reading the file, {shown}"),
        );
        return None;
    };
    if stack.len() == 0 {
        docref(
            req,
            Level::Warning,
            &format!("No certificates in file, {shown}"),
        );
        return None;
    }
    Some(stack)
}

/// `php_openssl_bio_new_file`: the file a path parameter names, opened
/// for `mode`; `None` after php's warning or with the library's reasons
/// stored.
pub(crate) fn open_file(
    req: &mut Request,
    file: &[u8],
    arg: Arg,
    mode: &CStr,
) -> Option<Bio<'static>> {
    let path = check_path(req, file, Origin::Arg(arg), false)?;
    let bio = Bio::file(&path, mode);
    if bio.is_none() {
        store_errors(req);
    }
    bio
}
