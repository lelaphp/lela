//! The `sysvshm` extension: `php-src/ext/sysvshm` as one crate under
//! `ext/` (ADR-0004). Two workspace dependencies, the facade and
//! `standard`, whose serializer `sysvshm.c` calls through
//! `ext/standard/php_var.h` (design §4.10); System V shared memory
//! through `libc` in [`ffi`] alone, since nix binds none of it (design
//! §7.2, #1323); one entry, `extension()`. The phpt subset is
//! `php-src/ext/sysvshm/tests`, through `xtask phpt --subset`.
//!
//! Every native is `sysvshm.c`'s at the tag, in `sysvshm.stub.php`'s
//! order. The segment is laid out as php lays it, since a php process on
//! the same key reads it: `sysvshm_chunk_head` (the magic `PHP_SM`, then
//! `start`, `end`, `free` and `total`), then one `sysvshm_chunk` per
//! variable (its key, the length of its serialized value, the distance to
//! the next, then the bytes), each padded to a `zend_long`.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ffi;
mod throwables;

use php_ext::abi::{
    EngineState, Extension, ExtensionBuilder, Instance, OrFalse, Refusal, Refusals, Start, True,
};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, Value};
use php_ext_standard::{Serializer, Unserializer};

use crate::ffi::Segment;
use crate::throwables::Throwables;

/// `sizeof(sysvshm_chunk_head)`: the magic and four `size_t`s.
const HEAD: usize = 40;
/// `offsetof(sysvshm_chunk, mem)`: a variable's bytes follow its key,
/// length and distance to the next.
const MEM: usize = 24;
/// `sizeof(sysvshm_chunk)`, `mem`'s one byte padded.
const CHUNK: usize = 32;
/// The magic, `strcpy`'s seven bytes.
const MAGIC: &[u8; 7] = b"PHP_SM\0";

/// Engine-scoped (ADR-0005): `php_sysvshm.init_mem`, the size
/// `shm_attach` makes a segment without `$size`, read once from the
/// startup configuration as `cfg_get_long` reads `sysvshm.init_mem`.
#[derive(EngineState)]
struct Config {
    init_mem: i64,
    throwables: Throwables,
}

impl Config {
    /// `PHP_MINIT(sysvshm)`: the entry's leading number, 10000 without
    /// one.
    fn start(start: &Start) -> Result<Config, Refusal> {
        let init_mem = start
            .ini()
            .entries
            .iter()
            .find(|(name, _)| name.as_slice() == b"sysvshm.init_mem")
            .map_or(10000, |(_, value)| value.as_deref().map_or(0, leading_long));
        Ok(Config {
            init_mem,
            throwables: Throwables::start(start)?,
        })
    }
}

/// `zval_get_long` of a string: the number it opens with, zero for none.
fn leading_long(bytes: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start();
    let end = text
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(text.len(), |(i, _)| i);
    text[..end].parse().unwrap_or(0)
}

/// The extension value (design §7.1): the seven natives, the class and
/// the engine state.
///
/// ```php
/// var_dump(extension_loaded('sysvshm'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            shm_attach::DECLARED,
            shm_detach::DECLARED,
            shm_has_var::DECLARED,
            shm_remove::DECLARED,
            shm_put_var::DECLARED,
            shm_get_var::DECLARED,
            shm_remove_var::DECLARED,
        ])
        .class::<SysvSharedMemory>()
        .engine_state(Config::start)
        .info(sysvshm_info)
        .build("sysvshm")
}

/// `PHP_MINFO_FUNCTION(sysvshm)`: the one-row table.
///
/// ```php
/// (new ReflectionExtension('sysvshm'))->info();
/// ```
/// ```expect
/// sysvshm
///
/// sysvshm support => enabled
/// ```
fn sysvshm_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["sysvshm support", "enabled"]]);
}

/// `sysvshm_shm` less its `zend_object`: the key, the segment's id and
/// the attachment, `None` once `shm_detach` detached it (php's
/// `ptr == NULL`), as on an object no native made.
#[derive(Default)]
pub(crate) struct Shm {
    key: i32,
    id: i32,
    segment: Option<Segment>,
}

/// `final class SysvSharedMemory`, `@strict-properties`,
/// `@not-serializable`.
///
/// ```php
/// try { new SysvSharedMemory; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $s = shm_attach(ftok(__FILE__, "a"), 128);
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $s->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($s, $s == $s);
/// shm_remove($s);
/// ```
/// ```expect
/// Cannot directly construct SysvSharedMemory, use shm_attach() instead
/// Trying to clone an uncloneable object of class SysvSharedMemory
/// Serialization of 'SysvSharedMemory' is not allowed
/// Cannot create dynamic property SysvSharedMemory::$x
/// object(SysvSharedMemory)#1 (0) {
/// }
/// bool(true)
/// ```
#[php_class(
    final,
    not_serializable,
    size = 80, payload = Shm,
    handlers = SYSVSHM,
    dynamic_properties = forbidden
)]
pub(crate) struct SysvSharedMemory;

#[php_class]
impl SysvSharedMemory {}

/// `sysvshm_object_handlers`: `std_object_handlers` with
/// `get_constructor` refusing, `clone_obj` php's null slot and `compare`
/// `zend_objects_not_comparable`; `free_obj`'s `shmdt` is the payload's
/// drop.
const SYSVSHM: Refusals = Refusals::NONE
    .construction(Some("shm_attach()"))
    .cloning()
    .comparison();

/// A `size_t` of the segment as the `usize` it indexes with.
fn at(word: u64) -> usize {
    usize::try_from(word).unwrap_or(usize::MAX)
}

/// A count of bytes as a `size_t` word.
fn word(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// `sysvshm_chunk_head` less its magic.
struct Head {
    start: usize,
    end: usize,
    free: usize,
}

impl Head {
    /// The head the segment holds.
    fn read(seg: &Segment) -> Option<Head> {
        Some(Head {
            start: at(seg.word(8)?),
            end: at(seg.word(16)?),
            free: at(seg.word(24)?),
        })
    }

    /// `end` and `free` written back.
    fn write(&self, seg: &Segment) {
        seg.set_word(16, word(self.end));
        seg.set_word(24, word(self.free));
    }

    /// `php_check_shm_data`: where the variable `key` starts, walking
    /// the chunks from `start` by their `next`.
    fn find(&self, seg: &Segment, key: i64) -> Option<usize> {
        let mut pos = self.start;
        loop {
            if pos >= self.end {
                return None;
            }
            if seg.word(pos)?.cast_signed() == key {
                return Some(pos);
            }
            let next = at(seg.word(pos + 16)?);
            pos = pos.wrapping_add(next);
            if next == 0 || pos < self.start {
                return None;
            }
        }
    }

    /// `php_remove_shm_data`: the chunk at `pos` taken out and the ones
    /// after it moved down over it.
    fn remove(&mut self, seg: &Segment, pos: usize) {
        let next = seg.word(pos + 16).map_or(0, at);
        let len = self.end.wrapping_sub(pos).wrapping_sub(next);
        self.free = self.free.wrapping_add(next);
        self.end = self.end.wrapping_sub(next);
        self.write(seg);
        if len > 0 {
            seg.shift(pos.wrapping_add(next), pos, len);
        }
    }

    /// `php_put_shm_data`: `data` stored as the variable `key`, in place
    /// of one there, where it fits.
    fn put(&mut self, seg: &Segment, key: i64, data: &[u8]) -> bool {
        let total = (data.len() + CHUNK - 1) / 8 * 8 + 8;
        if let Some(pos) = self.find(seg, key) {
            self.remove(seg, pos);
        }
        if self.free < total {
            return false;
        }
        let pos = self.end;
        let fits = seg.set_word(pos, key.cast_unsigned())
            && seg.set_word(pos + 8, word(data.len()))
            && seg.set_word(pos + 16, word(total))
            && seg.write(pos + MEM, data);
        if !fits {
            return false;
        }
        self.end += total;
        self.free -= total;
        self.write(seg);
        true
    }
}

/// `shm_attach(int $key, ?int $size = null, int $permissions = 0666):
/// SysvSharedMemory|false`: the segment under `$key`, made with `$size`
/// bytes (`sysvshm.init_mem`, 10000, without one) where there is none,
/// and its head laid down where it has no magic, over the size the
/// kernel answers. A segment too small for the head is refused, and one
/// this call made but could not use is removed again (8.5.12).
///
/// ```php
/// $k = ftok(__FILE__, "b");
/// $s = shm_attach($k, 128);
/// shm_put_var($s, 1, "one");
/// $t = shm_attach($k);
/// var_dump(shm_get_var($t, 1));
/// shm_remove($s);
/// try { shm_attach($k, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(shm_attach($k, 39));
/// var_dump(shm_attach($k, 1024, 0));
/// $u = shm_attach($k, 1024, 0600);
/// var_dump(shm_put_var($u, 1, "value"), shm_get_var($u, 1), shm_remove($u));
/// $big = shm_attach($k, 4096);
/// shm_detach($big);
/// $v = shm_attach($k, 1 << 20);
/// var_dump(shm_put_var($v, 1, str_repeat("A", 8192)), shm_remove($v));
/// $raw = shmop_open($k, "n", 0600, 8);
/// var_dump(shm_attach($k, 1024));
/// shmop_delete($raw);
/// ```
/// ```expect
/// string(3) "one"
/// shm_attach(): Argument #2 ($size) must be greater than 0
///
/// Warning: shm_attach(): Failed for key 0x%x: memorysize too small in %s on line 9
/// bool(false)
///
/// Warning: shm_attach(): Failed for key 0x%x: Permission denied in %s on line 10
/// bool(false)
/// bool(true)
/// string(5) "value"
/// bool(true)
///
/// Warning: shm_put_var(): Not enough shared memory left in %s on line 16
/// bool(false)
/// bool(true)
///
/// Warning: shm_attach(): Failed for key 0x%x: segment too small in %s on line 18
/// bool(false)
/// ```
#[php_function]
#[allow(clippy::cast_possible_truncation)]
fn shm_attach(
    req: &mut Request,
    key: i64,
    #[php(default = null)] size: Option<i64>,
    #[php(default = 0666)] permissions: i64,
) -> Result<OrFalse<Owner<Instance<SysvSharedMemory>>>, Thrown> {
    let size = match size {
        Some(size) => size,
        None => req
            .engine::<Config>()
            .map_or(10000, |config| config.init_mem),
    };
    if size < 1 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let failed = |req: &mut Request, why: &str| {
        let text = format!("Failed for key 0x{:x}: {why}", key.cast_unsigned());
        docref(req, Level::Warning, &text);
        OrFalse::False
    };
    let len = usize::try_from(size).unwrap_or(usize::MAX);
    let (id, created) = match ffi::get(key as i32, 0, 0) {
        Ok(id) => (id, false),
        Err(_) if len < HEAD => return Ok(failed(req, "memorysize too small")),
        Err(_) => {
            let flags = permissions as i32 | libc::IPC_CREAT | libc::IPC_EXCL;
            match ffi::get(key as i32, len, flags) {
                Ok(id) => (id, true),
                Err(errno) => {
                    let text = req.environment().os_error_text(errno);
                    return Ok(failed(req, &text));
                }
            }
        }
    };
    // A segment this call made and could not use is removed, so the key
    // is free again (8.5.12).
    let unmake = || {
        if created {
            // php ignores what `shmctl(IPC_RMID)` answers here.
            ffi::remove(id).ok();
        }
    };
    let segment = match Segment::attach(id) {
        Ok(segment) => segment,
        Err(errno) => {
            let text = req.environment().os_error_text(errno);
            let refused = failed(req, &text);
            unmake();
            return Ok(refused);
        }
    };
    // The size is the kernel's, `shm_desc.shm_segsz`, not `$size`.
    let len = segment.size();
    if len < HEAD {
        let refused = failed(req, "segment too small");
        drop(segment);
        unmake();
        return Ok(refused);
    }
    let magic = segment.read(0, MAGIC.len());
    if magic.as_deref().map(php_ext::coerce::c_str) != Some(&b"PHP_SM"[..]) {
        segment.write(0, MAGIC);
        segment.set_word(8, word(HEAD));
        segment.set_word(16, word(HEAD));
        segment.set_word(24, word(len - HEAD));
        segment.set_word(32, word(len));
    }
    let Some(class) = lookup_class(req, b"SysvSharedMemory")? else {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "Class \"SysvSharedMemory\" not found",
        ));
    };
    let shm = new_object(req, class)?.into_instance::<SysvSharedMemory>();
    *shm.payload_mut(req)? = Shm {
        key: key as i32,
        id,
        segment: Some(segment),
    };
    Ok(OrFalse::Value(shm))
}

/// The `Error` every native but `shm_attach` throws on a detached
/// object.
fn destroyed(req: &mut Request) -> Thrown {
    let ce = match Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw_error(
        req,
        ce.error,
        "Shared memory block has already been destroyed",
    )
}

/// What `f` answers of the attached segment, or the `Error` for one
/// detached.
fn with<R>(
    req: &mut Request,
    shm: Instance<SysvSharedMemory>,
    f: impl FnOnce(&Segment) -> R,
) -> Result<R, Thrown> {
    match shm.payload_mut(req)?.segment.as_ref().map(f) {
        Some(answer) => Ok(answer),
        None => Err(destroyed(req)),
    }
}

/// `shm_detach(SysvSharedMemory $shm): true`: `shmdt`, after which every
/// native refuses the object.
///
/// ```php
/// $s = shm_attach(ftok(__FILE__, "c"), 64);
/// $t = shm_attach(ftok(__FILE__, "c"));
/// var_dump(shm_detach($s));
/// try { shm_has_var($s, 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// shm_remove($t);
/// ```
/// ```expect
/// bool(true)
/// Shared memory block has already been destroyed
/// ```
#[php_function]
fn shm_detach(req: &mut Request, shm: Instance<SysvSharedMemory>) -> Result<True, Thrown> {
    if shm.payload_mut(req)?.segment.take().is_none() {
        return Err(destroyed(req));
    }
    Ok(True)
}

/// `shm_has_var(SysvSharedMemory $shm, int $key): bool`.
///
/// ```php
/// $s = shm_attach(ftok(__FILE__, "d"), 128);
/// shm_put_var($s, -5, null);
/// var_dump(shm_has_var($s, -5), shm_has_var($s, 5));
/// shm_remove($s);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn shm_has_var(
    req: &mut Request,
    shm: Instance<SysvSharedMemory>,
    key: i64,
) -> Result<bool, Thrown> {
    with(req, shm, |seg| {
        Head::read(seg)
            .and_then(|head| head.find(seg, key))
            .is_some()
    })
}

/// `shm_remove(SysvSharedMemory $shm): bool`: `IPC_RMID`, the segment
/// removed once the last process detaches.
///
/// ```php
/// $s = shm_attach(ftok(__FILE__, "e"), 64);
/// var_dump(shm_remove($s));
/// shm_detach($s);
/// try { shm_remove($s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// Shared memory block has already been destroyed
/// ```
#[php_function]
fn shm_remove(req: &mut Request, shm: Instance<SysvSharedMemory>) -> Result<bool, Thrown> {
    with(req, shm, |_| ())?;
    let (key, id) = {
        let shm = shm.payload_mut(req)?;
        (shm.key, shm.id)
    };
    if let Err(errno) = ffi::remove(id) {
        let text = req.environment().os_error_text(errno);
        let hex = key.cast_unsigned();
        let text = format!("Failed for key 0x{hex:x}, id {id}: {text}");
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    Ok(true)
}

/// `shm_put_var(SysvSharedMemory $shm, int $key, mixed $value): bool`:
/// the value serialized and stored as the variable `$key`, in place of
/// one there.
///
/// ```php
/// $s = shm_attach(ftok(__FILE__, "f"), 128);
/// var_dump(shm_put_var($s, 1, str_repeat("x", 40)), shm_put_var($s, 2, str_repeat("y", 40)));
/// var_dump(shm_put_var($s, 1, "short"), shm_put_var($s, 2, "y"), shm_get_var($s, 1));
/// class C { function __serialize(): array { global $s; shm_detach($s); return []; } }
/// try { shm_put_var($s, 3, new C); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $t = shm_attach(ftok(__FILE__, "f"));
/// shm_remove($t);
/// ```
/// ```expect
/// Warning: shm_put_var(): Not enough shared memory left in %s on line 3
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// string(5) "short"
/// Shared memory block has been destroyed by the serialization function
/// ```
#[php_function]
fn shm_put_var(
    req: &mut Request,
    shm: Instance<SysvSharedMemory>,
    key: i64,
    value: Value,
) -> Result<bool, Thrown> {
    with(req, shm, |_| ())?;
    let mut out = Vec::new();
    let mut hash = Serializer::new(req);
    let written = hash.write(req, &mut out, &value);
    hash.finish(req);
    if shm.payload_mut(req)?.segment.is_none() {
        let text = "Shared memory block has been destroyed by the serialization function";
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, text));
    }
    written?;
    let put = with(req, shm, |seg| {
        Head::read(seg).is_some_and(|mut head| head.put(seg, key, &out))
    })?;
    if !put {
        docref(req, Level::Warning, "Not enough shared memory left");
    }
    Ok(put)
}

/// `shm_get_var(SysvSharedMemory $shm, int $key): mixed`: the variable
/// `$key` unserialized.
///
/// ```php
/// $s = shm_attach(ftok(__FILE__, "g"), 256);
/// shm_put_var($s, 7, ["a" => new ArrayObject([1])]);
/// var_dump(shm_get_var($s, 7), shm_get_var($s, 8));
/// shm_remove($s);
/// ```
/// ```expect
/// Warning: shm_get_var(): Variable key 8 doesn't exist in %s on line 4
/// array(1) {
///   ["a"]=>
///   object(ArrayObject)#%d (1) {
///     ["storage":"ArrayObject":private]=>
///     array(1) {
///       [0]=>
///       int(1)
///     }
///   }
/// }
/// bool(false)
/// ```
#[php_function]
fn shm_get_var(
    req: &mut Request,
    shm: Instance<SysvSharedMemory>,
    key: i64,
) -> Result<Owner<Value>, Thrown> {
    let bytes = with(req, shm, |seg| {
        let pos = Head::read(seg)?.find(seg, key)?;
        let len = at(seg.word(pos + 8)?);
        Some(seg.read(pos + MEM, len))
    })?;
    let Some(bytes) = bytes else {
        let text = format!("Variable key {key} doesn't exist");
        docref(req, Level::Warning, &text);
        return Ok(Owner::bool(false));
    };
    let mut reader = Unserializer::new(req, bytes.as_deref().unwrap_or_default())?;
    // The value read is the caller's, kept beside the read's outcome.
    let (outcome, read) = match reader.local(req) {
        Ok(read) => (Ok(()), read),
        Err(thrown) => (Err(thrown), None),
    };
    if let Err(thrown) = reader.finish(req, outcome) {
        if let Some(value) = read {
            value.give_up(req);
        }
        return Err(thrown);
    }
    match read {
        Some(value) if bytes.is_some() => Ok(value),
        read => {
            if let Some(value) = read {
                value.give_up(req);
            }
            docref(
                req,
                Level::Warning,
                "Variable data in shared memory is corrupted",
            );
            Ok(Owner::bool(false))
        }
    }
}

/// `shm_remove_var(SysvSharedMemory $shm, int $key): bool`: the variable
/// `$key` taken out, the ones after it moved down.
///
/// ```php
/// $s = shm_attach(ftok(__FILE__, "h"), 256);
/// shm_put_var($s, 1, 1); shm_put_var($s, 2, 2); shm_put_var($s, 3, 3);
/// var_dump(shm_remove_var($s, 2), shm_get_var($s, 1), shm_get_var($s, 3), shm_remove_var($s, 2));
/// shm_remove($s);
/// ```
/// ```expect
/// Warning: shm_remove_var(): Variable key 2 doesn't exist in %s on line 4
/// bool(true)
/// int(1)
/// int(3)
/// bool(false)
/// ```
#[php_function]
fn shm_remove_var(
    req: &mut Request,
    shm: Instance<SysvSharedMemory>,
    key: i64,
) -> Result<bool, Thrown> {
    let removed = with(req, shm, |seg| {
        let Some(mut head) = Head::read(seg) else {
            return false;
        };
        let Some(pos) = head.find(seg, key) else {
            return false;
        };
        head.remove(seg, pos);
        true
    })?;
    if !removed {
        let text = format!("Variable key {key} doesn't exist");
        docref(req, Level::Warning, &text);
    }
    Ok(removed)
}
