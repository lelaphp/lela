//! The arguments openssl.c reads the same way in many natives, each
//! decoded once at the boundary by one function: a path
//! (`php_openssl_check_path_ex`), a certificate
//! (`php_openssl_x509_from_param`), a signing request
//! (`php_openssl_csr_from_param`) and the certificates of a file
//! (`php_openssl_load_all_certs_from_file`). The key an argument gives is
//! [`crate::key::key_from`].

use std::ffi::CString;

use php_ext::abi::{Instance, Union};
use php_ext::raising::{Level, Thrown, argument_error, docref, raise};
use php_ext::request::{PathIntent, Request, Resolve};
use php_ext::values::PhpString;

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
        let _ = php_ext::raising::clear(req, thrown);
    }
}

/// `EG(exception)` as a native tests it before a warning php skips
/// while a throwable stands.
pub(crate) fn raised(req: &mut Request) -> bool {
    req.state::<State>().thrown.is_some()
}

/// A native's body, and what [`defer`] held thrown when it returns,
/// whatever it answered: php's return with `EG(exception)` set.
pub(crate) fn finish<T>(
    req: &mut Request,
    body: impl FnOnce(&mut Request) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    let out = body(req);
    match req.state::<State>().thrown.take() {
        Some(thrown) => {
            if let Err(other) = out {
                let _ = php_ext::raising::clear(req, other);
            }
            Err(thrown)
        }
        None => out,
    }
}

/// A byte string as C reads it: up to its first NUL.
pub(crate) fn c_string(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// `php_openssl_check_path_ex`: the path made absolute, `file://` taken
/// off first where `scheme`; the empty path is itself. `None` where it
/// cannot be, after the warning php raises, and a `ValueError` for a NUL
/// in it where a parameter gave it.
pub(crate) fn check_path(
    req: &mut Request,
    path: &[u8],
    origin: Origin,
    scheme: bool,
) -> Result<Option<CString>, Thrown> {
    if path.is_empty() {
        return Ok(Some(CString::default()));
    }
    let path = if scheme {
        match path
            .strip_prefix(b"file://")
            .filter(|rest| !rest.is_empty())
        {
            Some(rest) => rest,
            None => return Ok(None),
        }
    } else {
        path
    };
    let (fatal, text) = if path.contains(&0) {
        (true, "must not contain any null bytes")
    } else {
        let cwd = req.cwd().0.into_os_string().into_encoded_bytes();
        match req.virtual_file_ex(&cwd, path, Resolve::FilePath) {
            Some(real) => {
                req.fs_guard(&req.absolutize(&real), PathIntent::Read)?;
                return Ok(Some(c_string(&real)));
            }
            None => (false, "must be a valid file path"),
        }
    };
    let (arg, tail) = match origin {
        Origin::Setting(option) => {
            let message = format!("Path for {option} option {text}");
            docref(req, Level::Warning, &message);
            return Ok(None);
        }
        Origin::Arg(arg) => (arg, text.to_owned()),
        Origin::Item(arg, Some(option)) => (arg, format!("option {option} array item {text}")),
        Origin::Item(arg, None) => (arg, format!("array item {text}")),
    };
    if fatal {
        let thrown = argument_error(req, "ValueError", arg.position, &tail);
        defer(req, thrown);
        return Ok(None);
    }
    // `php_verror(NULL, arg_name, E_WARNING, ...)`: the parameter's name
    // between the parentheses.
    let message = format!("{}({}): {tail}", req.active_function_name(), arg.name);
    raise(req, Level::Warning, &message);
    Ok(None)
}

/// `OpenSSLCertificate|string`, `Z_PARAM_OBJ_OF_CLASS_OR_STR`.
pub(crate) type CertParam = Union<Instance<OpenSSLCertificate>, PhpString>;

/// `OpenSSLCertificateSigningRequest|string`.
pub(crate) type CsrParam = Union<Instance<OpenSSLCertificateSigningRequest>, PhpString>;

/// `php_openssl_x509_from_str`: a certificate from a `file://` path (any
/// PEM certificate label) or from the string itself (`CERTIFICATE`
/// alone), what the library said of a failure stored.
pub(crate) fn cert_from_str(
    req: &mut Request,
    text: &[u8],
    origin: Origin,
) -> Result<Option<Certificate>, Thrown> {
    let cert = if text.len() > 7 && text.starts_with(b"file://") {
        let Some(path) = check_path(req, text, origin, true)? else {
            return Ok(None);
        };
        let Some(bio) = Bio::file(&path, c"r") else {
            store_errors(req);
            return Ok(None);
        };
        Certificate::read_pem(&bio)
    } else {
        let Some(bio) = Bio::reading(text) else {
            store_errors(req);
            return Ok(None);
        };
        Certificate::read_pem_certificate(&bio)
    };
    if cert.is_none() {
        store_errors(req);
    }
    Ok(cert)
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
        Union::B(text) => cert_from_str(req, text.as_bytes(), Origin::Arg(arg)),
    }
}

/// `php_openssl_csr_from_str`: a request from a `file://` path or from
/// the string itself.
fn csr_from_str(req: &mut Request, text: &[u8], arg: Arg) -> Result<Option<Csr>, Thrown> {
    let bio = if text.len() > 7 && text.starts_with(b"file://") {
        let Some(path) = check_path(req, text, Origin::Arg(arg), true)? else {
            return Ok(None);
        };
        Bio::file(&path, c"r")
    } else {
        Bio::reading(text)
    };
    let Some(bio) = bio else {
        store_errors(req);
        return Ok(None);
    };
    let csr = Csr::read_pem(&bio);
    if csr.is_none() {
        store_errors(req);
    }
    Ok(csr)
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
        Union::B(text) => csr_from_str(req, text.as_bytes(), arg),
    }
}

/// `php_openssl_load_all_certs_from_file`: every certificate of a PEM
/// file, after php's warnings for a file that does not open, read, or
/// hold one.
pub(crate) fn certs_from_file(
    req: &mut Request,
    file: &[u8],
    arg: Arg,
) -> Result<Option<CertStack>, Thrown> {
    let Some(path) = check_path(req, file, Origin::Arg(arg), false)? else {
        return Ok(None);
    };
    let shown = String::from_utf8_lossy(path.as_bytes()).into_owned();
    let Some(bio) = Bio::file(&path, c"r") else {
        store_errors(req);
        docref(
            req,
            Level::Warning,
            &format!("Error opening the file, {shown}"),
        );
        return Ok(None);
    };
    let Some(stack) = read_all_certs(&bio) else {
        store_errors(req);
        docref(
            req,
            Level::Warning,
            &format!("Error reading the file, {shown}"),
        );
        return Ok(None);
    };
    if stack.len() == 0 {
        docref(
            req,
            Level::Warning,
            &format!("No certificates in file, {shown}"),
        );
        return Ok(None);
    }
    Ok(Some(stack))
}
