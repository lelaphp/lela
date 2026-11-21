//! The host a module is rendered through. php registers most
//! modules in every host, and the lane renders those through the CLI's
//! `--re` and `--ri`; a host's own module exists only in that host:
//! `cgi-fcgi` in php-cgi and php-fpm (`sapi/cgi/cgi_main.c`,
//! `sapi/fpm/fpm/fpm_main.c` at the tag), `cli_server` in the CLI only
//! while it serves as the built-in server (`sapi/cli/php_cli_server.c`).
//! php-cgi and php-fpm have no `--re` flag, and the CLI's `--re
//! cli_server` runs no server, so the oracle's CLI renders neither. Such
//! a module is rendered on both sides by its host serving the column's
//! program as a request, as the witness lane serves a `request` fence:
//! the oracle's host from `[sapi]` beside the engine's witness build, the
//! response's head let go and its body held.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::Column;
use crate::oracle::hosts::CLI;
use crate::oracle::{self, Ended, Observation, Oracle, Profile, normalise_in};
use crate::scratch::Tree;
use crate::server::Server;
use crate::witness::{self, fence::Served};

/// What serves a module's rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    /// The CLI's `php --re` and `php --ri`: every module but a host's own.
    Cli,
    /// php-cgi, serving the program as a request of the web.
    Cgi,
    /// php-fpm, over a pool of one child, as a `request fpm` fence.
    Fpm,
    /// The CLI's built-in server, `php -S`, serving one request.
    Server,
}

/// The module php-cgi and php-fpm each register as their own.
const CGI_FCGI: &str = "cgi-fcgi";
/// The module the built-in server registers.
const CLI_SERVER: &str = "cli_server";

/// The request a CGI-shaped host is handed: a `GET` with nothing in it,
/// in the `.phpt` sections a `request` fence carries.
const REQUEST: &str = "--CGI--\n";

/// The hosts that render `module`, one reading pair each: both of
/// `cgi-fcgi`'s, the server for `cli_server`, the CLI for every other.
pub(crate) fn hosts(module: &str) -> &'static [Host] {
    match module {
        CGI_FCGI => &[Host::Cgi, Host::Fpm],
        CLI_SERVER => &[Host::Server],
        _ => &[Host::Cli],
    }
}

impl Host {
    /// The manifest row of the host whose sources the engine's side is:
    /// none for the CLI, whose reading stands over the extension's crate.
    pub(crate) fn krate(self) -> Option<&'static str> {
        match self {
            Host::Cli => None,
            Host::Cgi => Some("sapi/cgi"),
            Host::Fpm => Some("sapi/fpm"),
            Host::Server => Some("sapi/cli"),
        }
    }

    /// The reading's scope: [`Column::scope`] for the CLI, and the host's
    /// row with the module under it for a host's own module,
    /// `sapi/cgi/cgi-fcgi` and `sapi/cgi/cgi-fcgi/ri`.
    pub(crate) fn scope(self, module: &str, column: Column) -> String {
        match self.krate() {
            None => column.scope(module),
            Some(krate) => match column {
                Column::Re => format!("{krate}/{module}"),
                Column::Ri => format!("{krate}/{module}/ri"),
            },
        }
    }

    /// The divergence key of the reading [`Host::scope`] names:
    /// `crosscheck:ctype`, `crosscheck:ctype/ri`,
    /// `crosscheck:sapi/cgi/cgi-fcgi`.
    pub(crate) fn id(self, module: &str, column: Column) -> String {
        super::id_of(&self.scope(module, column))
    }

    /// The invocation as a reading names it.
    pub(crate) fn invocation(self) -> &'static str {
        match self {
            Host::Cli => "php",
            Host::Cgi => "php-cgi",
            Host::Fpm => "php-fpm",
            Host::Server => "php -S",
        }
    }

    /// The engine's own host of this kind, by the name ADR-0016 gives
    /// it, as a refusal of the engine's side names it.
    pub(crate) fn engine_invocation(self) -> String {
        use crate::oracle::hosts;
        match self {
            Host::Cli => hosts::CLI.binary.to_owned(),
            Host::Cgi => hosts::CGI.binary.to_owned(),
            Host::Fpm => hosts::FPM.binary.to_owned(),
            Host::Server => format!("{} -S", hosts::CLI.binary),
        }
    }
}

/// The rendering of `module` through `host`, which is not the CLI: by the
/// oracle where `engine` is none, the pinned host from `[sapi]` with its
/// digest asserted or the pinned `php` for the server, and by the
/// engine's witness build serving the same request where it is one; with
/// the profile of that build, none for the oracle's.
pub(crate) fn side(
    oracle: &Oracle<'_>,
    engine: Option<&witness::Binary>,
    host: Host,
    module: &str,
    column: Column,
) -> Result<(Observation, Option<Profile>)> {
    let program = column.program(module);
    let served = requested(host);
    let (served, profile) = match (host, engine) {
        (Host::Cli, _) => bail!("the CLI renders {module} by its flags, not as a request"),
        (Host::Cgi | Host::Fpm, None) => {
            let path = oracle.pinned_host(witness::host_of(served))?;
            let served = witness::served_as(oracle, served, &path, &program, REQUEST, None, &[]);
            (served?, None)
        }
        (Host::Cgi | Host::Fpm, Some(binary)) => {
            let (served, _, profile) =
                binary.serve_request(oracle, served, &program, REQUEST, None)?;
            (served, Some(profile))
        }
        (Host::Server, None) => {
            let php = &oracle.pin().oracle.path;
            (served_by_server(oracle, php, &program)?, None)
        }
        (Host::Server, Some(binary)) => {
            let served = served_by_server(oracle, &binary.host(CLI)?, &program)?;
            (served, binary.profile(CLI))
        }
    };
    Ok((body(oracle, served), profile))
}

/// How a witness fence names the host that serves `host`'s request.
fn requested(host: Host) -> Served {
    match host {
        Host::Fpm => Served::Fpm,
        Host::Cli | Host::Cgi | Host::Server => Served::Cgi,
    }
}

/// The response's body: a CGI-shaped host writes its head first, and the
/// head is the host's and not the module's, so it is let go where every
/// line before the first empty one is a header.
fn body(oracle: &Oracle<'_>, mut served: Observation) -> Observation {
    served.stdout = without_head(&oracle.pin().recipe.normalise, served.stdout);
    served
}

/// `bytes`, normalised, less a head of `Name: value` lines and the empty
/// line after it; untouched where no such head opens it.
pub(crate) fn without_head(rules: &oracle::Normalise, bytes: Vec<u8>) -> Vec<u8> {
    let text = oracle::normalise(rules, bytes);
    let Some(end) = text.windows(2).position(|pair| pair == b"\n\n") else {
        return text;
    };
    let headed = text[..end].split(|b| *b == b'\n').all(|line| {
        line.iter().position(|b| *b == b':').is_some_and(|colon| {
            colon > 0
                && line[..colon]
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        })
    });
    if headed {
        oracle::normalise(rules, text[end + 2..].to_vec())
    } else {
        text
    }
}

/// `snippet` served by the built-in server of the CLI at `php`: the
/// program written as the recipe's `script_name` in a tree of its own,
/// with the recipe's preamble; the server started on a loopback port
/// under the recipe's arguments, ini and environment with that tree as
/// its root; and one `GET` of the program once the server has said, on
/// its own second stream, that it listens there, so the response is this
/// server's and no other process's. The HTTP status stands for the exit:
/// 200 is 0, and any other is itself.
pub(crate) fn served_by_server(
    oracle: &Oracle<'_>,
    php: &Path,
    snippet: &str,
) -> Result<Observation> {
    let recipe = &oracle.pin().recipe;
    let own = Tree::new("crosscheck-server")?;
    let program = own.join(&recipe.script_name);
    crate::write(&program, format!("{}{snippet}", recipe.preamble))?;
    let timeout = Duration::from_secs(recipe.limits.timeout_s);
    let started = Instant::now();
    let command = || -> Result<Command> {
        let mut server = Command::new(php);
        server.args(&recipe.args);
        witness::under_recipe(recipe, &mut server, &[], &[])?;
        Ok(server)
    };
    let server = Server::start(command, own.path(), timeout)
        .with_context(|| format!("serving through {}", php.display()))?;
    let (status, body) = server.get(&format!("/{}", recipe.script_name), timeout)?;
    Ok(Observation {
        stdout: normalise_in(&recipe.normalise, own.path(), body),
        stderr: Vec::new(),
        exit: Ended::Status(if status == 200 { 0 } else { i32::from(status) }),
        wall: started.elapsed(),
    })
}
