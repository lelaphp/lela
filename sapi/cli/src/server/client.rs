//! One connection of the built-in server, `php_cli_server_client`, as a
//! state machine ([`Connection`]) the event loop drives: the request
//! read as its bytes arrive and parsed as
//! `php_cli_server_client_read_request` parses it, its path translated
//! against the document root, then dispatched as
//! `php_cli_server_dispatch` dispatches it, through the engine where a
//! router or a PHP file serves it, on the loop's thread before it waits
//! again, or as a static file or an error page the server writes itself
//! as the socket takes it, and the connection closed.

use std::fmt::Write as _;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use php_proto::http::{self, Credentials, Events, Parser};
use php_proto::vpath;
use php_sapi::cgi::Disk;
use php_sapi::{
    Aborted, Chdir, Method, OutputSink, PHP_VERSION, ProgramFile, ProgramServed,
    RequestDescription, ResponseHead, Routed, WebRequest,
};

use super::Site;
use super::log::Type;
use crate::ffi::{Interest, Polled};

/// Where one connection stands.
pub(super) enum Connection {
    /// `php_cli_server_client_read_request`: read and parsed as its bytes
    /// arrive, until the request is whole.
    Reading(Client),
    /// A response the server writes itself, a static file or an error
    /// page, `content_sender`: sent as the socket takes it.
    Sending(Client, Pending),
    /// Closed, and `%s Closing` logged.
    Closed,
}

impl Connection {
    /// `php_cli_server_client_ctor`: the connection `stream` accepted,
    /// its peer named `name` in the log, its request yet to read.
    pub(super) fn accepted(stream: TcpStream, name: String) -> Connection {
        Connection::Reading(Client {
            stream,
            name,
            parser: Parser::new(),
            building: Building::default(),
        })
    }

    /// The socket as the loop waits on it, for a read while the request
    /// is read and for a write while a response is sent
    /// (`php_cli_server_poller_add`); `None` once it is closed.
    pub(super) fn polled(&self) -> Option<Polled> {
        match self {
            Connection::Reading(client) => {
                Some(Polled::new(client.stream.as_raw_fd(), Interest::Read))
            }
            Connection::Sending(client, _) => {
                Some(Polled::new(client.stream.as_raw_fd(), Interest::Write))
            }
            Connection::Closed => None,
        }
    }

    /// Whether the connection is still open, and so kept by the loop.
    pub(super) fn is_open(&self) -> bool {
        !matches!(self, Connection::Closed)
    }

    /// The socket ready for what [`Connection::polled`] waited on it for:
    /// `php_cli_server_recv_event_read_request`
    /// (`Z/sapi/cli/php_cli_server.c:2636`) for a request being read,
    /// which serves the request once it is whole, and
    /// `php_cli_server_send_event` for a response being sent.
    pub(super) fn ready(self, site: &Arc<Site>) -> Connection {
        match self {
            Connection::Reading(client) => read(site, client),
            Connection::Sending(client, pending) => send(site, client, pending),
            Connection::Closed => Connection::Closed,
        }
    }

    /// `php_cli_server_client_dtor_wrapper`
    /// (`Z/sapi/cli/php_cli_server.c:2415`) as `php_cli_server_dtor`
    /// destroys the clients: the socket shut, nothing logged.
    pub(super) fn drop_unlogged(self) {
        if let Connection::Reading(client) | Connection::Sending(client, _) = self {
            client.stream.shutdown(Shutdown::Both).ok();
        }
    }
}

/// A connection's own: the socket, the peer's name, the parser, and the
/// request it is building.
pub(super) struct Client {
    stream: TcpStream,
    name: String,
    parser: Parser,
    building: Building,
}

/// The request as the parser's callbacks build it
/// (`php_cli_server_request` and the client's header state).
#[derive(Default)]
struct Building {
    request: Request,
    /// `last_header_element`: what the last header callback carried.
    last: Element,
    name: Vec<u8>,
    value: Vec<u8>,
    /// `request_read`.
    read: bool,
    /// The document root, for `php_cli_server_request_translate_vpath`.
    root: Vec<u8>,
}

/// `HEADER_NONE`, `HEADER_FIELD`, `HEADER_VALUE`.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Element {
    #[default]
    None,
    Field,
    Value,
}

/// `php_cli_server_request`.
#[derive(Default)]
struct Request {
    method: Option<http::Method>,
    /// `protocol_version`, the major version times 100 and the minor.
    version: u16,
    uri: Option<Vec<u8>>,
    vpath: Option<Vec<u8>>,
    path_translated: Option<Vec<u8>>,
    path_info: Option<Vec<u8>>,
    query: Option<Vec<u8>>,
    /// `headers`, by the name lowered, in the order first seen.
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// `headers_original_case`.
    original: Vec<(Vec<u8>, Vec<u8>)>,
    content: Option<Vec<u8>>,
    ext: Vec<u8>,
    /// `sb.st_size` of what the path translated to.
    size: u64,
}

impl Request {
    /// A header by its lowered name.
    fn header(&self, lowered: &[u8]) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(name, _)| name == lowered)
            .map(|(_, value)| value.as_slice())
    }

    fn method(&self) -> http::Method {
        self.method.unwrap_or(http::Method::NOT_IMPLEMENTED)
    }
}

impl Events for Building {
    fn path(&mut self, at: &[u8]) -> bool {
        if self.request.vpath.is_some() {
            return false;
        }
        self.request.vpath = Some(http::normalize_vpath(at));
        true
    }

    fn query_string(&mut self, at: &[u8]) -> bool {
        self.request
            .query
            .get_or_insert_default()
            .extend_from_slice(at);
        true
    }

    fn url(&mut self, method: http::Method, at: &[u8]) -> bool {
        self.request.method.get_or_insert(method);
        self.request
            .uri
            .get_or_insert_default()
            .extend_from_slice(at);
        true
    }

    fn header_field(&mut self, at: &[u8]) -> bool {
        match self.last {
            Element::Value => {
                self.save_header();
                self.name = at.to_vec();
            }
            Element::None => self.name = at.to_vec(),
            Element::Field => self.name.extend_from_slice(at),
        }
        self.last = Element::Field;
        true
    }

    fn header_value(&mut self, at: &[u8]) -> bool {
        match self.last {
            Element::Field => self.value = at.to_vec(),
            Element::Value | Element::None => self.value.extend_from_slice(at),
        }
        self.last = Element::Value;
        true
    }

    fn headers_complete(&mut self) -> bool {
        match self.last {
            Element::None => {}
            Element::Field => {
                self.value.clear();
                self.save_header();
            }
            Element::Value => self.save_header(),
        }
        self.last = Element::None;
        true
    }

    fn body(&mut self, at: &[u8]) {
        self.request
            .content
            .get_or_insert_default()
            .extend_from_slice(at);
    }

    fn message_complete(&mut self, major: u16, minor: u16) -> bool {
        self.request.version = major.saturating_mul(100).saturating_add(minor);
        translate_vpath(&mut self.request, &self.root);
        if let Some(vpath) = &self.request.vpath {
            self.request.ext = vpath::extension(vpath).to_vec();
        }
        self.read = true;
        true
    }
}

impl Building {
    /// `php_cli_server_client_save_header`: the header under its lowered
    /// name, its value joined to one already there with `, ` but for
    /// `Set-Cookie`, and under its name as sent with the same value.
    fn save_header(&mut self) {
        let name = std::mem::take(&mut self.name);
        let value = std::mem::take(&mut self.value);
        let lowered = name.to_ascii_lowercase();
        let headers = &mut self.request.headers;
        let value = match headers.iter().position(|(had, _)| *had == lowered) {
            Some(at) if lowered != b"set-cookie" => {
                let mut joined = headers[at].1.clone();
                joined.extend_from_slice(b", ");
                joined.extend_from_slice(&value);
                headers[at].1.clone_from(&joined);
                joined
            }
            Some(at) => {
                headers[at].1.clone_from(&value);
                value
            }
            None => {
                headers.push((lowered, value.clone()));
                value
            }
        };
        let original = &mut self.request.original;
        match original.iter_mut().find(|(had, _)| *had == name) {
            Some((_, had)) => *had = value,
            None => original.push((name, value)),
        }
    }
}

/// `php_cli_server_request_translate_vpath`, the rule `php_proto::vpath`
/// holds for both servers: the request's path under the document root,
/// left as it was where it translates to nothing.
fn translate_vpath(request: &mut Request, root: &[u8]) {
    let Some(translated) = request
        .vpath
        .as_deref()
        .and_then(|vpath| vpath::translate(vpath, root, &Disk))
    else {
        return;
    };
    request.vpath = Some(translated.vpath);
    request.path_translated = Some(translated.path_translated);
    request.path_info = translated.path_info;
    request.size = translated.size;
}

/// A path's bytes as the file system names it.
fn os(bytes: &[u8]) -> &std::path::Path {
    use std::os::unix::ffi::OsStrExt;
    std::path::Path::new(std::ffi::OsStr::from_bytes(bytes))
}

/// `php_cli_server_recv_event_read_request`: one `recv` of up to 16383
/// bytes fed to the parser, nothing where none has arrived yet; a
/// request read whole is dispatched, a read that failed or found the end
/// first and a request php refuses close the connection with php's line.
fn read(site: &Arc<Site>, mut client: Client) -> Connection {
    let mut buf = [0u8; 16383];
    let why = match client.stream.read(&mut buf) {
        Ok(0) => "Unexpected EOF".to_owned(),
        Err(error) if error.kind() == ErrorKind::WouldBlock => {
            return Connection::Reading(client);
        }
        Err(error) => php_sapi::strerror(&error),
        Ok(read) => {
            client.building.root.clone_from(&site.document_root);
            let taken = client.parser.execute(&mut client.building, &buf[..read]);
            if taken == read {
                if !client.building.read {
                    return Connection::Reading(client);
                }
                if client.building.request.method() == http::Method::NOT_IMPLEMENTED {
                    return error_page(site, client, 501);
                }
                return dispatch(site, client);
            }
            if buf[0] & 0x80 != 0 || buf[0] == 0x16 {
                "Unsupported SSL request".to_owned()
            } else {
                "Malformed HTTP request".to_owned()
            }
        }
    };
    if why == "Unexpected EOF" && client.parser.at_start() {
        site.log.write(
            Type::Message,
            &format!(
                "{} Closed without sending a request; it was probably just an unused speculative preconnection",
                client.name
            ),
        );
    } else {
        site.log.write(
            Type::Error,
            &format!("{} Invalid request ({why})", client.name),
        );
    }
    close(site, &client)
}

/// `php_cli_server_close_connection`: `%s Closing`, and the socket shut.
fn close(site: &Site, client: &Client) -> Connection {
    site.log
        .write(Type::Message, &format!("{} Closing", client.name));
    client.stream.shutdown(Shutdown::Both).ok();
    Connection::Closed
}

/// `php_cli_server_dispatch` (`Z/sapi/cli/php_cli_server.c:2294`): a
/// request with no router whose file is not PHP is a static file; any
/// other is served by the engine, the router first where there is one,
/// and a static file after it where it declined.
fn dispatch(site: &Arc<Site>, client: Client) -> Connection {
    let request = &client.building.request;
    let is_static = !vpath::is_program(&request.ext, request.path_translated.is_some());
    if site.router.is_none() && is_static {
        return send_static(site, client);
    }
    let sink = Box::new(WireSink {
        wire: Wire {
            stream: client.stream.try_clone().ok(),
            version: request.version,
            host: request.header(b"host").map(<[u8]>::to_vec),
        },
        site: Arc::clone(site),
        headers_written: false,
    });
    let description = describe(site, &client, is_static);
    let (_, dispatched, _) = site
        .engine
        .dispatch(description, site.router.as_deref(), sink);
    match &dispatched.program {
        Some(ProgramServed::Ran { code, fatal }) => {
            log_response(site, &client, *code, None, fatal.as_ref());
        }
        // `php_cli_server_send_error_page`'s line for the 400 page that
        // `php_request_shutdown`'s head and the connection's close leave
        // unsent.
        Some(ProgramServed::NulInPath) => {
            let errno = php_sapi::strerror(&std::io::Error::last_os_error());
            log_response(site, &client, 400, Some(&errno), None);
        }
        None => {}
    }
    if dispatched.routed == Routed::Declined && is_static {
        // The file, after what the router wrote under the head it did
        // not send.
        return send_static(site, client);
    }
    close(site, &client)
}

/// The request description a PHP request is served under
/// (`php_cli_server_request_startup`, `sapi_cli_server_register_variables`).
fn describe(site: &Site, client: &Client, is_static: bool) -> RequestDescription {
    let request = &client.building.request;
    let method = request.method().name().as_bytes();
    RequestDescription {
        env: std::env::vars_os()
            .map(|(n, v)| (n.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
            .collect(),
        // The server's directory: the router is served in it
        // (`SAPI_OPTION_NO_CHDIR`), and `php_execute_script` enters the
        // program's (`php_cli_server_dispatch_script`).
        cwd: std::env::current_dir().unwrap_or_default(),
        chdir: Chdir::IntoProgramDir,
        server: server_variables(site, client),
        sapi_name: String::from(super::NAME),
        web: Some(WebRequest {
            method: Some(Method::of(method)),
            request_uri: request.uri.clone(),
            query: request.query.clone(),
            cookies: request.header(b"cookie").map(<[u8]>::to_vec),
            content_type: request.header(b"content-type").map(<[u8]>::to_vec),
            content_length: request.content.as_ref().map_or(0, |content| {
                i64::try_from(content.len()).unwrap_or(i64::MAX)
            }),
            body: php_sapi::Body::from(request.content.clone().unwrap_or_default()),
            headers: request.original.clone(),
            no_headers: false,
            response_code: 200,
            head_ini: &[],
            // The server's `path_translated` is the request's own,
            // `pestrndup`'d off the request's heap.
            path_copies: 0,
        }),
        script_name: if is_static {
            None
        } else {
            request.path_translated.clone()
        },
        program_file: ProgramFile::Unopened,
        ..RequestDescription::default()
    }
}

/// `sapi_cli_server_register_variables`
/// (`Z/sapi/cli/php_cli_server.c:684`), then the credentials
/// `php_handle_auth_data` read off the `Authorization` header, which
/// `php_register_server_variables` registers after the host's rows.
fn server_variables(site: &Site, client: &Client) -> Vec<(Vec<u8>, Vec<u8>)> {
    let request = &client.building.request;
    let mut rows = vec![(b"DOCUMENT_ROOT".to_vec(), site.document_root.clone())];
    match client.name.rfind(':') {
        Some(colon) => {
            let address = client.name[..colon]
                .trim_start_matches('[')
                .trim_end_matches(']');
            rows.push((b"REMOTE_ADDR".to_vec(), address.as_bytes().to_vec()));
            let port: String = client.name[colon + 1..].chars().take(7).collect();
            rows.push((b"REMOTE_PORT".to_vec(), port.into_bytes()));
        }
        None => rows.push((b"REMOTE_ADDR".to_vec(), client.name.as_bytes().to_vec())),
    }
    rows.push((
        b"SERVER_SOFTWARE".to_vec(),
        format!("PHP/{PHP_VERSION} (Development Server)").into_bytes(),
    ));
    rows.push((
        b"SERVER_PROTOCOL".to_vec(),
        format!("HTTP/{}.{}", request.version / 100, request.version % 100).into_bytes(),
    ));
    rows.push((b"SERVER_NAME".to_vec(), site.host.as_bytes().to_vec()));
    rows.push((b"SERVER_PORT".to_vec(), site.port.to_string().into_bytes()));
    if let Some(uri) = &request.uri {
        rows.push((b"REQUEST_URI".to_vec(), uri.clone()));
    }
    rows.push((
        b"REQUEST_METHOD".to_vec(),
        request.method().name().as_bytes().to_vec(),
    ));
    if let Some(vpath) = &request.vpath {
        rows.push((b"SCRIPT_NAME".to_vec(), vpath.clone()));
    }
    // `SCRIPT_FILENAME` is registered through `strlen`, and `PHP_SELF`
    // with the path's info through `%s%s`, so each stops at a NUL where
    // `SCRIPT_NAME` and `PATH_INFO`, registered with their lengths, keep it.
    let script_filename = match (&request.path_translated, &site.router) {
        (Some(file), _) => Some(php_proto::c_str(file)),
        (None, router) => router.as_deref(),
    };
    if let Some(file) = script_filename {
        rows.push((b"SCRIPT_FILENAME".to_vec(), file.to_vec()));
    }
    if let Some(info) = &request.path_info {
        rows.push((b"PATH_INFO".to_vec(), info.clone()));
    }
    let vpath = request.vpath.clone().unwrap_or_default();
    match &request.path_info {
        Some(info) if !info.is_empty() => {
            rows.push((
                b"PHP_SELF".to_vec(),
                [php_proto::c_str(&vpath), php_proto::c_str(info)].concat(),
            ));
        }
        _ if request.vpath.is_some() => rows.push((b"PHP_SELF".to_vec(), vpath)),
        _ => {}
    }
    if let Some(query) = &request.query {
        rows.push((b"QUERY_STRING".to_vec(), query.clone()));
    }
    for (name, value) in &request.headers {
        let key: Vec<u8> = name
            .iter()
            .map(|&b| {
                if b == b'-' {
                    b'_'
                } else {
                    b.to_ascii_uppercase()
                }
            })
            .collect();
        if key == b"CONTENT_TYPE" || key == b"CONTENT_LENGTH" {
            rows.push((key.clone(), value.clone()));
        }
        rows.push(([&b"HTTP_"[..], &key].concat(), value.clone()));
    }
    match request.header(b"authorization").and_then(http::credentials) {
        Some(Credentials::Basic { user, password }) => {
            rows.push((b"PHP_AUTH_USER".to_vec(), user));
            if let Some(password) = password {
                rows.push((b"PHP_AUTH_PW".to_vec(), password));
            }
        }
        Some(Credentials::Digest(digest)) => rows.push((b"PHP_AUTH_DIGEST".to_vec(), digest)),
        None => {}
    }
    rows
}

/// The socket a PHP request writes its response to, with what its head
/// reads of the request: the version and the `Host` header.
struct Wire {
    stream: Option<TcpStream>,
    version: u16,
    host: Option<Vec<u8>>,
}

impl Wire {
    /// `sapi_cli_server_send_headers` (`Z/sapi/cli/php_cli_server.c:551`):
    /// the status line the request set, or one from the code; the
    /// server's own headers; the request's; the blank line.
    fn render(&self, head: &ResponseHead) -> Vec<u8> {
        let mut out = match &head.status_line {
            Some(line) => [line.as_slice(), b"\r\n"].concat(),
            None => status_line(self.version, head.code),
        };
        let dated = head
            .lines
            .iter()
            .any(|line| line.len() > 5 && line[..5].eq_ignore_ascii_case(b"Date:"));
        essential_headers(&mut out, self.host.as_deref(), !dated);
        for line in head.lines.iter().filter(|line| !line.is_empty()) {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out
    }

    /// `php_cli_server_client_send_through`
    /// (`Z/sapi/cli/php_cli_server.c:1898`): every byte, waiting up to ten
    /// seconds each time the socket takes no more, or as many as the peer
    /// took before it went away or the wait ran out; whether all of them
    /// went, which a peer that is gone refuses.
    fn send(&mut self, mut bytes: &[u8]) -> bool {
        let Some(stream) = &mut self.stream else {
            return false;
        };
        while !bytes.is_empty() {
            match stream.write(bytes) {
                Ok(0) => {
                    self.stream = None;
                    return false;
                }
                Ok(sent) => bytes = bytes.get(sent..).unwrap_or_default(),
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        && crate::ffi::writable_within(stream.as_raw_fd(), SEND_WAIT) => {}
                Err(_) => {
                    self.stream = None;
                    return false;
                }
            }
        }
        true
    }
}

/// How long a PHP request's write waits for a socket that takes no more,
/// in milliseconds: `send_through`'s `tv` of ten seconds.
const SEND_WAIT: i32 = 10_000;

/// The sink a PHP request owns, `sapi_cli_server_ub_write` and
/// `sapi_cli_server_send_headers`: the body and the head onto the
/// [`Wire`] as the request writes them, the log onto the server's.
struct WireSink {
    wire: Wire,
    site: Arc<Site>,
    /// `client->headers_written`: the block was handed to the wire, sent
    /// or not, so a later ask sends nothing.
    headers_written: bool,
}

impl OutputSink for WireSink {
    /// `sapi_cli_server_ub_write`: a client that went away is
    /// `php_handle_aborted_connection`, with no status set, so a program
    /// that ignores the abort sees `connection_aborted()`.
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/srv' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/index.php", '<?php ignore_user_abort(true); for ($i = 0; $i < 50 && !connection_aborted(); $i++) { echo str_repeat("x", 65536); flush(); usleep(20000); } file_put_contents(__DIR__ . "/log", connection_aborted() . connection_status() . ($i < 50 ? " early" : " late"));');
    /// $p = proc_open([PHP_BINARY, '-n', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// $ready = [$pipes[2]]; $none = null;
    /// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// $c = stream_socket_client("tcp://127.0.0.1:$port");
    /// fwrite($c, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    /// fread($c, 1000);
    /// fclose($c);
    /// for ($i = 0; $i < 50 && !file_exists("$dir/log"); $i++) usleep(100000);
    /// echo file_get_contents("$dir/log"), "\n";
    /// proc_terminate($p); proc_close($p);
    /// unlink("$dir/log"); unlink("$dir/index.php"); rmdir($dir);
    /// ```
    /// ```expect process
    /// 11 early
    /// ```
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        if self.wire.send(bytes) {
            Ok(())
        } else {
            Err(Aborted { exit_status: None })
        }
    }

    fn flush(&mut self) {}

    /// `sapi_cli_server_send_headers` (`Z/sapi/cli/php_cli_server.c:551-587`):
    /// once, and [`Aborted`] where `send_through` sent fewer bytes than
    /// the block holds, `SAPI_HEADER_SEND_FAILED`, as a client that reset
    /// the connection leaves it (GH-23425).
    fn send_headers(&mut self, head: &ResponseHead) -> Result<(), Aborted> {
        if std::mem::replace(&mut self.headers_written, true) {
            return Ok(());
        }
        let bytes = self.wire.render(head);
        if self.wire.send(&bytes) {
            Ok(())
        } else {
            Err(Aborted { exit_status: None })
        }
    }

    /// `sapi_cli_server_log_message`: the entry on the server's log.
    fn log(&mut self, entry: &[u8]) {
        self.site
            .log
            .write(Type::Message, &String::from_utf8_lossy(entry));
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().lock().write_all(bytes).ok();
    }

    /// `php://stdout` is the server's own fd 1, never the client's
    /// socket: `php_stream_url_wrap_php` opens it as `dup(STDOUT_FILENO)`
    /// under any `sapi_module` not named `cli`, and the built-in server's is
    /// `cli-server` (`Z/ext/standard/php_fopen_wrapper.c:260-272`), so
    /// what a router logs there, as Laravel's `artisan serve` router logs
    /// each request, reaches the terminal the server was started from.
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/srv' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/index.php", '<?php file_put_contents("php://stdout", "to stdout\n"); echo "body\n";');
    /// $p = proc_open([PHP_BINARY, '-n', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// $ready = [$pipes[2]]; $none = null;
    /// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// $c = stream_socket_client("tcp://127.0.0.1:$port");
    /// fwrite($c, "GET / HTTP/1.0\r\nHost: x\r\n\r\n");
    /// $response = stream_get_contents($c);
    /// fclose($c);
    /// echo strtok($response, "\r\n"), "\n", substr($response, strpos($response, "\r\n\r\n") + 4);
    /// $ready = [$pipes[1]];
    /// echo stream_select($ready, $none, $none, 30) ? fgets($pipes[1]) : "nothing on the server's stdout\n";
    /// proc_terminate($p); proc_close($p);
    /// unlink("$dir/index.php"); rmdir($dir);
    /// ```
    /// ```expect process
    /// HTTP/1.0 200 OK
    /// body
    /// to stdout
    /// ```
    fn write_stdout(&mut self, bytes: &[u8]) -> bool {
        std::io::stdout().lock().flush().ok();
        crate::ffi::write_stdout(bytes).ok();
        true
    }
}

/// `append_http_status_line`: `HTTP/<major>.<minor> <code> <reason>`,
/// the code 200 where the request set none.
fn status_line(version: u16, code: i64) -> Vec<u8> {
    let code = if code == 0 { 200 } else { code };
    let reason = http::reason(code).unwrap_or("Unknown Status Code");
    format!(
        "HTTP/{}.{} {code} {reason}\r\n",
        version / 100,
        version % 100
    )
    .into_bytes()
}

/// `append_essential_headers`: the request's `Host`, the `Date` where
/// `dated` asks for it, and `Connection: close`.
fn essential_headers(out: &mut Vec<u8>, host: Option<&[u8]>, dated: bool) {
    if let Some(host) = host {
        out.extend_from_slice(b"Host: ");
        out.extend_from_slice(host);
        out.extend_from_slice(b"\r\n");
    }
    if dated {
        out.extend_from_slice(b"Date: ");
        out.extend_from_slice(http_date().as_bytes());
        out.extend_from_slice(b" GMT\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n");
}

/// `php_format_date("D, d M Y H:i:s", now, 0)`: the time now in UTC.
fn http_date() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    php_proto::http::date(seconds)
}

/// `php_cli_server_begin_send_static` (`Z/sapi/cli/php_cli_server.c:2144`):
/// a method that writes refused with 405, a path with a NUL with 400, a
/// file that does not open with 404; else the head with the type the
/// extension maps to and the length the path translated with, and the
/// file after it but for `HEAD`.
fn send_static(site: &Site, client: Client) -> Connection {
    let request = &client.building.request;
    if [http::Method::DELETE, http::Method::PUT, http::Method::PATCH].contains(&request.method()) {
        return error_page(site, client, 405);
    }
    if request
        .path_translated
        .as_deref()
        .is_some_and(vpath::holds_nul)
    {
        return error_page(site, client, 400);
    }
    let Some(file) = request
        .path_translated
        .as_ref()
        .and_then(|path| std::fs::File::open(os(path)).ok())
    else {
        return error_page(site, client, 404);
    };
    let mut head = status_line(request.version, 200);
    essential_headers(&mut head, request.header(b"host"), true);
    if let Some(content_type) = php_proto::mime::content_type(&request.ext) {
        head.extend_from_slice(b"Content-Type: ");
        head.extend_from_slice(content_type.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(format!("Content-Length: {}\r\n\r\n", request.size).as_bytes());
    let file = (request.method() != http::Method::HEAD).then_some(file);
    log_response(site, &client, 200, None, None);
    Connection::Sending(client, Pending::new(head, file))
}

/// `php_cli_server_send_error_page` (`Z/sapi/cli/php_cli_server.c:2014`):
/// php's page for `status` with the request's URI escaped into it, but
/// for `HEAD`, under the server's own head, and the line logged with
/// `strerror(errno)` as it stands.
fn error_page(site: &Site, client: Client, status: i64) -> Connection {
    let errno = php_sapi::strerror(&std::io::Error::last_os_error());
    let request = &client.building.request;
    let reason = http::reason(status).unwrap_or("Unknown Status Code");
    let mut body = Vec::new();
    if request.method() != http::Method::HEAD {
        let uri = escape_html(request.uri.as_deref().unwrap_or_default());
        body.extend_from_slice(format!("<!doctype html><html><head><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{status} {reason}</title>").as_bytes());
        body.extend_from_slice(CSS.as_bytes());
        body.extend_from_slice(b"</head><body>");
        let text = match status {
            400 => format!(
                "<h1>{reason}</h1><p>Your browser sent a request that this server could not understand.</p>"
            ),
            404 => format!(
                "<h1>{reason}</h1><p>The requested resource <code class=\"url\">{uri}</code> was not found on this server.</p>"
            ),
            405 => format!("<h1>{reason}</h1><p>Requested method not allowed.</p>"),
            500 => format!("<h1>{reason}</h1><p>The server is temporarily unavailable.</p>"),
            _ => format!("<h1>{reason}</h1><p>Request method not supported.</p>"),
        };
        body.extend_from_slice(text.as_bytes());
        body.extend_from_slice(b"</body></html>");
    }
    let mut head = status_line(request.version, status);
    essential_headers(&mut head, request.header(b"host"), true);
    head.extend_from_slice(format!("X-Powered-By: PHP/{PHP_VERSION}\r\n").as_bytes());
    head.extend_from_slice(b"Content-Type: text/html; charset=UTF-8\r\n");
    head.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    if status == 405 {
        head.extend_from_slice(b"Allow: GET, HEAD, POST\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head.extend_from_slice(&body);
    log_response(site, &client, status, Some(&errno), None);
    Connection::Sending(client, Pending::new(head, None))
}

/// `php_cli_server_css`, copied from `ext/standard/info.c`.
const CSS: &str = "<style>\n\
body { background-color: #fcfcfc; color: #333333; margin: 0; padding:0; }\n\
h1 { font-size: 1.5em; font-weight: normal; background-color: #9999cc; min-height:2em; line-height:2em; border-bottom: 1px inset black; margin: 0; }\n\
h1, p { padding-left: 10px; }\n\
code.url { background-color: #eeeeee; font-family:monospace; padding:0 2px;}\n\
</style>\n";

/// `php_escape_html_entities_ex` under `ENT_QUOTES` with no double
/// encoding: `&`, `<`, `>`, `"` and `'` escaped, an entity already
/// written kept, and nothing at all for bytes that are not UTF-8.
fn escape_html(bytes: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return String::new();
    };
    let mut out = String::with_capacity(text.len());
    for (at, c) in text.char_indices() {
        match c {
            '&' if is_entity(&text[at..]) => out.push('&'),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#039;"),
            c => out.push(c),
        }
    }
    out
}

/// Whether `text`, which opens with `&`, opens with an entity: `&#` and
/// decimal digits, `&#x` and hexadecimal ones, or a name, then `;`.
fn is_entity(text: &str) -> bool {
    let rest = &text[1..];
    let body = rest.split(';').next().unwrap_or_default();
    if body.len() == rest.len() || body.is_empty() {
        return false;
    }
    if let Some(number) = body.strip_prefix('#') {
        return match number.strip_prefix(['x', 'X']) {
            Some(hex) => !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            None => !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
        };
    }
    body.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && body.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `php_cli_server_log_response`: `<peer> [<status>]: <method> <uri>`,
/// ` - <message>` where one is given and ` - <error> in <file> on line
/// <n>` where the request ended in a fatal, which makes a 200 a 500 for
/// the colour; the whole in its colour where the log is coloured.
fn log_response(
    site: &Site,
    client: &Client,
    status: i64,
    message: Option<&str>,
    fatal: Option<&(String, Vec<u8>, u32)>,
) {
    let request = &client.building.request;
    let mut line = format!(
        "{} [{status}]: {} {}",
        client.name,
        request.method().name(),
        String::from_utf8_lossy(request.uri.as_deref().unwrap_or_default())
    );
    if let Some(message) = message {
        line.push_str(" - ");
        line.push_str(message);
    }
    let mut effective = status;
    if let Some((error, file, at)) = fatal {
        if status == 200 {
            effective = 500;
        }
        write!(
            line,
            " - {error} in {} on line {at}",
            String::from_utf8_lossy(file)
        )
        .ok();
    }
    if site.log.colored() {
        let color = match effective {
            500.. => Some(1),
            400.. => Some(3),
            200.. => Some(2),
            _ => None,
        };
        if let Some(color) = color {
            line = format!("\x1b[3{color}m{line}\x1b[0m");
        }
    }
    site.log.write(Type::Message, &line);
}

/// What the server still has to send of a response it writes itself,
/// `php_cli_server_content_sender`: the bytes not yet taken, and the
/// file the rest is read from where one is still open.
pub(super) struct Pending {
    bytes: Vec<u8>,
    sent: usize,
    file: Option<std::fs::File>,
}

impl Pending {
    fn new(head: Vec<u8>, file: Option<std::fs::File>) -> Pending {
        Pending {
            bytes: head,
            sent: 0,
            file,
        }
    }
}

/// `php_cli_server_send_event` (`Z/sapi/cli/php_cli_server.c:2668`): the
/// next 131072 bytes of the file read where everything before them went
/// (`php_cli_server_content_sender_pull`, `Z/sapi/cli/php_cli_server.c:1137`),
/// as much as the socket takes
/// sent, and the connection closed once all of it is written, the peer
/// went away or the file could not be read.
fn send(site: &Site, mut client: Client, mut pending: Pending) -> Connection {
    if pending.sent == pending.bytes.len()
        && let Some(file) = &mut pending.file
    {
        pending.bytes.resize(131_072, 0);
        pending.sent = 0;
        match file.read(&mut pending.bytes) {
            Ok(read) => {
                pending.bytes.truncate(read);
                if read == 0 {
                    pending.file = None;
                }
            }
            Err(error) => {
                site.log.write(Type::Error, &php_sapi::strerror(&error));
                return close(site, &client);
            }
        }
    }
    while pending.sent < pending.bytes.len() {
        match client.stream.write(&pending.bytes[pending.sent..]) {
            Ok(0) => return close(site, &client),
            Ok(sent) => pending.sent += sent,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(_) => return close(site, &client),
        }
    }
    if pending.sent == pending.bytes.len() && pending.file.is_none() {
        return close(site, &client);
    }
    Connection::Sending(client, pending)
}
