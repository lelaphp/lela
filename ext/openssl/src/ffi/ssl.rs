//! TLS over a socket, libssl as `xp_ssl.c` drives it: [`Ctx`], one
//! `SSL_CTX` configured as `php_openssl_setup_crypto` configures it;
//! [`Setup`], the `context` a socket's crypto is being set up in with the
//! [`Hooks`] libssl's callbacks read; and [`Tls`], the `SSL` made in it,
//! which holds both. The hooks are reached from C through the `SSL`'s
//! application data and the callbacks' arguments, so they are owned
//! through a raw pointer, freed after the `SSL`, and lent to Rust only
//! between library calls.

use std::ffi::{CStr, c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_void};
use std::os::fd::RawFd;
use std::ptr::{self, NonNull};

use openssl_sys as sys;

use super::{BIO_free, BIO_new_file, Certificate, copied, stack_items};
use crate::xp_ssl::{Cipher, Hooks, Io, San};

// SAFETY: libssl's and libcrypto's own declarations, from `openssl/ssl.h`,
// `openssl/x509.h`, `openssl/pem.h` and `openssl/err.h` of OpenSSL 3,
// which `openssl-sys` does not declare; the libraries `openssl-sys` links
// define each.
unsafe extern "C" {
    fn SSL_set_fd(ssl: *mut sys::SSL, fd: c_int) -> c_int;
    fn SSL_copy_session_id(to: *mut sys::SSL, from: *const sys::SSL) -> c_int;
    fn SSL_SESSION_is_resumable(s: *const sys::SSL_SESSION) -> c_int;
    fn SSL_set_info_callback(ssl: *mut sys::SSL, cb: Option<InfoCallback>);
    fn SSL_CTX_set_default_passwd_cb(ctx: *mut sys::SSL_CTX, cb: Option<PasswdCallback>);
    fn SSL_CTX_set_default_passwd_cb_userdata(ctx: *mut sys::SSL_CTX, u: *mut c_void);
    fn X509_NAME_get_text_by_NID(
        n: *const sys::X509_NAME,
        nid: c_int,
        buf: *mut c_char,
        len: c_int,
    ) -> c_int;
    fn PEM_read_bio_Parameters(bp: *mut sys::BIO, x: *mut *mut sys::EVP_PKEY)
    -> *mut sys::EVP_PKEY;
    fn ERR_peek_error() -> c_ulong;
    fn ERR_error_string_n(e: c_ulong, buf: *mut c_char, len: usize);
    fn GENERAL_NAME_get0_otherName(
        name: *const sys::GENERAL_NAME,
        oid: *mut *mut sys::ASN1_OBJECT,
        value: *mut *mut sys::ASN1_TYPE,
    ) -> c_int;
}

/// `id-on-dnsSRV` (RFC 4985), the otherName an SRV-ID is, which php
/// matches by its dotted OID.
const SRV_NAME_OID: &[u8] = b"1.3.6.1.5.5.7.8.7";

/// `SSL_set_info_callback`'s and `SSL_CTX_set_default_passwd_cb`'s
/// callback types.
type InfoCallback = unsafe extern "C" fn(*const sys::SSL, c_int, c_int);
type PasswdCallback = unsafe extern "C" fn(*mut c_char, c_int, c_int, *mut c_void) -> c_int;

/// `SSL_CB_HANDSHAKE_START`, `SSL_RECEIVED_SHUTDOWN`, `SSL_TLSEXT_ERR_OK`
/// and `TLSEXT_NAMETYPE_host_name`.
const SSL_CB_HANDSHAKE_START: c_int = 0x10;
const SSL_RECEIVED_SHUTDOWN: c_int = 2;
const SSL_TLSEXT_ERR_OK: c_int = 0;
const TLSEXT_NAMETYPE_HOST_NAME: c_int = 0;

/// The application data slot of an `SSL`, `SSL_set_app_data`'s index.
const APP_DATA: c_int = 0;

owned!(
    /// An `SSL_CTX`: one reference of its count.
    Ctx,
    sys::SSL_CTX,
    sys::SSL_CTX_free
);

// SAFETY: an SSL_CTX is used by one socket's calls at a time here, and its
// reference count is atomic; OpenSSL 3 lets it move between threads.
unsafe impl Send for Ctx {}

impl Ctx {
    /// `SSL_CTX_new(SSLv23_client_method())`, or the server's method.
    pub(crate) fn new(client: bool) -> Option<Ctx> {
        let method = if client {
            // SAFETY: the method is a static table of the library.
            unsafe { sys::TLS_client_method() }
        } else {
            // SAFETY: as above.
            unsafe { sys::TLS_server_method() }
        };
        // SAFETY: the method is one of the library's.
        Ctx::owned(unsafe { sys::SSL_CTX_new(method) })
    }

    /// `SSL_CTX_get_options`.
    pub(crate) fn options(&self) -> u64 {
        // SAFETY: the context is live.
        unsafe { sys::SSL_CTX_get_options(self.as_ptr()) }
    }

    /// `SSL_CTX_set_options`, which adds to what the `context` has.
    pub(crate) fn set_options(&self, options: u64) {
        // SAFETY: the context is live.
        unsafe { sys::SSL_CTX_set_options(self.as_ptr(), options) };
    }

    /// `SSL_VERIFY_PEER` with `verify_callback` where `peer`, else
    /// `php_openssl_disable_peer_verification`'s `SSL_VERIFY_NONE`.
    pub(crate) fn verify(&self, peer: bool) {
        let (mode, callback) = if peer {
            (sys::SSL_VERIFY_PEER, Some(verify_callback as _))
        } else {
            (sys::SSL_VERIFY_NONE, None)
        };
        // SAFETY: the context is live; the callback is ours.
        unsafe { sys::SSL_CTX_set_verify(self.as_ptr(), mode, callback) };
    }

    /// `SSL_load_client_CA_file` then `SSL_CTX_set_client_CA_list`, which
    /// takes the list; `false` where the file gave none.
    pub(crate) fn client_ca_file(&self, file: &CStr) -> bool {
        // SAFETY: the path is NUL-terminated.
        let names = unsafe { sys::SSL_load_client_CA_file(file.as_ptr()) };
        if !names.is_null() {
            // SAFETY: the context is live; it takes the list.
            unsafe { sys::SSL_CTX_set_client_CA_list(self.as_ptr(), names) };
        }
        !names.is_null()
    }

    /// `SSL_CTX_load_verify_locations`.
    pub(crate) fn load_verify_locations(&self, file: Option<&CStr>, dir: Option<&CStr>) -> bool {
        let (file, dir) = (
            file.map_or(ptr::null(), CStr::as_ptr),
            dir.map_or(ptr::null(), CStr::as_ptr),
        );
        // SAFETY: the context is live; each path is null or NUL-terminated.
        unsafe { sys::SSL_CTX_load_verify_locations(self.as_ptr(), file, dir) == 1 }
    }

    /// `X509_STORE_add_cert(SSL_CTX_get_cert_store(ctx), cert)`, which
    /// counts one more owner of the certificate.
    pub(crate) fn add_store_cert(&self, cert: &Certificate) -> bool {
        // SAFETY: the context is live; the store is its own.
        let store = unsafe { sys::SSL_CTX_get_cert_store(self.as_ptr()) };
        // SAFETY: the store and the certificate are live.
        unsafe { sys::X509_STORE_add_cert(store, cert.as_ptr()) == 1 }
    }

    /// `SSL_CTX_set_default_verify_paths`.
    pub(crate) fn default_verify_paths(&self) -> bool {
        // SAFETY: the context is live.
        unsafe { sys::SSL_CTX_set_default_verify_paths(self.as_ptr()) == 1 }
    }

    /// `SSL_CTX_set_cipher_list`.
    pub(crate) fn cipher_list(&self, list: &CStr) -> bool {
        // SAFETY: the context is live; the list is NUL-terminated.
        unsafe { sys::SSL_CTX_set_cipher_list(self.as_ptr(), list.as_ptr()) == 1 }
    }

    /// `SSL_CTX_set_security_level`.
    pub(crate) fn security_level(&self, level: c_int) {
        // SAFETY: the context is live.
        unsafe { sys::SSL_CTX_set_security_level(self.as_ptr(), level) };
    }

    /// `SSL_CTX_set_alpn_protos`: the list a client offers, in wire form;
    /// whether the library took it, which it refuses where a name is
    /// empty (its answer is zero on success).
    pub(crate) fn alpn_protos(&self, wire: &[u8]) -> bool {
        let len = c_uint::try_from(wire.len()).unwrap_or(0);
        // SAFETY: the context is live; the library copies `len` bytes.
        let answer = unsafe { sys::SSL_CTX_set_alpn_protos(self.as_ptr(), wire.as_ptr(), len) };
        answer == 0
    }

    /// `SSL_CTX_use_certificate_chain_file`.
    pub(crate) fn chain_file(&self, path: &CStr) -> bool {
        // SAFETY: the context is live; the path is NUL-terminated.
        unsafe { sys::SSL_CTX_use_certificate_chain_file(self.as_ptr(), path.as_ptr()) == 1 }
    }

    /// `SSL_CTX_use_PrivateKey_file(ctx, path, SSL_FILETYPE_PEM)`.
    pub(crate) fn private_key_file(&self, path: &CStr) -> bool {
        let pem = sys::SSL_FILETYPE_PEM;
        // SAFETY: the context is live; the path is NUL-terminated.
        unsafe { sys::SSL_CTX_use_PrivateKey_file(self.as_ptr(), path.as_ptr(), pem) == 1 }
    }

    /// `SSL_CTX_check_private_key`.
    pub(crate) fn check_private_key(&self) -> bool {
        // SAFETY: the context is live.
        unsafe { sys::SSL_CTX_check_private_key(self.as_ptr()) == 1 }
    }

    /// `SSL_CTX_set_min_proto_version` and `SSL_CTX_set_max_proto_version`.
    pub(crate) fn proto_versions(&self, min: c_int, max: c_int) {
        // SAFETY: the context is live.
        unsafe { sys::SSL_CTX_set_min_proto_version(self.as_ptr(), min) };
        // SAFETY: the context is live.
        unsafe { sys::SSL_CTX_set_max_proto_version(self.as_ptr(), max) };
    }

    /// `php_openssl_set_server_dh_param`'s OpenSSL 3 arm: the parameters
    /// read from the file and handed to the `context`, which takes them;
    /// the `Err` is the warning php raises for the step that failed.
    pub(crate) fn dh_params(&self, path: &CStr) -> Result<(), &'static str> {
        // SAFETY: both strings are NUL-terminated.
        let bio = unsafe { BIO_new_file(path.as_ptr(), c"rb".as_ptr()) };
        if bio.is_null() {
            return Err("Invalid dh_param");
        }
        // SAFETY: the BIO is live; no key is handed in.
        let pkey = unsafe { PEM_read_bio_Parameters(bio, ptr::null_mut()) };
        // SAFETY: the BIO is ours, freed once.
        unsafe { BIO_free(bio) };
        if pkey.is_null() {
            return Err("Failed reading DH params");
        }
        // SAFETY: the context is live; it takes the key on success.
        if unsafe { sys::SSL_CTX_set0_tmp_dh_pkey(self.as_ptr(), pkey) } == 0 {
            // SAFETY: the key is still ours on failure, freed once.
            unsafe { sys::EVP_PKEY_free(pkey) };
            return Err("Failed assigning DH params");
        }
        Ok(())
    }
}

/// The hooks owned through their pointer: made once, freed once.
fn hooks_new() -> NonNull<Hooks> {
    NonNull::from(Box::leak(Box::default()))
}

/// Frees what [`hooks_new`] made.
///
/// # Safety
///
/// `hooks` came from [`hooks_new`], is freed once, and nothing reads it
/// after.
unsafe fn hooks_free(hooks: NonNull<Hooks>) {
    // SAFETY: the caller's contract.
    drop(unsafe { Box::from_raw(hooks.as_ptr()) });
}

/// A socket's crypto being set up: the `SSL_CTX` and the hooks, before
/// the `SSL` ([`Setup::open`]).
pub(crate) struct Setup {
    ctx: Ctx,
    hooks: NonNull<Hooks>,
}

// SAFETY: the context and the hooks are owned here alone.
unsafe impl Send for Setup {}

impl Drop for Setup {
    fn drop(&mut self) {
        // SAFETY: the hooks are this value's, freed once; no SSL was made.
        unsafe { hooks_free(self.hooks) };
    }
}

impl Setup {
    /// The `context` taken over, with hooks that allow the defaults.
    pub(crate) fn new(ctx: Ctx) -> Setup {
        Setup {
            ctx,
            hooks: hooks_new(),
        }
    }

    /// The `context`, to configure.
    pub(crate) fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// The hooks, to fill before the callbacks that read them are set.
    pub(crate) fn hooks(&mut self) -> &mut Hooks {
        // SAFETY: the hooks are live, and no library call runs while the
        // setup is borrowed.
        unsafe { self.hooks.as_mut() }
    }

    /// `SSL_CTX_set_default_passwd_cb` with the hooks as its data: the
    /// key file decrypted with the `passphrase`.
    pub(crate) fn passphrase_callback(&self) {
        // SAFETY: the context is live; the hooks outlive it as its data.
        unsafe {
            SSL_CTX_set_default_passwd_cb_userdata(self.ctx.as_ptr(), self.hooks.as_ptr().cast());
        };
        // SAFETY: the context is live; the callback is ours.
        unsafe { SSL_CTX_set_default_passwd_cb(self.ctx.as_ptr(), Some(passwd_callback)) };
    }

    /// `SSL_CTX_set_alpn_select_cb`: a server picks from the hooks' list.
    pub(crate) fn alpn_select_callback(&self) {
        let (ctx, arg) = (self.ctx.as_ptr(), self.hooks.as_ptr().cast());
        // SAFETY: the context is live; the hooks outlive it as its data.
        unsafe { sys::SSL_CTX_set_alpn_select_cb__fixed_rust(ctx, Some(alpn_callback), arg) };
    }

    /// `SSL_CTX_set_tlsext_servername_callback`: a server picks among the
    /// hooks' SNI certificates.
    pub(crate) fn sni_callback(&self) {
        let cb = Some(servername_callback as unsafe extern "C" fn(_, _, _) -> _);
        // SAFETY: the context is live; the callback is ours.
        unsafe { sys::SSL_CTX_set_tlsext_servername_callback__fixed_rust(self.ctx.as_ptr(), cb) };
        // SAFETY: the context is live; the hooks outlive it as its data.
        unsafe {
            sys::SSL_CTX_set_tlsext_servername_arg(self.ctx.as_ptr(), self.hooks.as_ptr().cast())
        };
    }

    /// `SSL_new` with the hooks as its application data; `None` where
    /// either failed.
    pub(crate) fn open(self) -> Option<Tls> {
        // SAFETY: the context is live.
        let ssl = NonNull::new(unsafe { sys::SSL_new(self.ctx.as_ptr()) })?;
        let setup = std::mem::ManuallyDrop::new(self);
        // SAFETY: the context is read out of a value that is never dropped,
        // so it is owned once, by the TLS.
        let ctx = unsafe { ptr::read(&raw const setup.ctx) };
        let tls = Tls {
            ssl,
            _ctx: ctx,
            hooks: setup.hooks,
        };
        // SAFETY: the SSL is live; the hooks outlive it.
        let set =
            unsafe { sys::SSL_set_ex_data(ssl.as_ptr(), APP_DATA, tls.hooks.as_ptr().cast()) };
        (set == 1).then_some(tls)
    }
}

/// A socket's TLS: the `SSL`, the `SSL_CTX` it was made in and the hooks
/// its callbacks read.
pub(crate) struct Tls {
    ssl: NonNull<sys::SSL>,
    /// The reference to the `context` this value holds, given up after the
    /// `SSL`, which counts one of its own.
    _ctx: Ctx,
    hooks: NonNull<Hooks>,
}

// SAFETY: the SSL, its context and the hooks are owned here alone and used
// by one call at a time; OpenSSL 3 lets an SSL move between threads.
unsafe impl Send for Tls {}

impl Drop for Tls {
    fn drop(&mut self) {
        // SAFETY: the SSL is owned here and freed once, before the hooks
        // it reads.
        unsafe { sys::SSL_free(self.ssl.as_ptr()) };
        // SAFETY: the hooks are this value's, freed once; no SSL reads
        // them any more.
        unsafe { hooks_free(self.hooks) };
    }
}

impl Tls {
    fn ssl(&self) -> *mut sys::SSL {
        self.ssl.as_ptr()
    }

    /// The hooks, between library calls.
    pub(crate) fn hooks(&mut self) -> &mut Hooks {
        // SAFETY: the hooks are live, and no library call runs while the
        // TLS is borrowed.
        unsafe { self.hooks.as_mut() }
    }

    /// `SSL_set_fd`.
    pub(crate) fn set_fd(&self, fd: RawFd) -> bool {
        // SAFETY: the SSL is live; the socket outlives it.
        unsafe { SSL_set_fd(self.ssl(), fd) == 1 }
    }

    /// `SSL_set_mode`.
    pub(crate) fn set_mode(&self, mode: c_long) {
        // SAFETY: the SSL is live; the control takes a number.
        unsafe { sys::SSL_ctrl(self.ssl(), sys::SSL_CTRL_MODE, mode, ptr::null_mut()) };
    }

    /// `SSL_set_info_callback`, which counts each handshake that begins
    /// into the hooks' renegotiation bucket.
    pub(crate) fn reneg_callback(&self) {
        // SAFETY: the SSL is live; the callback is ours.
        unsafe { SSL_set_info_callback(self.ssl(), Some(info_callback)) };
    }

    /// `SSL_SESSION_is_resumable(SSL_get_session(ssl))`: whether the
    /// session the connection holds is one the server issued for
    /// resumption, by id or by TLS 1.3's `NewSessionTicket`, which is when
    /// OpenSSL hands it to a new-session callback.
    pub(crate) fn session_resumable(&self) -> bool {
        // SAFETY: the SSL is live; the session it answers is borrowed.
        let session = unsafe { sys::SSL_get_session(self.ssl()) };
        if session.is_null() {
            return false;
        }
        // SAFETY: the session is the SSL's own, live while the SSL is.
        unsafe { SSL_SESSION_is_resumable(session) == 1 }
    }

    /// `SSL_copy_session_id(ssl, from)`.
    pub(crate) fn copy_session_from(&self, from: &Tls) -> bool {
        // SAFETY: both SSLs are live.
        unsafe { SSL_copy_session_id(self.ssl(), from.ssl()) == 1 }
    }

    /// `SSL_set_tlsext_host_name`.
    pub(crate) fn set_host_name(&self, name: &CStr) {
        // SAFETY: the SSL is live; the library copies the name.
        unsafe { sys::SSL_set_tlsext_host_name(self.ssl(), name.as_ptr().cast_mut()) };
    }

    /// `SSL_set_connect_state` for a client, `SSL_set_accept_state` for a
    /// server.
    pub(crate) fn set_state(&self, client: bool) {
        if client {
            // SAFETY: the SSL is live.
            unsafe { sys::SSL_set_connect_state(self.ssl()) };
        } else {
            // SAFETY: the SSL is live.
            unsafe { sys::SSL_set_accept_state(self.ssl()) };
        }
    }

    /// `SSL_connect` for a client, `SSL_accept` for a server.
    pub(crate) fn handshake(&self, client: bool) -> Io {
        if client {
            // SAFETY: the SSL is live.
            with_errno(unsafe { sys::SSL_connect(self.ssl()) })
        } else {
            // SAFETY: the SSL is live.
            with_errno(unsafe { sys::SSL_accept(self.ssl()) })
        }
    }

    /// `SSL_read` into `buf`, at most `INT_MAX` bytes.
    pub(crate) fn read(&self, buf: &mut [u8]) -> Io {
        let len = c_int::try_from(buf.len()).unwrap_or(c_int::MAX);
        // SAFETY: the SSL is live; the buffer holds `len` bytes.
        with_errno(unsafe { sys::SSL_read(self.ssl(), buf.as_mut_ptr().cast(), len) })
    }

    /// `SSL_write` of `buf`, at most `INT_MAX` bytes.
    pub(crate) fn write(&self, buf: &[u8]) -> Io {
        let len = c_int::try_from(buf.len()).unwrap_or(c_int::MAX);
        // SAFETY: the SSL is live; the buffer holds `len` bytes.
        with_errno(unsafe { sys::SSL_write(self.ssl(), buf.as_ptr().cast(), len) })
    }

    /// `SSL_peek` of one byte.
    pub(crate) fn peek(&self) -> Io {
        let mut byte = 0u8;
        // SAFETY: the SSL is live; the buffer holds one byte.
        with_errno(unsafe { sys::SSL_peek(self.ssl(), ptr::from_mut(&mut byte).cast(), 1) })
    }

    /// `SSL_get_error` of what a call answered.
    pub(crate) fn error(&self, ret: c_int) -> c_int {
        // SAFETY: the SSL is live.
        unsafe { sys::SSL_get_error(self.ssl(), ret) }
    }

    /// `SSL_shutdown`.
    pub(crate) fn shutdown(&self) {
        // SAFETY: the SSL is live.
        unsafe { sys::SSL_shutdown(self.ssl()) };
    }

    /// `SSL_set_shutdown(ssl, SSL_SENT_SHUTDOWN|SSL_RECEIVED_SHUTDOWN)`.
    pub(crate) fn set_shut_down(&self) {
        let both = sys::SSL_SENT_SHUTDOWN | SSL_RECEIVED_SHUTDOWN;
        // SAFETY: the SSL is live.
        unsafe { sys::SSL_set_shutdown(self.ssl(), both) };
    }

    /// `SSL_pending`.
    pub(crate) fn pending(&self) -> usize {
        // SAFETY: the SSL is live.
        usize::try_from(unsafe { sys::SSL_pending(self.ssl()) }).unwrap_or(0)
    }

    /// `SSL_version`.
    pub(crate) fn version(&self) -> c_int {
        // SAFETY: the SSL is live.
        unsafe { sys::SSL_version(self.ssl()) }
    }

    /// `SSL_get_current_cipher` with its name, bits and version.
    pub(crate) fn cipher(&self) -> Option<Cipher> {
        // SAFETY: the SSL is live.
        let cipher = unsafe { sys::SSL_get_current_cipher(self.ssl()) };
        if cipher.is_null() {
            return None;
        }
        // SAFETY: the cipher is the library's.
        let name = unsafe { sys::SSL_CIPHER_get_name(cipher) };
        // SAFETY: as above.
        let version = unsafe { sys::SSL_CIPHER_get_version(cipher) };
        // SAFETY: the cipher is the library's; no out parameter.
        let bits = unsafe { sys::SSL_CIPHER_get_bits(cipher, ptr::null_mut()) };
        // SAFETY: the name is the cipher's own, NUL-terminated.
        let name = unsafe { copied(name) }.unwrap_or_default();
        // SAFETY: the version is the cipher's own, NUL-terminated.
        let version = unsafe { copied(version) }.unwrap_or_default();
        Some(Cipher {
            name,
            bits,
            version,
        })
    }

    /// `SSL_get0_alpn_selected`: the protocol agreed, where one was.
    pub(crate) fn alpn_selected(&self) -> Option<Vec<u8>> {
        let (mut data, mut len): (*const c_uchar, c_uint) = (ptr::null(), 0);
        // SAFETY: the SSL is live; both out parameters are ours.
        unsafe { sys::SSL_get0_alpn_selected(self.ssl(), &raw mut data, &raw mut len) };
        if data.is_null() {
            return None;
        }
        // SAFETY: the library points at `len` bytes it keeps.
        Some(
            unsafe { std::slice::from_raw_parts(data, usize::try_from(len).unwrap_or(0)) }.to_vec(),
        )
    }

    /// `SSL_get_peer_certificate`, one owner of it.
    pub(crate) fn peer_certificate(&self) -> Option<Certificate> {
        // SAFETY: the SSL is live; the certificate comes with an owner.
        Certificate::owned(unsafe { sys::SSL_get1_peer_certificate(self.ssl()) })
    }

    /// `SSL_get_peer_cert_chain`, each certificate `X509_dup`ed; `None`
    /// where there is none or it is empty.
    pub(crate) fn peer_chain(&self) -> Option<Vec<Certificate>> {
        // SAFETY: the SSL is live; the stack is its own.
        let chain = unsafe { sys::SSL_get_peer_cert_chain(self.ssl()) };
        if chain.is_null() {
            return None;
        }
        // SAFETY: the stack is live.
        let items = unsafe { stack_items(chain.cast()) };
        let certs = items
            .into_iter()
            // SAFETY: each item is an X509 of the stack.
            .filter_map(|item| Certificate::owned(unsafe { sys::X509_dup(item.cast()) }))
            .collect::<Vec<_>>();
        (!certs.is_empty()).then_some(certs)
    }

    /// `SSL_get_verify_result`.
    pub(crate) fn verify_result(&self) -> c_long {
        // SAFETY: the SSL is live.
        unsafe { sys::SSL_get_verify_result(self.ssl()) }
    }
}

/// What a call answered with `errno` as it left it.
fn with_errno(ret: c_int) -> Io {
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    Io { ret, errno }
}

/// `ERR_peek_error()`.
pub(crate) fn peek_error() -> c_ulong {
    // SAFETY: reads this thread's queue; it takes nothing.
    unsafe { ERR_peek_error() }
}

/// `ERR_error_string_n(code, esbuf, 512)`.
pub(crate) fn error_string(code: c_ulong) -> Vec<u8> {
    let mut buf = [0 as c_char; 512];
    // SAFETY: the buffer is 512 bytes and the length says so.
    unsafe { ERR_error_string_n(code, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: the library NUL-terminates within the length.
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_bytes().to_vec()
}

/// `X509_verify_cert_error_string`.
pub(crate) fn verify_error_text(code: c_long) -> Vec<u8> {
    // SAFETY: the library answers a static string for any code.
    let text = unsafe { sys::X509_verify_cert_error_string(code) };
    // SAFETY: the string is static and NUL-terminated.
    unsafe { copied(text) }.unwrap_or_default()
}

/// The subjectAltName names of `cert`, in order, as
/// `php_openssl_matches_san_list` reads them: a DNS name that converts to
/// UTF-8 with no NUL before its end, or one that does not, an IP
/// address's bytes, a URI and an SRV-ID; the other kinds are left out.
pub(crate) fn san_names(cert: &Certificate) -> Vec<San> {
    let (nid, none) = (sys::NID_subject_alt_name, ptr::null_mut());
    // SAFETY: the certificate is live; no critical flag or index asked.
    let stack: *mut sys::OPENSSL_STACK =
        unsafe { sys::X509_get_ext_d2i(cert.as_ptr(), nid, none, none) }.cast();
    if stack.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // SAFETY: the stack is ours until freed below.
    for item in unsafe { stack_items(stack) } {
        let name: *mut sys::GENERAL_NAME = item.cast();
        // SAFETY: each item is a GENERAL_NAME of the stack.
        let kind = unsafe { (*name).type_ };
        // SAFETY: as above.
        let data = unsafe { (*name).d }.cast::<sys::ASN1_STRING>();
        if kind == sys::GEN_DNS {
            // SAFETY: GEN_DNS holds a live ASN1_IA5STRING.
            let stored = usize::try_from(unsafe { sys::ASN1_STRING_length(data) }).unwrap_or(0);
            let mut utf8: *mut c_uchar = ptr::null_mut();
            // SAFETY: the string is live; the output is ours to free.
            if unsafe { sys::ASN1_STRING_to_UTF8(&raw mut utf8, data) } < 0 || utf8.is_null() {
                out.push(San::DnsUnreadable);
                continue;
            }
            // SAFETY: the library wrote a NUL-terminated string.
            let text = unsafe { CStr::from_ptr(utf8.cast()) }.to_bytes().to_vec();
            // SAFETY: the output is ours, freed once.
            unsafe { sys::OPENSSL_free(utf8.cast()) };
            out.push(if stored == text.len() {
                San::Dns(text)
            } else {
                San::DnsUnreadable
            });
        } else if kind == sys::GEN_URI {
            out.push(San::Uri);
        } else if kind == sys::GEN_OTHERNAME {
            let mut oid: *mut sys::ASN1_OBJECT = ptr::null_mut();
            // SAFETY: the name is live; the OID answered is the name's own.
            let got = unsafe { GENERAL_NAME_get0_otherName(name, &raw mut oid, ptr::null_mut()) };
            // SAFETY: a nonzero answer leaves the name's live OID in `oid`;
            // php's buffer is 32 bytes.
            if got != 0 && !oid.is_null() && unsafe { super::object_oid(oid, 32) } == SRV_NAME_OID {
                out.push(San::Srv);
            }
        } else if kind == sys::GEN_IPADD {
            // SAFETY: GEN_IPADD holds a live ASN1_OCTET_STRING.
            let stored = usize::try_from(unsafe { sys::ASN1_STRING_length(data) }).unwrap_or(0);
            // SAFETY: the string is live and holds `stored` bytes.
            let bytes = unsafe { sys::ASN1_STRING_get0_data(data) };
            if !bytes.is_null() {
                // SAFETY: as above.
                out.push(San::Ip(
                    unsafe { std::slice::from_raw_parts(bytes, stored) }.to_vec(),
                ));
            }
        }
    }
    // SAFETY: the stack and its names are ours, freed once.
    unsafe { sys::OPENSSL_sk_pop_free(stack, Some(free_general_name)) };
    out
}

/// `GENERAL_NAME_free` as a stack's element destructor.
unsafe extern "C" fn free_general_name(name: *mut c_void) {
    // SAFETY: the stack hands each of its GENERAL_NAMEs once.
    unsafe { sys::GENERAL_NAME_free(name.cast()) };
}

/// `X509_NAME_get_text_by_NID(subject, NID_commonName, buf, 1024)`: the
/// length the library answered and the text up to its first NUL, `None`
/// for `-1`.
pub(crate) fn common_name(cert: &Certificate) -> Option<(usize, Vec<u8>)> {
    let mut buf = [0 as c_char; 1024];
    // SAFETY: the certificate is live; its subject is its own.
    let subject = unsafe { sys::X509_get_subject_name(cert.as_ptr()) };
    let (nid, len) = (sys::NID_commonName, c_int::try_from(buf.len()).unwrap_or(0));
    // SAFETY: the name is live; the buffer holds `len` bytes.
    let got =
        usize::try_from(unsafe { X509_NAME_get_text_by_NID(subject, nid, buf.as_mut_ptr(), len) })
            .ok()?;
    // SAFETY: the library NUL-terminates within the buffer.
    Some((
        got,
        unsafe { CStr::from_ptr(buf.as_ptr()) }.to_bytes().to_vec(),
    ))
}

/// The hooks of the `SSL` a callback serves, `None` for one with none.
///
/// # Safety
///
/// `ssl` is live, and its application data is null or a live [`Hooks`]
/// no Rust reference points into while the callback runs.
unsafe fn hooks_of<'a>(ssl: *const sys::SSL) -> Option<&'a mut Hooks> {
    // SAFETY: the caller's contract: the SSL is live.
    let data = unsafe { sys::SSL_get_ex_data(ssl, APP_DATA) };
    // SAFETY: the caller's contract: null or live hooks, unaliased.
    unsafe { data.cast::<Hooks>().as_mut() }
}

/// `verify_callback`: a self-signed leaf passes where `allow_self_signed`
/// says, and a chain deeper than `verify_depth` fails as too long.
extern "C" fn verify_callback(preverify_ok: c_int, store: *mut sys::X509_STORE_CTX) -> c_int {
    // SAFETY: the store is live for the callback.
    let err = unsafe { sys::X509_STORE_CTX_get_error(store) };
    // SAFETY: as above.
    let depth = unsafe { sys::X509_STORE_CTX_get_error_depth(store) };
    // SAFETY: the index is the library's.
    let index = unsafe { sys::SSL_get_ex_data_X509_STORE_CTX_idx() };
    // SAFETY: the store is live; the index names the SSL it verifies for.
    let ssl: *const sys::SSL = unsafe { sys::X509_STORE_CTX_get_ex_data(store, index) }.cast();
    if ssl.is_null() {
        return preverify_ok;
    }
    // SAFETY: the SSL is ours, its data our hooks, read and not kept.
    let Some(hooks) = (unsafe { hooks_of(ssl) }) else {
        return preverify_ok;
    };
    let (ok, too_deep) = hooks.verify(preverify_ok, err, depth);
    if too_deep {
        // SAFETY: the store is live.
        unsafe { sys::X509_STORE_CTX_set_error(store, sys::X509_V_ERR_CERT_CHAIN_TOO_LONG) };
    }
    ok
}

/// `php_openssl_passwd_callback`: the `passphrase` with its NUL where it
/// fits in `num - 1` bytes, and nothing otherwise.
unsafe extern "C" fn passwd_callback(
    buf: *mut c_char,
    num: c_int,
    _: c_int,
    data: *mut c_void,
) -> c_int {
    // SAFETY: the data is the context's hooks, which outlive it.
    let Some(passphrase) =
        (unsafe { data.cast::<Hooks>().as_ref() }).and_then(|h| h.passphrase.as_ref())
    else {
        return 0;
    };
    if buf.is_null() || passphrase.len() >= usize::try_from(num).unwrap_or(0).saturating_sub(1) {
        return 0;
    }
    // SAFETY: the buffer holds `num` bytes, more than the passphrase.
    let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), passphrase.len() + 1) };
    out[..passphrase.len()].copy_from_slice(passphrase);
    out[passphrase.len()] = 0;
    c_int::try_from(passphrase.len()).unwrap_or(0)
}

/// `php_openssl_server_alpn_callback`: `SSL_select_next_proto` over the
/// server's list.
unsafe extern "C" fn alpn_callback(
    _: *mut sys::SSL,
    out: *mut *const c_uchar,
    outlen: *mut c_uchar,
    inbuf: *const c_uchar,
    inlen: c_uint,
    arg: *mut c_void,
) -> c_int {
    // SAFETY: the argument is the context's hooks, which outlive it.
    let Some(hooks) = (unsafe { arg.cast::<Hooks>().as_ref() }) else {
        return sys::SSL_TLSEXT_ERR_NOACK;
    };
    let (alpn, len) = (&hooks.alpn, c_uint::try_from(hooks.alpn.len()).unwrap_or(0));
    // SAFETY: the out parameters are the library's; both lists are live.
    let selected =
        unsafe { sys::SSL_select_next_proto(out.cast(), outlen, alpn.as_ptr(), len, inbuf, inlen) };
    if selected == sys::OPENSSL_NPN_NEGOTIATED {
        SSL_TLSEXT_ERR_OK
    } else {
        sys::SSL_TLSEXT_ERR_NOACK
    }
}

/// `php_openssl_server_sni_callback`: the first certificate whose name
/// matches the one the client asked for, its `context` swapped in.
unsafe extern "C" fn servername_callback(
    ssl: *mut sys::SSL,
    _: *mut c_int,
    arg: *mut c_void,
) -> c_int {
    // SAFETY: the SSL is live for the callback.
    let asked = unsafe { sys::SSL_get_servername(ssl, TLSEXT_NAMETYPE_HOST_NAME) };
    // SAFETY: the argument is the context's hooks, which outlive it.
    let Some(hooks) = (unsafe { arg.cast::<Hooks>().as_ref() }) else {
        return sys::SSL_TLSEXT_ERR_NOACK;
    };
    // SAFETY: the library's name, NUL-terminated, or null.
    let Some(asked) = (!asked.is_null()).then(|| unsafe { CStr::from_ptr(asked) }) else {
        return sys::SSL_TLSEXT_ERR_NOACK;
    };
    let Some((_, ctx)) = hooks
        .sni
        .iter()
        .find(|(name, _)| crate::xp_ssl::matches_wildcard_name(asked.to_bytes(), name.as_bytes()))
    else {
        return sys::SSL_TLSEXT_ERR_NOACK;
    };
    // SAFETY: both are live; the SSL counts one more owner of the context.
    unsafe { sys::SSL_set_SSL_CTX(ssl, ctx.as_ptr()) };
    SSL_TLSEXT_ERR_OK
}

/// `php_openssl_info_callback`: each handshake that begins counted into
/// the renegotiation bucket, which the Rust side reads once the call that
/// ran it returns.
unsafe extern "C" fn info_callback(ssl: *const sys::SSL, where_: c_int, _: c_int) {
    if where_ & SSL_CB_HANDSHAKE_START != 0 {
        // SAFETY: the SSL is ours, its data our hooks; nothing else reads
        // them while the library runs.
        if let Some(hooks) = unsafe { hooks_of(ssl) } {
            hooks.handshake_started();
        }
    }
}
