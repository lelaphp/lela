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
//! #2637 added nginx's documented split of the path past the program,
//! `fastcgi_split_path_info` with `PATH_INFO` sent first, as Debian's
//! `snippets/fastcgi-php.conf` sends it ahead of `fastcgi.conf`, and #2650
//! `PATH_TRANSLATED` after it, sent only with a path past the program.

use anyhow::{Result, anyhow, bail};
use regex_lite::Regex;
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
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "File")]
pub struct Mapping {
    /// The nginx release, `1.30.5`.
    pub nginx: String,
    /// Where the file was copied from, at that release's tag.
    pub source: String,
    /// `fastcgi_split_path_info` (#2637); none in a pin from before it,
    /// whose mapping splits nothing.
    pub split_path_info: Option<Split>,
    pub params: Vec<Param>,
}

impl Mapping {
    /// `$fastcgi_script_name` and `$fastcgi_path_info` for the request's
    /// `uri`, as `fastcgi_split_path_info` sets them: the pattern's two
    /// captures where it matches, and the whole `uri` with no path info
    /// where it does not.
    pub fn split<'a>(&self, uri: &'a str) -> (&'a str, &'a str) {
        (self.split_path_info.as_ref())
            .and_then(|split| split.0.captures(uri))
            .and_then(|found| Some((found.get(1)?.as_str(), found.get(2)?.as_str())))
            .unwrap_or((uri, ""))
    }
}

/// `fastcgi_split_path_info`'s pattern, compiled once as the pin is read,
/// and of two captures, the program's name and the path past it.
#[derive(Debug, Clone)]
pub struct Split(Regex);

impl Split {
    /// The pattern as the pin spells it.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// `[fastcgi]` as the pin spells it: `params` the file's bytes whole,
/// held to `md5`, `script_filename`, `path_info` and `path_translated` the
/// values nginx gives those parameters, and `split_path_info` the split's
/// pattern. The split's two keys are both there or neither, and
/// `path_translated` only beside them, so a binary reads a pin from
/// before any of them, as another worktree's binary may.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    nginx: String,
    source: String,
    md5: String,
    script_filename: String,
    #[serde(default)]
    split_path_info: Option<String>,
    #[serde(default)]
    path_info: Option<String>,
    #[serde(default)]
    path_translated: Option<String>,
    params: String,
}

/// One `fastcgi_param`: the name php-fpm is handed, its value as text and
/// nginx variables, and when nginx hands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub value: Vec<Piece>,
    pub sent: Sent,
}

/// When a parameter is handed to php-fpm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// On every request.
    Always,
    /// Where its value is not empty (`if_not_empty`, as nginx leaves
    /// `HTTPS` out on a plain connection).
    IfNotEmpty,
    /// Where `$fastcgi_path_info` is not empty: `PATH_TRANSLATED`, which
    /// RFC 3875 §4.1.6 sets only for a path past the program (#2650), and
    /// which nginx's one-line `fastcgi_param` cannot say.
    WithPathInfo,
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
    /// `$fastcgi_script_name`: the request URI, or its first capture
    /// where `fastcgi_split_path_info` matches it, with `fastcgi_index`
    /// appended where it ends in a slash; named for what it holds, since
    /// the glossary keeps nginx's word out of item names.
    FastcgiUri,
    /// `$fastcgi_path_info`: the split's second capture, empty where the
    /// pattern does not match.
    PathInfo,
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
    pub const ALL: [Variable; 18] = [
        Variable::DocumentRoot,
        Variable::FastcgiUri,
        Variable::PathInfo,
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
            Variable::PathInfo => "fastcgi_path_info",
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
        let (split_path_info, path_info) = match (&file.split_path_info, &file.path_info) {
            (Some(split), Some(path_info)) => match Regex::new(split) {
                Ok(pattern) if pattern.captures_len() == 3 => (
                    Some(Split(pattern)),
                    Some(("PATH_INFO", path_info, Sent::Always)),
                ),
                _ => bail!("[fastcgi] split_path_info is not a pattern of two captures"),
            },
            (None, None) => (None, None),
            _ => bail!("[fastcgi] split_path_info and path_info are both set or neither"),
        };
        if file.path_translated.is_some() && split_path_info.is_none() {
            bail!("[fastcgi] path_translated is set only beside split_path_info");
        }
        let path_translated = (file.path_translated.as_ref())
            .map(|value| ("PATH_TRANSLATED", value, Sent::WithPathInfo));
        let mut params = Vec::new();
        for (name, value, sent) in path_info.into_iter().chain(path_translated).chain([(
            "SCRIPT_FILENAME",
            &file.script_filename,
            Sent::Always,
        )]) {
            params.push(Param {
                name: name.to_owned(),
                value: pieces(value).map_err(|e| anyhow!("[fastcgi] {name}: {e}"))?,
                sent,
            });
        }
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
            split_path_info,
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
    let (name, value, sent) = match words[..] {
        ["fastcgi_param", name, value] => (name, value, Sent::Always),
        ["fastcgi_param", name, value, "if_not_empty"] => (name, value, Sent::IfNotEmpty),
        _ => bail!("{line:?} is not `fastcgi_param <name> <value> [if_not_empty];`"),
    };
    Ok(Param {
        name: name.to_owned(),
        value: pieces(value)?,
        sent,
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
