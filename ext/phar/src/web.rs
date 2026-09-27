//! `Phar::running()`, `Phar::mount()`, `Phar::mungServer()` and
//! `Phar::webPhar()`: what a running archive asks of the archive it runs
//! from.

use php_ext::abi::Callable;
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString};

use crate::archive::{archives, lossy};
use crate::entry;
use crate::open;
use crate::path::{self, Create, Exec};

/// `PHAR_MUNG_*`: the `$_SERVER` entries `mungServer()` names, one bit
/// each, kept for a web request.
pub(crate) const MUNG_PHP_SELF: u8 = 1;
pub(crate) const MUNG_REQUEST_URI: u8 = 2;
pub(crate) const MUNG_SCRIPT_NAME: u8 = 4;
pub(crate) const MUNG_SCRIPT_FILENAME: u8 = 8;

/// The file being served as `phar_split_fname` reads a `phar://` URL,
/// where it is one.
fn running_split(req: &mut Request) -> Result<Option<(Vec<u8>, path::Split)>, Thrown> {
    let Some(file) = req.current_file().map(<[u8]>::to_vec) else {
        return Ok(None);
    };
    if file.len() < 7 || !file[..7].eq_ignore_ascii_case(b"phar://") {
        return Ok(None);
    }
    Ok(path::split_fname(req, &file, Exec::Either, Create::No)?
        .ok()
        .map(|split| (file, split)))
}

/// `Phar::running()`: the archive the file being served is in, as a
/// `phar://` URL or as its name; empty outside one.
pub(crate) fn running(req: &mut Request, as_url: bool) -> Result<Vec<u8>, Thrown> {
    Ok(match running_split(req)? {
        Some((file, split)) if as_url => file[..(split.arch.len() + 7).min(file.len())].to_vec(),
        Some((_, split)) => split.arch,
        None => Vec::new(),
    })
}

/// `Phar::mount()`: an outside path mounted in the archive the file being
/// served is, or is in.
pub(crate) fn mount(req: &mut Request, inner: &[u8], outer: &[u8]) -> Result<(), Thrown> {
    let file = req.current_file().map(<[u8]>::to_vec).unwrap_or_default();
    let (arch, path): (Option<Vec<u8>>, Vec<u8>) = if file.len() > 7 && file.starts_with(b"phar://")
    {
        match path::split_fname(req, &file, Exec::Either, Create::No)? {
            Ok(split) => {
                if inner.len() > 7 && inner.starts_with(b"phar://") {
                    return Err(throw(
                        req,
                        "PharException",
                        &format!(
                            "Can only mount internal paths within a phar archive, use a relative path instead of \"{}\"",
                            lossy(inner)
                        ),
                    ));
                }
                (Some(split.arch), inner.to_vec())
            }
            Err(_) => return fallback(req, &file, inner, outer),
        }
    } else {
        return fallback(req, &file, inner, outer);
    };
    let arch = arch.unwrap_or_default();
    mount_in(req, &arch, Some(&arch), &path, outer)
}

/// `Phar::mount()` from a file not inside an archive: the file itself
/// where it is an archive, else the archive `$pharPath` names.
fn fallback(req: &mut Request, file: &[u8], inner: &[u8], outer: &[u8]) -> Result<(), Thrown> {
    if archives(req).by_fname(file).is_some() {
        return mount_in(req, file, None, inner, outer);
    }
    if let Ok(split) = path::split_fname(req, inner, Exec::Either, Create::No)? {
        return mount_in(req, &split.arch, Some(&split.arch), &split.entry, outer);
    }
    Err(throw(
        req,
        "PharException",
        &format!("Mounting of {} to {} failed", lossy(inner), lossy(outer)),
    ))
}

/// `carry_on2` and `carry_on`: the archive looked up by name, and
/// `phar_mount_entry`; `arch` is what the message names, php's `(null)`
/// where it had none.
fn mount_in(
    req: &mut Request,
    fname: &[u8],
    arch: Option<&[u8]>,
    path: &[u8],
    outer: &[u8],
) -> Result<(), Thrown> {
    let shown = arch.map_or_else(|| "(null)".to_string(), |a| lossy(a).into_owned());
    let Some(id) = archives(req).by_fname(fname) else {
        return Err(throw(
            req,
            "PharException",
            &format!("{shown} is not a phar archive, cannot mount"),
        ));
    };
    if !entry::mount(req, id, outer, path)? {
        return Err(throw(
            req,
            "PharException",
            &format!(
                "Mounting of {} to {} within phar {shown} failed",
                lossy(path),
                lossy(outer)
            ),
        ));
    }
    Ok(())
}

/// `Phar::mungServer()`: the names kept, at most four, each a string.
pub(crate) fn mung_server(req: &mut Request, names: PhpArray) -> Result<(), Thrown> {
    const LIST: &str = "expecting an array of any of these strings: PHP_SELF, REQUEST_URI, SCRIPT_FILENAME, SCRIPT_NAME";
    let values = names.snapshot();
    if values.is_empty() {
        return Err(throw(
            req,
            "PharException",
            &format!("No values passed to Phar::mungServer(), {LIST}"),
        ));
    }
    if values.len() > 4 {
        return Err(throw(
            req,
            "PharException",
            &format!("Too many values passed to Phar::mungServer(), {LIST}"),
        ));
    }
    for (_, value) in values {
        let Some(name) = value.as_str_bytes().map(<[u8]>::to_vec) else {
            return Err(throw(
                req,
                "PharException",
                &format!("Non-string value passed to Phar::mungServer(), {LIST}"),
            ));
        };
        let bit = match name.as_slice() {
            b"PHP_SELF" => MUNG_PHP_SELF,
            b"REQUEST_URI" => MUNG_REQUEST_URI,
            b"SCRIPT_NAME" => MUNG_SCRIPT_NAME,
            b"SCRIPT_FILENAME" => MUNG_SCRIPT_FILENAME,
            _ => 0,
        };
        req.state::<crate::State>().mung |= bit;
    }
    Ok(())
}

/// `Phar::webPhar()`: the file being served read as an archive, as
/// `mapPhar()` reads it; then, for a request that came with a method and
/// a URI, the entry the URI names served. The hosts here give a request
/// neither (`SG(request_info)` as the CLI leaves it), so the call ends
/// where php's ends under the CLI.
pub(crate) fn web_phar(
    req: &mut Request,
    alias: Option<PhpString>,
    index: Option<PhpString>,
    missing: Option<PhpString>,
    mimes: PhpArray,
    rewrite: Option<Callable>,
) -> Result<(), Thrown> {
    let _ = (index, missing, mimes, rewrite);
    let alias = alias.map(|a| a.as_bytes().to_vec());
    match open::open_executed(req, alias.as_deref())? {
        Ok(()) | Err(None) => Ok(()),
        Err(Some(error)) => Err(throw(req, "PharException", &error)),
    }
}
