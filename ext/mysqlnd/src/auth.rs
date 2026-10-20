//! `mysqlnd_auth.c`: the authentication exchange of a connect and of a
//! change of user, and the four plugins php registers:
//! `mysql_native_password`, `mysql_clear_password`, `sha256_password`
//! and `caching_sha2_password`. The scrambles are `mysql_common`'s; the
//! RSA an authentication method encrypts the password with where the
//! stream is not secure is `ext/openssl`'s, reached through its natives as php's
//! mysqlnd reaches OpenSSL's `EVP_PKEY_encrypt` with OAEP padding.

use php_ext::abi::Callable;
use php_ext::coerce::callable_checked;
use php_ext::raising::{Level, Thrown, raise};
use php_ext::request::{OwnedStream as _, Request, Streams as _};
use php_ext::values::{Owner, PhpRef, PhpString, Value, ValueRef};

use crate::alloc::Pstr;
use crate::charset;
use crate::conn::ConnData;
use crate::consts::{
    ConnState, DEFAULT_AUTH_PROTOCOL, MAX_ALLOWED_DB_LEN, MAX_ALLOWED_USER_LEN, SCRAMBLE_LENGTH,
    client, cr,
};
use crate::error::{OLD_PASSWD, SERVER_GONE};
use crate::wire::store_length;

/// One of the plugins php registers, by the name the server asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plugin {
    Native,
    ClearPassword,
    Sha256,
    CachingSha2,
}

impl Plugin {
    /// `fetch_auth_plugin_by_name`: `auth_plugin_<name>` among the
    /// registered plugins.
    fn named(name: &[u8]) -> Option<Plugin> {
        Some(match name {
            b"mysql_native_password" => Plugin::Native,
            b"mysql_clear_password" => Plugin::ClearPassword,
            b"sha256_password" => Plugin::Sha256,
            b"caching_sha2_password" => Plugin::CachingSha2,
            _ => return None,
        })
    }

    /// The `mysqlnd` and `auth_plugin_*` names `php --ri mysqlnd` lists.
    pub(crate) const LOADED: &'static str = "mysqlnd,debug_trace,auth_plugin_mysql_native_password,auth_plugin_mysql_clear_password,auth_plugin_caching_sha2_password,auth_plugin_sha256_password";
}

/// What an authentication method's `handle_server_response` asked for:
/// nothing, or a switch to another method with its data.
type Switch = Option<(Vec<u8>, Vec<u8>)>;

/// The user, password and database of one exchange.
pub(crate) type Credentials<'a> = (&'a [u8], &'a [u8], &'a [u8]);

impl ConnData {
    /// `mysqlnd_connect_run_authentication`: TLS switched on where it
    /// was asked for, then the exchange.
    pub(crate) fn connect_run_authentication(
        &mut self,
        req: &mut Request,
        credentials: Credentials<'_>,
        plugin_data: &[u8],
        protocol: Option<&[u8]>,
        charset_no: u32,
        flags: u32,
    ) -> Result<bool, Thrown> {
        if !self.switch_to_ssl_if_needed(req, charset_no, flags)? {
            return Ok(false);
        }
        self.run_authentication(
            req,
            credentials,
            plugin_data,
            protocol,
            charset_no,
            flags,
            (false, false),
        )
    }

    /// `mysqlnd_switch_to_ssl_if_needed` and `mysqlnd_command::enable_ssl`:
    /// the short auth packet with `CLIENT_SSL`, then the handshake.
    fn switch_to_ssl_if_needed(
        &mut self,
        req: &mut Request,
        mut charset_no: u32,
        flags: u32,
    ) -> Result<bool, Thrown> {
        if let Some(found) = self
            .options
            .charset_name
            .as_ref()
            .map(Pstr::bytes)
            .and_then(charset::by_name)
        {
            charset_no = found.nr;
        }
        if flags & client::SSL == 0 {
            return Ok(true);
        }
        if self.server_capabilities & client::SSL == 0 {
            return self.close_gone(req);
        }
        let mut packet = Vec::with_capacity(32);
        packet.extend_from_slice(&flags.to_le_bytes());
        packet.extend_from_slice(&crate::consts::ASSEMBLED_PACKET_MAX_SIZE.to_le_bytes());
        packet.push(charset_no.to_le_bytes()[0]);
        packet.extend_from_slice(&[0; 23]);
        if self
            .net
            .send(req, &packet, &mut self.stats, &mut self.error)?
            == 0
        {
            self.state = ConnState::QuitSent;
            return self.close_gone(req);
        }
        self.net.options.ssl_verify_peer = if flags & client::SSL_VERIFY_SERVER_CERT != 0 {
            crate::net::SslPeer::Verify
        } else if flags & client::SSL_DONT_VERIFY_SERVER_CERT != 0 {
            crate::net::SslPeer::DontVerify
        } else {
            crate::net::SslPeer::Default
        };
        if !self.net.enable_ssl(req)? {
            self.state = ConnState::QuitSent;
            self.error.client(
                req,
                cr::CONNECTION_ERROR,
                "Cannot connect to MySQL using SSL",
            )?;
            return Ok(false);
        }
        Ok(true)
    }

    /// `enable_ssl`'s `close_conn`.
    fn close_gone(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        self.state = ConnState::QuitSent;
        self.send_close(req)?;
        self.error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
        Ok(false)
    }

    /// `mysqlnd_run_authentication`: the authentication method the server
    /// named, or native on the first round for one php has not; its data
    /// sent; round after round while the server asks for another method.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_authentication(
        &mut self,
        req: &mut Request,
        credentials: Credentials<'_>,
        plugin_data: &[u8],
        protocol: Option<&[u8]>,
        charset_no: u32,
        flags: u32,
        (silent, change_user): (bool, bool),
    ) -> Result<bool, Thrown> {
        let mut first_call = true;
        let mut data = plugin_data.to_vec();
        let mut requested = protocol
            .unwrap_or(DEFAULT_AUTH_PROTOCOL.as_bytes())
            .to_vec();
        loop {
            let mut plugin = Plugin::named(&requested);
            if plugin.is_none() {
                if first_call {
                    requested = DEFAULT_AUTH_PROTOCOL.as_bytes().to_vec();
                    plugin = Plugin::named(&requested);
                } else {
                    let name = String::from_utf8_lossy(&requested).into_owned();
                    self.error.client(
                        req,
                        cr::NOT_IMPLEMENTED,
                        &format!("The server requested authentication method unknown to the client [{name}]"),
                    )?;
                    return Ok(false);
                }
            }
            let kept = Pstr::copy(req, &data, self.persistent)?;
            Pstr::put(req, &mut self.auth_plugin_data, Some(kept));
            let scrambled = match plugin {
                Some(p) => self.auth_data(req, p, credentials.1, &data)?,
                None => Vec::new(),
            };
            if self.error.errno != 0 {
                return Ok(false);
            }
            let (ok, switch) = if change_user {
                self.auth_change_user(
                    req,
                    credentials,
                    silent,
                    first_call,
                    &requested,
                    plugin,
                    &data,
                    &scrambled,
                )?
            } else {
                self.auth_handshake(
                    req,
                    credentials,
                    flags,
                    charset_no,
                    first_call,
                    &requested,
                    plugin,
                    &data,
                    &scrambled,
                )?
            };
            first_call = false;
            if ok {
                let protocol = Pstr::dup(req, &requested, self.persistent)?;
                Pstr::put(req, &mut self.options.auth_protocol, Some(protocol));
                return Ok(true);
            }
            match switch {
                Some((protocol, switch_data)) if self.error.errno == 0 => {
                    requested = protocol;
                    data = switch_data;
                }
                _ => return Ok(false),
            }
        }
    }

    /// `get_auth_data` of each authentication method: what goes in the auth
    /// packet.
    fn auth_data(
        &mut self,
        req: &mut Request,
        plugin: Plugin,
        password: &[u8],
        data: &[u8],
    ) -> Result<Vec<u8>, Thrown> {
        match plugin {
            Plugin::Native => {
                if data.len() < SCRAMBLE_LENGTH {
                    self.error.client(
                        req,
                        cr::MALFORMED_PACKET,
                        "The server sent wrong length for scramble",
                    )?;
                    return Ok(Vec::new());
                }
                if password.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(
                    mysql_common::auth::plugins::mysql_native_password::scramble_native(
                        &data[..SCRAMBLE_LENGTH],
                        password,
                    )
                    .map(|s| s.to_vec())
                    .unwrap_or_default(),
                )
            }
            Plugin::ClearPassword => {
                // The NUL php sends after it (bug #78680).
                let mut out = password.to_vec();
                out.push(0);
                Ok(out)
            }
            Plugin::CachingSha2 => {
                if data.len() < SCRAMBLE_LENGTH {
                    self.error.client(
                        req,
                        cr::MALFORMED_PACKET,
                        "The server sent wrong length for scramble",
                    )?;
                    return Ok(Vec::new());
                }
                if password.is_empty() {
                    return Ok(Vec::new());
                }
                let mut nonce = [0u8; SCRAMBLE_LENGTH];
                nonce.copy_from_slice(&data[..SCRAMBLE_LENGTH]);
                Ok(
                    mysql_common::auth::plugins::caching_sha2_password::scramble_sha256(
                        &nonce, password,
                    )
                    .map(|s| s.to_vec())
                    .unwrap_or_default(),
                )
            }
            Plugin::Sha256 => {
                if self.net.ssl {
                    let mut out = password.to_vec();
                    out.push(0);
                    return Ok(out);
                }
                let Some(key) = self.public_key(req, false)? else {
                    return Ok(Vec::new());
                };
                Ok(self
                    .rsa_password(req, &key, password, data)?
                    .unwrap_or_default())
            }
        }
    }

    /// `mysqlnd_auth_handshake`: the full auth packet on the first round,
    /// the bare switch response after; the method's own exchange; the
    /// answer read.
    #[allow(clippy::too_many_arguments)]
    fn auth_handshake(
        &mut self,
        req: &mut Request,
        (user, password, db): Credentials<'_>,
        flags: u32,
        server_charset_no: u32,
        full: bool,
        protocol: &[u8],
        plugin: Option<Plugin>,
        orig_data: &[u8],
        scrambled: &[u8],
    ) -> Result<(bool, Switch), Thrown> {
        if full {
            let charset_no = match self
                .options
                .charset_name
                .as_ref()
                .map(Pstr::bytes)
                .and_then(charset::by_name)
            {
                Some(found) => found.nr,
                None => server_charset_no,
            };
            let mut packet = Vec::with_capacity(128);
            packet.extend_from_slice(&flags.to_le_bytes());
            packet.extend_from_slice(&self.options.max_allowed_packet.to_le_bytes());
            packet.push(charset_no.to_le_bytes()[0]);
            packet.extend_from_slice(&[0; 23]);
            if !self.auth_body(req, &mut packet, user, db, scrambled, Some(protocol), false)? {
                return Ok((false, None));
            }
            if self
                .net
                .send(req, &packet, &mut self.stats, &mut self.error)?
                == 0
            {
                self.state = ConnState::QuitSent;
                return Ok((false, None));
            }
            self.charset = charset::by_nr(charset_no);
        } else if self
            .net
            .send(req, scrambled, &mut self.stats, &mut self.error)?
            == 0
        {
            self.state = ConnState::QuitSent;
            self.error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
            return Ok((false, None));
        }
        if plugin == Some(Plugin::CachingSha2) {
            match self.caching_sha2_response(req, orig_data, password)? {
                Ok(()) => {}
                Err(switch) => return Ok((false, switch)),
            }
        }
        let Some(response) = self.read_auth_response(req)? else {
            return Ok((false, None));
        };
        if response.response_code == 0xFE {
            return match response.new_auth_protocol {
                None => {
                    self.error.client(req, cr::UNKNOWN_ERROR, OLD_PASSWD)?;
                    Ok((false, None))
                }
                Some(protocol) => Ok((false, Some((protocol, response.new_auth_protocol_data)))),
            };
        }
        if response.response_code == 0xFF {
            if let Some(e) = response.error {
                self.error
                    .set(req, e.errno, crate::consts::UNKNOWN_SQLSTATE, &e.error)?;
            }
            return Ok((false, None));
        }
        self.set_last_message(req, response.message.as_deref())?;
        Ok((true, None))
    }

    /// `mysqlnd_auth_change_user`.
    #[allow(clippy::too_many_arguments)]
    fn auth_change_user(
        &mut self,
        req: &mut Request,
        (user, password, db): Credentials<'_>,
        silent: bool,
        full: bool,
        protocol: &[u8],
        plugin: Option<Plugin>,
        orig_data: &[u8],
        scrambled: &[u8],
    ) -> Result<(bool, Switch), Thrown> {
        let old_charset = self.charset;
        if full {
            let mut payload = Vec::with_capacity(128);
            if !self.auth_body(req, &mut payload, user, db, scrambled, Some(protocol), true)? {
                return Ok((false, None));
            }
            if !self.send_command(req, crate::consts::Com::ChangeUser, &payload, silent)? {
                self.state = ConnState::QuitSent;
                self.error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
                return Ok((false, None));
            }
        } else if self
            .net
            .send(req, scrambled, &mut self.stats, &mut self.error)?
            == 0
        {
            self.state = ConnState::QuitSent;
            self.error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
            return Ok((false, None));
        }
        if plugin == Some(Plugin::CachingSha2) {
            match self.caching_sha2_response(req, orig_data, password)? {
                Ok(()) => {}
                Err(switch) => return Ok((false, switch)),
            }
        }
        let response = self.read_change_user_response(req)?;
        let mut ok = response.is_some();
        let mut switch = None;
        let asked_323 = response.as_ref().is_some_and(|r| r.asked_323);
        if let Some(response) = response {
            match &response.error {
                Some(e) => e.set_on(req, &mut self.error)?,
                None => self.error.reset(req),
            }
            if response.response_code == 0xFE {
                ok = false;
                match response.new_auth_protocol {
                    None => self.error.client(req, cr::UNKNOWN_ERROR, OLD_PASSWD)?,
                    Some(protocol) => switch = Some((protocol, response.new_auth_protocol_data)),
                }
            }
        }
        if self.error.errno != 0 {
            ok = false;
            let version = self.server_version();
            if version > 50113 && version < 50118 {
                // `redundant_error_packet`: 5.1.14 to 5.1.17 send one more
                // packet after the error, read and freed
                // (`Z/ext/mysqlnd/mysqlnd_auth.c:476-484`).
                drop(self.read_ok(req)?);
            }
        }
        if ok {
            self.set_last_message(req, None)?;
            self.upsert.reset();
            if self.server_version() < 50123
                && let Some(old) = old_charset
            {
                ok = self.set_charset(req, old.name.as_bytes())?;
            }
        } else if asked_323 {
            self.error.client(req, cr::UNKNOWN_ERROR, OLD_PASSWD)?;
        }
        Ok((ok, switch))
    }

    /// The common half of `php_mysqlnd_auth_write`: the user, the auth
    /// data, the database, the change-user charset, the method's name and
    /// the connection attributes; the `Err` is the memory limit's fatal.
    #[allow(clippy::too_many_arguments)]
    fn auth_body(
        &mut self,
        req: &mut Request,
        p: &mut Vec<u8>,
        user: &[u8],
        db: &[u8],
        auth_data: &[u8],
        plugin_name: Option<&[u8]>,
        change_user: bool,
    ) -> Result<bool, Thrown> {
        let user = php_ext::coerce::c_str(user);
        p.extend_from_slice(&user[..user.len().min(MAX_ALLOWED_USER_LEN)]);
        p.push(0);
        if auth_data.len() > 0xFF {
            let msg = "Authentication data too long. Won't fit into the buffer and will be truncated. Authentication will thus fail";
            self.error.client(req, cr::UNKNOWN_ERROR, msg)?;
            php_ext::raising::docref(req, Level::Warning, msg);
            return Ok(false);
        }
        p.push(u8::try_from(auth_data.len()).unwrap_or(u8::MAX));
        p.extend_from_slice(auth_data);
        if !db.is_empty() {
            p.extend_from_slice(&db[..db.len().min(MAX_ALLOWED_DB_LEN)]);
            p.push(0);
        } else if change_user {
            p.push(0);
        }
        if change_user && self.server_version() >= 50123 {
            let nr = self.charset.map_or(0, |c| c.nr);
            if nr != 0 {
                p.extend_from_slice(&nr.to_le_bytes()[..2]);
            }
        }
        if let Some(name) = plugin_name {
            p.extend_from_slice(php_ext::coerce::c_str(name));
            p.push(0);
        }
        let pairs = self
            .options
            .connect_attr
            .as_ref()
            .map(|attrs| attrs.pairs(req))
            .unwrap_or_default();
        if self.server_capabilities & client::CONNECT_ATTRS != 0 && !pairs.is_empty() {
            let mut attrs = Vec::new();
            for (key, value) in &pairs {
                store_length(&mut attrs, key.len() as u64);
                attrs.extend_from_slice(key);
                store_length(&mut attrs, value.len() as u64);
                attrs.extend_from_slice(value);
            }
            store_length(p, attrs.len() as u64);
            p.extend_from_slice(&attrs);
        }
        Ok(true)
    }

    /// `mysqlnd_caching_sha2_handle_server_response`: after the scramble,
    /// the server's fast-auth answer; for a full auth the password in
    /// clear over a secure stream, RSA-encrypted otherwise. `Err` is
    /// php's `FAIL`, with the switch the server asked for where it asked.
    fn caching_sha2_response(
        &mut self,
        req: &mut Request,
        data: &[u8],
        password: &[u8],
    ) -> Result<Result<(), Switch>, Thrown> {
        if password.is_empty() {
            return Ok(Ok(()));
        }
        let Some(result) = self.read_cached_sha2(req)? else {
            return Ok(Ok(()));
        };
        match result.response_code {
            0xFF => {
                if let Some(e) = result.error {
                    self.error
                        .set(req, e.errno, crate::consts::UNKNOWN_SQLSTATE, &e.error)?;
                }
                Ok(Err(None))
            }
            0xFE => Ok(Err(result
                .new_auth_protocol
                .map(|p| (p, result.new_auth_protocol_data)))),
            3 => Ok(Ok(())),
            4 => {
                let payload = if self.net.ssl || self.net.unix {
                    let mut out = password.to_vec();
                    out.push(0);
                    out
                } else {
                    match self.public_key(req, true)? {
                        Some(key) => self
                            .rsa_password(req, &key, password, data)?
                            .unwrap_or_default(),
                        None => Vec::new(),
                    }
                };
                self.net
                    .send(req, &payload, &mut self.stats, &mut self.error)?;
                Ok(Ok(()))
            }
            other => {
                self.error.client(
                    req,
                    cr::NOT_IMPLEMENTED,
                    &format!("Unexpected server response while doing caching_sha2 auth: {other}"),
                )?;
                Ok(Ok(()))
            }
        }
    }

    /// `mysqlnd_sha256_get_rsa_key` and `mysqlnd_caching_sha2_get_key`:
    /// the server's public key from the file the option or
    /// `mysqlnd.sha256_server_public_key` names, or asked of the server
    /// (with `\1` for `sha256_password`, `\2` for `caching_sha2_password`).
    fn public_key(&mut self, req: &mut Request, caching: bool) -> Result<Option<Vec<u8>>, Thrown> {
        let file = self
            .net
            .sha256_server_public_key
            .as_ref()
            .map(|f| f.bytes().to_vec())
            .filter(|f| !f.is_empty())
            .or_else(|| {
                req.ini()
                    .get(b"mysqlnd.sha256_server_public_key")
                    .filter(|f| !f.is_empty())
                    .map(<[u8]>::to_vec)
            });
        if let Some(file) = file {
            return read_file(req, &file);
        }
        let request: &[u8] = if caching { b"\x02" } else { b"\x01" };
        if self
            .net
            .send(req, request, &mut self.stats, &mut self.error)?
            == 0
        {
            raise(
                req,
                Level::Warning,
                &format!(
                    "Error while sending public key request packet. PID={}",
                    std::process::id()
                ),
            );
            self.state = ConnState::QuitSent;
            return Ok(None);
        }
        if let Some(key) = self.read_public_key(req)? {
            return Ok(Some(key));
        }
        raise(
            req,
            Level::Warning,
            &format!(
                "Error while receiving public key. PID={}",
                std::process::id()
            ),
        );
        self.state = ConnState::QuitSent;
        Ok(None)
    }

    /// `mysqlnd_xor_string` and the public encrypt: the password and its
    /// NUL, each byte xored with the scramble, encrypted under the key
    /// with OAEP padding; refused where php's length check refuses it.
    fn rsa_password(
        &mut self,
        req: &mut Request,
        pem: &[u8],
        password: &[u8],
        scramble: &[u8],
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let mut xored = password.to_vec();
        xored.push(0);
        let nonce = &scramble[..scramble.len().min(SCRAMBLE_LENGTH)];
        if !nonce.is_empty() {
            for (i, b) in xored.iter_mut().enumerate() {
                *b ^= nonce[i % nonce.len()];
            }
        }
        let Some(size) = rsa_size(req, pem)? else {
            return Ok(None);
        };
        if size <= password.len() + 41 {
            self.error
                .client(req, cr::UNKNOWN_ERROR, "password is too long")?;
            return Ok(None);
        }
        rsa_encrypt(req, pem, &xored)
    }
}

/// A native of the bundle by name, where it is loaded.
fn native(req: &mut Request, name: &[u8]) -> Result<Option<Callable>, Thrown> {
    let name = PhpString::new(req, name)?;
    let callable: Result<Callable, String> = callable_checked(req, &Value::string(*name));
    name.give_up(req);
    Ok(callable.ok())
}

/// `php_stream_open_wrapper(fname, "rb", REPORT_ERRORS, NULL)` and
/// `php_stream_copy_to_mem`: a file that does not open warns in the name
/// of the native being served, as mysqlnd's open does.
fn read_file(req: &mut Request, path: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let opening = php_ext::request::Opening {
        warn: true,
        ..php_ext::request::Opening::default()
    };
    let Some(stream) = req.open_stream_with(path, b"rb", opening)? else {
        return Ok(None);
    };
    let read = stream.copy_to_mem(req, None);
    stream.free(req);
    let read = read?;
    let bytes = read.as_bytes().to_vec();
    read.give_up(req);
    Ok(Some(bytes))
}

/// `EVP_PKEY_size`: the key's modulus in bytes, through
/// `openssl_pkey_get_details`.
fn rsa_size(req: &mut Request, pem: &[u8]) -> Result<Option<usize>, Thrown> {
    let (Some(get), Some(details)) = (
        native(req, b"openssl_pkey_get_public")?,
        native(req, b"openssl_pkey_get_details")?,
    ) else {
        return Ok(None);
    };
    let pem = PhpString::new(req, pem)?;
    let key = php_ext::call::call(req, &get, &[Value::string(*pem)]);
    pem.give_up(req);
    let key = key?;
    if matches!(key.as_ref(), ValueRef::Bool(false)) {
        key.give_up(req);
        return Ok(None);
    }
    let got = php_ext::call::call(req, &details, std::slice::from_ref(&*key));
    key.give_up(req);
    let got = got?;
    let bits = match got.as_ref() {
        ValueRef::Array(a) => a.find(b"bits").and_then(|v| match v.as_ref() {
            ValueRef::Int(n) => usize::try_from(n).ok(),
            _ => None,
        }),
        _ => None,
    };
    got.give_up(req);
    Ok(bits.map(|b| b.div_ceil(8)))
}

/// `EVP_PKEY_encrypt` under `RSA_PKCS1_OAEP_PADDING`, through
/// `openssl_public_encrypt`.
fn rsa_encrypt(req: &mut Request, pem: &[u8], data: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    /// `OPENSSL_PKCS1_OAEP_PADDING`.
    const OAEP: i64 = 4;
    let Some(encrypt) = native(req, b"openssl_public_encrypt")? else {
        return Ok(None);
    };
    let out = PhpRef::new(req, Owner::null())?;
    let data = PhpString::new(req, data)?;
    let pem = PhpString::new(req, pem)?;
    let args = [
        Value::string(*data),
        out.as_value(),
        Value::string(*pem),
        Value::int(OAEP),
    ];
    let done = php_ext::call::call(req, &encrypt, &args);
    let encrypted = match out.get().as_ref() {
        ValueRef::Str(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    };
    data.give_up(req);
    out.give_up(req);
    pem.give_up(req);
    let done = done?;
    let ok = matches!(done.as_ref(), ValueRef::Bool(true));
    done.give_up(req);
    Ok(encrypted.filter(|_| ok))
}
