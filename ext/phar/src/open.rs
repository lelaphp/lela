//! `phar.c`'s opening of archives: the table of open archives asked first
//! (`phar_open_parsed_phar`), then the file read and its format found
//! (`phar_open_from_fp`), a whole-file gzip decompressed on the way, and
//! the phar format's manifest parsed (`phar_parse_pharfile`) with its
//! signature verified; and the creation of a new archive in memory
//! (`phar_create_or_parse_filename`, `phar_open_or_create_filename`).
//! An error is php's message, `None` where php sets none.

use std::sync::Arc;

use php_ext::abi::{Beneath, Compiled, chain};
use php_ext::raising::Thrown;
use php_ext::request::{Ini, OwnedStream as _, Request, Streams as _};
use php_ext::values::{Owner, Value};

use crate::archive::{
    self, API_MIN_DIR, API_MIN_READ, API_VER_MASK, Archive, ArchiveId, ENT_COMPRESSED_BZ2,
    ENT_COMPRESSED_GZ, ENT_COMPRESSION_MASK, ENT_PERM_DEF_DIR, Entry, FILE_COMPRESSED_BZ2,
    FILE_COMPRESSED_GZ, FILE_COMPRESSION_MASK, Format, HDR_COMPRESSION_MASK, HDR_SIGNATURE,
    Metadata, SIG_MD5, SIG_OPENSSL, SIG_OPENSSL_SHA256, SIG_OPENSSL_SHA512, SIG_SHA1, SIG_SHA256,
    SIG_SHA512, archives, lossy,
};
use crate::path::{self, Create, Exec};

/// A failure with php's message, where it sets one.
pub(crate) type Failed = Option<String>;

/// What an open answers: the archive, or the failure.
pub(crate) type Opened = Result<ArchiveId, Failed>;

/// A verified signature: its type and its hex, or php's message.
type Verified = Result<(u32, Option<Vec<u8>>), String>;

/// `PHAR_G(readonly)`, the `phar.readonly` entry as it stands.
pub(crate) fn readonly(req: &Request) -> bool {
    req.ini().get(b"phar.readonly").is_none_or(Ini::parse_bool)
}

/// `PHAR_G(require_hash)`.
pub(crate) fn require_hash(req: &Request) -> bool {
    req.ini()
        .get(b"phar.require_hash")
        .is_none_or(Ini::parse_bool)
}

/// `PHAR_G(has_zlib)` and `has_bz2`: whether the extension is loaded.
pub(crate) fn has_zlib(req: &Request) -> bool {
    req.extensions().named(b"zlib").is_some()
}

pub(crate) fn has_bz2(req: &Request) -> bool {
    req.extensions().named(b"bz2").is_some()
}

/// `phar_open_parsed_phar`: an archive already in the name table, under
/// the name or, with no alias asked for, an alias; an executable open of
/// a tar or zip archive without a stub refused while writes are off.
pub(crate) fn open_parsed(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
    is_data: bool,
) -> Opened {
    let id = archive::get_archive(req, fname, alias)?;
    let ro = readonly(req);
    let Some(a) = archives(req).get(id) else {
        return Err(None);
    };
    if alias.is_some_and(|a| !a.is_empty()) && fname != a.fname.as_slice() {
        return Err(None);
    }
    if !is_data
        && a.halt_offset == 0
        && !a.is_brandnew
        && (a.is_tar() || a.is_zip())
        && ro
        && !a.manifest.contains(b".phar/stub.php")
    {
        return Err(Some(format!(
            "'{}' is not a phar archive. Use PharData::__construct() for a standard zip or tar archive",
            lossy(fname)
        )));
    }
    Ok(id)
}

/// `phar_open_from_filename`: the name table, then the file.
pub(crate) fn open_from_filename(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
    report: bool,
) -> Result<Opened, Thrown> {
    let is_data = path::find(fname, b".phar").is_none();
    match open_parsed(req, fname, alias, is_data) {
        Ok(id) => return Ok(Ok(id)),
        Err(Some(error)) => return Ok(Err(Some(error))),
        Err(None) => {}
    }
    if req
        .environment_mut()
        .check_open_basedir(fname, true)
        .is_err()
    {
        return Ok(Err(None));
    }
    let Some(opened) = open_file(req, fname, alias, false)? else {
        return Ok(Err(report.then(|| {
            format!("unable to open phar for reading \"{}\"", lossy(fname))
        })));
    };
    Ok(opened)
}

/// `php_stream_open_wrapper(fname, "rb", IGNORE_URL|STREAM_MUST_SEEK,
/// &actual)` and [`open_from_fp`] over what it reads, under the name it
/// opened under, the plain files wrapper's `expand_filepath`: an archive
/// the parse took keeps the stream as its `fp`, unless the whole file is
/// compressed and its `fp` is the decompressed copy. `None` where the
/// file will not open.
fn open_file(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
    warn: bool,
) -> Result<Option<Opened>, Thrown> {
    let Some(stream) = archive::open_rb(req, fname, warn)? else {
        return Ok(None);
    };
    let actual = archive::expand_filepath(req, fname).unwrap_or_else(|| fname.to_vec());
    let opened =
        archive::copy_all(req, *stream).and_then(|bytes| open_from_fp(req, bytes, &actual, alias));
    let Ok(Ok(id)) = opened else {
        stream.free(req);
        return opened.map(Some);
    };
    match archives(req).get_mut(id) {
        Some(a) if a.flags & FILE_COMPRESSION_MASK == 0 => a.fp = Some(archive::Fp::File(stream)),
        _ => stream.free(req),
    }
    Ok(Some(Ok(id)))
}

const TOKEN: &[u8] = b"__HALT_COMPILER();";
const WINDOW: usize = 1024;

/// `phar_open_from_fp`: the first window's magic read for a whole-file
/// gzip or bzip2, a zip or a tar, and otherwise the file searched in
/// 1024-byte windows for `__HALT_COMPILER();` before its manifest is
/// parsed; a window shorter than the token is php's truncated entry.
pub(crate) fn open_from_fp(
    req: &mut Request,
    mut bytes: Vec<u8>,
    fname: &[u8],
    alias: Option<&[u8]>,
) -> Result<Opened, Thrown> {
    let fail = |text: &str| Ok(Err(Some(text.replace("%s", &lossy(fname)))));
    let mut compression = 0;
    for rounds_left in (0..3).rev() {
        let first = &bytes[..bytes.len().min(WINDOW)];
        if first.len() < TOKEN.len() {
            return fail("internal corruption of phar \"%s\" (truncated entry)");
        }
        if first.starts_with(b"\x1f\x8b\x08") {
            if !has_zlib(req) {
                return fail(
                    "unable to decompress gzipped phar archive \"%s\" to temporary file, enable zlib extension in php.ini",
                );
            }
            let params = window_params(req, 15 + 32)?;
            let out = archive::filter_bytes(req, b"zlib.inflate", Some(&params), &bytes);
            params.give_up(req);
            let Some(out) = out? else {
                return fail("unable to decompress gzipped phar archive \"%s\" to temporary file");
            };
            bytes = out;
            compression = FILE_COMPRESSED_GZ;
            if rounds_left == 0 {
                return fail("unable to decompress gzipped phar archive \"%s\"");
            }
            continue;
        }
        if first.starts_with(b"BZh") {
            if !has_bz2(req) {
                return fail(
                    "unable to decompress bzipped phar archive \"%s\" to temporary file, enable bz2 extension in php.ini",
                );
            }
            let Some(out) = archive::filter_bytes(req, b"bzip2.decompress", None, &bytes)? else {
                return fail(
                    "unable to decompress bzipped phar archive \"%s\", filter creation failed",
                );
            };
            bytes = out;
            compression = FILE_COMPRESSED_BZ2;
            if rounds_left == 0 {
                return fail("unable to decompress bzipped phar archive \"%s\"");
            }
            continue;
        }
        if first.starts_with(b"PK\x03\x04") {
            return crate::zip::parse(req, bytes, fname, alias);
        }
        if first.len() >= 512 && crate::tar::is_tar(first, fname) {
            return crate::tar::parse(req, bytes, fname, alias, compression);
        }
        break;
    }
    let Some(halt) = find_halt(&bytes) else {
        return if !bytes.len().is_multiple_of(WINDOW) && bytes.len() % WINDOW < TOKEN.len() {
            fail("internal corruption of phar \"%s\" (truncated entry)")
        } else {
            fail("internal corruption of phar \"%s\" (__HALT_COMPILER(); not found)")
        };
    };
    parse_pharfile(req, bytes, fname, alias, halt, compression)
}

/// Where the first `__HALT_COMPILER();` ends, matched without case, as
/// php's windows find it: one ending past the last full window is found
/// only where that window is at least the token's length.
fn find_halt(bytes: &[u8]) -> Option<usize> {
    let start = bytes
        .windows(TOKEN.len())
        .position(|w| w.eq_ignore_ascii_case(TOKEN))?;
    let end = start + TOKEN.len();
    let tail = bytes.len() % WINDOW;
    if tail != 0 && tail < TOKEN.len() && end > bytes.len() - tail {
        return None;
    }
    Some(end)
}

/// `array("window" => $bits)`, the zlib filters' parameters.
pub(crate) fn window_params(
    req: &mut Request,
    bits: i64,
) -> Result<php_ext::values::Owner<Value>, Thrown> {
    let mut params = php_ext::values::PhpArray::new(req)?;
    params.set_str(req, b"window", Owner::int(bits))?;
    Ok(params.into_value())
}

/// A little-endian word at `at`.
pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// `phar_parse_pharfile`: the manifest after the stub, its signature
/// verified, each entry's record read, and the archive registered under
/// its name and, where it has one, its alias.
#[allow(clippy::too_many_lines)] // php's one function, its checks in its order
fn parse_pharfile(
    req: &mut Request,
    fp: Vec<u8>,
    fname: &[u8],
    alias: Option<&[u8]>,
    mut halt_offset: usize,
    compression: u32,
) -> Result<Opened, Thrown> {
    let fail = |text: &str| Ok(Err(Some(text.replace("%s", &lossy(fname)))));
    let byte = |at: usize| fp.get(at).copied();
    if fp.len() < halt_offset + 3 {
        return fail("internal corruption of phar \"%s\" (truncated manifest at stub end)");
    }
    if matches!(byte(halt_offset), Some(b' ' | b'\n'))
        && byte(halt_offset + 1) == Some(b'?')
        && byte(halt_offset + 2) == Some(b'>')
    {
        halt_offset += 3;
        match byte(halt_offset) {
            None => {
                return fail("internal corruption of phar \"%s\" (truncated manifest at stub end)");
            }
            Some(b'\r') => {
                if byte(halt_offset + 1) != Some(b'\n') {
                    return fail(
                        "internal corruption of phar \"%s\" (truncated manifest at stub end)",
                    );
                }
                halt_offset += 2;
            }
            Some(b'\n') => halt_offset += 1,
            Some(_) => {}
        }
    }
    if fp.len() < halt_offset + 4 {
        return fail("internal corruption of phar \"%s\" (truncated manifest at manifest length)");
    }
    let manifest_len = le32(&fp, halt_offset) as usize;
    if manifest_len > 1_048_576 * 100 {
        return fail("manifest cannot be larger than 100 MB in phar \"%s\"");
    }
    let start = halt_offset + 4;
    if manifest_len < 18 || fp.len() < start + manifest_len {
        return fail("internal corruption of phar \"%s\" (truncated manifest header)");
    }
    let buf = &fp[start..start + manifest_len];
    let mut at = 0;
    let manifest_count = le32(buf, at);
    at += 4;
    if manifest_count == 0 {
        return fail(
            "in phar \"%s\", manifest claims to have zero entries.  Phars must have at least 1 entry",
        );
    }
    let manifest_ver = u16::from_be_bytes([buf[at], buf[at + 1]]);
    at += 2;
    if manifest_ver & API_VER_MASK < API_MIN_READ {
        return Ok(Err(Some(format!(
            "phar \"{}\" is API version {}.{}.{}, and cannot be processed",
            lossy(fname),
            manifest_ver >> 12,
            (manifest_ver >> 8) & 0xF,
            (manifest_ver >> 4) & 0xF
        ))));
    }
    let mut manifest_flags = le32(buf, at);
    at += 4;
    manifest_flags &= !HDR_COMPRESSION_MASK;
    manifest_flags &= !FILE_COMPRESSION_MASK;
    manifest_flags |= compression;
    let (sig_flags, signature) = if manifest_flags & HDR_SIGNATURE != 0 {
        match read_signature(req, &fp, fname)? {
            Ok(found) => found,
            Err(error) => return Ok(Err(Some(error))),
        }
    } else if require_hash(req) {
        return fail("phar \"%s\" does not have a signature");
    } else {
        (0, None)
    };
    let tmp_len = le32(buf, at) as usize;
    at += 4;
    if at + tmp_len > buf.len() {
        return fail("internal corruption of phar \"%s\" (buffer overrun)");
    }
    if manifest_len < 18 + tmp_len {
        return fail("internal corruption of phar \"%s\" (truncated manifest header)");
    }
    let (alias, register_alias, temp_alias) = if tmp_len != 0 {
        let stored = &buf[at..at + tmp_len];
        if let Some(asked) = alias.filter(|a| !a.is_empty())
            && asked != stored
        {
            return Ok(Err(Some(format!(
                "cannot load phar \"{}\" with implicit alias \"{}\" under different alias \"{}\"",
                lossy(fname),
                lossy(stored),
                lossy(asked)
            ))));
        }
        at += tmp_len;
        (Some(stored.to_vec()), true, false)
    } else {
        match alias.filter(|a| !a.is_empty()) {
            Some(asked) => (Some(asked.to_vec()), true, true),
            None => (None, false, false),
        }
    };
    if manifest_count as usize > (manifest_len - 18 - tmp_len) / 21 {
        return fail(
            "internal corruption of phar \"%s\" (too many manifest entries for size of manifest)",
        );
    }
    if buf.len() < at + 4 {
        return fail("internal corruption of phar \"%s\" (truncated manifest header)");
    }
    let len = le32(buf, at) as usize;
    at += 4;
    if len > buf.len() - at {
        return fail("internal corruption of phar \"%s\" (trying to read past buffer end)");
    }
    let mut data = Archive::new(fname, Format::Phar);
    data.metadata = lazy_metadata(&buf[at..at + len]);
    at += len;
    let first_offset = i64::try_from(halt_offset + manifest_len + 4).unwrap_or(i64::MAX);
    let mut offset = first_offset;
    for _ in 0..manifest_count {
        if at + 28 > buf.len() {
            return fail("internal corruption of phar \"%s\" (truncated manifest entry)");
        }
        let filename_len = le32(buf, at) as usize;
        at += 4;
        if filename_len == 0 {
            return fail("zero-length filename encountered in phar \"%s\"");
        }
        if filename_len > buf.len() - at - 24 {
            return fail("internal corruption of phar \"%s\" (truncated manifest entry)");
        }
        let is_dir =
            manifest_ver & API_VER_MASK >= API_MIN_DIR && buf[at + filename_len - 1] == b'/';
        let raw = &buf[at..at + filename_len];
        data.add_virtual_dirs(raw);
        at += filename_len;
        let mut entry = Entry::new(if is_dir { &raw[..raw.len() - 1] } else { raw });
        entry.is_dir = is_dir;
        entry.uncompressed_filesize = le32(buf, at);
        entry.timestamp = le32(buf, at + 4);
        if offset == first_offset {
            data.min_timestamp = entry.timestamp;
            data.max_timestamp = entry.timestamp;
        } else if data.min_timestamp > entry.timestamp {
            data.min_timestamp = entry.timestamp;
        } else if data.max_timestamp < entry.timestamp {
            data.max_timestamp = entry.timestamp;
        }
        entry.compressed_filesize = le32(buf, at + 8);
        entry.crc32 = le32(buf, at + 12);
        entry.flags = le32(buf, at + 16);
        if is_dir {
            entry.flags |= ENT_PERM_DEF_DIR;
        }
        let meta_len = le32(buf, at + 20) as usize;
        at += 24;
        if meta_len > buf.len() - at {
            return fail("internal corruption of phar \"%s\" (truncated manifest entry)");
        }
        entry.metadata = lazy_metadata(&buf[at..at + meta_len]);
        at += meta_len;
        entry.offset = offset;
        entry.offset_abs = offset;
        offset += i64::from(entry.compressed_filesize);
        match entry.flags & ENT_COMPRESSION_MASK {
            ENT_COMPRESSED_GZ if !has_zlib(req) => {
                return fail("zlib extension is required for gz compressed .phar file \"%s\"");
            }
            ENT_COMPRESSED_BZ2 if !has_bz2(req) => {
                return fail("bz2 extension is required for bzip2 compressed .phar file \"%s\"");
            }
            ENT_COMPRESSED_GZ | ENT_COMPRESSED_BZ2 => {}
            _ if entry.uncompressed_filesize != entry.compressed_filesize => {
                return fail(
                    "internal corruption of phar \"%s\" (compressed and uncompressed size does not match for uncompressed entry)",
                );
            }
            _ => {}
        }
        manifest_flags |= entry.flags & ENT_COMPRESSION_MASK;
        entry.is_crc_checked = manifest_flags & HDR_SIGNATURE != 0;
        entry.set_inode(fname, 1, false);
        let name = entry.filename.clone();
        data.manifest.add(&name, entry).ok();
    }
    data.version = format!(
        "{}.{}.{}",
        manifest_ver >> 12,
        (manifest_ver >> 8) & 0xF,
        (manifest_ver >> 4) & 0xF
    );
    data.halt_offset = halt_offset;
    data.flags = manifest_flags;
    data.find_ext();
    data.alias = Some(alias.clone().unwrap_or_else(|| fname.to_vec()));
    data.sig_flags = sig_flags;
    data.signature = signature;
    data.fp = Some(archive::Fp::Written(Arc::new(fp)));
    data.is_temporary_alias = !register_alias || temp_alias;
    if register_alias {
        let alias = alias.unwrap_or_default();
        if !path::valid_alias(&alias) {
            return fail("Cannot open archive \"%s\", invalid alias");
        }
        if let Some(holder) = archives(req).by_alias(&alias)
            && !archive::free_alias(req, holder)
        {
            return fail("Cannot open archive \"%s\", alias is already in use by existing archive");
        }
        let id = archives(req).insert(data);
        archives(req).alias_map.add(&alias, id).ok();
        archives(req).fname_map.add(fname, id).ok();
        return Ok(Ok(id));
    }
    let id = archives(req).insert(data);
    archives(req).fname_map.add(fname, id).ok();
    Ok(Ok(id))
}

/// `phar_parse_metadata_lazy`: the serialised string kept, unread.
pub(crate) fn lazy_metadata(bytes: &[u8]) -> Metadata {
    Metadata {
        val: None,
        str: (!bytes.is_empty()).then(|| bytes.to_vec()),
    }
}

/// The signature at the end of the file, verified: its type and its hex,
/// or php's message.
fn read_signature(req: &mut Request, fp: &[u8], fname: &[u8]) -> Result<Verified, Thrown> {
    let name = lossy(fname).into_owned();
    let broken = || Ok(Err(format!("phar \"{name}\" has a broken signature")));
    if fp.len() < 20 + 8 || &fp[fp.len() - 4..] != b"GBMB" {
        return broken();
    }
    let sig_flags = le32(fp, fp.len() - 8);
    let (digest_len, label) = match sig_flags {
        SIG_SHA512 => (64, "SHA512"),
        SIG_SHA256 => (32, "SHA256"),
        SIG_SHA1 => (20, "SHA1"),
        SIG_MD5 => (16, "MD5"),
        SIG_OPENSSL | SIG_OPENSSL_SHA256 | SIG_OPENSSL_SHA512 => {
            let length = le32(fp, fp.len() - 12) as usize;
            let Some(end) = (fp.len() - 12).checked_sub(length).filter(|end| *end != 0) else {
                return Ok(Err(format!(
                    "phar \"{name}\" openssl signature could not be read"
                )));
            };
            let sig = &fp[end..end + length];
            return Ok(
                match crate::signature::verify(req, fp, end, sig_flags, sig, fname)? {
                    Ok(hex) => Ok((sig_flags, Some(hex))),
                    Err(error) => Err(format!(
                        "phar \"{name}\" openssl signature could not be verified: {error}"
                    )),
                },
            );
        }
        _ => {
            return Ok(Err(format!(
                "phar \"{name}\" has a broken or unsupported signature"
            )));
        }
    };
    let Some(end) = fp.len().checked_sub(8 + digest_len) else {
        return broken();
    };
    let digest = &fp[end..end + digest_len];
    Ok(
        match crate::signature::verify(req, fp, end, sig_flags, digest, fname)? {
            Ok(hex) => Ok((sig_flags, Some(hex))),
            Err(error) => Err(format!(
                "phar \"{name}\" {label} signature could not be verified: {error}"
            )),
        },
    )
}

/// `phar_create_or_parse_filename`: the file parsed where it is there,
/// and otherwise a new archive in memory, registered under its expanded
/// name, which writes are needed to create.
pub(crate) fn create_or_parse(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
    is_data: bool,
    report: bool,
) -> Result<Opened, Thrown> {
    if req
        .environment_mut()
        .check_open_basedir(fname, true)
        .is_err()
    {
        return Ok(Err(None));
    }
    if let Some(opened) = open_file(req, fname, alias, false)? {
        if let Ok(id) = opened {
            let ro = readonly(req);
            if let Some(a) = archives(req).get_mut(id)
                && (a.is_data || !ro)
            {
                a.is_writeable = true;
            }
        }
        return Ok(opened);
    }
    if readonly(req) && !is_data {
        return Ok(Err(report.then(|| {
            format!(
                "creating archive \"{}\" disabled by the php.ini setting phar.readonly",
                lossy(fname)
            )
        })));
    }
    let Some(expanded) = archive::expand_filepath(req, fname) else {
        return Ok(Err(None));
    };
    let mut data = Archive::new(&expanded, if is_data { Format::Tar } else { Format::Phar });
    data.find_ext();
    data.version = archive::API_VERSION.to_string();
    data.is_temporary_alias = alias.is_none();
    data.is_writeable = true;
    data.is_brandnew = true;
    data.is_data = is_data;
    let alias = alias.filter(|_| !is_data);
    if !is_data {
        data.alias = Some(alias.map_or_else(|| expanded.clone(), <[u8]>::to_vec));
    }
    let id = archives(req).insert(data);
    archives(req).fname_map.add(&expanded, id).ok();
    if let Some(alias) = alias.filter(|a| !a.is_empty()) {
        if let Some(holder) = archives(req).by_alias(alias)
            && !archive::free_alias(req, holder)
        {
            archive::fname_map_del(req, &expanded);
            return Ok(Err(Some(format!(
                "phar error: phar \"{}\" cannot set alias \"{}\", already in use by another phar archive",
                lossy(&expanded),
                lossy(alias)
            ))));
        }
        if archives(req).alias_map.add(alias, id).is_err() {
            archive::fname_map_del(req, &expanded);
            return Ok(Err(report.then(|| {
                format!(
                    "archive \"{}\" cannot be associated with alias \"{}\", already in use",
                    lossy(fname),
                    lossy(alias)
                )
            })));
        }
    }
    Ok(Ok(id))
}

/// `phar_open_or_create_filename`: an existing archive the name's
/// extension allows, else one created in the format the extension names.
pub(crate) fn open_or_create(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
    is_data: bool,
    report: bool,
) -> Result<Opened, Thrown> {
    let exec = if is_data {
        Exec::Data
    } else {
        Exec::Executable
    };
    let ext = match path::detect_ext(req, fname, exec, Create::No, true)? {
        Ok(found) => found,
        Err(_) => match path::detect_ext(req, fname, exec, Create::Yes, true)? {
            Ok(found) => found,
            Err(path::NoExt::Url) => {
                return Ok(Err(Some(format!(
                    "Cannot create a phar archive from a URL like \"{}\". Phar objects can only be created from local files",
                    lossy(fname)
                ))));
            }
            Err(_) => {
                return Ok(Err(Some(format!(
                    "Cannot create phar '{}', file extension (or combination) not recognised or the directory does not exist",
                    lossy(fname)
                ))));
            }
        },
    };
    match open_parsed(req, fname, alias, is_data) {
        Ok(id) => {
            let ro = readonly(req);
            let Some(a) = archives(req).get_mut(id) else {
                return Ok(Err(None));
            };
            if a.is_data && !a.is_tar() && !a.is_zip() {
                return Ok(Err(Some(format!(
                    "Cannot open '{}' as a PharData object. Use Phar::__construct() for executable archives",
                    lossy(fname)
                ))));
            }
            if ro
                && !a.is_data
                && (a.is_tar() || a.is_zip())
                && !a.manifest.contains(b".phar/stub.php")
            {
                return Ok(Err(Some(format!(
                    "'{}' is not a phar archive. Use PharData::__construct() for a standard zip or tar archive",
                    lossy(fname)
                ))));
            }
            if !ro || a.is_data {
                a.is_writeable = true;
            }
            return Ok(Ok(id));
        }
        Err(Some(error)) => return Ok(Err(Some(error))),
        Err(None) => {}
    }
    let (at, len) = ext;
    let ext = &fname[at..at + len];
    let names = |letter: u8, rest: &[u8]| {
        ext.len() > 3
            && ext
                .iter()
                .position(|b| *b == letter)
                .is_some_and(|z| ext.len() - z >= 2 && ext[z + 1..].starts_with(rest))
    };
    if names(b'z', b"ip") {
        return crate::zip::open_or_create(req, fname, alias, is_data, report);
    }
    if names(b't', b"ar") {
        return crate::tar::open_or_create(req, fname, alias, is_data, report);
    }
    create_or_parse(req, fname, alias, is_data, report)
}

/// `phar_open_executed_filename`: the file being served, as `mapPhar()`
/// reads it, which must declare `__HALT_COMPILER();`.
pub(crate) fn open_executed(
    req: &mut Request,
    alias: Option<&[u8]>,
) -> Result<Result<(), Failed>, Thrown> {
    let Some(fname) = req.frames().current_file().map(<[u8]>::to_vec) else {
        return Ok(Err(Some(
            "cannot initialize a phar outside of PHP execution".into(),
        )));
    };
    if open_parsed(req, &fname, alias, false).is_ok() {
        return Ok(Ok(()));
    }
    match php_ext::call::constant(req, b"__COMPILER_HALT_OFFSET__")? {
        Some(_) => {}
        None => {
            return Ok(Err(Some(
                "__HALT_COMPILER(); must be declared in a phar".into(),
            )));
        }
    }
    if req
        .environment_mut()
        .check_open_basedir(&fname, true)
        .is_err()
    {
        return Ok(Err(None));
    }
    let Some(opened) = open_file(req, &fname, alias, true)? else {
        return Ok(Err(Some(format!(
            "unable to open phar for reading \"{}\"",
            lossy(&fname)
        ))));
    };
    Ok(opened.map(|_| ()))
}

/// `phar_compile_file`, phar's `zend_compile_file`: a file whose name
/// says `.phar`, and no `://`, opened as an archive; a tar or zip one
/// hands the one it saved, `phar_orig_compile_file`, its
/// `.phar/stub.php`, a whole-file compressed one its decompressed bytes,
/// to compile under the file's own name, and any other file is left to
/// it as it is. The witness includes a tar-based archive built by
/// hand, which the recipe's `phar.readonly` forbids `Phar` to write.
///
/// ```php
/// function member($name, $data) {
///     $h = pack('a100a8a8a8a12a12a8a1a100a6a2a247', $name, '0000644', '', '', sprintf('%011o', strlen($data)), '00000000000', '        ', '0', '', 'ustar', '00', '');
///     $h = substr_replace($h, sprintf('%06o', array_sum(array_map('ord', str_split($h)))) . "\0 ", 148, 8);
///     return $h . str_pad($data, (strlen($data) + 511) & ~511, "\0");
/// }
/// $t = __DIR__ . '/compile-witness.phar.tar';
/// $tar = member('.phar/stub.php', '<?php echo "tar stub in ", basename(__FILE__), "\n"; __HALT_COMPILER();');
/// $tar .= member('.phar/signature.bin', pack('VV', 2, 20) . sha1($tar, true)) . str_repeat("\0", 1024);
/// file_put_contents($t, $tar);
/// include $t;
/// unlink($t);
/// ```
/// ```expect
/// tar stub in compile-witness.phar.tar
/// ```
pub(crate) fn compile_file(
    req: &mut Request,
    beneath: Beneath<'_, chain::CompileFile>,
) -> Result<Option<Compiled>, Thrown> {
    let path = beneath.path();
    match archive_source(req, path)? {
        Some(Stub {
            source,
            over_archive,
        }) => {
            let beneath = beneath.with_source(&source);
            if over_archive {
                beneath.opened(path).serve(req)
            } else {
                beneath.serve(req)
            }
        }
        None => beneath.serve(req),
    }
}

/// What [`compile_file`] hands the one beneath for an archive: the source
/// to compile, and whether the stream it read stands over the archive
/// file itself, an uncompressed tar or zip one's `phar->fp`, which the
/// `zend_file_handle` it replaces then holds open; a compressed one's
/// reads a decompressed copy.
struct Stub {
    source: Vec<u8>,
    over_archive: bool,
}

/// The stub [`compile_file`] compiles the archive at `path` from, or
/// `None` where it is no archive phar serves.
fn archive_source(req: &mut Request, path: &[u8]) -> Result<Option<Stub>, Thrown> {
    if path::find(path, b".phar").is_none() || path::find(path, b"://").is_some() {
        return Ok(None);
    }
    let Ok(id) = open_from_filename(req, path, None, false)? else {
        return Ok(None);
    };
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    let compressed = a.flags & FILE_COMPRESSION_MASK != 0;
    if a.is_tar() || a.is_zip() {
        let url = [b"phar://".as_slice(), path, b"/.phar/stub.php"].concat();
        let opening = php_ext::request::Opening {
            use_path: true,
            warn: true,
            for_include: true,
            ..php_ext::request::Opening::default()
        };
        let Some(stream) = req.open_stream_with(&url, b"rb", opening)? else {
            return Ok(None);
        };
        let source = archive::copy_all(req, *stream);
        stream.free(req);
        return Ok(Some(Stub {
            source: source?,
            over_archive: !compressed,
        }));
    }
    if compressed {
        let source = a.fp.as_ref().and_then(|fp| match fp {
            archive::Fp::Written(bytes) => Some(bytes.to_vec()),
            archive::Fp::File(_) => None,
        });
        return Ok(source.map(|source| Stub {
            source,
            over_archive: false,
        }));
    }
    Ok(None)
}
