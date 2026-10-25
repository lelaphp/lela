//! `ZipArchive::addGlob` and `ZipArchive::addPattern`
//! (`php_zip_add_from_pattern`, `php_zip_glob`, `php_zip_pcre`,
//! `php_zip_parse_options`): the files a glob or a regular expression
//! over one directory names, each added under a name the options shape.

use php_ext::abi::OrFalse;
use php_ext::coerce::{to_long, value_name};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, type_error, value_error};
use php_ext::request::{MAXPATHLEN, OwnedStream as _, Request, Streams as _};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::archive::FL_OVERWRITE;
use crate::object::{Intern, basedir_refuses, za};

/// `PHP_GLOB_ONLYDIR`.
const GLOB_ONLYDIR: i64 = 1 << 30;
/// `PHP_GLOB_AVAILABLE_FLAGS`.
const GLOB_AVAILABLE: i64 = 0x0080 | 0x0008 | 0x0020 | 0x0010 | 0x1000 | 0x0004 | GLOB_ONLYDIR;

/// Which of the two natives: the glob's flags, or the directory the
/// expression is matched in.
pub(crate) enum Kind {
    Glob(i64),
    Pcre(Vec<u8>),
}

/// `zip_options`.
struct Options {
    remove_all_path: bool,
    remove_path: Option<Vec<u8>>,
    add_path: Option<Vec<u8>>,
    flags: i64,
    comp_method: i64,
    comp_flags: i64,
    enc_method: i64,
    enc_password: Option<Vec<u8>>,
}

impl Default for Options {
    /// `PHP_ZIP_DEFAULT_OPTIONS`: `ZIP_FL_OVERWRITE`, and the method and
    /// the encryption left to libzip (`-1`), with options or without.
    fn default() -> Options {
        Options {
            remove_all_path: false,
            remove_path: None,
            add_path: None,
            flags: i64::from(FL_OVERWRITE),
            comp_method: -1,
            comp_flags: 0,
            enc_method: -1,
            enc_password: None,
        }
    }
}

/// `php_zip_parse_options`: php's warnings for a loose type, its errors
/// for a path option that is not a non-empty string.
fn parse_options(req: &mut Request, options: PhpArray) -> Result<Options, Thrown> {
    let mut o = Options::default();
    let find = |name: &str| options.find(name.as_bytes()).map(|v| v.deref());
    // `zval_get_long` after php's warning for a value of the wrong type.
    let long = |req: &mut Request, name: &str, v: Value| -> Result<i64, Thrown> {
        let flag = name == "remove_all_path";
        let fits = match v.as_ref() {
            ValueRef::Bool(_) => flag,
            ValueRef::Int(_) => !flag,
            _ => false,
        };
        if !fits {
            let (want, given) = (if flag { "bool" } else { "int" }, value_name(req, &v));
            let message = format!("Option \"{name}\" must be of type {want}, {given} given");
            docref(req, Level::Warning, &message);
        }
        to_long(req, &v)
    };
    let type_error = |req: &mut Request, name: &str, want: &str, v: &Value| -> Thrown {
        let given = value_name(req, v);
        let message = format!("Option \"{name}\" must be of type {want}, {given} given");
        type_error(req, &message)
    };
    if let Some(v) = find("remove_all_path") {
        o.remove_all_path = long(req, "remove_all_path", v)? != 0;
    }
    if let Some(v) = find("comp_method") {
        o.comp_method = long(req, "comp_method", v)?;
        if let Some(v) = find("comp_flags") {
            o.comp_flags = long(req, "comp_flags", v)?;
        }
    }
    if let Some(v) = find("enc_method") {
        o.enc_method = long(req, "enc_method", v)?;
        if let Some(v) = find("enc_password") {
            let Some(p) = v.as_str_bytes() else {
                return Err(type_error(req, "enc_password", "string", &v));
            };
            o.enc_password = Some(p.to_vec());
        }
    }
    for name in ["remove_path", "add_path"] {
        let Some(v) = find(name) else {
            continue;
        };
        let Some(s) = v.as_str_bytes() else {
            return Err(type_error(req, name, "string", &v));
        };
        let wrong = if s.is_empty() {
            format!("Option \"{name}\" must not be empty")
        } else if s.len() >= MAXPATHLEN {
            format!(
                "Option \"{name}\" must be less than {} bytes",
                MAXPATHLEN - 1
            )
        } else {
            *(if name == "add_path" {
                &mut o.add_path
            } else {
                &mut o.remove_path
            }) = Some(s.to_vec());
            continue;
        };
        return Err(value_error(req, &wrong));
    }
    if let Some(v) = find("flags") {
        let ValueRef::Int(flags) = v.as_ref() else {
            return Err(type_error(req, "flags", "int", &v));
        };
        o.flags = flags;
    }
    Ok(o)
}

/// What the walk found: the paths, or `None` for php's `-1`.
type Found = Option<Vec<Vec<u8>>>;

/// `php_zip_glob` as php's ZTS build runs it: a relative pattern made
/// absolute against the working directory, and that prefix taken off
/// each match.
fn glob(req: &mut Request, pattern: &[u8], flags: i64) -> Found {
    if pattern.len() >= MAXPATHLEN {
        let message =
            format!("Pattern exceeds the maximum allowed length of {MAXPATHLEN} characters");
        docref(req, Level::Warning, &message);
        return None;
    }
    if GLOB_AVAILABLE & flags != flags {
        docref(
            req,
            Level::Warning,
            "At least one of the passed flags is invalid or not supported on this platform",
        );
        return None;
    }
    let (work, skip) = if pattern.first() == Some(&b'/') {
        (pattern.to_vec(), 0)
    } else {
        let mut cwd = req
            .environment()
            .cwd()
            .into_os_string()
            .into_encoded_bytes();
        let skip = cwd.len() + 1;
        cwd.push(b'/');
        cwd.extend_from_slice(pattern);
        (cwd, skip)
    };
    let bits = i32::try_from(flags & !GLOB_ONLYDIR).unwrap_or(0);
    let Ok(paths) = php_ext_standard::php_glob(&work, bits) else {
        return Some(Vec::new());
    };
    let Some(first) = paths.first() else {
        return Some(Vec::new());
    };
    if basedir_refuses(req, first) {
        return None;
    }
    let mut out = Vec::new();
    for p in paths {
        if flags & GLOB_ONLYDIR != 0 && !host(&p).is_dir() {
            continue;
        }
        out.push(p.get(skip..).unwrap_or_default().to_vec());
    }
    Some(out)
}

fn host(path: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path))
}

/// `php_zip_pcre`: the regular files of `path` whose names match,
/// alphabetically, each as `path/name`.
fn pcre(req: &mut Request, regexp: &[u8], path: &[u8]) -> Result<Found, Thrown> {
    let work = if path.first() == Some(&b'/') {
        path.to_vec()
    } else {
        let mut cwd = req
            .environment()
            .cwd()
            .into_os_string()
            .into_encoded_bytes();
        cwd.push(b'/');
        cwd.extend_from_slice(path);
        cwd
    };
    if basedir_refuses(req, &work) {
        return Ok(None);
    }
    // `php_stream_scandir` under `REPORT_ERRORS`, `php_stream_dirent_alphasort`.
    let Some(dir) = req.open_dir(path)? else {
        return Ok(None);
    };
    let mut names: Vec<Vec<u8>> = Vec::new();
    loop {
        let name = match dir.read_dir(req) {
            Ok(Some(name)) => name,
            Ok(None) => break,
            Err(thrown) => {
                dir.free(req);
                return Err(thrown);
            }
        };
        names.push(name);
    }
    dir.free(req);
    names.sort();
    if names.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let Some(re) = php_ext_pcre::Regex::compile(req, regexp) else {
        docref(req, Level::Warning, "Invalid expression");
        return Ok(None);
    };
    let mut out = Vec::new();
    for name in names {
        if name == b"." || name == b".." {
            continue;
        }
        if path.len() + name.len() + 1 >= MAXPATHLEN {
            let message = format!(
                "add_path string too long (max: {}, {} given)",
                MAXPATHLEN - 1,
                path.len() + name.len() + 1
            );
            docref(req, Level::Warning, &message);
            break;
        }
        if !re.is_match(req, &name) {
            continue;
        }
        let mut full = path.to_vec();
        full.push(b'/');
        full.extend_from_slice(&name);
        let mut abs = work.clone();
        abs.push(b'/');
        abs.extend_from_slice(&name);
        match std::fs::metadata(host(&abs)) {
            Err(_) => {
                let message = format!("Cannot read <{}>", String::from_utf8_lossy(&full));
                docref(req, Level::Warning, &message);
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => out.push(full),
        }
    }
    Ok(Some(out))
}

/// `php_zip_add_from_pattern`.
pub(crate) fn add_from_pattern(
    req: &mut Request,
    object: PhpObject,
    pattern: &[u8],
    kind: &Kind,
    options: PhpArray,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if pattern.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let opts = if options.is_empty() {
        Options::default()
    } else {
        parse_options(req, options)?
    };
    let found = match kind {
        Kind::Glob(flags) => glob(req, pattern, *flags),
        Kind::Pcre(path) => pcre(req, pattern, path)?,
    };
    let Some(found) = found else {
        return Ok(OrFalse::False);
    };
    let mut out = PhpArray::new(req)?;
    for file in &found {
        let stripped: &[u8] = if opts.remove_all_path {
            php_ext_standard::php_basename(file, b"")
        } else if let Some(remove) = &opts.remove_path
            && file.len() > remove.len()
            && file.starts_with(remove)
        {
            if file[remove.len()] == b'/' {
                &file[remove.len() + 1..]
            } else {
                &file[remove.len()..]
            }
        } else {
            file
        };
        let mut entry = Vec::new();
        if let Some(add) = &opts.add_path {
            if add.len() + stripped.len() > MAXPATHLEN {
                let message = format!(
                    "Entry name too long (max: {}, {} given)",
                    MAXPATHLEN - 1,
                    add.len() + stripped.len()
                );
                docref(req, Level::Warning, &message);
                out.give_up(req);
                return Ok(OrFalse::False);
            }
            entry.extend_from_slice(add);
        }
        entry.extend_from_slice(stripped);
        entry.truncate(MAXPATHLEN - 1);
        let added = match crate::object::add_file_for_pattern(req, object, file, &entry, opts.flags)
        {
            Ok(added) => added,
            Err(thrown) => {
                out.give_up(req);
                return Err(thrown);
            }
        };
        if !added {
            out.give_up(req);
            return Ok(OrFalse::False);
        }
        if !settle(req, object, &opts) {
            out.give_up(req);
            return Ok(OrFalse::False);
        }
        let v = PhpString::new(req, file)?.into_value();
        out.append(req, v)?;
    }
    Ok(OrFalse::Value(out))
}

/// The compression and encryption the options ask of the entry added
/// last.
fn settle(req: &mut Request, object: PhpObject, opts: &Options) -> bool {
    let Some(i) = object.native::<Intern>(req) else {
        return false;
    };
    let idx = usize::try_from(i.last_id.0).unwrap_or(usize::MAX);
    let Some(archive) = za(req, object) else {
        return false;
    };
    if opts.comp_method >= 0
        && archive
            .set_compression(
                idx,
                crate::c::i32(opts.comp_method),
                crate::c::u32(opts.comp_flags),
            )
            .is_err()
    {
        return false;
    }
    if opts.enc_method >= 0 {
        if archive.set_encryption(idx, 0, None).is_err() {
            docref(req, Level::Warning, "password reset failed");
            return false;
        }
        let Some(archive) = za(req, object) else {
            return false;
        };
        let password = opts.enc_password.as_deref().map(php_ext::coerce::c_str);
        if archive
            .set_encryption(idx, crate::c::u16(opts.enc_method), password)
            .is_err()
        {
            return false;
        }
    }
    true
}
