//! `zip.c`: a zip archive read from its central directory
//! (`phar_parse_zipfile`), opened or created as one
//! (`phar_open_or_create_zip`), and written (`phar_zip_flush`), each
//! entry's permissions in an `nu` extra field and its `metadata` in its
//! file comment, the archive's in the zip comment.

use std::sync::Arc;

use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::archive::{
    self, Archive, ArchiveId, Content, ENT_COMPRESSED_BZ2, ENT_COMPRESSED_GZ, ENT_COMPRESSION_MASK,
    ENT_PERM_DEF_DIR, ENT_PERM_DEF_FILE, ENT_PERM_MASK, Entry, Format, Metadata, SIG_SHA256,
    archives, lossy,
};
use crate::entry;
use crate::ffi;
use crate::open::{self, Failed, Opened, has_bz2, has_zlib, require_hash};
use crate::path;
use crate::write;

/// `sizeof(phar_zip_dir_end)`, `sizeof(phar_zip_file_header)`,
/// `sizeof(phar_zip_central_dir_file)` and `sizeof(phar_zip_unix3)`.
const EOCD: usize = 22;
const LOCAL: usize = 30;
const CENTRAL: usize = 46;
const UNIX3: usize = 18;

/// The stub a new executable zip archive carries.
const NEWSTUB: &[u8] = b"<?php // zip-based phar archive stub file\n__HALT_COMPILER();";

fn get16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn get32(b: &[u8], at: usize) -> u32 {
    open::le32(b, at)
}

fn set16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn set32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// `phar_zip_d2u_time`: a DOS time and date as `mktime` reads them in
/// the host's local time, over the broken-down current time so the
/// fields it does not set (the daylight-saving field among them) are
/// now's; the answer truncated to the entry's 32-bit timestamp as C's
/// conversion truncates it.
fn d2u_time(dtime: u16, ddate: u16) -> u32 {
    let (dtime, ddate) = (i32::from(dtime), i32::from(ddate));
    let Some(mut tm) = ffi::local_time(i64::from(write::now())) else {
        return 0;
    };
    tm.tm_year = ((ddate >> 9) & 127) + 1980 - 1900;
    tm.tm_mon = ((ddate >> 5) & 15) - 1;
    tm.tm_mday = ddate & 31;
    tm.tm_hour = (dtime >> 11) & 31;
    tm.tm_min = (dtime >> 5) & 63;
    tm.tm_sec = (dtime << 1) & 0x3E;
    u32::try_from(ffi::make_time(&mut tm).rem_euclid(1 << 32)).unwrap_or(0)
}

/// `phar_zip_u2d_time`: a time as a DOS time and date in the host's
/// local time, 1980-01-01 for any earlier one.
///
/// The recipe's `TZ` is `UTC` and this engine's `putenv` is the
/// request's alone, so the witness runs `PHP_BINARY` under each `TZ`,
/// which is why it is served by the binary (`process`): a zip
/// written from a tar whose times are fixed, its bytes and its first
/// entry's DOS time and date under each zone, and one written under
/// `UTC` read back under another. `mktime` reads the daylight-saving
/// field of the current time, as php leaves it, so the zone read under
/// keeps none and the reading holds in every season.
///
/// ```php
/// file_put_contents(__DIR__ . '/zip-time.php', <<<'PHP'
/// <?php
/// function member($name, $data, $mtime) {
///     $h = pack('a100a8a8a8a12a12a8a1a100a6a2a247', $name, '0000644', '', '', sprintf('%011o', strlen($data)), sprintf('%011o', $mtime), '        ', '0', '', 'ustar', '00', '');
///     $h = substr_replace($h, sprintf('%06o', array_sum(array_map('ord', str_split($h)))) . "\0 ", 148, 8);
///     return $h . str_pad($data, (strlen($data) + 511) & ~511, "\0");
/// }
/// $zip = __DIR__ . '/zip-time.zip';
/// if ($argv[1] === 'write') {
///     file_put_contents(__DIR__ . '/zip-time.tar', member('summer.txt', 'hi', 645446096) . member('winter.txt', 'yo', 632827696) . str_repeat("\0", 1024));
///     (new PharData(__DIR__ . '/zip-time.tar'))->convertToData(Phar::ZIP);
///     unlink(__DIR__ . '/zip-time.tar');
///     $bytes = file_get_contents($zip);
///     echo md5($bytes), ' ', bin2hex(substr($bytes, 10, 4)), "\n";
/// } else {
///     foreach (new PharData($zip) as $f) $read[] = $f->getFilename() . ' ' . date('Y-m-d H:i:s', $f->getMTime());
///     echo implode('; ', $read), "\n";
/// }
/// PHP);
/// foreach ([['UTC', 'write'], ['Asia/Kolkata', 'read'], ['Europe/Amsterdam', 'write'], ['America/New_York', 'write']] as [$tz, $act]) {
///     if ($act === 'write') @unlink(__DIR__ . '/zip-time.zip');
///     $h = proc_open([PHP_BINARY, '-n', __DIR__ . '/zip-time.php', $act], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes, null, ['TZ' => $tz]);
///     echo "$tz $act: ", stream_get_contents($pipes[1]), stream_get_contents($pipes[2]);
///     proc_close($h);
/// }
/// unlink(__DIR__ . '/zip-time.zip');
/// unlink(__DIR__ . '/zip-time.php');
/// ```
/// ```expect process
/// UTC write: 9571c14880f7d738c64cd5db82f0eca0 5c54cf14
/// Asia/Kolkata read: summer.txt 1990-06-15 05:04:56; winter.txt 1990-01-20 03:58:16
/// Europe/Amsterdam write: 83eb9278b589f9e0013d39074d7af178 5c64cf14
/// America/New_York write: 16a684fe1046f279308e9ab41bb929fe 5c34cf14
/// ```
fn u2d_time(time: u32) -> (u16, u16) {
    match ffi::local_time(i64::from(time)) {
        Some(tm) if tm.tm_year >= 80 => {
            let cdate = ((tm.tm_year + 1900 - 1980) << 9) + ((tm.tm_mon + 1) << 5) + tm.tm_mday;
            let ctime = (tm.tm_hour << 11) + (tm.tm_min << 5) + (tm.tm_sec >> 1);
            (
                u16::try_from(ctime & 0xFFFF).unwrap_or(0),
                u16::try_from(cdate & 0xFFFF).unwrap_or(0),
            )
        }
        _ => (0, (1 << 5) + 1),
    }
}

/// `phar_find_eocd`: the end of central directory record, searched
/// backwards for one whose comment ends the buffer.
fn find_eocd(buf: &[u8]) -> Option<usize> {
    let mut end = (buf.len() + 4).checked_sub(EOCD)?;
    loop {
        let start = buf[..end.min(buf.len())]
            .windows(4)
            .rposition(|w| w == b"PK\x05\x06")?;
        if start + EOCD <= buf.len() {
            let comment = usize::from(get16(buf, start + 20));
            if start + EOCD + comment == buf.len() {
                return Some(start);
            }
        }
        end = start + 3;
        if start == 0 {
            return None;
        }
    }
}

/// `phar_zip_process_extra`: the extra fields of a central record read,
/// `UT`'s modification time and `nu`'s permissions taken; `false` for
/// fields that do not fit.
fn process_extra(extra: &[u8], e: &mut Entry) -> bool {
    let mut at = 0;
    let mut len = extra.len();
    while len != 0 {
        if len < 4 {
            return false;
        }
        let (tag, size) = (
            [extra[at], extra[at + 1]],
            usize::from(get16(extra, at + 2)),
        );
        at += 4;
        len -= 4;
        if size > len {
            return false;
        }
        if tag == *b"UT" && size >= 5 {
            if extra[at] & 1 != 0 {
                e.timestamp = get32(extra, at + 1);
            }
            at += size;
            len -= size;
            continue;
        }
        if tag != *b"nu" {
            at += size;
            len -= size;
            continue;
        }
        let unix3 = UNIX3 - 4;
        let field = if size == unix3 - 4 { unix3 } else { size };
        if field < unix3 || field > len {
            return false;
        }
        let perms = u32::from(get16(extra, at + 4));
        at += field;
        len -= field;
        e.flags &= ENT_COMPRESSION_MASK;
        e.flags |= perms & ENT_PERM_MASK;
    }
    true
}

/// `phar_open_or_create_zip`: the archive opened or created, which must be
/// a zip one where it was there already.
pub(crate) fn open_or_create(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
    is_data: bool,
    report: bool,
) -> Result<Opened, Thrown> {
    let id = match open::create_or_parse(req, fname, alias, is_data, report)? {
        Ok(id) => id,
        Err(error) => return Ok(Err(error)),
    };
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(Err(None));
    };
    a.is_data = is_data;
    if a.is_zip() {
        return Ok(Ok(id));
    }
    if a.is_brandnew {
        a.format = Format::Zip;
        return Ok(Ok(id));
    }
    Ok(Err(Some(format!(
        "phar zip error: phar \"{}\" already exists as a regular phar and must be deleted from disk prior to creating as a zip-based phar",
        lossy(fname)
    ))))
}

/// An alias's contents read from the archive, decompressed as its flags
/// say, at most its size.
fn read_alias(
    req: &mut Request,
    bytes: &[u8],
    e: &Entry,
) -> Result<Result<Vec<u8>, &'static str>, Thrown> {
    let at = usize::try_from(e.offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let want = e.uncompressed_filesize as usize;
    let filter: Option<&[u8]> = if e.flags & ENT_COMPRESSED_GZ != 0 {
        Some(b"zlib.inflate")
    } else if e.flags & ENT_COMPRESSED_BZ2 != 0 {
        Some(b"bzip2.decompress")
    } else {
        None
    };
    let out = match filter {
        None => bytes[at..at.saturating_add(want).min(bytes.len())].to_vec(),
        Some(name) => match archive::filter_bytes(req, name, None, &bytes[at..])? {
            Some(mut out) => {
                out.truncate(want);
                out
            }
            None if e.flags & ENT_COMPRESSED_GZ != 0 => {
                return Ok(Err(
                    "unable to decompress alias, zlib filter creation failed",
                ));
            }
            None => return Ok(Err("unable to read in alias, bzip2 filter creation failed")),
        },
    };
    if out.is_empty() {
        return Ok(Err("unable to read in alias, truncated"));
    }
    Ok(Ok(out))
}

/// `phar_parse_zipfile`: the end record found in the last 64 KiB, the
/// central directory read entry by entry against each file's local
/// header, the signature verified, and the archive registered.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
pub(crate) fn parse(
    req: &mut Request,
    bytes: Vec<u8>,
    fname: &[u8],
    alias: Option<&[u8]>,
) -> Result<Opened, Thrown> {
    let name = lossy(fname).into_owned();
    let window = EOCD + 65536;
    let base = bytes.len().saturating_sub(window);
    let Some(found) = find_eocd(&bytes[base..]) else {
        return Ok(Err(Some(format!(
            "phar error: end of central directory not found in zip-based phar \"{name}\""
        ))));
    };
    let eocd = base + found;
    let rec = &bytes[eocd..eocd + EOCD];
    if get16(rec, 6) != 0 || get16(rec, 4) != 0 {
        return Ok(Err(Some(format!(
            "phar error: split archives spanning multiple zips cannot be processed in zip-based phar \"{name}\""
        ))));
    }
    if get16(rec, 8) != get16(rec, 10) {
        return Ok(Err(Some(format!(
            "phar error: corrupt zip archive, conflicting file count in end of central directory record in zip-based phar \"{name}\""
        ))));
    }
    let count = get16(rec, 10);
    let (cdir_offset, comment_len) = (get32(rec, 16) as usize, usize::from(get16(rec, 20)));
    let comment = &bytes[eocd + EOCD..];
    let mut phar = Archive::new(fname, Format::Zip);
    if comment_len != 0 {
        phar.metadata = open::lazy_metadata(&comment[..comment_len.min(comment.len())]);
    }
    phar.find_ext();
    phar.version = archive::API_VERSION.to_string();
    let fail = |phar: &mut Archive, req: &mut Request, text: &str| {
        phar.release_all(req);
        Ok(Err(Some(format!(
            "phar error: {text} in zip-based phar \"{name}\""
        ))))
    };
    let mut actual_alias: Option<Vec<u8>> = None;
    let mut alias_len = 0;
    let mut cur = cdir_offset;
    for i in 0..count {
        let beforeus = cur;
        let Some(zipentry) = bytes.get(cur..cur + CENTRAL) else {
            return fail(
                &mut phar,
                req,
                "unable to read central directory entry, truncated",
            );
        };
        cur += CENTRAL;
        if &zipentry[..4] != b"PK\x01\x02" {
            return fail(
                &mut phar,
                req,
                "corrupted central directory entry, no magic signature",
            );
        }
        let mut e = Entry::new(b"");
        e.compressed_filesize = get32(zipentry, 20);
        e.uncompressed_filesize = get32(zipentry, 24);
        e.crc32 = get32(zipentry, 16);
        e.timestamp = d2u_time(get16(zipentry, 12), get16(zipentry, 14));
        e.flags = ENT_PERM_DEF_FILE;
        e.header_offset = i64::from(get32(zipentry, 42));
        e.content = Content::Fp;
        if get16(zipentry, 8) & 1 != 0 {
            return fail(&mut phar, req, "Cannot process encrypted zip files");
        }
        let filename_len = usize::from(get16(zipentry, 28));
        if filename_len == 0 {
            return fail(
                &mut phar,
                req,
                "Cannot process zips created from stdin (zero-length filename)",
            );
        }
        let Some(raw_name) = bytes.get(cur..cur + filename_len) else {
            return fail(
                &mut phar,
                req,
                "unable to read in filename from central directory, truncated",
            );
        };
        cur += filename_len;
        let mut filename = raw_name.to_vec();
        if filename.last() == Some(&b'/') {
            e.is_dir = true;
            if filename.len() > 1 {
                filename.pop();
            }
            e.flags |= ENT_PERM_DEF_DIR;
        }
        e.filename.clone_from(&filename);
        let header_offset = usize::try_from(e.header_offset).unwrap_or(usize::MAX);
        let extra_len = usize::from(get16(zipentry, 30));
        let mut local: Option<&[u8]> = None;
        if e.is_dir {
            e.offset =
                e.header_offset + i64::try_from(LOCAL + filename.len() + extra_len).unwrap_or(0);
        } else {
            let Some(header) = bytes.get(header_offset..header_offset.saturating_add(LOCAL)) else {
                return fail(
                    &mut phar,
                    req,
                    "phar error: internal corruption (cannot read local file header)",
                );
            };
            if filename.len() != usize::from(get16(header, 26)) {
                return fail(
                    &mut phar,
                    req,
                    "phar error: internal corruption (local file header does not match central directory)",
                );
            }
            let past = LOCAL + filename.len() + usize::from(get16(header, 28));
            e.offset = e.header_offset + i64::try_from(past).unwrap_or(0);
            local = Some(header);
        }
        e.offset_abs = e.offset;
        if filename == b".phar/signature.bin" {
            if e.uncompressed_filesize > 0x10000 {
                return fail(
                    &mut phar,
                    req,
                    "signatures larger than 64 KiB are not supported",
                );
            }
            let mut hashed = bytes[..header_offset.min(bytes.len())].to_vec();
            hashed.extend_from_slice(&bytes[cdir_offset.min(beforeus)..beforeus]);
            if comment_len != 0 {
                hashed.extend_from_slice(&comment[..comment_len.min(comment.len())]);
            }
            let at = usize::try_from(e.offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let sig = &bytes[at..at
                .saturating_add(e.uncompressed_filesize as usize)
                .min(bytes.len())];
            if sig.len() != e.uncompressed_filesize as usize || sig.len() <= 8 {
                return fail(&mut phar, req, "signature cannot be read");
            }
            phar.sig_flags = get32(sig, 0);
            match crate::signature::verify(
                req,
                &hashed,
                hashed.len(),
                phar.sig_flags,
                &sig[8..],
                fname,
            )? {
                Ok(hex) => phar.signature = Some(hex),
                Err(error) => {
                    let mut text = format!("signature cannot be verified: {error}");
                    if text.len() > 127 {
                        let mut cut = 127;
                        while !text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        text.truncate(cut);
                    }
                    return fail(&mut phar, req, &text);
                }
            }
            if i != count - 1 {
                return fail(
                    &mut phar,
                    req,
                    "entries exist after signature, invalid phar",
                );
            }
            cur += extra_len;
            cur += usize::from(get16(zipentry, 32));
            continue;
        }
        phar.add_virtual_dirs(&filename);
        if extra_len != 0 {
            let extra = bytes.get(cur..cur + extra_len).unwrap_or_default();
            if extra.len() != extra_len || !process_extra(extra, &mut e) {
                return fail(
                    &mut phar,
                    req,
                    "Unable to process extra field header for file in central directory",
                );
            }
            cur += extra_len;
        }
        match get16(zipentry, 10) {
            0 => {}
            8 => {
                e.flags |= ENT_COMPRESSED_GZ;
                if !has_zlib(req) {
                    return fail(&mut phar, req, "zlib extension is required");
                }
            }
            12 => {
                e.flags |= ENT_COMPRESSED_BZ2;
                if !has_bz2(req) {
                    return fail(&mut phar, req, "bzip2 extension is required");
                }
            }
            method => {
                let what = match method {
                    1 => "Shrunk",
                    2..=5 => "Reduce",
                    6 => "Implode",
                    7 => "Tokenize",
                    9 => "Deflate64",
                    10 => "PKWare Implode/old IBM TERSE",
                    14 => "LZMA",
                    18 => "IBM TERSE",
                    19 => "IBM LZ77",
                    97 => "WavPack",
                    98 => "PPMd",
                    _ => "unknown",
                };
                return fail(
                    &mut phar,
                    req,
                    &format!("unsupported compression method ({what}) used in this zip"),
                );
            }
        }
        let comment_of = usize::from(get16(zipentry, 32));
        if comment_of != 0 {
            let Some(text) = bytes.get(cur..cur + comment_of) else {
                return fail(&mut phar, req, "unable to read in file comment, truncated");
            };
            e.metadata = open::lazy_metadata(text);
            cur += comment_of;
        }
        if actual_alias.is_none() && filename == b".phar/alias.txt" {
            let header = local.unwrap_or_default();
            if header.len() < LOCAL
                || e.crc32 != get32(header, 14)
                || e.uncompressed_filesize != get32(header, 22)
                || e.compressed_filesize != get32(header, 18)
            {
                e.free(req);
                return fail(
                    &mut phar,
                    req,
                    "phar error: internal corruption of zip-based phar (local header of alias does not match central directory)",
                );
            }
            alias_len = e.uncompressed_filesize as usize;
            match read_alias(req, &bytes, &e)? {
                Ok(read) => {
                    e.uncompressed_filesize = u32::try_from(read.len()).unwrap_or(u32::MAX);
                    actual_alias = Some(read);
                }
                Err(text) => {
                    e.free(req);
                    return fail(&mut phar, req, text);
                }
            }
        }
        e.set_inode(fname, 1, false);
        if let Err(refused) = phar.manifest.add(&filename, e) {
            refused.free(req);
        }
    }
    phar.is_data = !phar.manifest.contains(b".phar/stub.php");
    if !phar.is_data && require_hash(req) && phar.signature.is_none() {
        return fail(&mut phar, req, "signature is missing");
    }
    phar.fp = Some(archive::Fp::opened(bytes, phar.flags));
    let id = archives(req).insert(phar);
    archives(req).fname_map.add(fname, id).ok();
    let refuse = |req: &mut Request, text: String| {
        archive::fname_map_del(req, fname);
        Ok(Err(Some(text)))
    };
    if let Some(mut actual) = actual_alias {
        actual.truncate(alias_len);
        if !path::valid_alias(&actual) {
            let shown = lossy(php_ext::coerce::c_str(&actual)).into_owned();
            return refuse(
                req,
                format!("phar error: invalid alias \"{shown}\" in zip-based phar \"{name}\""),
            );
        }
        if let Some(holder) = archives(req).by_alias(&actual)
            && !archive::free_alias(req, holder)
        {
            return refuse(
                req,
                format!(
                    "phar error: Unable to add zip-based phar \"{name}\" with implicit alias, alias is already in use"
                ),
            );
        }
        archives(req).alias_map.add(&actual, id).ok();
        if let Some(a) = archives(req).get_mut(id) {
            a.alias = Some(actual);
            a.is_temporary_alias = false;
        }
    } else {
        let given = alias.filter(|a| !a.is_empty());
        if let Some(given) = given {
            if let Some(holder) = archives(req).by_alias(given)
                && !archive::free_alias(req, holder)
            {
                return refuse(
                    req,
                    format!(
                        "phar error: Unable to add zip-based phar \"{name}\" with explicit alias, alias is already in use"
                    ),
                );
            }
            archives(req).alias_map.add(given, id).ok();
        }
        if let Some(a) = archives(req).get_mut(id) {
            a.alias = Some(given.map_or_else(|| fname.to_vec(), <[u8]>::to_vec));
            a.is_temporary_alias = true;
        }
    }
    Ok(Ok(id))
}

/// One entry as `phar_zip_changed_apply_int` writes it: its local header,
/// name, `nu` field and contents to `files`, its central record, name,
/// `nu` field and comment to `central`.
struct Pass {
    files: Vec<u8>,
    central: Vec<u8>,
}

/// What an entry is written from.
enum Body {
    /// The contents to write whole, their crc and sizes computed here,
    /// compressed where the flags say.
    Fresh(Vec<u8>),
    /// The compressed bytes as they stand in the old archive, the sizes
    /// and crc kept.
    Kept(Vec<u8>),
    /// A directory: no contents.
    Dir,
}

/// The fields of the entry the headers carry.
struct Row<'a> {
    filename: &'a [u8],
    is_dir: bool,
    flags: u32,
    timestamp: u32,
    uncompressed: u32,
    compressed: u32,
    crc32: u32,
    comment: Option<&'a [u8]>,
}

/// `phar_zip_changed_apply_int`'s writes for one entry; the offset of its
/// contents.
fn write_row(pass: &mut Pass, row: &Row<'_>, contents: &[u8]) -> i64 {
    let mut local = [0_u8; LOCAL];
    let mut central = [0_u8; CENTRAL];
    let mut perms = [0_u8; UNIX3];
    local[..4].copy_from_slice(b"PK\x03\x04");
    central[..4].copy_from_slice(b"PK\x01\x02");
    set16(&mut central, 30, UNIX3 as u16);
    set16(&mut local, 28, UNIX3 as u16);
    perms[0] = b'n';
    perms[1] = b'u';
    set16(&mut perms, 2, (UNIX3 - 4) as u16);
    set16(&mut perms, 8, (row.flags & ENT_PERM_MASK) as u16);
    let crc = !php_ext_standard::crc32::update(u32::MAX, &perms[8..10]);
    set32(&mut perms, 4, crc);
    let method = if row.flags & ENT_COMPRESSED_BZ2 != 0 {
        12
    } else if row.flags & ENT_COMPRESSED_GZ != 0 {
        8
    } else {
        0
    };
    set16(&mut central, 10, method);
    set16(&mut local, 8, method);
    let (dtime, ddate) = u2d_time(row.timestamp);
    set16(&mut local, 10, dtime);
    set16(&mut local, 12, ddate);
    set16(&mut central, 12, dtime);
    set16(&mut central, 14, ddate);
    let name_len = (row.filename.len() + usize::from(row.is_dir)) as u16;
    set16(&mut central, 28, name_len);
    set16(&mut local, 26, name_len);
    set16(&mut central, 8, 1 << 11);
    set16(&mut local, 6, 1 << 11);
    set32(&mut central, 42, pass.files.len() as u32);
    if !row.is_dir {
        set32(&mut central, 16, row.crc32);
        set32(&mut local, 14, row.crc32);
        set32(&mut central, 20, row.compressed);
        set32(&mut local, 18, row.compressed);
        set32(&mut central, 24, row.uncompressed);
        set32(&mut local, 22, row.uncompressed);
    }
    if let Some(comment) = row.comment {
        set16(&mut central, 32, comment.len() as u16);
    }
    let header_offset = pass.files.len();
    let offset = header_offset + LOCAL + usize::from(name_len) + UNIX3;
    pass.files.extend_from_slice(&local);
    pass.central.extend_from_slice(&central);
    pass.files.extend_from_slice(row.filename);
    pass.central.extend_from_slice(row.filename);
    if row.is_dir {
        pass.files.push(b'/');
        pass.central.push(b'/');
    }
    pass.files.extend_from_slice(&perms);
    pass.central.extend_from_slice(&perms);
    pass.files.extend_from_slice(contents);
    if let Some(comment) = row.comment {
        pass.central.extend_from_slice(comment);
    }
    i64::try_from(offset).unwrap_or(i64::MAX)
}

/// An entry's contents compressed by its flags, or php's message.
fn compressed(
    req: &mut Request,
    flags: u32,
    data: &[u8],
    filename: &[u8],
    fname: &str,
) -> Result<Result<Vec<u8>, String>, Thrown> {
    let (filter, what): (&[u8], &str) = if flags & ENT_COMPRESSED_GZ != 0 {
        (b"zlib.deflate", "gzip")
    } else {
        (b"bzip2.compress", "bzip2")
    };
    Ok(match archive::filter_bytes(req, filter, None, data)? {
        Some(out) => Ok(out),
        None => Err(format!(
            "unable to {what} compress file \"{}\" to zip-based phar \"{fname}\"",
            lossy(filename)
        )),
    })
}

/// The magic entry a flush lays in the manifest.
fn magic(filename: &[u8], contents: Vec<u8>, compressed: u32, timestamp: u32) -> Entry {
    let mut e = Entry::new(filename);
    e.flags = ENT_PERM_DEF_FILE;
    e.timestamp = timestamp;
    e.is_modified = true;
    e.uncompressed_filesize = u32::try_from(contents.len()).unwrap_or(u32::MAX);
    e.compressed_filesize = compressed;
    e.content = Content::Mod(contents);
    e
}

/// The archive's own `metadata`, serialised where only its value is held.
fn archive_metadata(req: &mut Request, id: ArchiveId) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(mut meta) = archives(req)
        .get_mut(id)
        .map(|a| std::mem::take(&mut a.metadata))
    else {
        return Ok(None);
    };
    let text = if meta.has_data() {
        write::serialized(req, &mut meta)?
    } else {
        None
    };
    if let Some(a) = archives(req).get_mut(id) {
        a.metadata = meta;
    }
    Ok(text)
}

/// An entry's own `metadata`, serialised where only its value is held.
fn entry_metadata(
    req: &mut Request,
    id: ArchiveId,
    slot: usize,
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(mut meta) = archives(req)
        .get_mut(id)
        .and_then(|a| a.manifest.at_mut(slot))
        .map(|e| std::mem::take(&mut e.metadata))
    else {
        return Ok(None);
    };
    let text = if meta.has_data() {
        write::serialized(req, &mut meta)?
    } else {
        None
    };
    if let Some(e) = archives(req)
        .get_mut(id)
        .and_then(|a| a.manifest.at_mut(slot))
    {
        e.metadata = meta;
    } else {
        Metadata::free(&mut meta, req);
    }
    Ok(text)
}

/// `phar_zip_flush`: the magic alias and stub entries set, every entry
/// written with its central record, the signature entry last, then the
/// central directory and the end record carrying the archive's
/// `metadata` as the zip comment.
#[allow(clippy::too_many_lines)] // php's one function, its steps in its order
pub(crate) fn flush(
    req: &mut Request,
    id: ArchiveId,
    user_stub: Option<&[u8]>,
    is_default_stub: bool,
) -> Result<Failed, Thrown> {
    let timestamp = write::now();
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    let name = lossy(&a.fname).into_owned();
    if !a.is_data {
        let mut dropped = Vec::new();
        let alias = a
            .alias
            .clone()
            .filter(|alias| !a.is_temporary_alias && !alias.is_empty());
        let mut compressed_len = 0;
        match &alias {
            Some(alias) => {
                compressed_len = u32::try_from(alias.len()).unwrap_or(u32::MAX);
                let e = magic(b".phar/alias.txt", alias.clone(), compressed_len, timestamp);
                dropped.extend(a.manifest.update(b".phar/alias.txt", e).1);
            }
            None => dropped.extend(a.manifest.remove(b".phar/alias.txt")),
        }
        let (fname, registered) = (a.fname.clone(), a.alias.clone().filter(|a| !a.is_empty()));
        for e in dropped {
            e.free(req);
        }
        if let Some(registered) = registered
            && let Err(error) = archive::get_archive(req, &fname, Some(&registered))
        {
            return Ok(Some(error.unwrap_or_default()));
        }
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(None);
        };
        let mut dropped = Vec::new();
        if let Some(stub) = user_stub.filter(|_| !is_default_stub) {
            const HALT: &[u8] = b"__HALT_COMPILER();";
            let Some(pos) = stub
                .windows(HALT.len())
                .position(|w| w.eq_ignore_ascii_case(HALT))
            else {
                return Ok(Some(format!("illegal stub for zip-based phar \"{name}\"")));
            };
            let contents = [&stub[..pos + HALT.len()], b" ?>\r\n"].concat();
            let e = magic(b".phar/stub.php", contents, compressed_len, timestamp);
            dropped.extend(a.manifest.update(b".phar/stub.php", e).1);
        } else {
            let e = magic(
                b".phar/stub.php",
                NEWSTUB.to_vec(),
                NEWSTUB.len() as u32,
                timestamp,
            );
            if is_default_stub {
                dropped.extend(a.manifest.update(b".phar/stub.php", e).1);
            } else if !a.manifest.contains(b".phar/stub.php") {
                a.manifest.add(b".phar/stub.php", e).ok();
            }
        }
        for e in dropped {
            e.free(req);
        }
    }
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    if !a.is_data && a.sig_flags == 0 {
        a.sig_flags = SIG_SHA256;
    }
    let signing = a.sig_flags != 0;
    let count = a.manifest.len() + usize::from(signing);
    let old =
        a.fp.as_ref()
            .map(|fp| Arc::clone(&fp.bytes))
            .filter(|_| !a.is_brandnew);
    let fname = a.fname.clone();
    let old = match old {
        Some(old) => Some(old),
        None => archive::read_file(req, &fname, false)?.map(Arc::new),
    };
    let mut pass = Pass {
        files: Vec::new(),
        central: Vec::new(),
    };
    let slots = archives(req)
        .get(id)
        .map(|a| a.manifest.slots())
        .unwrap_or_default();
    let fail = |text: String| Ok(Some(format!("phar zip flush of \"{name}\" failed: {text}")));
    for slot in slots {
        let Some(e) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
            continue;
        };
        if e.is_mounted {
            continue;
        }
        if e.is_deleted {
            if e.can_remove()
                && let Some(gone) = archives(req)
                    .get_mut(id)
                    .and_then(|a| a.manifest.remove_at(slot))
            {
                gone.free(req);
            }
            continue;
        }
        let filename = e.filename.clone();
        if let Some(a) = archives(req).get_mut(id) {
            a.add_virtual_dirs(&filename);
        }
        let Some(e) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
            continue;
        };
        let (is_dir, modified, flags, old_flags) = (e.is_dir, e.is_modified, e.flags, e.old_flags);
        let (mut uncompressed, mut csize, mut crc) =
            (e.uncompressed_filesize, e.compressed_filesize, e.crc32);
        let (offset_abs, timestamp) = (e.offset_abs, e.timestamp);
        let really = modified
            && !(flags & ENT_COMPRESSION_MASK != 0 && (old_flags == flags || old_flags == 0));
        let body = if is_dir {
            Body::Dir
        } else if modified {
            if let Err(_error) = entry::open_entry_fp(req, id, slot, false)? {
                return fail(format!(
                    "unable to open file contents of file \"{}\" in zip-based phar \"{name}\"",
                    lossy(&filename)
                ));
            }
            if really {
                let data = entry::contents(req, id, slot)?;
                crc = !php_ext_standard::crc32::update(u32::MAX, &data);
                uncompressed = u32::try_from(data.len()).unwrap_or(u32::MAX);
                if flags & ENT_COMPRESSION_MASK == 0 {
                    csize = uncompressed;
                    Body::Fresh(data)
                } else {
                    match compressed(req, flags, &data, &filename, &name)? {
                        Ok(packed) => {
                            csize = u32::try_from(packed.len()).unwrap_or(u32::MAX);
                            Body::Fresh(packed)
                        }
                        Err(text) => return fail(text),
                    }
                }
            } else {
                Body::Kept(kept_bytes(old.as_deref(), offset_abs, csize))
            }
        } else {
            Body::Kept(kept_bytes(old.as_deref(), offset_abs, csize))
        };
        let comment = entry_metadata(req, id, slot)?;
        let row = Row {
            filename: &filename,
            is_dir,
            flags,
            timestamp,
            uncompressed,
            compressed: csize,
            crc32: crc,
            comment: comment.as_deref(),
        };
        let contents: &[u8] = match &body {
            Body::Fresh(bytes) | Body::Kept(bytes) => bytes,
            Body::Dir => &[],
        };
        let header_offset = pass.files.len();
        let offset = write_row(&mut pass, &row, contents);
        let Some(e) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        else {
            continue;
        };
        e.header_offset = i64::try_from(header_offset).unwrap_or(i64::MAX);
        e.is_modified = false;
        if matches!(body, Body::Fresh(_)) {
            e.crc32 = crc;
            e.uncompressed_filesize = uncompressed;
            e.compressed_filesize = csize;
            e.old_flags = e.flags;
        }
        if !is_dir || matches!(e.content, Content::Mod(_)) {
            e.content = Content::Fp;
        }
        if !is_dir {
            e.offset = offset;
            e.offset_abs = offset;
        }
    }
    let meta = archive_metadata(req, id)?;
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    if !a.is_data || a.sig_flags != 0 {
        let mut hashed = pass.files.clone();
        hashed.extend_from_slice(&pass.central);
        if let Some(meta) = &meta {
            hashed.extend_from_slice(meta);
        }
        let mut sig_flags = a.sig_flags;
        let signature = match crate::signature::create(req, &mut sig_flags, &hashed, &fname)? {
            Ok(signature) => signature,
            Err(error) => {
                return fail(format!(
                    "phar error: unable to write signature to zip-based phar: {error}"
                ));
            }
        };
        if let Some(a) = archives(req).get_mut(id) {
            a.sig_flags = sig_flags;
            a.signature = Some(crate::signature::hex(&signature));
        }
        let mut sigbuf = sig_flags.to_le_bytes().to_vec();
        sigbuf.extend_from_slice(
            &u32::try_from(signature.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        sigbuf.extend_from_slice(&signature);
        let len = u32::try_from(sigbuf.len()).unwrap_or(u32::MAX);
        let row = Row {
            filename: b".phar/signature.bin",
            is_dir: false,
            flags: 0,
            timestamp: 0,
            uncompressed: len,
            compressed: len,
            crc32: !php_ext_standard::crc32::update(u32::MAX, &sigbuf),
            comment: None,
        };
        write_row(&mut pass, &row, &sigbuf);
    }
    let (cdir_size, cdir_offset) = (pass.central.len(), pass.files.len());
    let mut out = pass.files;
    out.extend_from_slice(&pass.central);
    let mut eocd = [0_u8; EOCD];
    eocd[..4].copy_from_slice(b"PK\x05\x06");
    set16(&mut eocd, 8, count as u16);
    set16(&mut eocd, 10, count as u16);
    set32(&mut eocd, 12, cdir_size as u32);
    set32(&mut eocd, 16, cdir_offset as u32);
    if let Some(meta) = &meta {
        set16(&mut eocd, 20, meta.len() as u16);
    }
    out.extend_from_slice(&eocd);
    if let Some(meta) = &meta {
        out.extend_from_slice(meta);
    }
    if let Some(a) = archives(req).get_mut(id) {
        a.ufp = None;
        a.is_brandnew = false;
    }
    write::write_out(req, id, out)
}

/// The compressed bytes of an unchanged entry, read from the old archive.
fn kept_bytes(old: Option<&Vec<u8>>, offset_abs: i64, csize: u32) -> Vec<u8> {
    let Some(old) = old else {
        return Vec::new();
    };
    let at = usize::try_from(offset_abs)
        .unwrap_or(usize::MAX)
        .min(old.len());
    old[at..at.saturating_add(csize as usize).min(old.len())].to_vec()
}
