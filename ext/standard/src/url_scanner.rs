//! `Z/ext/standard/url_scanner_ex.re`, the URL rewriter: a buffer
//! handler, `URL-Rewriter`, that scans the HTML passing through it and
//! adds the variables `output_add_rewrite_var` registered to each URL an
//! attribute named in `url_rewriter.tags` holds, and a hidden input for
//! each to each `<form>`, where the URL's host is one of
//! `url_rewriter.hosts` or it has none; and the two natives of
//! `Z/main/output.c` over it, `output_add_rewrite_var` and
//! `output_reset_rewrite_vars`. The scanner is re2c's automaton as php
//! generates it, one state per `STATE_*`, each stopping where the chunk
//! ends before it can decide and keeping what it has not decided for the
//! next chunk, as `YYFILL` does.
//!
//! php keeps a second rewriter for `session.use_trans_sid` beside this
//! one, `url_adapt_session_ex`, the same scanner and handler over
//! `session.trans_sid_tags` and `session.trans_sid_hosts`; this crate
//! holds both and the four entries, and ext/session reaches the second
//! through what `url_scanner_ex.h` declares `PHPAPI`,
//! [`php_url_scanner_add_session_var`] and
//! [`php_url_scanner_reset_session_var`] (design §4.10).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::attr::php_function;
use php_ext::decl::{Bytes, IniDecl, Name};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    BufferHandler, HandlerCall, HandlerStatus, OutputOp, Request, StartHandler,
};
use php_ext::values::{PhpString, SmartStr};

use crate::State;
use crate::string::Decimal;

/// The four entries `url_scanner_ex.re` registers, `PHP_INI_ALL` each.
///
/// ```php
/// var_dump(ini_get('url_rewriter.tags'), ini_get('url_rewriter.hosts'), ini_get('session.trans_sid_tags'), ini_set('session.trans_sid_hosts', 'x.org'), ini_set('session.trans_sid_tags', 'a=href'));
/// ```
/// ```expect
/// Deprecated: ini_set(): Usage of session.trans_sid_hosts INI setting is deprecated in %s on line 2
///
/// Deprecated: ini_set(): Usage of session.trans_sid_tags INI setting is deprecated in %s on line 2
/// string(5) "form="
/// string(0) ""
/// string(32) "a=href,area=href,frame=src,form="
/// string(0) ""
/// string(32) "a=href,area=href,frame=src,form="
/// ```
pub(crate) fn ini() -> Vec<IniDecl> {
    let entry = |name: &'static [u8], default: &'static [u8]| IniDecl {
        name: Name(Bytes::borrowed(name)),
        default: Some(Bytes::borrowed(default)),
        user: true,
        per_dir: true,
        system: true,
    };
    vec![
        entry(b"session.trans_sid_tags", SESSION_TAGS),
        entry(b"session.trans_sid_hosts", b""),
        entry(b"url_rewriter.tags", b"form="),
        entry(b"url_rewriter.hosts", b""),
    ]
}

/// `session.trans_sid_tags`' default, which `OnUpdateSessionTags` takes
/// without a word.
const SESSION_TAGS: &[u8] = b"a=href,area=href,frame=src,form=";

/// `OnUpdateSessionTags`: a value that does not begin with the default
/// is deprecated, and taken.
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::on_modify`'s signature
pub(super) fn on_session_tags(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    if !value.starts_with(SESSION_TAGS) {
        docref(
            req,
            Level::Deprecated,
            "Usage of session.trans_sid_tags INI setting is deprecated",
        );
    }
    Ok(true)
}

/// `OnUpdateSessionHosts`: any value but the empty one is deprecated,
/// and taken.
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::on_modify`'s signature
pub(super) fn on_session_hosts(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    if !value.is_empty() {
        docref(
            req,
            Level::Deprecated,
            "Usage of session.trans_sid_hosts INI setting is deprecated",
        );
    }
    Ok(true)
}

/// Where the scanner stands between two chunks, `STATE_*`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Scan {
    #[default]
    Plain,
    Tag,
    NextArg,
    Arg,
    BeforeVal,
    Val,
}

/// What the tag being scanned is, `TAG_NORMAL` or `TAG_FORM`, and what
/// its attribute is, `ATTR_NORMAL` or `ATTR_ACTION`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TagType {
    #[default]
    Normal,
    Form,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AttrType {
    #[default]
    Normal,
    Action,
}

/// Which of php's two rewriters is meant, the `is_session` every
/// function of `url_scanner_ex.re` takes: `BG(url_adapt_output_ex)`,
/// `output_add_rewrite_var`'s over `url_rewriter.*`, or
/// `BG(url_adapt_session_ex)`, `session.use_trans_sid`'s over
/// `session.trans_sid_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Output,
    Session,
}

impl Context {
    /// The entries the rewriter's tag table and host list are built from.
    fn entries(self) -> (&'static [u8], &'static [u8]) {
        match self {
            Context::Output => (b"url_rewriter.tags", b"url_rewriter.hosts"),
            Context::Session => (b"session.trans_sid_tags", b"session.trans_sid_hosts"),
        }
    }

    /// The rewriter's state, in this crate's request state.
    fn rewriter(self, req: &mut Request) -> &mut Rewriter {
        let state = req.state::<State>();
        match self {
            Context::Output => &mut state.url_rewriter,
            Context::Session => &mut state.session_rewriter,
        }
    }

    /// The handler `php_output_start_internal` starts for the rewriter.
    fn handler(self) -> BufferHandler {
        match self {
            Context::Output => OUTPUT_REWRITER,
            Context::Session => SESSION_REWRITER,
        }
    }
}

/// `url_adapt_state_ex_t`: the scanner's state and what it holds between
/// chunks, and what is added, the query pairs and the hidden inputs.
#[derive(Debug, Default)]
pub(crate) struct Rewriter {
    /// `active`: the handler was started, once per request.
    active: bool,
    state: Scan,
    /// What the scanner has not decided yet.
    buf: Vec<u8>,
    /// The tag last read, lowered, and the attribute its table entry names.
    tag: Vec<u8>,
    lookup: Vec<u8>,
    arg: Vec<u8>,
    /// The last `action` of a `<form>`, which `check_host_whitelist` reads.
    attr_val: Vec<u8>,
    tag_type: TagType,
    attr_type: AttrType,
    /// `url_app`, `name=value` pairs joined by `arg_separator.output`,
    /// a `smart_str` on the request's heap as php's is.
    url_app: SmartStr,
    /// `form_app`, the hidden inputs, the same.
    form_app: SmartStr,
}

/// What the scanner reads off the ini as a chunk is handled: the table
/// `php_ini_on_update_tags` builds, the host list
/// `php_ini_on_update_hosts` builds, and `PG(arg_separator).output`.
struct Settings {
    tags: Vec<(Vec<u8>, Vec<u8>)>,
    hosts: Vec<Vec<u8>>,
    separator: Vec<u8>,
    /// `$_SERVER['HTTP_HOST']` cut at its `:`, for a form when the
    /// host list is empty.
    http_host: Option<Vec<u8>>,
}

impl Settings {
    fn read(req: &mut Request, context: Context) -> Settings {
        let (tags_entry, hosts_entry) = context.entries();
        let ini = |name: &[u8]| req.ini().get(name).unwrap_or_default().to_vec();
        // `php_strtok_r(tmp, ",")`, whose empty pieces are skipped.
        let pieces = |value: &[u8]| -> Vec<Vec<u8>> {
            value
                .split(|c| *c == b',')
                .filter(|piece| !piece.is_empty())
                .map(<[u8]>::to_vec)
                .collect()
        };
        let mut tags: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for piece in pieces(&ini(tags_entry)) {
            let Some(at) = piece.iter().position(|c| *c == b'=') else {
                continue;
            };
            let key = piece[..at].to_ascii_lowercase();
            // `zend_hash_add_mem`: the first entry of a name stands.
            if !tags.iter().any(|(k, _)| *k == key) {
                tags.push((key, piece[at + 1..].to_vec()));
            }
        }
        let hosts = pieces(&ini(hosts_entry))
            .into_iter()
            .map(|host| host.to_ascii_lowercase())
            .collect();
        let separator = separator(req);
        // `check_http_host`: `EG(symbol_table)`'s `$_SERVER`, the
        // program's, not armed where the program has not made it.
        let http_host = req.variables_mut().global(b"_SERVER").and_then(|server| {
            let host = server.as_array()?.find(b"HTTP_HOST")?;
            let host = host.deref().as_php_string()?.as_bytes().to_vec();
            let end = host.iter().position(|c| *c == b':').unwrap_or(host.len());
            Some(host[..end].to_vec())
        });
        Settings {
            tags,
            hosts,
            separator,
            http_host,
        }
    }
}

/// `PG(arg_separator).output`, `&` where it is empty.
fn separator(req: &Request) -> Vec<u8> {
    match req.ini().get(b"arg_separator.output") {
        Some(separator) if !separator.is_empty() => separator.to_vec(),
        _ => b"&".to_vec(),
    }
}

/// `append_modified_url`: the URL with `url_app` added to its query, or
/// as it was where it is malformed, a `#mark`, of a scheme other than
/// `http` and `https`, or of a host not listed.
fn append_modified_url(url: &[u8], dest: &mut Vec<u8>, url_app: &[u8], settings: &Settings) {
    let Some(parts) = crate::url::parse(url) else {
        dest.extend_from_slice(url);
        return;
    };
    if parts.fragment.is_some() && url.first() == Some(&b'#') {
        dest.extend_from_slice(url);
        return;
    }
    if let Some(scheme) = &parts.scheme
        && !scheme.eq_ignore_ascii_case(b"http")
        && !scheme.eq_ignore_ascii_case(b"https")
    {
        dest.extend_from_slice(url);
        return;
    }
    if let Some(host) = &parts.host
        && !settings.hosts.contains(&host.to_ascii_lowercase())
    {
        dest.extend_from_slice(url);
        return;
    }
    if parts.path.is_none() && parts.query.is_none() && parts.fragment.is_none() {
        dest.extend_from_slice(url);
        dest.extend_from_slice(b"/?");
        dest.extend_from_slice(url_app);
        return;
    }
    if let Some(scheme) = &parts.scheme {
        dest.extend_from_slice(scheme);
        dest.extend_from_slice(b"://");
    } else if url.starts_with(b"//") {
        dest.extend_from_slice(b"//");
    }
    if let Some(user) = &parts.user {
        dest.extend_from_slice(user);
        if let Some(pass) = &parts.pass {
            dest.extend_from_slice(pass);
            dest.push(b':');
        }
        dest.push(b'@');
    }
    if let Some(host) = &parts.host {
        dest.extend_from_slice(host);
    }
    if let Some(port) = parts.port.filter(|port| *port != 0) {
        dest.push(b':');
        dest.extend_from_slice(Decimal::unsigned(u64::from(port)).as_bytes());
    }
    if let Some(path) = &parts.path {
        dest.extend_from_slice(path);
    }
    dest.push(b'?');
    if let Some(query) = &parts.query {
        dest.extend_from_slice(query);
        dest.extend_from_slice(&settings.separator);
    }
    dest.extend_from_slice(url_app);
    if let Some(fragment) = &parts.fragment {
        dest.push(b'#');
        dest.extend_from_slice(fragment);
    }
}

/// `isalpha` as re2c's `alpha` spells it.
fn alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

/// One chunk through the scanner: its decided bytes appended to `out`,
/// and what it could not decide kept in the state's buffer.
struct Chunk<'a> {
    ctx: &'a mut Rewriter,
    settings: &'a Settings,
    data: Vec<u8>,
    out: Vec<u8>,
}

impl Chunk<'_> {
    fn passthru(&mut self, start: usize, end: usize) {
        self.out.extend_from_slice(&self.data[start..end]);
    }

    /// `handle_tag`: the tag lowered, and whether the table names it.
    fn handle_tag(&mut self, start: usize, end: usize) {
        self.ctx.tag = self.data[start..end].to_ascii_lowercase();
        let found = self
            .settings
            .tags
            .iter()
            .find(|(key, _)| *key == self.ctx.tag)
            .map(|(_, attr)| attr.clone());
        self.ctx.state = match found {
            Some(attr) => {
                self.ctx.lookup = attr;
                self.ctx.tag_type = if self.ctx.tag == b"form" {
                    TagType::Form
                } else {
                    TagType::Normal
                };
                Scan::NextArg
            }
            None => Scan::Plain,
        };
    }

    /// `handle_arg`: the attribute's name, and whether it is a form's
    /// `action`, `strncasecmp` over the name's own length.
    fn handle_arg(&mut self, start: usize, end: usize) {
        self.ctx.arg = self.data[start..end].to_vec();
        let prefix = b"action".get(..self.ctx.arg.len());
        self.ctx.attr_type = if self.ctx.tag_type == TagType::Form
            && prefix.is_some_and(|p| p.eq_ignore_ascii_case(&self.ctx.arg))
        {
            AttrType::Action
        } else {
            AttrType::Normal
        };
    }

    /// `handle_val` and `tag_arg`: the value, the URL rewritten where the
    /// attribute is the one the table names for the tag.
    fn handle_val(&mut self, start: usize, end: usize, quote: Option<u8>) {
        let q = usize::from(quote.is_some());
        let val = self.data[start + q..end - q].to_vec();
        if self.ctx.tag_type == TagType::Form && self.ctx.attr_type == AttrType::Action {
            self.ctx.attr_val.clone_from(&val);
        }
        if let Some(quote) = quote {
            self.out.push(quote);
        }
        if self.ctx.arg.eq_ignore_ascii_case(&self.ctx.lookup) {
            append_modified_url(
                &val,
                &mut self.out,
                self.ctx.url_app.as_bytes(),
                self.settings,
            );
        } else {
            self.out.extend_from_slice(&val);
        }
        if let Some(quote) = quote {
            self.out.push(quote);
        }
    }

    /// `handle_form`: after a form tag's `>`, the hidden inputs, where
    /// its action passes `check_host_whitelist`.
    fn handle_form(&mut self) {
        let form_app = self.ctx.form_app.as_bytes();
        if !form_app.is_empty() && self.ctx.tag == b"form" && self.host_allowed() {
            self.out.extend_from_slice(form_app);
        }
    }

    /// `check_host_whitelist`: an empty action, a scheme-less one with no
    /// host, and an `http` or `https` one of a listed host (or, with no
    /// list, of `HTTP_HOST`) pass.
    fn host_allowed(&self) -> bool {
        if self.ctx.attr_val.is_empty() {
            return true;
        }
        let Some(parts) = crate::url::parse(&self.ctx.attr_val) else {
            return false;
        };
        if let Some(scheme) = &parts.scheme
            && !scheme.eq_ignore_ascii_case(b"http")
            && !scheme.eq_ignore_ascii_case(b"https")
        {
            return false;
        }
        let Some(host) = &parts.host else {
            return true;
        };
        if self.settings.hosts.is_empty()
            && self
                .settings
                .http_host
                .as_ref()
                .is_some_and(|http| http.eq_ignore_ascii_case(host))
        {
            return true;
        }
        self.settings.hosts.contains(host)
    }

    /// `xx_mainloop`: the automaton over the chunk from the state it was
    /// left in; where a state needs more bytes than the chunk has, what
    /// it started on is kept for the next chunk.
    #[allow(clippy::too_many_lines)] // one re2c scanner, kept whole
    fn run(&mut self) {
        let len = self.data.len();
        let mut p = 0;
        let mut start;
        let at = |data: &[u8], i: usize| data.get(i).copied();
        // `YYFILL(n)` before reading `n` bytes from `p`.
        macro_rules! fill {
            ($n:expr) => {
                if len - p < $n {
                    break;
                }
            };
        }
        loop {
            start = p;
            match self.ctx.state {
                Scan::Plain => {
                    fill!(1);
                    if self.data[p] == b'<' {
                        p += 1;
                        self.passthru(start, p);
                        self.ctx.state = Scan::Tag;
                    } else {
                        p += 1;
                        fill!(1);
                        while self.data[p] != b'<' {
                            p += 1;
                            fill!(1);
                        }
                        self.passthru(start, p);
                    }
                }
                Scan::Tag => {
                    fill!(1);
                    let name = |c: u8| c.is_ascii_alphabetic() || c == b':';
                    if name(self.data[p]) {
                        p += 1;
                        fill!(1);
                        while name(self.data[p]) {
                            p += 1;
                            fill!(1);
                        }
                        self.handle_tag(start, p);
                        self.passthru(start, p);
                    } else {
                        p += 1;
                        self.passthru(start, p);
                        self.ctx.state = Scan::Plain;
                    }
                }
                Scan::NextArg => {
                    fill!(2);
                    let space = |c: u8| matches!(c, b' ' | 0x0b | b'\r' | b'\t' | b'\n');
                    let c = self.data[p];
                    if space(c) {
                        p += 1;
                        fill!(1);
                        while space(self.data[p]) {
                            p += 1;
                            fill!(1);
                        }
                        self.passthru(start, p);
                    } else if c == b'>' || (c == b'/' && at(&self.data, p + 1) == Some(b'>')) {
                        p += if c == b'>' { 1 } else { 2 };
                        self.passthru(start, p);
                        self.handle_form();
                        self.ctx.state = Scan::Plain;
                    } else if alpha(c) {
                        self.ctx.state = Scan::Arg;
                    } else {
                        p += 1;
                        self.passthru(start, p);
                        self.ctx.state = Scan::Plain;
                    }
                }
                Scan::Arg => {
                    fill!(1);
                    if alpha(self.data[p]) {
                        p += 1;
                        fill!(1);
                        while alpha(self.data[p]) || self.data[p] == b'-' {
                            p += 1;
                            fill!(1);
                        }
                        self.passthru(start, p);
                        self.handle_arg(start, p);
                        self.ctx.state = Scan::BeforeVal;
                    } else {
                        p += 1;
                        self.passthru(start, p);
                        self.ctx.state = Scan::NextArg;
                    }
                }
                Scan::BeforeVal => {
                    fill!(2);
                    // `[ ]* "=" [ ]*`, or nothing taken.
                    let mut q = p;
                    while self.data[q] == b' ' {
                        q += 1;
                        if q == len {
                            break;
                        }
                    }
                    if q == len {
                        // Spaces to the chunk's end: re2c stops only
                        // past the first, where it reads on.
                        if q > p + 1 {
                            break;
                        }
                    }
                    if at(&self.data, q) == Some(b'=') {
                        p = q + 1;
                        fill!(1);
                        while self.data[p] == b' ' {
                            p += 1;
                            fill!(1);
                        }
                        self.passthru(start, p);
                        self.ctx.state = Scan::Val;
                    } else {
                        self.ctx.state = Scan::NextArg;
                    }
                }
                Scan::Val => {
                    fill!(2);
                    let c = self.data[p];
                    let unquoted =
                        |c: u8| !matches!(c, b' ' | b'\r' | b'\t' | b'\n' | b'>' | b'\'' | b'"');
                    if unquoted(c) {
                        p += 1;
                        fill!(1);
                        while unquoted(self.data[p]) {
                            p += 1;
                            fill!(1);
                        }
                        self.handle_val(start, p, None);
                        self.ctx.state = Scan::NextArg;
                    } else if c == b'"' || c == b'\'' {
                        // A quoted value to its closing quote, a `>`
                        // before it leaving the quote alone taken.
                        let mut q = p + 1;
                        if self.data[q] == b'>' {
                            p += 1;
                            self.passthru(start, p);
                            self.ctx.state = Scan::NextArg;
                            continue;
                        }
                        let mut closed = false;
                        loop {
                            let d = self.data[q];
                            if d == c {
                                closed = true;
                                break;
                            }
                            if d == b'>' {
                                break;
                            }
                            q += 1;
                            if q == len {
                                break;
                            }
                        }
                        if q == len {
                            break;
                        }
                        if closed {
                            p = q + 1;
                            self.handle_val(start, p, Some(c));
                        } else {
                            p += 1;
                            self.passthru(start, p);
                        }
                        self.ctx.state = Scan::NextArg;
                    } else {
                        p += 1;
                        self.passthru(start, p);
                        self.ctx.state = Scan::NextArg;
                    }
                }
            }
        }
        // `stop:`: what the state started on and could not decide.
        self.ctx.buf = self.data[start..].to_vec();
    }
}

/// `PHP_RSHUTDOWN_FUNCTION(url_scanner)`
/// (`Z/ext/standard/url_scanner_ex.re:1000`): both rewriters' query
/// pairs and hidden inputs `smart_str_free`d, which
/// `output_reset_rewrite_vars` only empties.
pub(crate) fn rshutdown(req: &mut Request) {
    for context in [Context::Session, Context::Output] {
        let mut url_app = std::mem::take(&mut context.rewriter(req).url_app);
        let mut form_app = std::mem::take(&mut context.rewriter(req).form_app);
        form_app.free(req);
        url_app.free(req);
    }
}

/// `php_url_scanner_session_handler_impl`: with pairs to add, the chunk
/// through the scanner, and on a flush or the last call what it had not
/// decided after it; with none, the chunk after whatever the scanner had
/// kept.
fn handle(req: &mut Request, call: &mut HandlerCall<'_>, context: Context) -> HandlerStatus {
    let settings = Settings::read(req, context);
    let ctx = context.rewriter(req);
    if ctx.url_app.as_bytes().is_empty() {
        let mut out = std::mem::take(&mut ctx.buf);
        out.extend_from_slice(call.input);
        call.out = out;
        return HandlerStatus::Success;
    }
    let mut data = std::mem::take(&mut ctx.buf);
    data.extend_from_slice(call.input);
    let mut chunk = Chunk {
        ctx,
        settings: &settings,
        data,
        out: Vec::new(),
    };
    chunk.run();
    let mut out = chunk.out;
    if call.op.flush || call.op.finish {
        out.extend_from_slice(&std::mem::take(&mut ctx.buf));
        ctx.attr_val.clear();
    }
    call.out = out;
    HandlerStatus::Success
}

/// `php_url_scanner_output_handler`.
#[allow(clippy::unnecessary_wraps)] // `BufferHandler::handler`'s signature
fn output_handler(req: &mut Request, call: &mut HandlerCall<'_>) -> Result<HandlerStatus, Thrown> {
    Ok(handle(req, call, Context::Output))
}

/// `php_url_scanner_session_handler`.
#[allow(clippy::unnecessary_wraps)] // `BufferHandler::handler`'s signature
fn session_handler(req: &mut Request, call: &mut HandlerCall<'_>) -> Result<HandlerStatus, Thrown> {
    Ok(handle(req, call, Context::Session))
}

/// The handlers `php_output_start_internal` starts, each under the name
/// `URL-Rewriter`.
const OUTPUT_REWRITER: BufferHandler = BufferHandler {
    name: "URL-Rewriter",
    conflicts: &[],
    handler: output_handler,
    init: None,
};
const SESSION_REWRITER: BufferHandler = BufferHandler {
    name: "URL-Rewriter",
    conflicts: &[],
    handler: session_handler,
    init: None,
};

/// `php_url_scanner_add_var_impl`: the pair, raw-URL-encoded where
/// `encode` is set, added to the rewriter's query pairs, and as a hidden
/// input, HTML escaped, to its forms; the first pair of a request
/// activates the rewriter and starts its `URL-Rewriter` handler.
fn add_var(
    req: &mut Request,
    context: Context,
    name: &[u8],
    value: &[u8],
    encode: bool,
) -> Result<(), Thrown> {
    let separator = separator(req);
    let entity = |req: &mut Request, text: &[u8]| {
        crate::html::escape(
            req,
            text,
            false,
            crate::html::QUOTES_SUBSTITUTE,
            None,
            false,
        )
    };
    let (hname, hvalue) = if encode {
        (entity(req, name), entity(req, value))
    } else {
        (name.to_vec(), value.to_vec())
    };
    let ctx = context.rewriter(req);
    let start = !ctx.active;
    if start {
        // `php_url_scanner_ex_activate`: the scanner and both lists
        // cleared, once a request.
        *ctx = Rewriter {
            active: true,
            ..Rewriter::default()
        };
    }
    let (sname, svalue) = if encode {
        let (mut sname, mut svalue) = (Vec::new(), Vec::new());
        crate::url::url_encode(name, true, &mut sname);
        crate::url::url_encode(value, true, &mut svalue);
        (sname, svalue)
    } else {
        (name.to_vec(), value.to_vec())
    };
    let mut url_app = std::mem::take(&mut ctx.url_app);
    let mut form_app = std::mem::take(&mut ctx.form_app);
    let separated = !url_app.as_bytes().is_empty();
    let appended = (|| {
        if separated {
            url_app.append(req, &separator)?;
        }
        url_app.append(req, &sname)?;
        url_app.append(req, b"=")?;
        url_app.append(req, &svalue)?;
        form_app.append(req, b"<input type=\"hidden\" name=\"")?;
        form_app.append(req, &hname)?;
        form_app.append(req, b"\" value=\"")?;
        form_app.append(req, &hvalue)?;
        form_app.append(req, b"\" />")
    })();
    let ctx = context.rewriter(req);
    ctx.url_app = url_app;
    ctx.form_app = form_app;
    appended?;
    if start {
        req.response_mut()
            .buffer_op(&OutputOp::Start {
                handler: StartHandler::Registered(context.handler()),
                chunk_size: 0,
                flags: 0x70,
            })?
            .give_up(req);
    }
    Ok(())
}

/// `php_url_scanner_reset_var_impl`: the pair named `name` taken out of
/// the rewriter's query pairs with one separator beside it, or all of
/// them where it is the only one, and its hidden input out of the forms;
/// `false` where no pair of that name was added. The name is found as
/// php finds it, as the first `name=` anywhere in the pairs.
fn reset_var(req: &mut Request, context: Context, name: &[u8], encode: bool) -> bool {
    if context.rewriter(req).url_app.as_bytes().is_empty() {
        return true;
    }
    let separator = separator(req);
    let (sname, hname) = if encode {
        let mut sname = Vec::new();
        crate::url::url_encode(name, true, &mut sname);
        let charset = req.ini().get(b"default_charset").map(<[u8]>::to_vec);
        let hname = crate::html::escape(
            req,
            name,
            false,
            crate::html::QUOTES_SUBSTITUTE,
            charset.as_deref(),
            false,
        );
        (sname, hname)
    } else {
        (name.to_vec(), name.to_vec())
    };
    let mut url_var = sname;
    url_var.push(b'=');
    let mut form_var = b"<input type=\"hidden\" name=\"".to_vec();
    form_var.extend_from_slice(&hname);
    form_var.extend_from_slice(b"\" value=\"");
    let ctx = context.rewriter(req);
    let url_app = ctx.url_app.as_bytes();
    let Some(mut start) = find(url_app, &url_var) else {
        return false;
    };
    let limit = url_app.len();
    let mut end = start + url_var.len();
    let mut separator_removed = false;
    while end < limit {
        if url_app[end..].starts_with(&separator) {
            end += separator.len();
            separator_removed = true;
            break;
        }
        end += 1;
    }
    let end = end.min(limit);
    if end - start == limit {
        ctx.url_app.clear();
        ctx.form_app.clear();
        return true;
    }
    if !separator_removed
        && start >= separator.len()
        && url_app[start - separator.len()..start] == separator[..]
    {
        start -= separator.len();
    }
    ctx.url_app.remove(start..end);
    let form_app = ctx.form_app.as_bytes();
    let Some(start) = find(form_app, &form_var) else {
        ctx.url_app.clear();
        ctx.form_app.clear();
        return false;
    };
    let end = form_app[start + form_var.len()..]
        .iter()
        .position(|c| *c == b'>')
        .map_or(form_app.len(), |at| start + form_var.len() + at + 1);
    ctx.form_app.remove(start..end);
    true
}

/// `php_memnstr`: where `needle` first stands in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `php_url_scanner_add_session_var` (`url_scanner_ex.h`): the pair added
/// to every URL and form the session's tags name, by the session
/// rewriter's `URL-Rewriter`, started with the first pair of a request.
/// What `php_session_reset_id` calls where `session.use_trans_sid` is on
/// and no cookie carries the id; ext/session's `reset_id` carries the
/// witness.
///
/// # Errors
///
/// What starting the handler threw.
pub fn php_url_scanner_add_session_var(
    req: &mut Request,
    name: &[u8],
    value: &[u8],
    encode: bool,
) -> Result<(), Thrown> {
    add_var(req, Context::Session, name, value, encode)
}

/// `php_url_scanner_reset_session_var` (`url_scanner_ex.h`): the session
/// rewriter's pair of that name taken out, so a new id replaces the old
/// one rather than joining it; `false` where it was not there.
/// ext/session's `reset_id` carries the witness.
pub fn php_url_scanner_reset_session_var(req: &mut Request, name: &[u8], encode: bool) -> bool {
    reset_var(req, Context::Session, name, encode)
}

/// `output_add_rewrite_var(string $name, string $value): bool`, php's
/// `PHP_FUNCTION(output_add_rewrite_var)` over
/// `php_url_scanner_add_var`: the pair, raw-URL-encoded, added to every
/// relative or listed URL the tags name, and as a hidden input, HTML
/// escaped, to every form, by the `URL-Rewriter` handler the first call
/// of a request starts.
///
/// ```php
/// ini_set('url_rewriter.tags', 'a=href,form=');
/// ini_set('url_rewriter.hosts', 'php.net');
/// var_dump(output_add_rewrite_var('<n>', 'v&w'), output_add_rewrite_var('b', '2'));
/// ?>
/// <a href="x.php?q=1#f">x</a> <a href='http://php.net'>y</a> <a href="http://other.org/">z</a> <a href="#top">t</a>
/// <form action="x.php" method="get"></form> <form action="http://other.org/">
/// <?php
/// print_r(ob_list_handlers());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <a href="x.php?q=1&%3Cn%3E=v%26w&b=2#f">x</a> <a href='http://php.net/?%3Cn%3E=v%26w&b=2'>y</a> <a href="http://other.org/">z</a> <a href="#top">t</a>
/// <form action="x.php" method="get"><input type="hidden" name="&lt;n&gt;" value="v&amp;w" /><input type="hidden" name="b" value="2" /></form> <form action="http://other.org/">
/// Array
/// (
///     [0] => URL-Rewriter
/// )
/// ```
///
/// ```php
/// echo new ReflectionFunction('output_add_rewrite_var');
/// ```
/// ```expect
/// Function [ <internal:standard> function output_add_rewrite_var ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $name ]
///     Parameter #1 [ <required> string $value ]
///   }
///   - Return [ bool ]
/// }
/// ```
#[php_function]
fn output_add_rewrite_var(
    req: &mut Request,
    name: PhpString,
    value: PhpString,
) -> Result<bool, Thrown> {
    add_var(
        req,
        Context::Output,
        name.as_bytes(),
        value.as_bytes(),
        true,
    )?;
    Ok(true)
}

/// `output_reset_rewrite_vars(): bool`: the pairs and the inputs
/// dropped, the handler left where it stands.
///
/// ```php
/// output_add_rewrite_var('a', '1'); var_dump(output_reset_rewrite_vars());
/// ?>
/// <form action="x.php"></form>
/// ```
/// ```expect
/// bool(true)
/// <form action="x.php"></form>
/// ```
///
/// ```php
/// echo new ReflectionFunction('output_reset_rewrite_vars');
/// ```
/// ```expect
/// Function [ <internal:standard> function output_reset_rewrite_vars ] {
///
///   - Parameters [0] {
///   }
///   - Return [ bool ]
/// }
/// ```
#[php_function]
fn output_reset_rewrite_vars(req: &mut Request) -> bool {
    let ctx = &mut req.state::<State>().url_rewriter;
    ctx.form_app.clear();
    ctx.url_app.clear();
    true
}
