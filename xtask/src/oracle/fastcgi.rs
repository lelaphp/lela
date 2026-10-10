//! `[fastcgi]` of the pin: the HTTP-to-FastCGI mapping a request is handed
//! to the pinned php-fpm under, for a host php does not ship (#2334,
//! research/production-server.r2.md §5.4). Under FPM the web server, not
//! php, fills the CGI variables (RFC 3875 §4.1), so the pinned php-fpm is
//! the oracle for what PHP code sees only under a mapping written down
//! once. Mitchell ruled it nginx's stock `conf/fastcgi_params`, copied
//! verbatim at a named release and held to its md5 here, with
//! `SCRIPT_FILENAME` as nginx sets it. [`Mapping`] is that file read as
//! one typed value, so the production server's `$_SERVER` (#2347) and the
//! php-fpm comparison (#2349) read the same parameters in the same order.

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

/// The mapping: the nginx release the parameters were copied from, where
/// from, and every parameter in the order nginx sends it. php-fpm
/// registers `$_SERVER` in the reverse of that order: `fcgi_hash_set`
/// puts each new parameter at the head of its list and `fcgi_hash_apply`
/// walks from the head (`Z/main/fastcgi.c:353-354`, `:399-407`), so the
/// request headers, sent last, come out first, and `SCRIPT_FILENAME` last
/// of the parameters. `SCRIPT_FILENAME` stands first,
/// where nginx's own `conf/fastcgi.conf` at the same tag puts it (that
/// file is `fastcgi_params` with that one line added), and the file's
/// parameters follow. After the parameters nginx passes every request
/// header as `HTTP_<NAME>`, by its `fastcgi_pass_request_headers on`
/// default, as the documentation of `ngx_http_fastcgi_module` says of
/// the parameters it passes; that is nginx's and no line of the file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "File")]
pub struct Mapping {
    /// The nginx release, `1.30.5`.
    pub nginx: String,
    /// Where the file was copied from, at that release's tag.
    pub source: String,
    pub params: Vec<Param>,
}

/// `[fastcgi]` as the pin spells it: `params` the file's bytes whole,
/// held to `md5`, and `script_filename` the value nginx gives that one
/// parameter.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    nginx: String,
    source: String,
    md5: String,
    script_filename: String,
    params: String,
}

/// One `fastcgi_param`: the name php-fpm is handed, its value as text and
/// nginx variables, and whether nginx leaves it out where the value is
/// empty (`if_not_empty`, as it does `HTTPS` on a plain connection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub value: Vec<Piece>,
    pub if_not_empty: bool,
}

/// A stretch of a parameter's value: text as written, or a variable nginx
/// fills for the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Variable(Variable),
}

/// The nginx variables the mapping names, each as nginx's documentation
/// defines it (the "Embedded Variables" of `ngx_http_core_module` and
/// `ngx_http_fastcgi_module`). A variable outside them refuses the pin, so a
/// reader of the mapping fills every one it can meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Variable {
    DocumentRoot,
    /// `$fastcgi_script_name`: the request URI, with `fastcgi_index`
    /// appended where it ends in a slash; named for what it holds, since
    /// the glossary keeps nginx's word out of item names.
    FastcgiUri,
    QueryString,
    RequestMethod,
    ContentType,
    ContentLength,
    RequestUri,
    DocumentUri,
    ServerProtocol,
    Scheme,
    Https,
    NginxVersion,
    RemoteAddr,
    RemotePort,
    ServerAddr,
    ServerPort,
    ServerName,
}

impl Variable {
    pub const ALL: [Variable; 17] = [
        Variable::DocumentRoot,
        Variable::FastcgiUri,
        Variable::QueryString,
        Variable::RequestMethod,
        Variable::ContentType,
        Variable::ContentLength,
        Variable::RequestUri,
        Variable::DocumentUri,
        Variable::ServerProtocol,
        Variable::Scheme,
        Variable::Https,
        Variable::NginxVersion,
        Variable::RemoteAddr,
        Variable::RemotePort,
        Variable::ServerAddr,
        Variable::ServerPort,
        Variable::ServerName,
    ];

    /// The name nginx spells after the `$`.
    pub fn name(self) -> &'static str {
        match self {
            Variable::DocumentRoot => "document_root",
            Variable::FastcgiUri => "fastcgi_script_name",
            Variable::QueryString => "query_string",
            Variable::RequestMethod => "request_method",
            Variable::ContentType => "content_type",
            Variable::ContentLength => "content_length",
            Variable::RequestUri => "request_uri",
            Variable::DocumentUri => "document_uri",
            Variable::ServerProtocol => "server_protocol",
            Variable::Scheme => "scheme",
            Variable::Https => "https",
            Variable::NginxVersion => "nginx_version",
            Variable::RemoteAddr => "remote_addr",
            Variable::RemotePort => "remote_port",
            Variable::ServerAddr => "server_addr",
            Variable::ServerPort => "server_port",
            Variable::ServerName => "server_name",
        }
    }
}

impl TryFrom<File> for Mapping {
    type Error = anyhow::Error;

    fn try_from(file: File) -> Result<Mapping> {
        let actual = format!("{:x}", md5::compute(&file.params));
        if actual != file.md5 {
            bail!(
                "[fastcgi] params has md5 {actual} and the pin says {}: the file is copied verbatim from {}, never edited",
                file.md5,
                file.source
            );
        }
        let mut params = vec![Param {
            name: "SCRIPT_FILENAME".to_owned(),
            value: pieces(&file.script_filename)
                .map_err(|e| anyhow!("[fastcgi] script_filename: {e}"))?,
            if_not_empty: false,
        }];
        for (n, line) in file.params.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // A `try_from` error reaches the reader through its Display,
            // which prints one layer, so the line goes into the message.
            params.push(param(line).map_err(|e| anyhow!("[fastcgi] params line {}: {e}", n + 1))?);
        }
        if let Some((_, twice)) = params
            .iter()
            .enumerate()
            .find(|(n, p)| params[..*n].iter().any(|q| q.name == p.name))
        {
            bail!("[fastcgi] names the parameter {} twice", twice.name);
        }
        Ok(Mapping {
            nginx: file.nginx,
            source: file.source,
            params,
        })
    }
}

/// One `fastcgi_param <name> <value> [if_not_empty];` line, the only
/// directive the file carries; nginx's quoting and `${}` are not in it,
/// so they are refused rather than read.
fn param(line: &str) -> Result<Param> {
    let words: Vec<&str> = line
        .strip_suffix(';')
        .ok_or_else(|| anyhow!("{line:?} does not end in `;`"))?
        .split_whitespace()
        .collect();
    let (name, value, if_not_empty) = match words[..] {
        ["fastcgi_param", name, value] => (name, value, false),
        ["fastcgi_param", name, value, "if_not_empty"] => (name, value, true),
        _ => bail!("{line:?} is not `fastcgi_param <name> <value> [if_not_empty];`"),
    };
    Ok(Param {
        name: name.to_owned(),
        value: pieces(value)?,
        if_not_empty,
    })
}

/// A value as text and the variables after each `$`, a variable's name
/// running as far as letters, digits and `_` do, as nginx reads one.
fn pieces(value: &str) -> Result<Vec<Piece>> {
    if value.contains(['"', '\'', '{', '}', '\\']) {
        bail!("{value:?} uses nginx syntax the mapping does not carry");
    }
    let mut out = Vec::new();
    let mut segments = value.split('$');
    if let Some(text) = segments.next().filter(|t| !t.is_empty()) {
        out.push(Piece::Text(text.to_owned()));
    }
    for segment in segments {
        let end = segment
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(segment.len());
        let (name, text) = segment.split_at(end);
        if name.is_empty() {
            bail!("{value:?} has a `$` that names no variable");
        }
        let variable = Variable::ALL
            .into_iter()
            .find(|v| v.name() == name)
            .ok_or_else(|| {
                anyhow!("{value:?} names the variable ${name}, which is not one the mapping knows")
            })?;
        out.push(Piece::Variable(variable));
        if !text.is_empty() {
            out.push(Piece::Text(text.to_owned()));
        }
    }
    Ok(out)
}
