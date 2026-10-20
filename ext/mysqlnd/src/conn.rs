//! `mysqlnd_connection.c` and `mysqlnd_commands.c`: one connection's
//! data, `MYSQLND_CONN_DATA`, its state machine and the commands sent
//! over it, each the C function's steps in the C function's order.

use php_ext::raising::{Level, Thrown, docref, raise};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpString, walk};

use crate::alloc::{
    Alloc, CONN_DATA, MYSQLND, PAYLOAD_DECODER_FACTORY, PFC, PLUGIN_ROOM, Pstr, STATS, VIO,
};
use crate::charset::{self, Charset};
use crate::consts::{
    ASSEMBLED_PACKET_MAX_SIZE, CloseType, Com, ConnState, NET_CMD_BUFFER_MIN_SIZE, QueryType,
    ServerOption, client, cr, status, trans_cor, trans_start,
};
use crate::error::{ErrorInfo, OUT_OF_SYNC, SERVER_GONE, Upsert};
use crate::net::{Net, SslPeer};
use crate::result::Res;
use crate::stats::{Stat, Stats};
use crate::wire::RsetHeader;

/// An option `mysql_options` sets, `enum mysqlnd_client_option` with
/// the value each takes, decoded once where php reads a `char *` as an
/// `unsigned int` or a string by the option's number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOption<'a> {
    ConnectTimeout(u32),
    Compress,
    NamedPipe,
    InitCommand(&'a [u8]),
    ReadDefaultFile,
    ReadDefaultGroup,
    SetCharsetName(&'a [u8]),
    LocalInfile(bool),
    Protocol(u32),
    ReadTimeout(u32),
    WriteTimeout(u32),
    SslVerifyServerCert(Option<bool>),
    LoadDataLocalDir(Option<&'a [u8]>),
    ConnectAttrReset,
    ConnectAttrAdd(&'a [u8], &'a [u8]),
    ConnectAttrDelete(&'a [u8]),
    ServerPublicKey(Option<&'a [u8]>),
    ExpiredPasswords(bool),
    IntAndFloatNative(bool),
    NetCmdBufferSize(u32),
    NetReadBufferSize(u32),
    SslKey(Option<&'a [u8]>),
    SslCert(Option<&'a [u8]>),
    SslCa(Option<&'a [u8]>),
    SslCapath(Option<&'a [u8]>),
    SslCipher(Option<&'a [u8]>),
    MaxAllowedPacket(u32),
    AuthProtocol(Option<&'a [u8]>),
}

/// `MYSQLND_SESSION_OPTIONS`.
#[derive(Debug, Default)]
pub(crate) struct Options {
    pub(crate) flags: u32,
    pub(crate) init_commands: InitCommands,
    pub(crate) auth_protocol: Option<Pstr>,
    /// `connect_attr`, made by the first attribute added.
    pub(crate) connect_attr: Option<ConnectAttr>,
    pub(crate) protocol: u32,
    pub(crate) charset_name: Option<Pstr>,
    pub(crate) max_allowed_packet: u32,
    pub(crate) int_and_float_native: bool,
    pub(crate) local_infile_directory: Option<Pstr>,
}

/// `init_commands` and `num_commands`: the `mnd_perealloc`'d array of a
/// pointer per command, grown by one for each, and each command
/// `mnd_pestrdup`'d.
#[derive(Debug, Default)]
pub(crate) struct InitCommands {
    table: Option<Alloc>,
    commands: Vec<Pstr>,
}

impl InitCommands {
    /// `MYSQL_INIT_COMMAND`: the array one pointer longer, the command
    /// copied into it; the `Err` is the memory limit's fatal.
    fn push(&mut self, req: &mut Request, command: &[u8], persistent: bool) -> Result<(), Thrown> {
        let len = 8 * (self.commands.len() + 1);
        self.table = Some(match self.table.take() {
            Some(table) => table.resize(req, len)?,
            None => Alloc::new(req, len, persistent)?,
        });
        self.commands.push(Pstr::dup(req, command, persistent)?);
        Ok(())
    }

    /// The commands in the order they were set.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.commands.iter().map(Pstr::bytes)
    }

    /// `free_options`' loop: each command, then the array.
    fn free(&mut self, req: &mut Request) {
        for command in self.commands.drain(..) {
            command.free(req);
        }
        if let Some(table) = self.table.take() {
            table.free(req);
        }
    }
}

/// `connect_attr`, the attributes in insertion order with an update kept
/// in place, as a `HashTable` keeps them: a request's connection's in an
/// array of the request's, which is the `mnd_pemalloc(sizeof(HashTable))`
/// with its table and its `zend_string_init`'d keys and values as php
/// counts them; a persistent connection's in pairs of its own.
#[derive(Debug)]
pub(crate) enum ConnectAttr {
    Heap(Owner<PhpArray>),
    Malloc(Vec<(Vec<u8>, Vec<u8>)>),
}

impl ConnectAttr {
    /// `zend_hash_init(attrs, 0, ...)`; the `Err` is the memory limit's
    /// fatal.
    fn new(req: &mut Request, persistent: bool) -> Result<ConnectAttr, Thrown> {
        Ok(if persistent {
            ConnectAttr::Malloc(Vec::new())
        } else {
            ConnectAttr::Heap(PhpArray::new(req)?)
        })
    }

    /// `zend_hash_update(attrs, key, value)`.
    fn add(&mut self, req: &mut Request, key: &[u8], value: &[u8]) -> Result<(), Thrown> {
        match self {
            ConnectAttr::Heap(attrs) => {
                let value = PhpString::new(req, value)?.into_value();
                attrs.set_str(req, key, value)
            }
            ConnectAttr::Malloc(attrs) => {
                match attrs.iter_mut().find(|(k, _)| k == key) {
                    Some((_, v)) => *v = value.to_vec(),
                    None => attrs.push((key.to_vec(), value.to_vec())),
                }
                Ok(())
            }
        }
    }

    /// `zend_hash_str_del(attrs, key)`.
    fn delete(&mut self, req: &mut Request, key: &[u8]) -> Result<(), Thrown> {
        match self {
            ConnectAttr::Heap(attrs) => {
                let key = PhpString::new(req, key)?.into_key();
                let deleted = attrs.unset(req, &key);
                key.give_up(req);
                deleted.map(drop)
            }
            ConnectAttr::Malloc(attrs) => {
                attrs.retain(|(k, _)| k != key);
                Ok(())
            }
        }
    }

    /// `zend_hash_clean(attrs)`: every attribute deleted, the table kept.
    fn clean(&mut self, req: &mut Request) -> Result<(), Thrown> {
        match self {
            ConnectAttr::Heap(attrs) => {
                let keys: Vec<Key> = walk::rows(req, **attrs).map(|(key, _)| key).collect();
                for key in keys {
                    attrs.unset(req, &key)?;
                }
                Ok(())
            }
            ConnectAttr::Malloc(attrs) => {
                attrs.clear();
                Ok(())
            }
        }
    }

    /// Whether no attribute is set, `zend_hash_num_elements(attrs) == 0`.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            ConnectAttr::Heap(attrs) => attrs.is_empty(),
            ConnectAttr::Malloc(attrs) => attrs.is_empty(),
        }
    }

    /// The attributes in order, as the handshake sends them.
    pub(crate) fn pairs(&self, req: &Request) -> Vec<(Vec<u8>, Vec<u8>)> {
        match self {
            ConnectAttr::Heap(attrs) => walk::rows(req, **attrs)
                .map(|(key, value)| {
                    let key = match key {
                        Key::Str(s) => s.as_bytes().to_vec(),
                        Key::Int(n) => n.to_string().into_bytes(),
                    };
                    let value = value.as_str_bytes().map(<[u8]>::to_vec);
                    (key, value.unwrap_or_default())
                })
                .collect(),
            ConnectAttr::Malloc(attrs) => attrs.clone(),
        }
    }

    /// `zend_hash_destroy` and `mnd_pefree`.
    fn free(self, req: &mut Request) {
        if let ConnectAttr::Heap(attrs) = self {
            attrs.give_up(req);
        }
    }
}

/// `MYSQLND_CONN_DATA`.
pub struct ConnData {
    /// What `get_connection` allocates for the connection, freed by
    /// [`ConnData::dtor`].
    held: Option<Held>,
    /// The `MYSQLND` its owner holds, which `mysqlnd_conn::close` frees
    /// whatever statements or rows still hold the data.
    pub(crate) mysqlnd: Option<Alloc>,
    pub(crate) net: Net,
    pub(crate) error: ErrorInfo,
    pub(crate) upsert: Upsert,
    pub(crate) state: ConnState,
    pub(crate) stats: Stats,
    pub(crate) options: Options,
    pub(crate) persistent: bool,
    pub(crate) scheme: Option<Pstr>,
    pub(crate) host_info: Option<Pstr>,
    pub(crate) server_version: Option<Pstr>,
    pub(crate) thread_id: u64,
    pub(crate) protocol_version: u8,
    pub(crate) server_capabilities: u32,
    pub(crate) port: u32,
    pub(crate) charset: Option<&'static Charset>,
    pub(crate) greet_charset: Option<&'static Charset>,
    /// `last_message`, `mysqlnd_set_string`'s: always the request's,
    /// a persistent connection's too, which `end_psession` frees.
    pub(crate) last_message: Option<Pstr>,
    pub(crate) field_count: u32,
    pub(crate) last_query_type: QueryType,
    /// `current_result`: the rows whose column definitions the last query
    /// read, until `store_result` or `use_result` takes them.
    pub(crate) current_result: Option<Box<Res>>,
    /// `authentication_plugin_data`, kept for `change_user`.
    pub(crate) auth_plugin_data: Option<Pstr>,
}

/// The structures `get_connection` allocates beside `MYSQLND`, each with
/// its `PLUGIN_ROOM`: the data, the statistics, the frame codec, the
/// `MYSQLND_VIO` and the payload decoder factory.
struct Held {
    data: Alloc,
    stats: Alloc,
    pfc: Alloc,
    vio: Alloc,
    payload_decoder_factory: Alloc,
}

impl ConnData {
    /// `mysqlnd_object_factory::get_connection` with `mysqlnd_vio::init`
    /// and `mysqlnd_pfc::init`: the structures allocated in php's order,
    /// on the request's heap unless `persistent`; the buffers and the read
    /// timeout from the `mysqlnd.*` ini, the affected rows `~0`. The `Err`
    /// is the memory limit's fatal.
    pub(crate) fn new(req: &mut Request, persistent: bool) -> Result<ConnData, Thrown> {
        let ini = |req: &Request, name: &[u8], default: u32| -> u32 {
            req.ini()
                .get(name)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|v| v.trim().parse::<i64>().ok())
                .map_or(default, |v| {
                    u32::try_from(v & 0xFFFF_FFFF).unwrap_or(default)
                })
        };
        let alloc = |req: &mut Request, len: usize| Alloc::new(req, len, persistent);
        let mysqlnd = alloc(req, MYSQLND + PLUGIN_ROOM)?;
        let data = alloc(req, CONN_DATA + PLUGIN_ROOM)?;
        let stats = alloc(req, STATS)?;
        let pfc = alloc(req, PFC)?;
        let mut net = Net::default();
        let cmd_buffer = ini(req, b"mysqlnd.net_cmd_buffer_size", NET_CMD_BUFFER_MIN_SIZE);
        net.set_cmd_buffer(req, cmd_buffer as usize, persistent)?;
        let vio = alloc(req, VIO)?;
        // The two `OnUpdateLong` entries' quantities, cut to `unsigned int`.
        let quantity = |req: &Request, name: &[u8]| -> u32 {
            u32::try_from(req.ini().long(name) & 0xFFFF_FFFF).unwrap_or(u32::MAX)
        };
        net.options.net_read_buffer_size = quantity(req, b"mysqlnd.net_read_buffer_size");
        net.options.timeout_read = quantity(req, b"mysqlnd.net_read_timeout");
        let payload_decoder_factory = alloc(req, PAYLOAD_DECODER_FACTORY + PLUGIN_ROOM)?;
        let mut upsert = Upsert::default();
        upsert.affected_rows_to_error();
        Ok(ConnData {
            held: Some(Held {
                data,
                stats,
                pfc,
                vio,
                payload_decoder_factory,
            }),
            mysqlnd: Some(mysqlnd),
            net,
            error: ErrorInfo::new(persistent),
            upsert,
            state: ConnState::default(),
            stats: Stats::default(),
            options: Options::default(),
            persistent,
            scheme: None,
            host_info: None,
            server_version: None,
            thread_id: 0,
            protocol_version: 0,
            server_capabilities: 0,
            port: 0,
            charset: None,
            greet_charset: None,
            last_message: None,
            field_count: 0,
            last_query_type: QueryType::default(),
            current_result: None,
            auth_plugin_data: None,
        })
    }

    /// `mysqlnd_conn_data::dtor`: the contents, the options and the error
    /// info freed, then the frame codec's buffer and the structures `get_connection`
    /// allocated.
    pub(crate) fn dtor(mut self, req: &mut Request) {
        self.free_contents(req);
        self.free_options(req);
        self.error.reset(req);
        if let Some(buffer) = self.net.cmd_buffer.take() {
            buffer.free(req);
        }
        if let Some(held) = self.held.take() {
            held.payload_decoder_factory.free(req);
            held.stats.free(req);
            held.pfc.free(req);
            held.vio.free(req);
            held.data.free(req);
        }
        if let Some(mysqlnd) = self.mysqlnd.take() {
            mysqlnd.free(req);
        }
    }

    /// `mysqlnd_conn_data::free_options`: the strings and the attributes
    /// the options hold, in its order.
    fn free_options(&mut self, req: &mut Request) {
        Pstr::put(req, &mut self.options.charset_name, None);
        Pstr::put(req, &mut self.options.auth_protocol, None);
        self.options.init_commands.free(req);
        if let Some(attrs) = self.options.connect_attr.take() {
            attrs.free(req);
        }
        Pstr::put(req, &mut self.options.local_infile_directory, None);
    }

    /// `mysqlnd_set_string(&conn->last_message, ...)`: the old message
    /// `mnd_efree`'d, the new one `mnd_pestrndup`'d on the request's heap
    /// whatever the connection's persistence; the `Err` is the memory
    /// limit's fatal. An OK with an info string keeps it, and the next
    /// OK without one frees it:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $m = memory_get_usage();
    /// mysqli_query($l, "UPDATE mysql.user SET plugin = plugin WHERE 1 = 0");
    /// echo memory_get_usage() - $m, "\n";
    /// mysqli_query($l, "DO 1");
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect mysql
    /// 48
    /// 32
    /// ```
    pub(crate) fn set_last_message(
        &mut self,
        req: &mut Request,
        message: Option<&[u8]>,
    ) -> Result<(), Thrown> {
        Pstr::put(req, &mut self.last_message, None);
        if let Some(message) = message {
            self.last_message = Some(Pstr::dup(req, message, false)?);
        }
        Ok(())
    }

    /// `mysqlnd_conn_data::set_client_option`, and `set_client_option_2d`
    /// for the attribute added. Each string option is `mnd_pestrdup`'d,
    /// the one it replaces freed, and the init commands' array grown a
    /// pointer per command; the close frees them all:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $m = memory_get_usage();
    /// $at = fn() => print(memory_get_usage() - $m . "\n");
    /// mysqli_options($l, MYSQLI_OPT_CONNECT_TIMEOUT, 5); $at();
    /// mysqli_options($l, MYSQLI_INIT_COMMAND, 'SET @a = 1'); $at();
    /// mysqli_options($l, MYSQLI_INIT_COMMAND, 'SET @b = 1'); $at();
    /// mysqli_options($l, MYSQLI_SET_CHARSET_NAME, 'utf8mb4'); $at();
    /// mysqli_options($l, MYSQLI_SET_CHARSET_NAME, 'latin1'); $at();
    /// mysqli_options($l, MYSQLI_OPT_LOAD_DATA_LOCAL_DIR, '/tmp'); $at();
    /// mysqli_options($l, MYSQLI_SERVER_PUBLIC_KEY, '/tmp/x.pem'); $at();
    /// mysqli_ssl_set($l, 'k', 'c', 'ca', null, 'cipher'); $at();
    /// $n = memory_get_usage();
    /// mysqli_close($l);
    /// echo $n - memory_get_usage(), "\n";
    /// ```
    /// ```expect mysql
    /// 760
    /// 824
    /// 840
    /// 856
    /// 856
    /// 872
    /// 888
    /// 952
    /// 41928
    /// ```
    pub(crate) fn set_option(
        &mut self,
        req: &mut Request,
        option: ClientOption<'_>,
    ) -> Result<bool, Thrown> {
        let persistent = self.persistent;
        // `value ? mnd_pestrdup(value, pers) : NULL`, put where the old
        // one is `mnd_pefree`'d.
        let set = |req: &mut Request, field: &mut Option<Pstr>, v: Option<&[u8]>| {
            let v = v.map(|v| Pstr::dup(req, v, persistent)).transpose()?;
            Pstr::put(req, field, v);
            Ok::<(), Thrown>(())
        };
        let vio = &mut self.net.options;
        match option {
            ClientOption::ReadTimeout(s) => vio.timeout_read = s,
            ClientOption::ConnectTimeout(s) => vio.timeout_connect = s,
            ClientOption::NetReadBufferSize(n) => vio.net_read_buffer_size = n,
            ClientOption::SslKey(v) => set(req, &mut vio.ssl_key, v)?,
            ClientOption::SslCert(v) => set(req, &mut vio.ssl_cert, v)?,
            ClientOption::SslCa(v) => set(req, &mut vio.ssl_ca, v)?,
            ClientOption::SslCapath(v) => set(req, &mut vio.ssl_capath, v)?,
            ClientOption::SslCipher(v) => set(req, &mut vio.ssl_cipher, v)?,
            ClientOption::SslVerifyServerCert(v) => {
                vio.ssl_verify_peer = match v {
                    Some(true) => SslPeer::Verify,
                    Some(false) => SslPeer::DontVerify,
                    None => SslPeer::Default,
                };
            }
            ClientOption::NetCmdBufferSize(n) => {
                if n < NET_CMD_BUFFER_MIN_SIZE {
                    return Ok(false);
                }
                self.net.set_cmd_buffer(req, n as usize, self.persistent)?;
            }
            ClientOption::Compress => self.net.use_compression = true,
            ClientOption::ServerPublicKey(v) => {
                set(req, &mut self.net.sha256_server_public_key, v)?;
            }
            ClientOption::IntAndFloatNative(on) => self.options.int_and_float_native = on,
            ClientOption::LocalInfile(on) => {
                if on {
                    self.options.flags |= client::LOCAL_FILES;
                } else {
                    self.options.flags &= !client::LOCAL_FILES;
                }
            }
            ClientOption::LoadDataLocalDir(dir) => {
                let dir = dir.filter(|d| !d.is_empty());
                set(req, &mut self.options.local_infile_directory, dir)?;
            }
            ClientOption::InitCommand(command) => {
                self.options.init_commands.push(req, command, persistent)?;
            }
            ClientOption::WriteTimeout(_)
            | ClientOption::ReadDefaultFile
            | ClientOption::ReadDefaultGroup => {}
            ClientOption::SetCharsetName(name) => {
                if charset::by_name(name).is_none() {
                    self.error
                        .client(req, cr::CANT_FIND_CHARSET, "Unknown character set")?;
                    return Ok(false);
                }
                set(req, &mut self.options.charset_name, Some(name))?;
            }
            ClientOption::NamedPipe => self.options.protocol = 3,
            ClientOption::Protocol(p) => {
                if p < 5 {
                    self.options.protocol = p;
                }
            }
            ClientOption::MaxAllowedPacket(n) => {
                if n > (1 << 16) {
                    self.options.max_allowed_packet = n;
                }
            }
            ClientOption::AuthProtocol(v) => set(req, &mut self.options.auth_protocol, v)?,
            ClientOption::ExpiredPasswords(on) => {
                if on {
                    self.options.flags |= client::CAN_HANDLE_EXPIRED_PASSWORDS;
                } else {
                    self.options.flags &= !client::CAN_HANDLE_EXPIRED_PASSWORDS;
                }
            }
            ClientOption::ConnectAttrReset => {
                if let Some(attrs) = self.options.connect_attr.as_mut() {
                    attrs.clean(req)?;
                }
            }
            ClientOption::ConnectAttrDelete(key) => {
                if let Some(attrs) = self.options.connect_attr.as_mut() {
                    attrs.delete(req, key)?;
                }
            }
            ClientOption::ConnectAttrAdd(key, value) => {
                if self.options.connect_attr.is_none() {
                    self.options.connect_attr = Some(ConnectAttr::new(req, self.persistent)?);
                }
                if let Some(attrs) = self.options.connect_attr.as_mut() {
                    attrs.add(req, key, value)?;
                }
            }
        }
        Ok(true)
    }

    // ---- commands

    /// `mysqlnd_protocol::send_command`: refused out of the ready state
    /// with php's two texts; else the command written, and a write that
    /// fails warns (unless `silent` or the server is gone) and closes.
    pub(crate) fn send_command(
        &mut self,
        req: &mut Request,
        command: Com,
        arg: &[u8],
        silent: bool,
    ) -> Result<bool, Thrown> {
        match self.state {
            ConnState::Ready => {}
            ConnState::QuitSent => {
                self.error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
                return Ok(false);
            }
            _ => {
                self.error
                    .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
                return Ok(false);
            }
        }
        self.upsert.affected_rows_to_error();
        self.error.reset(req);
        // `STAT_COM_QUIT + command - 1`.
        self.stats.inc(com_stat(command));
        // `php_mysqlnd_cmd_write`.
        self.net.reset();
        self.stats.inc(Stat::PacketsSentCmd);
        let mut payload = Vec::with_capacity(arg.len() + 1);
        payload.push(command as u8);
        payload.extend_from_slice(arg);
        if arg.len() + 1 + crate::net::HEADER_SIZE > self.net.cmd_buffer_len() {
            self.stats.inc(Stat::CmdBufferTooSmall);
        }
        let sent = self
            .net
            .send(req, &payload, &mut self.stats, &mut self.error)?;
        if sent == 0 {
            self.state = ConnState::QuitSent;
            if !silent && self.error.errno != cr::SERVER_GONE_ERROR {
                raise(
                    req,
                    Level::Warning,
                    &format!(
                        "Error while sending {} packet. PID={}",
                        command.text(),
                        std::process::id()
                    ),
                );
            }
            self.state = ConnState::QuitSent;
            self.send_close(req)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// `send_command_handle_OK`.
    pub(crate) fn handle_ok(
        &mut self,
        req: &mut Request,
        ignore_upsert: bool,
    ) -> Result<bool, Thrown> {
        let Some(ok) = self.read_ok(req)? else {
            if self.error.errno != cr::SERVER_GONE_ERROR {
                self.error
                    .client(req, cr::MALFORMED_PACKET, "Malformed packet")?;
            }
            return Ok(false);
        };
        if let Some(e) = ok.error {
            e.set_on(req, &mut self.error)?;
            self.upsert.server_status &= !status::MORE_RESULTS_EXISTS;
            self.upsert.affected_rows_to_error();
            return Ok(false);
        }
        self.set_last_message(req, ok.message.as_deref())?;
        if !ignore_upsert {
            self.upsert.reset();
            self.upsert.warning_count = ok.warning_count;
            self.upsert.server_status = ok.server_status;
            self.upsert.affected_rows = ok.affected_rows;
            self.upsert.last_insert_id = ok.last_insert_id;
        }
        Ok(true)
    }

    /// `send_command_handle_EOF`.
    pub(crate) fn handle_eof(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let Some(eof) = self.read_eof(req)? else {
            self.error
                .client(req, cr::MALFORMED_PACKET, "Malformed packet")?;
            return Ok(false);
        };
        if let Some(e) = eof.error {
            e.set_on(req, &mut self.error)?;
            self.upsert.affected_rows_to_error();
        } else if eof.field_count != crate::wire::EODATA_MARKER {
            self.error
                .client(req, cr::MALFORMED_PACKET, "Malformed packet")?;
            docref(
                req,
                Level::Warning,
                &format!(
                    "EOF packet expected, field count wasn't 0xFE but 0x{:2X}",
                    eof.field_count
                ),
            );
        }
        Ok(true)
    }

    /// `send_command_handle_response`: the OK or EOF read, and php's
    /// warning where it was malformed.
    pub(crate) fn handle_response(
        &mut self,
        req: &mut Request,
        eof: bool,
        silent: bool,
        command: Com,
        ignore_upsert: bool,
    ) -> Result<bool, Thrown> {
        let ok = if eof {
            self.handle_eof(req)?
        } else {
            self.handle_ok(req, ignore_upsert)?
        };
        if !silent && self.error.errno == cr::MALFORMED_PACKET {
            docref(
                req,
                Level::Warning,
                &format!(
                    "Error while reading {}'s response packet. PID={}",
                    command.text(),
                    std::process::id()
                ),
            );
        }
        Ok(ok)
    }

    /// A command and its OK, `mysqlnd_command`'s common shape.
    fn command_ok(
        &mut self,
        req: &mut Request,
        command: Com,
        arg: &[u8],
        silent: bool,
    ) -> Result<bool, Thrown> {
        Ok(self.send_command(req, command, arg, silent)?
            && self.handle_response(req, false, silent, command, true)?)
    }

    // ---- connecting

    /// `mysqlnd_conn_data::get_scheme`: the stream's scheme for the host,
    /// `localhost` the unix socket (`/tmp/mysql.sock` where none is
    /// named), an IPv6 address bracketed, `:3306` or the number asked for
    /// added where the host carries none; and whether it is a unix socket.
    fn scheme(host: &[u8], socket: &mut Option<Vec<u8>>, port: u32) -> (Vec<u8>, bool) {
        if host.eq_ignore_ascii_case(b"localhost") {
            let path = socket
                .get_or_insert_with(|| b"/tmp/mysql.sock".to_vec())
                .clone();
            let mut s = b"unix://".to_vec();
            s.extend_from_slice(&path);
            return (s, true);
        }
        let port = if port == 0 { 3306 } else { port };
        let ipv6 =
            host.first() != Some(&b'[') && host.iter().filter(|b| **b == b':').nth(1).is_some();
        if ipv6 {
            let mut s = b"tcp://[".to_vec();
            s.extend_from_slice(host);
            s.extend_from_slice(format!("]:{port}").as_bytes());
            return (s, false);
        }
        let has_port = if host.first() == Some(&b'[') {
            host.iter()
                .position(|b| *b == b']')
                .is_some_and(|at| host.get(at + 1) == Some(&b':'))
        } else {
            host.contains(&b':')
        };
        let mut s = b"tcp://".to_vec();
        s.extend_from_slice(host);
        if !has_port {
            s.extend_from_slice(format!(":{port}").as_bytes());
        }
        (s, false)
    }

    /// `mysqlnd_conn_data::get_updated_connect_flags`.
    fn connect_flags(&self, mut flags: u32) -> u32 {
        flags |= client::MYSQLND_CAPABILITIES;
        flags |= self.options.flags;
        if self.net.use_compression {
            flags |= client::COMPRESS;
        }
        if self.net.options.wants_ssl() {
            flags |= client::SSL;
        }
        if self
            .options
            .connect_attr
            .as_ref()
            .is_some_and(|attrs| !attrs.is_empty())
        {
            flags |= client::CONNECT_ATTRS;
        }
        flags
    }

    /// `mysqlnd_conn::connect`'s two attributes, then
    /// `mysqlnd_conn_data::connect`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn connect(
        &mut self,
        req: &mut Request,
        host: Option<&[u8]>,
        user: Option<&[u8]>,
        password: Option<&[u8]>,
        db: Option<&[u8]>,
        port: u32,
        socket: Option<&[u8]>,
        flags: u32,
    ) -> Result<bool, Thrown> {
        self.set_option(
            req,
            ClientOption::ConnectAttrAdd(b"_client_name", b"mysqlnd"),
        )?;
        if let Some(host) = host.filter(|h| !h.is_empty()) {
            self.set_option(req, ClientOption::ConnectAttrAdd(b"_server_host", host))?;
        }
        self.connect_data(req, host, user, password, db, port, socket, flags)
    }

    /// `mysqlnd_conn_data::connect`.
    #[allow(clippy::too_many_arguments)]
    fn connect_data(
        &mut self,
        req: &mut Request,
        host: Option<&[u8]>,
        user: Option<&[u8]>,
        password: Option<&[u8]>,
        db: Option<&[u8]>,
        port: u32,
        socket: Option<&[u8]>,
        mut flags: u32,
    ) -> Result<bool, Thrown> {
        let mut reconnect = false;
        self.error.reset(req);
        self.upsert.affected_rows_to_error();
        if self.state > ConnState::Alloced {
            if self.state < ConnState::QuitSent {
                self.stats.inc(Stat::CloseImplicit);
                reconnect = true;
                self.send_close(req)?;
            }
            self.free_contents(req);
            self.net.compressed = false;
            self.net.ssl = false;
        } else {
            self.options.max_allowed_packet = ASSEMBLED_PACKET_MAX_SIZE;
        }
        let host = host.filter(|h| !h.is_empty()).unwrap_or(b"localhost");
        let user = user.unwrap_or_default();
        let password = password.unwrap_or_default();
        let db = db.filter(|d| !d.is_empty()).unwrap_or_default();
        if !db.is_empty() {
            flags |= client::CONNECT_WITH_DB;
        }
        let mut socket = socket.map(<[u8]>::to_vec);
        let (transport, unix) = ConnData::scheme(host, &mut socket, port);
        let flags = self.connect_flags(flags);
        // `mysqlnd_vio::open_tcp_or_unix`'s `persistent_id`, the
        // connection's address.
        self.net.persistent_id = self
            .persistent
            .then(|| format!("{:p}", std::ptr::from_ref(self)).into_bytes());
        // `connect_handshake`.
        let connected = self.net.connect(req, &transport, &mut self.error)? && {
            self.net.reset();
            self.handshake(req, user, password, db, flags)?
        };
        if !connected {
            if self.error.errno == 0 {
                let via = String::from_utf8_lossy(&transport).into_owned();
                self.error.client(
                    req,
                    cr::CONNECTION_ERROR,
                    &format!("Unknown error while trying to connect via {via}"),
                )?;
            }
            self.free_contents(req);
            self.stats.inc(Stat::ConnectFailure);
            return Ok(false);
        }
        self.state = ConnState::Ready;
        self.net.compressed = flags & client::COMPRESS != 0;
        let scheme = Pstr::dup(req, &transport, self.persistent)?;
        Pstr::put(req, &mut self.scheme, Some(scheme));
        self.port = port;
        let info = if unix {
            b"Localhost via UNIX socket".to_vec()
        } else {
            let mut info = host.to_vec();
            info.extend_from_slice(b" via TCP/IP");
            info
        };
        let info = Pstr::dup(req, &info, self.persistent)?;
        Pstr::put(req, &mut self.host_info, Some(info));
        self.error.reset(req);
        if !self.execute_init_commands(req)? {
            if self.error.errno == 0 {
                self.error.client(
                    req,
                    cr::CONNECTION_ERROR,
                    "Unknown error while trying to connect",
                )?;
            }
            self.free_contents(req);
            self.stats.inc(Stat::ConnectFailure);
            return Ok(false);
        }
        self.stats.inc(Stat::ConnectSuccess);
        self.stats.inc(Stat::OpenedConnections);
        if reconnect {
            self.stats.global(Stat::Reconnect, 1);
        }
        if self.persistent {
            self.stats.inc(Stat::PconnectSuccess);
            self.stats.inc(Stat::OpenedPersistentConnections);
        }
        Ok(true)
    }

    /// `mysqlnd_command::handshake`: the greeting read, the connection's
    /// facts taken from it, the authentication carried out.
    fn handshake(
        &mut self,
        req: &mut Request,
        user: &[u8],
        password: &[u8],
        db: &[u8],
        flags: u32,
    ) -> Result<bool, Thrown> {
        let Some(greet) = self.read_greet(req)? else {
            docref(
                req,
                Level::Warning,
                &format!(
                    "Error while reading greeting packet. PID={}",
                    std::process::id()
                ),
            );
            self.server_capabilities = 0;
            return Ok(false);
        };
        if let Some(e) = &greet.error {
            e.set_on(req, &mut self.error)?;
            self.server_capabilities = 0;
            return Ok(false);
        }
        if greet.pre41 {
            let version = String::from_utf8_lossy(&greet.server_version);
            let shown: String = version.chars().take(32).collect();
            self.error.client(
                req,
                cr::NOT_IMPLEMENTED,
                &format!("Connecting to 3.22, 3.23 & 4.0 is not supported. Server is {shown}"),
            )?;
            self.server_capabilities = 0;
            return Ok(false);
        }
        self.thread_id = u64::from(greet.thread_id);
        self.protocol_version = greet.protocol_version;
        let version = Pstr::dup(req, &greet.server_version, self.persistent)?;
        Pstr::put(req, &mut self.server_version, Some(version));
        let mut charset_no = u32::from(greet.charset_no);
        self.greet_charset = charset::by_nr(charset_no);
        if self.greet_charset.is_none() {
            charset_no = if self.server_version() >= 50500 {
                charset::UTF8_MB4_DEFAULT_ID
            } else {
                charset::UTF8_MB3_DEFAULT_ID
            };
            self.greet_charset = charset::by_nr(charset_no);
        }
        self.server_capabilities = greet.server_capabilities;
        let authenticated = self.connect_run_authentication(
            req,
            (user, password, db),
            &greet.auth_plugin_data,
            greet.auth_protocol.as_deref(),
            charset_no,
            flags,
        )?;
        if !authenticated {
            self.server_capabilities = 0;
            return Ok(false);
        }
        self.upsert.reset();
        self.upsert.server_status = u32::from(greet.server_status);
        Ok(true)
    }

    /// `mysqlnd_conn_data::execute_init_commands`.
    fn execute_init_commands(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let commands: Vec<Vec<u8>> = self
            .options
            .init_commands
            .iter()
            .map(<[u8]>::to_vec)
            .collect();
        for command in commands {
            self.stats.inc(Stat::InitCommandExecutedCount);
            if !self.query(req, &command)? {
                self.stats.inc(Stat::InitCommandFailedCount);
                return Ok(false);
            }
            loop {
                if self.last_query_type == QueryType::Select
                    && let Some(result) = self.use_unbuffered(req)?
                {
                    result.free(req, self, true)?;
                }
                if !self.next_rowset(req)? {
                    break;
                }
            }
        }
        Ok(true)
    }

    // ---- queries

    /// `mysqlnd_conn_data::query`: sent and its answer read; affected
    /// rows counted.
    pub(crate) fn query(&mut self, req: &mut Request, sql: &[u8]) -> Result<bool, Thrown> {
        if !(self.send_query(req, sql)? && self.reap_query(req)?) {
            return Ok(false);
        }
        if self.last_query_type == QueryType::Upsert && self.upsert.affected_rows != 0 {
            self.stats
                .add(Stat::RowsAffectedNormal, self.upsert.affected_rows);
        }
        Ok(true)
    }

    /// `mysqlnd_command::query`.
    pub(crate) fn send_query(&mut self, req: &mut Request, sql: &[u8]) -> Result<bool, Thrown> {
        let sent = self.send_command(req, Com::Query, sql, false)?;
        if sent {
            self.state = ConnState::QuerySent;
        }
        Ok(sent)
    }

    /// `mysqlnd_command::reap_result`.
    pub(crate) fn reap_query(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        if self.state <= ConnState::Ready || self.state == ConnState::QuitSent {
            docref(
                req,
                Level::Warning,
                "Connection not opened, clear or has been closed",
            );
            return Ok(false);
        }
        self.read_result_set_header(req, None)
    }

    /// `mysqlnd_query_read_result_set_header`: the first answer to a
    /// query or a `COM_STMT_EXECUTE`; for a statement (`stmt`) the column
    /// definitions go to the statement and not to `current_result`.
    pub(crate) fn read_result_set_header(
        &mut self,
        req: &mut Request,
        stmt: Option<&mut Option<Box<Res>>>,
    ) -> Result<bool, Thrown> {
        self.upsert.affected_rows_to_error();
        let Some(header) = self.read_rset_header(req)? else {
            if self.error.errno != cr::SERVER_GONE_ERROR
                && self.error.errno != cr::CLIENT_INTERACTION_TIMEOUT
            {
                docref(req, Level::Warning, "Error reading result set's header");
            }
            return Ok(false);
        };
        match header {
            RsetHeader::Error(e) => {
                self.upsert.server_status &= !status::MORE_RESULTS_EXISTS;
                e.set_on(req, &mut self.error)?;
                self.state = ConnState::Ready;
                Ok(false)
            }
            RsetHeader::LocalInfile(file) => {
                self.error.errno = 0;
                self.last_query_type = QueryType::LoadLocal;
                self.field_count = 0;
                self.state = ConnState::SendingLoadData;
                let (ok, warning) = crate::loaddata::handle(req, self, &file)?;
                self.state = if ok || warning {
                    ConnState::Ready
                } else {
                    ConnState::QuitSent
                };
                self.stats.inc(Stat::NonRsetQuery);
                Ok(ok)
            }
            RsetHeader::Upsert {
                affected_rows,
                last_insert_id,
                server_status,
                warning_count,
                info,
            } => {
                self.error.errno = 0;
                self.last_query_type = QueryType::Upsert;
                self.field_count = 0;
                self.upsert.reset();
                self.upsert.warning_count = warning_count;
                self.upsert.server_status = server_status;
                self.upsert.affected_rows = affected_rows;
                self.upsert.last_insert_id = last_insert_id;
                self.set_last_message(req, info.as_deref())?;
                self.state = if server_status & status::MORE_RESULTS_EXISTS != 0 {
                    ConnState::NextResultPending
                } else {
                    ConnState::Ready
                };
                self.stats.inc(Stat::NonRsetQuery);
                Ok(true)
            }
            RsetHeader::Fields(count) => self.read_fields_header(req, count, stmt),
        }
    }

    /// The rows' half of `mysqlnd_query_read_result_set_header`: the
    /// definitions of `count` columns read into the statement's
    /// `MYSQLND_RES` or a new `current_result`, then the EOF and its status.
    fn read_fields_header(
        &mut self,
        req: &mut Request,
        count: u64,
        mut stmt: Option<&mut Option<Box<Res>>>,
    ) -> Result<bool, Thrown> {
        self.error.errno = 0;
        self.set_last_message(req, None)?;
        self.stats.inc(Stat::RsetQuery);
        self.upsert.reset();
        self.upsert.affected_rows_to_error();
        self.last_query_type = QueryType::Select;
        self.state = ConnState::FetchingData;
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        self.field_count = count;
        let held = stmt.as_deref_mut().and_then(Option::take);
        let mut result = match held {
            Some(res) if res.field_count == count => res,
            Some(res) => {
                res.free(req, self, true)?;
                Box::new(Res::new(req, count)?)
            }
            None => Box::new(Res::new(req, count)?),
        };
        if !result.read_metadata(req, self)? {
            match stmt {
                Some(held) => *held = Some(result),
                None => result.free(req, self, true)?,
            }
            return Ok(false);
        }
        let Some(eof) = self.read_eof(req)? else {
            result.release(req);
            if let Some(held) = stmt {
                *held = None;
            }
            return Ok(false);
        };
        self.upsert.warning_count = eof.warning_count;
        self.upsert.server_status = eof.server_status;
        let counted = if eof.server_status & status::QUERY_NO_GOOD_INDEX_USED != 0 {
            Some(Stat::BadIndexUsed)
        } else if eof.server_status & status::QUERY_NO_INDEX_USED != 0 {
            Some(Stat::NoIndexUsed)
        } else if eof.server_status & status::QUERY_WAS_SLOW != 0 {
            Some(Stat::QueryWasSlow)
        } else {
            None
        };
        if let Some(counted) = counted {
            self.stats.inc(counted);
        }
        match stmt {
            Some(held) => *held = Some(result),
            None => self.current_result = Some(result),
        }
        Ok(true)
    }

    /// `mysqlnd_conn_data::more_results`.
    pub(crate) fn more_rowsets(&self) -> bool {
        self.upsert.server_status & status::MORE_RESULTS_EXISTS != 0
    }

    /// `mysqlnd_conn_data::next_result`.
    pub(crate) fn next_rowset(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        self.error.reset(req);
        if self.state != ConnState::NextResultPending {
            return Ok(false);
        }
        self.upsert.affected_rows_to_error();
        if !self.read_result_set_header(req, None)? {
            if self.error.errno == 0 {
                docref(
                    req,
                    Level::Warning,
                    &format!("Serious error. PID={}", std::process::id()),
                );
                self.state = ConnState::QuitSent;
                self.send_close(req)?;
            }
            return Ok(false);
        }
        if self.last_query_type == QueryType::Upsert && self.upsert.affected_rows != 0 {
            self.stats
                .add(Stat::RowsAffectedNormal, self.upsert.affected_rows);
        }
        Ok(true)
    }

    /// `mysqlnd_conn_data::use_result`.
    pub(crate) fn use_unbuffered(&mut self, req: &mut Request) -> Result<Option<Box<Res>>, Thrown> {
        if self.current_result.is_none() {
            return Ok(None);
        }
        if self.last_query_type != QueryType::Select || self.state != ConnState::FetchingData {
            self.error
                .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
            return Ok(None);
        }
        self.stats.inc(Stat::UnbufferedSets);
        let Some(mut result) = self.current_result.take() else {
            return Ok(None);
        };
        result.use_unbuffered(req, false)?;
        Ok(Some(result))
    }

    /// `mysqlnd_conn_data::store_result`: a store that fails frees the
    /// rows, as php's caller does.
    pub(crate) fn store(&mut self, req: &mut Request) -> Result<Option<Box<Res>>, Thrown> {
        if self.current_result.is_none() {
            return Ok(None);
        }
        if self.last_query_type != QueryType::Select || self.state != ConnState::FetchingData {
            self.error
                .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
            return Ok(None);
        }
        self.stats.inc(Stat::BufferedSets);
        let Some(mut result) = self.current_result.take() else {
            return Ok(None);
        };
        if !result.store(req, self, false)? {
            result.free(req, self, true)?;
            return Ok(None);
        }
        Ok(Some(result))
    }

    // ---- the other commands

    /// `mysqlnd_command::init_db`.
    pub(crate) fn select_db(&mut self, req: &mut Request, db: &[u8]) -> Result<bool, Thrown> {
        let ok = self.send_command(req, Com::InitDb, db, false)?
            && self.handle_response(req, false, false, Com::InitDb, true)?;
        self.upsert.affected_rows_to_error();
        Ok(ok)
    }

    /// `mysqlnd_command::ping`.
    pub(crate) fn ping(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let ok = self.send_command(req, Com::Ping, &[], true)?
            && self.handle_response(req, false, true, Com::Ping, true)?;
        self.upsert.affected_rows_to_error();
        Ok(ok)
    }

    /// `mysqlnd_command::statistics`: the server's text.
    pub(crate) fn statistic(&mut self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        if !self.send_command(req, Com::Statistics, &[], false)? {
            return Ok(None);
        }
        self.read_stats(req)
    }

    /// `mysqlnd_conn_data::kill`: killing one's own connection reads no
    /// answer and closes.
    pub(crate) fn kill(&mut self, req: &mut Request, pid: u32) -> Result<bool, Thrown> {
        let read_response = u64::from(pid) != self.thread_id;
        let sent = self.send_command(req, Com::ProcessKill, &pid.to_le_bytes(), false)?;
        let ok = sent
            && (!read_response
                || self.handle_response(req, false, false, Com::ProcessKill, true)?);
        if read_response {
            self.upsert.affected_rows_to_error();
        } else if sent {
            self.state = ConnState::QuitSent;
            self.send_close(req)?;
        }
        Ok(ok)
    }

    /// `mysqlnd_conn_data::set_charset`.
    pub(crate) fn set_charset(&mut self, req: &mut Request, name: &[u8]) -> Result<bool, Thrown> {
        let Some(found) = charset::by_name(name) else {
            self.error.client(
                req,
                cr::CANT_FIND_CHARSET,
                "Invalid character set was provided",
            )?;
            return Ok(false);
        };
        let mut sql = b"SET NAMES ".to_vec();
        sql.extend_from_slice(name);
        if !self.query(req, &sql)? || self.error.errno != 0 {
            return Ok(false);
        }
        self.charset = Some(found);
        Ok(true)
    }

    /// `mysqlnd_command::set_option`.
    pub(crate) fn set_server_option(
        &mut self,
        req: &mut Request,
        option: ServerOption,
    ) -> Result<bool, Thrown> {
        let arg = (option as u16).to_le_bytes();
        Ok(self.send_command(req, Com::SetOption, &arg, false)?
            && self.handle_response(req, true, false, Com::SetOption, true)?)
    }

    /// `mysqlnd_command::debug`: `COM_DEBUG`, answered by an EOF.
    pub(crate) fn debug(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self.send_command(req, Com::Debug, &[], false)?
            && self.handle_response(req, true, false, Com::Debug, true)?)
    }

    /// `mysqlnd_command::refresh`.
    pub(crate) fn refresh(&mut self, req: &mut Request, options: u8) -> Result<bool, Thrown> {
        self.command_ok(req, Com::Refresh, &[options], false)
    }

    /// `mysqlnd_conn_data::set_autocommit`.
    pub(crate) fn autocommit(&mut self, req: &mut Request, on: bool) -> Result<bool, Thrown> {
        let sql: &[u8] = if on {
            b"SET AUTOCOMMIT=1"
        } else {
            b"SET AUTOCOMMIT=0"
        };
        self.query(req, sql)
    }

    /// `mysqlnd_conn_data::tx_commit_or_rollback`.
    pub(crate) fn tx_commit_or_rollback(
        &mut self,
        req: &mut Request,
        commit: bool,
        flags: u32,
        name: Option<&[u8]>,
    ) -> Result<bool, Thrown> {
        let mut options = String::new();
        let mut word = |w: &str| {
            if !options.is_empty() {
                options.push(' ');
            }
            options.push_str(w);
        };
        if flags & trans_cor::AND_CHAIN != 0 && flags & trans_cor::AND_NO_CHAIN == 0 {
            word("AND CHAIN");
        } else if flags & trans_cor::AND_NO_CHAIN != 0 && flags & trans_cor::AND_CHAIN == 0 {
            word("AND NO CHAIN");
        }
        if flags & trans_cor::RELEASE != 0 && flags & trans_cor::NO_RELEASE == 0 {
            word("RELEASE");
        } else if flags & trans_cor::NO_RELEASE != 0 && flags & trans_cor::RELEASE == 0 {
            word("NO RELEASE");
        }
        let mut sql = if commit {
            b"COMMIT".to_vec()
        } else {
            b"ROLLBACK".to_vec()
        };
        sql.extend_from_slice(&tx_name(req, name));
        sql.push(b' ');
        sql.extend_from_slice(options.as_bytes());
        self.query(req, &sql)
    }

    /// `mysqlnd_conn_data::tx_begin`.
    pub(crate) fn tx_begin(
        &mut self,
        req: &mut Request,
        mode: u32,
        name: Option<&[u8]>,
    ) -> Result<bool, Thrown> {
        let mut options = String::new();
        if mode & trans_start::WITH_CONSISTENT_SNAPSHOT != 0 {
            options.push_str("WITH CONSISTENT SNAPSHOT");
        }
        let mut characteristic = |w: &str| {
            if !options.is_empty() {
                options.push_str(", ");
            }
            options.push_str(w);
        };
        if mode & trans_start::READ_WRITE != 0 {
            characteristic("READ WRITE");
        } else if mode & trans_start::READ_ONLY != 0 {
            characteristic("READ ONLY");
        }
        let mut sql = b"START TRANSACTION".to_vec();
        sql.extend_from_slice(&tx_name(req, name));
        sql.push(b' ');
        sql.extend_from_slice(options.as_bytes());
        let ok = self.query(req, &sql)?;
        if ok
            && mode & (trans_start::READ_WRITE | trans_start::READ_ONLY) != 0
            && self.error.errno == 1064
        {
            self.error.client(
                req,
                cr::NOT_IMPLEMENTED,
                "This server version doesn't support 'READ WRITE' and 'READ ONLY'. Minimum 5.6.5 is required",
            )?;
        }
        Ok(ok)
    }

    /// `mysqlnd_conn_data::tx_savepoint` and `tx_savepoint_release`.
    pub(crate) fn savepoint(
        &mut self,
        req: &mut Request,
        name: Option<&[u8]>,
        release: bool,
    ) -> Result<bool, Thrown> {
        let Some(name) = name else {
            self.error
                .client(req, cr::UNKNOWN_ERROR, "Savepoint name not provided")?;
            return Ok(false);
        };
        let mut sql = if release {
            b"RELEASE SAVEPOINT `".to_vec()
        } else {
            b"SAVEPOINT `".to_vec()
        };
        sql.extend_from_slice(name);
        sql.push(b'`');
        self.query(req, &sql)
    }

    /// `mysqlnd_conn_data::change_user`.
    pub(crate) fn change_user(
        &mut self,
        req: &mut Request,
        user: Option<&[u8]>,
        password: Option<&[u8]>,
        db: Option<&[u8]>,
        silent: bool,
    ) -> Result<bool, Thrown> {
        self.error.reset(req);
        self.upsert.affected_rows_to_error();
        let user = user.unwrap_or_default();
        let password = password.unwrap_or_default();
        let db = db.unwrap_or_default();
        let db = php_ext::coerce::c_str(db);
        let data = self
            .auth_plugin_data
            .as_ref()
            .map(|d| d.bytes().to_vec())
            .unwrap_or_default();
        let protocol = self
            .options
            .auth_protocol
            .as_ref()
            .map(|p| p.bytes().to_vec());
        self.run_authentication(
            req,
            (user, password, db),
            &data,
            protocol.as_deref(),
            0,
            self.server_capabilities,
            (silent, true),
        )
    }

    /// `mysqlnd_conn_data::escape_string`: quotes doubled under
    /// `NO_BACKSLASH_ESCAPES`, backslashes added otherwise, by the
    /// connection's character set.
    pub(crate) fn escape_string(&self, from: &[u8]) -> Vec<u8> {
        let fallback = charset::by_nr(charset::UTF8_MB4_DEFAULT_ID);
        let Some(cset) = self.charset.or(fallback) else {
            return from.to_vec();
        };
        if self.upsert.server_status & status::NO_BACKSLASH_ESCAPES != 0 {
            charset::escape_quotes(cset, from)
        } else {
            charset::escape_slashes(cset, from)
        }
    }

    /// `mysqlnd_conn_data::get_server_version`: `major * 10000 + minor *
    /// 100 + patch`, read with `strtol` from the server's version.
    pub(crate) fn server_version(&self) -> u64 {
        let Some(version) = self.server_version.as_ref().map(Pstr::bytes) else {
            return 0;
        };
        let mut parts = [0u64; 3];
        let mut at = 0;
        for part in &mut parts {
            let digits = version
                .get(at..)
                .unwrap_or_default()
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            *part = std::str::from_utf8(version.get(at..at + digits).unwrap_or_default())
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(0);
            at += digits + 1;
        }
        parts[0] * 10000 + parts[1] * 100 + parts[2]
    }

    // ---- closing

    /// `mysqlnd_conn_data::send_close`.
    pub(crate) fn send_close(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let mut ok = true;
        if self.state >= ConnState::Ready {
            self.stats.global(Stat::OpenedConnections, u64::MAX);
            if self.persistent {
                self.stats
                    .global(Stat::OpenedPersistentConnections, u64::MAX);
            }
        }
        match self.state {
            // `mysqlnd_send_close`: the stream closed whatever the quit
            // came to, a link the server dropped among them.
            ConnState::Ready => {
                if self.net.open() {
                    let sent = self.send_command(req, Com::Quit, &[], true);
                    self.net.close(req);
                    self.state = ConnState::QuitSent;
                    ok = sent?;
                }
                self.state = ConnState::QuitSent;
            }
            ConnState::SendingLoadData
            | ConnState::NextResultPending
            | ConnState::QuerySent
            | ConnState::FetchingData => {
                self.stats.global(Stat::CloseInMiddle, 1);
                self.state = ConnState::QuitSent;
                self.net.close(req);
            }
            ConnState::Alloced => {
                self.state = ConnState::QuitSent;
                self.net.close(req);
            }
            ConnState::QuitSent => self.net.close(req),
        }
        Ok(ok)
    }

    /// `mysqlnd_conn_data::free_contents`.
    pub(crate) fn free_contents(&mut self, req: &mut Request) {
        if let Some(result) = self.current_result.take() {
            result.free(req, self, true).ok();
        }
        self.net.free_contents(req);
        self.net.close(req);
        Pstr::put(req, &mut self.scheme, None);
        Pstr::put(req, &mut self.server_version, None);
        Pstr::put(req, &mut self.host_info, None);
        Pstr::put(req, &mut self.auth_plugin_data, None);
        Pstr::put(req, &mut self.last_message, None);
        self.charset = None;
        self.greet_charset = None;
    }

    /// `mysqlnd_conn::close`: the close counted by its kind, the close
    /// sent.
    pub(crate) fn close(&mut self, req: &mut Request, kind: CloseType) -> Result<bool, Thrown> {
        if self.state >= ConnState::Ready {
            self.stats.inc(match kind {
                CloseType::Explicit => Stat::CloseExplicit,
                CloseType::Implicit => Stat::CloseImplicit,
                CloseType::Disconnected => Stat::CloseDisconnect,
            });
        }
        self.send_close(req)
    }

    /// `mysqlnd_conn_data::sqlstate`.
    pub(crate) fn sqlstate(&self) -> [u8; 5] {
        self.error.state()
    }
}

/// `STAT_COM_QUIT + command - 1`: the statistic of each command.
fn com_stat(command: Com) -> Stat {
    match command {
        Com::Quit => Stat::ComQuit,
        Com::InitDb => Stat::ComInitDb,
        Com::Query => Stat::ComQuery,
        Com::Refresh => Stat::ComRefresh,
        Com::Statistics => Stat::ComStatistics,
        Com::ProcessKill => Stat::ComProcessKill,
        Com::Debug => Stat::ComDebug,
        Com::Ping => Stat::ComPing,
        Com::ChangeUser => Stat::ComChangeUser,
        Com::StmtPrepare => Stat::ComStmtPrepare,
        Com::StmtExecute => Stat::ComStmtExecute,
        Com::StmtSendLongData => Stat::ComStmtSendLongData,
        Com::StmtClose => Stat::ComStmtClose,
        Com::StmtReset => Stat::ComStmtReset,
        Com::SetOption => Stat::ComSetOption,
        Com::StmtFetch => Stat::ComStmtFetch,
    }
}

/// `mysqlnd_escape_string_for_tx_name_in_comment`: ` /*name*/` with the
/// characters php allows, warning once where it dropped one.
fn tx_name(req: &mut Request, name: Option<&[u8]>) -> Vec<u8> {
    let Some(name) = name else {
        return Vec::new();
    };
    let mut out = b" /*".to_vec();
    let mut warned = false;
    for &v in name.iter().take_while(|b| **b != 0) {
        if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_' | b' ' | b'=') {
            out.push(v);
        } else if !warned {
            docref(
                req,
                Level::Warning,
                "Transaction name has been truncated, since it can only contain the A-Z, a-z, 0-9, \"\\\", \"-\", \"_\", and \"=\" characters",
            );
            warned = true;
        }
    }
    out.extend_from_slice(b"*/");
    out
}
