//! libzip's error state re-derived (`zip_error_t`, `zip_err_str.c`, which
//! libzip's build generates from `zip.h`'s comments, and
//! `zip_error_strerror.c`): a code, `ZipArchive::$status`, and the number
//! beside it, `$statusSys`, read as an `errno` for the codes whose kind
//! is the system's.

use php_ext::request::Request;

/// `ZIP_ER_*`, each its `ZipArchive::ER_*`: the codes this crate sets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub(crate) enum Er {
    Multidisk = 1,
    Rename = 2,
    Seek = 4,
    Read = 5,
    Write = 6,
    Crc = 7,
    Zipclosed = 8,
    Noent = 9,
    Exists = 10,
    Open = 11,
    Tmpopen = 12,
    Changed = 15,
    Compnotsupp = 16,
    Eof = 17,
    Inval = 18,
    Nozip = 19,
    Internal = 20,
    Incons = 21,
    Remove = 22,
    Deleted = 23,
    Encrnotsupp = 24,
    Rdonly = 25,
    Nopasswd = 26,
    Wrongpasswd = 27,
    Opnotsupp = 28,
    CompressedData = 31,
    Cancelled = 32,
    DataLength = 33,
    NotAllowed = 34,
    TruncatedZip = 35,
}

/// `_zip_err_str`, as libzip's build generates it from `zip.h`'s comments:
/// each code's sentence after its kind, `S` where the system half is an
/// `errno`, `Z` a zlib code, `L` one of libzip's details, `N` none.
const STR: &str = "N No error|N Multi-disk zip archives not supported|S Renaming temporary file failed|S Closing zip archive failed|S Seek error|S Read error|S Write error|N CRC error|N Containing zip archive was closed|N No such file|N File already exists|S Can't open file|S Failure to create temporary file|Z Zlib error|N Malloc failure|N Entry has been changed|N Compression method not supported|N Premature end of file|N Invalid argument|N Not a zip archive|N Internal error|L Zip archive inconsistent|S Can't remove file|N Entry has been deleted|N Encryption method not supported|N Read-only archive|N No password provided|N Wrong password provided|N Operation not supported|N Resource still in use|S Tell error|N Compressed data invalid|N Operation cancelled|N Unexpected length of data|N Not allowed in torrentzip|N Possibly truncated or corrupted zip archive";

/// `zip_error_t`: the code and its system half.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Status {
    pub(crate) zip: i32,
    pub(crate) sys: i32,
}

impl Status {
    pub(crate) fn of(er: Er, sys: i32) -> Status {
        Status {
            zip: er as i32,
            sys,
        }
    }

    /// `zip_error_set`.
    pub(crate) fn set(&mut self, er: Er, sys: i32) {
        *self = Status::of(er, sys);
    }

    /// `zip_error_strerror`: the sentence, then `: ` and the `errno`'s
    /// text for a code whose kind has one. No zlib code or detail is ever
    /// set here, so those kinds have no system half.
    pub(crate) fn text(self, req: &Request) -> String {
        let entry = usize::try_from(self.zip)
            .ok()
            .and_then(|i| STR.split('|').nth(i));
        let Some((kind, sentence)) = entry.and_then(|e| e.split_once(' ')) else {
            return format!("Unknown error {}", self.zip);
        };
        match kind {
            "S" => format!("{sentence}: {}", req.os_error_text(self.sys)),
            "Z" => format!("{sentence}: "),
            _ => sentence.to_owned(),
        }
    }
}

/// A libzip call that failed, its error already kept in the archive's
/// [`Status`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Failed;

pub(crate) type Done<T = ()> = Result<T, Failed>;
