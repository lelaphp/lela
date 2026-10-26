//! `Phar::running()`, `Phar::mount()`, `Phar::mungServer()` and
//! `Phar::webPhar()`: what a running archive asks of the archive it runs
//! from.

use php_ext::abi::Callable;
use php_ext::raising::{Thrown, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value, walk};

use crate::archive::{ArchiveId, archives, lossy};
use crate::entry::{self, Dir, Found};
use crate::open;
use crate::path::{self, Create, Exec};
use crate::throwables::Throwables;

/// `PHAR_MUNG_*`: the `$_SERVER` entries `mungServer()` names, one bit
/// each, kept for a web request.
pub(crate) const MUNG_PHP_SELF: u8 = 1;
pub(crate) const MUNG_REQUEST_URI: u8 = 2;
pub(crate) const MUNG_SCRIPT_NAME: u8 = 4;
pub(crate) const MUNG_SCRIPT_FILENAME: u8 = 8;

/// The file being served as `phar_split_fname` reads a `phar://` URL,
/// where it is one.
fn running_split(req: &mut Request) -> Result<Option<(Vec<u8>, path::Split)>, Thrown> {
    let Some(file) = req.frames().current_file().map(<[u8]>::to_vec) else {
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
    let file = req
        .frames()
        .current_file()
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    let (arch, path): (Option<Vec<u8>>, Vec<u8>) = if file.len() > 7 && file.starts_with(b"phar://")
    {
        match path::split_fname(req, &file, Exec::Either, Create::No)? {
            Ok(split) => {
                if inner.len() > 7 && inner.starts_with(b"phar://") {
                    let ce = Throwables::of(req)?;
                    return Err(throw_exception_ex(
                        req,
                        ce.phar_exception,
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
    let ce = Throwables::of(req)?;
    Err(throw_exception_ex(
        req,
        ce.phar_exception,
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
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
            &format!("{shown} is not a phar archive, cannot mount"),
        ));
    };
    if !entry::mount(req, id, outer, path)? {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
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
    // Each value's bit, `None` for one that is no string, read before
    // any is kept.
    let bits: Vec<Option<u8>> = walk::rows(req, names)
        .map(|(_, value)| {
            value.deref().as_str_bytes().map(|name| match name {
                b"PHP_SELF" => MUNG_PHP_SELF,
                b"REQUEST_URI" => MUNG_REQUEST_URI,
                b"SCRIPT_NAME" => MUNG_SCRIPT_NAME,
                b"SCRIPT_FILENAME" => MUNG_SCRIPT_FILENAME,
                _ => 0,
            })
        })
        .collect();
    if bits.is_empty() {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
            &format!("No values passed to Phar::mungServer(), {LIST}"),
        ));
    }
    if bits.len() > 4 {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
            &format!("Too many values passed to Phar::mungServer(), {LIST}"),
        ));
    }
    for bit in bits {
        let Some(bit) = bit else {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.phar_exception,
                &format!("Non-string value passed to Phar::mungServer(), {LIST}"),
            ));
        };
        req.state::<crate::State>().mung |= bit;
    }
    Ok(())
}

/// `PHAR_MIME_*`: what an entry's extension makes of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mime {
    /// `PHAR_MIME_PHP`: included and served.
    Php,
    /// `PHAR_MIME_PHPS`: its source highlighted.
    Phps,
    /// `PHAR_MIME_OTHER`: sent as it is, under its type.
    Other,
}

/// `PHAR_G(mime_types)` as `PHP_GINIT_FUNCTION(phar)` fills it
/// (`Z/ext/phar/phar.c:3349-3388`).
const MIME_TYPES: [(&str, &str, Mime); 40] = [
    ("phps", "text/html", Mime::Phps),
    ("c", "text/plain", Mime::Other),
    ("cc", "text/plain", Mime::Other),
    ("cpp", "text/plain", Mime::Other),
    ("c++", "text/plain", Mime::Other),
    ("dtd", "text/plain", Mime::Other),
    ("h", "text/plain", Mime::Other),
    ("log", "text/plain", Mime::Other),
    ("rng", "text/plain", Mime::Other),
    ("txt", "text/plain", Mime::Other),
    ("xsd", "text/plain", Mime::Other),
    ("php", "", Mime::Php),
    ("inc", "", Mime::Php),
    ("avi", "video/avi", Mime::Other),
    ("bmp", "image/bmp", Mime::Other),
    ("css", "text/css", Mime::Other),
    ("gif", "image/gif", Mime::Other),
    ("htm", "text/html", Mime::Other),
    ("html", "text/html", Mime::Other),
    ("htmls", "text/html", Mime::Other),
    ("ico", "image/x-ico", Mime::Other),
    ("jpe", "image/jpeg", Mime::Other),
    ("jpg", "image/jpeg", Mime::Other),
    ("jpeg", "image/jpeg", Mime::Other),
    ("js", "application/x-javascript", Mime::Other),
    ("midi", "audio/midi", Mime::Other),
    ("mid", "audio/midi", Mime::Other),
    ("mod", "audio/mod", Mime::Other),
    ("mov", "movie/quicktime", Mime::Other),
    ("mp3", "audio/mp3", Mime::Other),
    ("mpg", "video/mpeg", Mime::Other),
    ("mpeg", "video/mpeg", Mime::Other),
    ("pdf", "application/pdf", Mime::Other),
    ("png", "image/png", Mime::Other),
    ("swf", "application/shockwave-flash", Mime::Other),
    ("tif", "image/tiff", Mime::Other),
    ("tiff", "image/tiff", Mime::Other),
    ("wav", "audio/wav", Mime::Other),
    ("xbm", "image/xbm", Mime::Other),
    ("xml", "text/xml", Mime::Other),
];

/// `phar_file_type`: the type and the action the entry's extension
/// names, `text/plain` where it has none and `application/octet-stream`
/// where the table has none.
fn file_type(entry: &[u8]) -> (Vec<u8>, Mime) {
    let Some(dot) = entry.iter().rposition(|b| *b == b'.') else {
        return (b"text/plain".to_vec(), Mime::Other);
    };
    let ext = &entry[dot + 1..];
    MIME_TYPES
        .iter()
        .find(|(name, _, _)| name.as_bytes() == ext)
        .map_or(
            (b"application/octet-stream".to_vec(), Mime::Other),
            |(_, mime, kind)| (mime.as_bytes().to_vec(), *kind),
        )
}

/// What ends the request once the front controller has answered it:
/// php's `zend_bailout()`, which leaves the status as it stands, as an
/// `exit("")` does.
fn bailout(req: &mut Request) -> Thrown {
    match PhpString::new(req, b"") {
        Ok(empty) => {
            let thrown = php_ext::raising::exit(req, &Value::string(*empty));
            empty.give_up(req);
            thrown
        }
        Err(thrown) => thrown,
    }
}

/// A string row of `$_SERVER` as it stands, made or not.
fn server_row(req: &mut Request, name: &[u8]) -> Option<Vec<u8>> {
    req.variables_mut()
        .made_superglobal(b"_SERVER")?
        .find(name)
        .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
}

/// A row of `$_SERVER` replaced, what it held kept under `kept`.
fn swap_server(
    req: &mut Request,
    name: &[u8],
    kept: &[u8],
    old: &[u8],
    new: &[u8],
) -> Result<(), Thrown> {
    let value = PhpString::new(req, new)?.into_value();
    req.variables_mut().update_server(name, value)?;
    let value = PhpString::new(req, old)?.into_value();
    req.variables_mut().update_server(kept, value)
}

/// `phar_mung_server_vars` (`Z/ext/phar/phar_object.c:57`): `PATH_INFO`
/// past the entry and `PATH_TRANSLATED` into the archive, always, and the
/// rows `mungServer()` named, each old value kept under `PHAR_` and its
/// name; nothing where the program never made `$_SERVER`.
fn mung_server_vars(
    req: &mut Request,
    fname: &[u8],
    entry: &[u8],
    basename: &[u8],
    ru_len: usize,
) -> Result<(), Thrown> {
    if req.variables_mut().made_superglobal(b"_SERVER").is_none() {
        return Ok(());
    }
    let mut url = b"phar://".to_vec();
    url.extend_from_slice(fname);
    url.extend_from_slice(entry);
    if let Some(info) = server_row(req, b"PATH_INFO")
        && info.len() > entry.len()
        && info.starts_with(entry)
    {
        let end = (entry.len() + ru_len).min(info.len());
        swap_server(
            req,
            b"PATH_INFO",
            b"PHAR_PATH_INFO",
            &info,
            &info[entry.len()..end],
        )?;
    }
    if let Some(translated) = server_row(req, b"PATH_TRANSLATED") {
        swap_server(
            req,
            b"PATH_TRANSLATED",
            b"PHAR_PATH_TRANSLATED",
            &translated,
            &url,
        )?;
    }
    let mung = req.state::<crate::State>().mung;
    for (bit, name, kept) in [
        (
            MUNG_REQUEST_URI,
            &b"REQUEST_URI"[..],
            &b"PHAR_REQUEST_URI"[..],
        ),
        (MUNG_PHP_SELF, b"PHP_SELF", b"PHAR_PHP_SELF"),
    ] {
        if mung & bit != 0
            && let Some(old) = server_row(req, name)
            && old.len() > basename.len()
            && old.starts_with(basename)
        {
            swap_server(req, name, kept, &old, &old[basename.len()..])?;
        }
    }
    if mung & MUNG_SCRIPT_NAME != 0
        && let Some(old) = server_row(req, b"SCRIPT_NAME")
    {
        swap_server(req, b"SCRIPT_NAME", b"PHAR_SCRIPT_NAME", &old, entry)?;
    }
    if mung & MUNG_SCRIPT_FILENAME != 0
        && let Some(old) = server_row(req, b"SCRIPT_FILENAME")
    {
        swap_server(req, b"SCRIPT_FILENAME", b"PHAR_SCRIPT_FILENAME", &old, &url)?;
    }
    Ok(())
}

/// `phar_file_action` (`Z/ext/phar/phar_object.c:143`): the entry
/// answered as its type says, and the request ended; `Ok` where an
/// included entry could not be compiled or was already included, which
/// `webPhar()` then returns from.
fn file_action(
    req: &mut Request,
    id: ArchiveId,
    slot: usize,
    (mime, kind): (&[u8], Mime),
    entry: &[u8],
    fname: &[u8],
    basename: Option<(&[u8], usize)>,
) -> Result<(), Thrown> {
    let mut name = b"phar://".to_vec();
    name.extend_from_slice(fname);
    if entry.first() != Some(&b'/') {
        name.push(b'/');
    }
    name.extend_from_slice(entry);
    match kind {
        Mime::Phps => {
            let function = PhpString::new(req, b"highlight_file")?.into_value();
            let callable = php_ext::coerce::callable_checked(req, &function);
            function.give_up(req);
            if let Ok(callable) = callable {
                let name = PhpString::new(req, &name)?.into_value();
                let highlighted = php_ext::call::call(req, &callable, &[*name]);
                name.give_up(req);
                highlighted?.give_up(req);
            }
            Err(bailout(req))
        }
        Mime::Other => {
            let mut line = b"Content-type: ".to_vec();
            line.extend_from_slice(mime);
            req.response_mut().header_add(&line, true, 0);
            let size = archives(req)
                .get(id)
                .and_then(|a| a.manifest.at(slot))
                .map_or(0, |e| e.uncompressed_filesize);
            req.response_mut()
                .header_add(format!("Content-length: {size}").as_bytes(), true, 0);
            let bytes = entry::contents(req, id, slot)?;
            req.response_mut().write(&bytes);
            Err(bailout(req))
        }
        Mime::Php => {
            if let Some((basename, ru_len)) = basename {
                mung_server_vars(req, fname, entry, basename, ru_len)?;
            }
            let dir = entry.iter().rposition(|b| *b == b'/');
            let state = req.state::<crate::State>();
            state.cwd = None;
            if let Some(cut) = dir {
                state.cwd_init = true;
                let start = usize::from(entry.first() == Some(&b'/'));
                state.cwd = (cut > 0).then(|| entry[start..cut].to_vec());
            }
            let served = php_ext::call::load_file(req, &name);
            let state = req.state::<crate::State>();
            state.cwd = None;
            state.cwd_init = false;
            if served? {
                return Err(bailout(req));
            }
            Ok(())
        }
    }
}

/// `phar_do_403`: the status line and php's page.
fn do_403(req: &mut Request) -> Thrown {
    req.response_mut()
        .header_add(b"HTTP/1.0 403 Access Denied", true, 403);
    req.response_mut().write(b"<html>\n <head>\n  <title>Access Denied</title>\n </head>\n <body>\n  <h1>403 - File Access Denied</h1>\n </body>\n</html>");
    bailout(req)
}

/// `phar_do_404`: the not-found file where one is named and the
/// archive has it, else the status line and php's page.
fn do_404(
    req: &mut Request,
    id: Option<ArchiveId>,
    fname: &[u8],
    f404: Option<&[u8]>,
) -> Result<(), Thrown> {
    if let (Some(id), Some(f404)) = (id, f404.filter(|f| !f.is_empty()))
        && let Ok(Found::Slot(slot)) = entry::find(req, id, f404, Dir::No, true)?
    {
        file_action(req, id, slot, (b"text/html", Mime::Php), f404, fname, None)?;
        return Err(bailout(req));
    }
    req.response_mut()
        .header_add(b"HTTP/1.0 404 Not Found", true, 404);
    req.response_mut().write(b"<html>\n <head>\n  <title>File Not Found</title>\n </head>\n <body>\n  <h1>404 - File Not Found</h1>\n </body>\n</html>");
    Err(bailout(req))
}

/// `phar_postprocess_ru_web`: the longest leading part of the entry the
/// archive holds, and what follows it, `/` first, as the request URI's
/// rest; the entry as it stands where no part is held.
fn postprocess_ru_web(req: &mut Request, id: ArchiveId, entry: &mut Vec<u8>) -> Option<Vec<u8>> {
    let e = entry.get(1..)?.to_vec();
    let mut cut = e.len();
    loop {
        let held = archives(req)
            .get(id)
            .is_some_and(|a| a.manifest.slot(&e[..cut]).is_some());
        if held {
            entry.truncate(cut + 1);
            return (cut < e.len()).then(|| e[cut..].to_vec());
        }
        cut = e[..cut].iter().rposition(|b| *b == b'/')?;
    }
}

/// `Phar::webPhar()` (`Z/ext/phar/phar_object.c:548`): the file being
/// served read as an archive, as `mapPhar()` reads it; then, for a
/// request of the web with a method php serves and a URI, the entry the
/// path names: under CGI the `PATH_INFO` after the `SCRIPT_NAME` that
/// names the archive, `$_SERVER`'s where the program made it and the
/// environment's where it did not; passed through the rewrite callback;
/// the archive's index answered with a 301 to it, an entry the archive
/// has not with the 404 file or page, and any other included and
/// served, highlighted or sent as its extension says. Answering ends the
/// request. The CLI has no such request, so there the call ends after
/// the read, as php's does.
pub(crate) fn web_phar(
    req: &mut Request,
    alias: Option<PhpString>,
    index: Option<PhpString>,
    missing: Option<PhpString>,
    mimes: PhpArray,
    rewrite: Option<Callable>,
) -> Result<(), Thrown> {
    let alias = alias.map(|a| a.as_bytes().to_vec());
    match open::open_executed(req, alias.as_deref())? {
        Ok(()) => {}
        Err(None) => return Ok(()),
        Err(Some(error)) => {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(req, ce.phar_exception, &error));
        }
    }
    let served = req
        .variables()
        .request_method()
        .is_some_and(|m| METHODS.contains(&m));
    if !served || req.variables().request_uri().is_none() {
        return Ok(());
    }
    let Some(fname) = req.frames().current_file().map(<[u8]>::to_vec) else {
        return Ok(());
    };
    let basename = fname
        .iter()
        .rposition(|b| *b == b'/')
        .map_or(fname.as_slice(), |at| &fname[at + 1..])
        .to_vec();
    let Some(located) = locate(req, &basename) else {
        return Ok(());
    };
    let Located {
        mut entry,
        path_info,
        pt,
        not_cgi,
    } = located;
    if let Some(rewrite) = rewrite {
        entry = rewritten(req, &rewrite, &entry)?;
    }
    let id = crate::archive::get_archive(req, &fname, None).ok();
    let mut ru = None;
    if !entry.is_empty()
        && let Some(id) = id
    {
        ru = postprocess_ru_web(req, id, &mut entry);
    }
    let f404 = missing.map(|m| m.as_bytes().to_vec());
    if entry.is_empty() || entry == b"/" {
        let mut index = index
            .map(|i| i.as_bytes().to_vec())
            .filter(|i| !i.is_empty())
            .unwrap_or_else(|| b"/index.php".to_vec());
        if index.first() != Some(&b'/') {
            index.insert(0, b'/');
        }
        let found = match id {
            Some(id) => entry::find(req, id, &index, Dir::No, false)?,
            None => Err(None),
        };
        if !matches!(found, Ok(Found::Slot(_))) {
            return do_404(req, id, &fname, f404.as_deref());
        }
        return Err(moved(
            req,
            &index,
            &path_info,
            not_cgi.then_some(basename.as_slice()),
        ));
    }
    let slot = match id {
        Some(id) => match entry::find(req, id, &entry, Dir::No, false)? {
            Ok(Found::Slot(slot)) => slot,
            _ => return do_404(req, Some(id), &fname, f404.as_deref()),
        },
        None => return do_404(req, None, &fname, f404.as_deref()),
    };
    let Some(id) = id else {
        return Ok(());
    };
    let (mime, kind) = mime_of(req, &entry, mimes)?;
    let ru_len = ru.as_ref().map_or(0, Vec::len);
    file_action(
        req,
        id,
        slot,
        (&mime, kind),
        &entry,
        &fname,
        Some((&pt, ru_len)),
    )
}

/// The methods `webPhar()` answers a request of.
const METHODS: [&[u8]; 7] = [
    b"GET", b"POST", b"DELETE", b"HEAD", b"OPTIONS", b"PATCH", b"PUT",
];

/// The entry the rewrite callback answers for `entry`: a string is the
/// entry, `true` or `false` the 403, anything else php's
/// `PharException`.
fn rewritten(req: &mut Request, rewrite: &Callable, entry: &[u8]) -> Result<Vec<u8>, Thrown> {
    let param = PhpString::new(req, entry)?.into_value();
    let answered = php_ext::call::call(req, rewrite, &[*param]);
    param.give_up(req);
    // A callback that threw left no return value: php's own exception
    // then, the thrown one its previous (`zend_throw_exception_ex` over a
    // pending one).
    let answered = answered.map_err(|thrown| {
        let ce = match Throwables::of(req) {
            Ok(ce) => ce,
            Err(fatal) => return fatal,
        };
        php_ext::raising::throw_chained(
            req,
            thrown,
            ce.phar_exception,
            "phar error: rewrite callback must return a string or false",
        )
    })?;
    if let Some(rewritten) = answered.as_str_bytes().map(<[u8]>::to_vec) {
        answered.give_up(req);
        return Ok(rewritten);
    }
    if answered.as_bool().is_some() {
        answered.give_up(req);
        return Err(do_403(req));
    }
    answered.give_up(req);
    let ce = Throwables::of(req)?;
    Err(throw_exception_ex(
        req,
        ce.phar_exception,
        "phar error: rewrite callback must return a string or false",
    ))
}

/// A request for the archive itself moved to its index, a 301 to the
/// path with the index after it, the path cut after the archive's name
/// outside CGI, and the request ended.
fn moved(req: &mut Request, index: &[u8], path_info: &[u8], cut_after: Option<&[u8]>) -> Thrown {
    req.response_mut()
        .header_add(b"HTTP/1.1 301 Moved Permanently", true, 301);
    let mut base = path_info.to_vec();
    if let Some(basename) = cut_after
        && let Some(at) = crate::path::find(&base, basename)
    {
        base.truncate(at + basename.len());
    }
    let mut location = b"Location: ".to_vec();
    location.extend_from_slice(&base);
    if base.last() == Some(&b'/') {
        location.extend_from_slice(&index[1..]);
    } else {
        location.extend_from_slice(index);
    }
    req.response_mut().header_add(&location, true, 0);
    bailout(req)
}

/// Where the request's path puts the entry, and what the 301 and the
/// server rows are built from.
struct Located {
    entry: Vec<u8>,
    path_info: Vec<u8>,
    pt: Vec<u8>,
    not_cgi: bool,
}

/// The entry the request's path names past the archive's own name: from
/// `SCRIPT_NAME` and `PATH_INFO` under CGI's hosts, from the request URI
/// under any other; `None` where the path does not name the archive.
fn locate(req: &mut Request, basename: &[u8]) -> Option<Located> {
    let cgi = matches!(
        req.environment().sapi_name(),
        "cgi-fcgi" | "fpm-fcgi" | "cgi" | "litespeed"
    );
    if !cgi {
        let path_info = req.variables().request_uri().unwrap_or_default().to_vec();
        let at = crate::path::find(&path_info, basename)?;
        let end = at + basename.len();
        return Some(Located {
            entry: path_info[end..].to_vec(),
            pt: path_info[..end].to_vec(),
            path_info,
            not_cgi: true,
        });
    }
    if req.variables_mut().made_superglobal(b"_SERVER").is_some() {
        let script_name = server_row(req, b"SCRIPT_NAME")?;
        crate::path::find(&script_name, basename)?;
        let entry = server_row(req, b"PATH_INFO").unwrap_or_default();
        let mut path_info = script_name.clone();
        path_info.extend_from_slice(&entry);
        return Some(Located {
            entry,
            path_info,
            pt: script_name,
            not_cgi: false,
        });
    }
    let env = |req: &Request, name: &[u8]| {
        req.environment()
            .env_pairs()
            .iter()
            .find(|(had, _)| had == name)
            .map(|(_, value)| value.clone())
    };
    let testit = env(req, b"SCRIPT_NAME")?;
    let at = crate::path::find(&testit, basename)?;
    let entry = env(req, b"PATH_INFO").unwrap_or_default();
    let mut path_info = testit.clone();
    path_info.extend_from_slice(&entry);
    Some(Located {
        entry,
        path_info,
        pt: testit[..(at + basename.len()).min(testit.len())].to_vec(),
        not_cgi: false,
    })
}

/// The type an entry is answered as: the `$mimeTypes` override for its
/// extension, `Phar::PHP` or `Phar::PHPS` or a type's name, and the
/// table's otherwise.
fn mime_of(req: &mut Request, entry: &[u8], mimes: PhpArray) -> Result<(Vec<u8>, Mime), Thrown> {
    if !mimes.is_empty()
        && let Some(dot) = entry.iter().rposition(|b| *b == b'.')
        && let Some(value) = mimes.find(&entry[dot + 1..])
    {
        if let Some(code) = value.as_int() {
            return match code {
                0 => Ok((Vec::new(), Mime::Php)),
                1 => Ok((Vec::new(), Mime::Phps)),
                _ => {
                    let ce = Throwables::of(req)?;
                    Err(throw_exception_ex(
                        req,
                        ce.phar_exception,
                        "Unknown mime type specifier used, only Phar::PHP, Phar::PHPS and a mime type string are allowed",
                    ))
                }
            };
        }
        if let Some(mime) = value.as_str_bytes() {
            return Ok((mime.to_vec(), Mime::Other));
        }
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
            "Unknown mime type specifier used (not a string or int), only Phar::PHP, Phar::PHPS and a mime type string are allowed",
        ));
    }
    Ok(file_type(entry))
}
