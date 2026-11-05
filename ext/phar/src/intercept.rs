//! `func_interceptors.c`: `fopen`, `file_get_contents`, `readfile`,
//! `opendir` and the stat family replaced as the engine is built, each
//! reading a relative path inside the archive the file being served
//! stands in once `Phar::interceptFileFuncs()` asked for it, and
//! otherwise serving the native it replaced.

use php_ext::abi::{Beneath, ExtensionBuilder, chain};
use php_ext::call::passthrough;
use php_ext::process;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Request, Streams as _, Unresolved};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::archive::{self, ENT_PERM_MASK, archives, lossy};
use crate::path::{self, Create, Exec};

/// `phar_intercept_functions`: the request marked, as php marks it
/// (`PHAR_G(intercepted)`), for the file functions to read a relative
/// path inside the archive being served.
pub(crate) fn intercept(req: &mut Request) {
    req.state::<crate::State>().intercepted = true;
}

/// `phar_intercept_functions_init`: the natives replaced. A file inside
/// an archive reads its neighbours by relative paths once it asked:
///
/// ```php
/// $p = __DIR__ . '/intercept-witness.phar';
/// $files = [
///     'a.php' => '<?php Phar::interceptFileFuncs(); var_dump(file_get_contents("b.txt"), is_file("b.txt"), is_dir("d"), file_exists("nope"), filesize("b.txt"), fileperms("b.txt"), filetype("d"), is_link("b.txt"), stat("b.txt")["size"], readfile("b.txt"), fgets(fopen("b.txt", "r"))); var_dump(@filemtime("nope"), error_get_last()["message"]); $d = opendir("d"); var_dump(readdir($d));',
///     'b.txt' => 'bee',
///     'd/c.txt' => 'see',
/// ];
/// $m = ''; $body = '';
/// foreach ($files as $n => $c) { $m .= pack('V', strlen($n)) . $n . pack('VVVVVV', strlen($c), 1700000000, strlen($c), crc32($c), 0x1a4, 0); $body .= $c; }
/// $m = pack('VnVV', count($files), 0x1110, 0x10000, 0) . pack('V', 0) . $m;
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . $body;
/// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
/// include "phar://$p/a.php";
/// var_dump(is_file('b.txt'), file_exists(__FILE__));
/// unlink($p);
/// ```
/// ```expect
/// beestring(3) "bee"
/// bool(true)
/// bool(true)
/// bool(false)
/// int(3)
/// int(33060)
/// string(3) "dir"
/// bool(false)
/// int(3)
/// int(3)
/// string(3) "bee"
/// bool(false)
/// string(33) "filemtime(): stat failed for nope"
/// string(5) "c.txt"
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn install(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .intercept(b"fopen", fopen)
        .intercept(b"file_get_contents", file_get_contents)
        .intercept(b"is_file", is_file)
        .intercept(b"is_link", is_link)
        .intercept(b"is_dir", is_dir)
        .intercept(b"opendir", opendir)
        .intercept(b"file_exists", file_exists)
        .intercept(b"fileperms", fileperms)
        .intercept(b"fileinode", fileinode)
        .intercept(b"filesize", filesize)
        .intercept(b"fileowner", fileowner)
        .intercept(b"filegroup", filegroup)
        .intercept(b"fileatime", fileatime)
        .intercept(b"filemtime", filemtime)
        .intercept(b"filectime", filectime)
        .intercept(b"filetype", filetype)
        .intercept(b"is_writable", is_writable)
        .intercept(b"is_readable", is_readable)
        .intercept(b"is_executable", is_executable)
        .intercept(b"lstat", lstat)
        .intercept(b"stat", stat)
        .intercept(b"readfile", readfile)
}

/// `PHAR_G(intercepted)` with an archive open: what every interceptor
/// asks before it reads anything.
fn active(req: &mut Request) -> bool {
    req.state::<crate::State>().intercepted && !archives(req).fname_map.is_empty()
}

/// An argument read as a path, `p` quietly: a string without a NUL.
fn path_arg(req: &Request, position: u32) -> Option<Vec<u8>> {
    let value = req.frames().argument(position)?.deref();
    match value.as_ref() {
        ValueRef::Str(s) if !s.as_bytes().contains(&0) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

/// An optional `b` argument read quietly: `false` where not given.
fn bool_arg(req: &Request, position: u32) -> Option<bool> {
    match req.frames().argument(position).map(|v| v.deref()) {
        None => Some(false),
        Some(value) => match value.as_ref() {
            ValueRef::Bool(b) => Some(b),
            ValueRef::Int(i) => Some(i != 0),
            _ => None,
        },
    }
}

/// `!IS_ABSOLUTE_PATH(...) && !strstr(..., "://")`.
fn relative(filename: &[u8]) -> bool {
    filename.first() != Some(&b'/') && path::find(filename, b"://").is_none()
}

/// The archive the file being served stands in, by the name
/// `phar_split_fname` finds in its `phar://` URL.
fn serving_archive(req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(file) = req.frames().current_file().map(<[u8]>::to_vec) else {
        return Ok(None);
    };
    if file.len() < 7 || !file[..7].eq_ignore_ascii_case(b"phar://") {
        return Ok(None);
    }
    Ok(path::split_fname(req, &file, Exec::Either, Create::No)?
        .ok()
        .map(|split| split.arch))
}

/// `phar_fix_filepath(entry, 1)` under `PHAR_G(cwd)`.
fn fixed(req: &mut Request, filename: &[u8]) -> Vec<u8> {
    let cwd = req.state::<crate::State>().cwd.clone();
    path::fix_filepath(filename, cwd.as_deref())
}

/// `phar://<arch>` and the fixed entry, with the `/` between.
fn url(arch: &[u8], entry: &[u8]) -> Vec<u8> {
    if entry.first() == Some(&b'/') {
        [b"phar://".as_slice(), arch, entry].concat()
    } else {
        [b"phar://".as_slice(), arch, b"/", entry].concat()
    }
}

/// `phar_get_name_for_relative_paths`: the `phar://` name a relative path
/// has in the archive being served, where the archive holds it.
fn relative_name(
    req: &mut Request,
    filename: &[u8],
    use_include_path: bool,
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(arch) = serving_archive(req)? else {
        return Ok(None);
    };
    let Ok(id) = archive::get_archive(req, &arch, None) else {
        return Ok(None);
    };
    if use_include_path {
        return find_in_include_path(req, &arch, filename);
    }
    let entry = fixed(req, filename);
    let held = entry.strip_prefix(b"/").unwrap_or(&entry);
    let present = archives(req)
        .get(id)
        .is_some_and(|a| a.manifest.contains(held));
    Ok(present.then(|| url(&arch, &entry)))
}

/// `phar_resolve_path`, phar's `zend_resolve_path`: what
/// `phar_find_in_include_path` finds for a path named from inside the
/// archive being served, where it knows the directory it is served from;
/// any other path left to the one it saved, `phar_save_resolve_path`.
/// `stream_resolve_include_path` from inside an archive comes through
/// here and through the walk of the `include_path` once:
///
/// ```php
/// $p = __DIR__ . '/resolve-witness.phar';
/// $files = ['d/a.php' => '<?php var_dump(stream_resolve_include_path("nope.php"), basename(stream_resolve_include_path("b.php"))); include "b.php";', 'd/b.php' => '<?php echo "bee\n";'];
/// $m = ''; $body = '';
/// foreach ($files as $n => $c) { $m .= pack('V', strlen($n)) . $n . pack('VVVVVV', strlen($c), 1700000000, strlen($c), crc32($c), 0x1a4, 0); $body .= $c; }
/// $m = pack('VnVV', count($files), 0x1110, 0x10000, 0) . pack('V', 0) . $m;
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . $body;
/// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
/// include "phar://$p/d/a.php";
/// unlink($p);
/// ```
/// ```expect
/// bool(false)
/// string(5) "b.php"
/// bee
/// ```
pub(crate) fn resolve_path(
    req: &mut Request,
    beneath: Beneath<'_, chain::ResolvePath>,
) -> Result<Vec<u8>, Unresolved> {
    match found_in_archive(req, beneath.path()).map_err(Unresolved::Threw)? {
        Some(path) => Ok(path),
        None => beneath.serve(req),
    }
}

/// What [`find_in_include_path`] finds for `filename` in the archive
/// being served, where one is.
fn found_in_archive(req: &mut Request, filename: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    if req.state::<crate::State>().cwd.is_none() {
        return Ok(None);
    }
    let Some(arch) = serving_archive(req)? else {
        return Ok(None);
    };
    in_archive(req, &arch, filename)
}

/// `phar_find_in_include_path`: what [`in_archive`] finds, then the
/// `include_path`, whose walk the chain's own link ends in without
/// asking the archive again past [`in_archive`].
fn find_in_include_path(
    req: &mut Request,
    arch: &[u8],
    filename: &[u8],
) -> Result<Option<Vec<u8>>, Thrown> {
    if let Some(found) = in_archive(req, arch, filename)? {
        return Ok(Some(found));
    }
    let function = archive::string(req, b"stream_resolve_include_path")?;
    let callable = php_ext::coerce::callable_checked(req, &function);
    function.give_up(req);
    let Ok(callable) = callable else {
        return Ok(None);
    };
    let name = archive::string(req, filename)?;
    let found = php_ext::call::call(req, &callable, &[*name]);
    name.give_up(req);
    let found = found?;
    let out = found.as_str_bytes().map(<[u8]>::to_vec);
    found.give_up(req);
    Ok(out)
}

/// The archive's part of `phar_find_in_include_path`: a `./` or `../`
/// path looked for in it, and any other along `phar://<arch>/<cwd>`;
/// `None` where there is no directory of the file being served
/// (`PHAR_G(cwd)` unset) or the archive holds nothing by the name.
fn in_archive(req: &mut Request, arch: &[u8], filename: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(cwd) = req.state::<crate::State>().cwd.clone() else {
        return Ok(None);
    };
    let Ok(id) = archive::get_archive(req, arch, None) else {
        return Ok(None);
    };
    if filename.first() == Some(&b'.') {
        let test = fixed(req, filename);
        let held = test.strip_prefix(b"/").unwrap_or(&test);
        if archives(req)
            .get(id)
            .is_some_and(|a| a.manifest.contains(held))
        {
            return Ok(Some(url(arch, &test)));
        }
    }
    let candidate = [b"phar://".as_slice(), arch, b"/", &cwd, b"/", filename].concat();
    let found = req.url_stat_quiet(&candidate, false)?.found().is_some();
    Ok(found.then_some(candidate))
}

/// The first argument replaced by `name` and the replaced native served,
/// what the interceptors that open a stream do: php's opens the name
/// with the arguments the call gave.
fn serve_named(req: &mut Request, name: &[u8]) -> Result<Owner<Value>, Thrown> {
    let name = archive::string(req, name)?;
    passthrough(req, Some(name))
}

/// `phar_fopen`.
fn fopen(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    if !active(req) {
        return passthrough(req, None);
    }
    let (Some(filename), Some(true), Some(use_path)) = (
        path_arg(req, 1),
        req.frames()
            .argument(2)
            .map(|m| m.deref().as_str_bytes().is_some()),
        bool_arg(req, 3),
    ) else {
        return passthrough(req, None);
    };
    if !(use_path || relative(&filename)) {
        return passthrough(req, None);
    }
    match relative_name(req, &filename, use_path)? {
        Some(name) => serve_named(req, &name),
        None => passthrough(req, None),
    }
}

/// `phar_file_get_contents` and `phar_readfile`: a relative path, or any
/// path under `$use_include_path`, that the archive being served holds,
/// served by the replaced native under its `phar://` name.
fn named_open(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    if !active(req) {
        return passthrough(req, None);
    }
    let (Some(filename), Some(use_path)) = (path_arg(req, 1), bool_arg(req, 2)) else {
        return passthrough(req, None);
    };
    if !(use_path || relative(&filename)) {
        return passthrough(req, None);
    }
    match relative_name(req, &filename, use_path)? {
        Some(name) => serve_named(req, &name),
        None => passthrough(req, None),
    }
}

fn file_get_contents(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    named_open(req)
}

fn readfile(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    named_open(req)
}

/// `phar_opendir`: a relative directory in the archive being served
/// opened as `phar://<arch>/<fixed>`, whether the archive holds it or not.
fn opendir(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    if !active(req) {
        return passthrough(req, None);
    }
    let Some(filename) = path_arg(req, 1) else {
        return passthrough(req, None);
    };
    if !relative(&filename) {
        return passthrough(req, None);
    }
    let Some(arch) = serving_archive(req)? else {
        return passthrough(req, None);
    };
    let entry = fixed(req, &filename);
    serve_named(req, &url(&arch, &entry))
}

/// `FS_*`, what a stat interceptor answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fs {
    Perms,
    Inode,
    Size,
    Owner,
    Group,
    Atime,
    Mtime,
    Ctime,
    Type,
    IsW,
    IsR,
    IsX,
    IsDir,
    Exists,
    Lstat,
    Stat,
}

impl Fs {
    /// `IS_EXISTS_CHECK`.
    fn exists_check(self) -> bool {
        matches!(self, Fs::Exists | Fs::IsW | Fs::IsR | Fs::IsX | Fs::IsDir)
    }

    /// `IS_LINK_OPERATION`.
    fn link_operation(self) -> bool {
        matches!(self, Fs::Type | Fs::Lstat)
    }
}

/// `PharFileFunction`: the replaced native where nothing is intercepted,
/// else `p` read as the native reads it and `phar_file_stat`.
fn file_function(req: &mut Request, kind: Fs) -> Result<Owner<Value>, Thrown> {
    if !req.state::<crate::State>().intercepted {
        return passthrough(req, None);
    }
    let Some(filename) = path_arg(req, 1).filter(|_| req.frames().argument_count() == 1) else {
        return passthrough(req, None);
    };
    file_stat(req, &filename, kind)
}

/// `phar_file_stat`: an entry, or a directory entries stand under, of the
/// archive being served, stated for a relative path; the replaced native
/// for any other.
fn file_stat(req: &mut Request, filename: &[u8], kind: Fs) -> Result<Owner<Value>, Thrown> {
    if filename.is_empty() {
        return Ok(Owner::bool(false));
    }
    if !relative(filename) {
        return passthrough(req, None);
    }
    let Some(arch) = serving_archive(req)? else {
        return passthrough(req, None);
    };
    let Ok(id) = archive::get_archive(req, &arch, None) else {
        return passthrough(req, None);
    };
    let entry = fixed(req, filename);
    let lookup = |req: &mut Request, entry: &[u8]| -> Option<[i64; 13]> {
        let a = archives(req).get(id)?;
        a.manifest.get(entry).map(|e| entry_stat(a, e))
    };
    let dir_stat = |req: &mut Request| -> Option<[i64; 13]> {
        let a = archives(req).get(id)?;
        let mut sb = [0_i64; 13];
        sb[2] = 0o777 | 0o040_000;
        for t in [8, 9, 10] {
            sb[t] = i64::from(a.max_timestamp);
        }
        Some(finish(a, sb, None))
    };
    let stated = if let Some(held) = entry.strip_prefix(b"/") {
        lookup(req, held)
    } else {
        match lookup(req, &entry) {
            Some(sb) => Some(sb),
            None if archives(req)
                .get(id)
                .is_some_and(|a| a.virtual_dirs.contains(&entry)) =>
            {
                if kind.exists_check() {
                    return Ok(Owner::bool(true));
                }
                dir_stat(req)
            }
            None => None,
        }
    };
    if let Some(sb) = stated {
        return fancy_stat(req, sb, kind);
    }
    // `entry + 1` of the path fixed under no directory, which cuts the
    // first byte whether or not it is the `/`.
    let bare = path::fix_filepath(filename, None);
    let held = bare.get(1..).unwrap_or_default().to_vec();
    let stated = if let Some(sb) = lookup(req, &held) {
        Some(sb)
    } else if archives(req)
        .get(id)
        .is_some_and(|a| a.virtual_dirs.contains(&held))
    {
        if kind.exists_check() {
            return Ok(Owner::bool(true));
        }
        dir_stat(req)
    } else {
        if !kind.exists_check() {
            let text = format!(
                "{}stat failed for {}",
                if kind.link_operation() { "L" } else { "" },
                lossy(filename)
            );
            docref(req, Level::Warning, &text);
        }
        return Ok(Owner::bool(false));
    };
    if kind.exists_check() {
        return Ok(Owner::bool(true));
    }
    match stated {
        Some(sb) => fancy_stat(req, sb, kind),
        None => Ok(Owner::bool(false)),
    }
}

/// `stat_entry` of `phar_file_stat`: an entry's size, permissions and
/// type, its time thrice.
fn entry_stat(a: &archive::Archive, e: &archive::Entry) -> [i64; 13] {
    let mut sb = [0_i64; 13];
    let perms = i64::from(e.flags & ENT_PERM_MASK);
    if e.is_dir {
        sb[2] = perms | 0o040_000 | if e.link.is_some() { 0o120_000 } else { 0 };
    } else {
        sb[7] = i64::from(e.uncompressed_filesize);
        sb[2] = perms | 0o100_000 | if e.link.is_some() { 0o120_000 } else { 0 };
    }
    for t in [8, 9, 10] {
        sb[t] = i64::from(e.timestamp);
    }
    finish(a, sb, Some(e))
}

/// `statme_baby`: the write bits taken off an archive that is not
/// writable, and the fields every phar stat carries.
fn finish(a: &archive::Archive, mut sb: [i64; 13], e: Option<&archive::Entry>) -> [i64; 13] {
    if !a.is_writeable {
        sb[2] = (sb[2] & 0o555) | (sb[2] & !0o777);
    }
    sb[3] = 1;
    sb[6] = -1;
    sb[0] = 0xc;
    if let Some(e) = e {
        sb[1] = i64::from(e.inode);
    }
    sb[11] = -1;
    sb[12] = -1;
    sb
}

/// `phar_fancy_stat`: what each `FS_*` answers of a stat. An access
/// check reads the owner's bits where the process's user owns the entry,
/// the group's where its group or one of its supplementary groups is the
/// entry's, and the others' otherwise; an entry's owner and group are
/// `0`, so root's process reads the owner's and one in group `0` the
/// group's.
///
/// The witness names the writable regime, since only an archive opened
/// for writing keeps its entries' write bits (`statme_baby`), and prints
/// which of the two the process is beside what it read. Whether group `0`
/// is among the process's groups is asked in a child, as ext/posix's
/// `posix_getgroups` witness asks them.
///
/// ```php
/// $p = __DIR__ . '/stat-witness.phar';
/// $files = [
///     'a.php' => '<?php Phar::interceptFileFuncs(); foreach (["u", "g", "o"] as $n) var_dump(is_writable("$n.w"), is_readable("$n.r"), is_executable("$n.x")); var_dump(fileowner("u.w"), filegroup("u.w"), posix_getuid() === 0);',
///     'u.w' => '', 'g.w' => '', 'o.w' => '', 'u.r' => '', 'g.r' => '', 'o.r' => '', 'u.x' => '', 'g.x' => '', 'o.x' => '',
/// ];
/// $perm = ['a.php' => 0644, 'u.w' => 0200, 'g.w' => 0020, 'o.w' => 0002, 'u.r' => 0400, 'g.r' => 0040, 'o.r' => 0004, 'u.x' => 0100, 'g.x' => 0010, 'o.x' => 0001];
/// $m = ''; $body = '';
/// foreach ($files as $n => $c) { $m .= pack('V', strlen($n)) . $n . pack('VVVVVV', strlen($c), 1700000000, strlen($c), crc32($c), $perm[$n], 0); $body .= $c; }
/// $m = pack('VnVV', count($files), 0x1110, 0x10000, 0) . pack('V', 0) . $m;
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . $body;
/// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
/// $phar = new Phar($p);
/// $child = proc_open([PHP_BINARY, '-n', '-r', 'var_dump(posix_getgid() === 0 || in_array(0, posix_getgroups()));'], [1 => ['pipe', 'w']], $pipes);
/// include "phar://$p/a.php";
/// echo stream_get_contents($pipes[1]);
/// unlink($p);
/// ```
/// ```expect regime=phar-writable process
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(0)
/// int(0)
/// bool(false)
/// bool(false)
/// ```
fn fancy_stat(req: &mut Request, sb: [i64; 13], kind: Fs) -> Result<Owner<Value>, Thrown> {
    const NAMES: [&[u8]; 13] = [
        b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
        b"ctime", b"blksize", b"blocks",
    ];
    let mode = sb[2];
    let is = |kind: i64| mode & 0o170_000 == kind;
    let triad = if matches!(kind, Fs::IsW | Fs::IsR | Fs::IsX) {
        access_triad(sb[4], sb[5])
    } else {
        0
    };
    Ok(match kind {
        Fs::Perms => Owner::int(mode),
        Fs::Inode => Owner::int(sb[1]),
        Fs::Size => Owner::int(sb[7]),
        Fs::Owner => Owner::int(sb[4]),
        Fs::Group => Owner::int(sb[5]),
        Fs::Atime => Owner::int(sb[8]),
        Fs::Mtime => Owner::int(sb[9]),
        Fs::Ctime => Owner::int(sb[10]),
        Fs::Type => {
            let name: &[u8] = if is(0o120_000) {
                b"link"
            } else if is(0o040_000) {
                b"dir"
            } else if is(0o100_000) {
                b"file"
            } else {
                docref(
                    req,
                    Level::Notice,
                    &format!("Unknown file type ({})", mode & 0o170_000),
                );
                b"unknown"
            };
            PhpString::new(req, name)?.into_value()
        }
        Fs::IsW => Owner::bool(mode & (0o2 << triad) != 0),
        Fs::IsR => Owner::bool(mode & (0o4 << triad) != 0),
        Fs::IsX => Owner::bool(mode & (0o1 << triad) != 0 && !is(0o040_000)),
        Fs::IsDir => Owner::bool(is(0o040_000)),
        Fs::Exists => Owner::bool(true),
        Fs::Lstat | Fs::Stat => PhpArray::filled(req, |req, out| {
            for field in sb {
                out.append(req, Owner::int(field))?;
            }
            for (name, field) in NAMES.iter().zip(sb) {
                out.set_str(req, name, Owner::int(field))?;
            }
            Ok(())
        })?
        .into_value(),
    })
}

/// The shift of the permission triad an access check reads of an entry
/// owned by `owner` and `group`, as `phar_fancy_stat` picks its masks: 6,
/// the owner's, where the process's user is `owner`; 3, the group's,
/// where its group is `group` or, asked only then, one of its
/// supplementary groups is; 0, the others', otherwise.
fn access_triad(owner: i64, group: i64) -> u32 {
    if owner == i64::from(process::uid()) {
        6
    } else if group == i64::from(process::gid())
        || process::groups()
            .unwrap_or_default()
            .into_iter()
            .any(|gid| group == i64::from(gid))
    {
        3
    } else {
        0
    }
}

/// `phar_is_file` and `phar_is_link`: whether a relative path names an
/// entry of the archive being served that is not a directory, or that is
/// a link.
fn is_entry(req: &mut Request, link: bool) -> Result<Owner<Value>, Thrown> {
    if !active(req) {
        return passthrough(req, None);
    }
    let Some(filename) = path_arg(req, 1).filter(|_| req.frames().argument_count() == 1) else {
        return passthrough(req, None);
    };
    if !relative(&filename) {
        return passthrough(req, None);
    }
    let Some(arch) = serving_archive(req)? else {
        return passthrough(req, None);
    };
    let Ok(id) = archive::get_archive(req, &arch, None) else {
        return Ok(Owner::bool(false));
    };
    let entry = fixed(req, &filename);
    let held = entry.strip_prefix(b"/").unwrap_or(&entry);
    let answer = archives(req)
        .get(id)
        .and_then(|a| a.manifest.get(held))
        .is_some_and(|e| if link { e.link.is_some() } else { !e.is_dir });
    Ok(Owner::bool(answer))
}

fn is_file(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    is_entry(req, false)
}

fn is_link(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    is_entry(req, true)
}

fn is_dir(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::IsDir)
}

fn file_exists(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Exists)
}

fn fileperms(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Perms)
}

fn fileinode(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Inode)
}

fn filesize(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Size)
}

fn fileowner(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Owner)
}

fn filegroup(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Group)
}

fn fileatime(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Atime)
}

fn filemtime(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Mtime)
}

fn filectime(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Ctime)
}

fn filetype(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Type)
}

fn is_writable(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::IsW)
}

fn is_readable(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::IsR)
}

fn is_executable(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::IsX)
}

fn lstat(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Lstat)
}

fn stat(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    file_function(req, Fs::Stat)
}
