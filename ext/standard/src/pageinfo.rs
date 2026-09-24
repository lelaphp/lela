//! `Z/ext/standard/pageinfo.c`: `getmyuid`, `getmygid`, `getmypid`,
//! `getmyinode` and `getlastmod`, the first two and the last two over
//! `php_statpage`, which stats the file the request serves once and
//! keeps what it found for the rest of the request, `BG(page_uid)` and
//! its kin.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::os::unix::fs::MetadataExt;

use php_ext::decl::FunctionDecl;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::output::Entry;
use crate::stub::stub;
use crate::{State, argument_count_between};

/// `BG(page_uid)`, `BG(page_gid)`, `BG(page_inode)` and `BG(page_mtime)`
/// once `php_statpage` has set them; `-1` is php's unset.
#[derive(Clone, Copy)]
pub(crate) struct Page {
    uid: i64,
    gid: i64,
    inode: i64,
    mtime: i64,
}

/// `php_statpage` (`Z/ext/standard/pageinfo.c:48`): the served file's
/// owner, group, inode and modification time, `sapi_get_stat`'s
/// `stat(2)` of the path the request was given; where there is none, as
/// under `php -r`, the process's user and group, and the other two
/// unset.
fn statpage(req: &mut Request) -> Page {
    if let Some(page) = req.state::<State>().page {
        return page;
    }
    let meta = req
        .script_name()
        .map(<[u8]>::to_vec)
        .and_then(|script| std::fs::metadata(req.absolutize(&script).0).ok());
    let page = match meta {
        Some(meta) => Page {
            uid: i64::from(meta.uid()),
            gid: i64::from(meta.gid()),
            inode: i64::try_from(meta.ino()).unwrap_or(-1),
            mtime: meta.mtime(),
        },
        None => Page {
            uid: i64::from(crate::ffi::uid()),
            gid: i64::from(crate::ffi::gid()),
            inode: -1,
            mtime: -1,
        },
    };
    req.state::<State>().page = Some(page);
    page
}

/// A number, or `false` for one below zero, as each native here answers.
fn long_or_false(n: i64) -> Value {
    if n < 0 {
        Value::bool(false)
    } else {
        Value::int(n)
    }
}

/// The five, as the stub declares them.
///
/// ```php
/// foreach (['getmyuid', 'getmygid', 'getmypid', 'getmyinode', 'getlastmod'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function getmyuid ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:standard> function getmygid ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:standard> function getmypid ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:standard> function getmyinode ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:standard> function getlastmod ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (stub("getmyuid(): int|false"), getmyuid as Entry),
        (stub("getmygid(): int|false"), getmygid),
        (stub("getmypid(): int|false"), getmypid),
        (stub("getmyinode(): int|false"), getmyinode),
        (stub("getlastmod(): int|false"), getlastmod),
    ]
}

/// `getmyuid(): int|false`, `PHP_FUNCTION(getmyuid)`
/// (`Z/ext/standard/pageinfo.c:83`): the owner of the file the request
/// serves.
///
/// ```php
/// var_dump(getmyuid() === fileowner(__FILE__), getmygid() === filegroup(__FILE__));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
fn getmyuid(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(long_or_false(statpage(req).uid))
}

/// `getmygid(): int|false`, `PHP_FUNCTION(getmygid)`
/// (`Z/ext/standard/pageinfo.c:98`): its group.
///
/// ```php
/// var_dump(is_int(getmygid()));
/// ```
/// ```expect
/// bool(true)
/// ```
fn getmygid(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(long_or_false(statpage(req).gid))
}

/// `getmypid(): int|false`, `PHP_FUNCTION(getmypid)`
/// (`Z/ext/standard/pageinfo.c:112`): `getpid(2)`, which differs from
/// one process to the next, so the witness pins its shape.
///
/// ```php
/// var_dump(getmypid() > 0, getmypid() === getmypid());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
fn getmypid(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(Value::int(i64::from(std::process::id())))
}

/// `getmyinode(): int|false`, `PHP_FUNCTION(getmyinode)`
/// (`Z/ext/standard/pageinfo.c:128`): the served file's inode.
///
/// ```php
/// var_dump(getmyinode() === fileinode(__FILE__));
/// ```
/// ```expect
/// bool(true)
/// ```
fn getmyinode(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(long_or_false(statpage(req).inode))
}

/// `getlastmod(): int|false`, `PHP_FUNCTION(getlastmod)`
/// (`Z/ext/standard/pageinfo.c:149`): the served file's modification
/// time, as `php_statpage` first read it.
///
/// ```php
/// $t = getlastmod(); touch(__FILE__, 1000);
/// var_dump($t === filemtime(__FILE__) || $t > 1000, getlastmod() === $t);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
fn getlastmod(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(long_or_false(statpage(req).mtime))
}
