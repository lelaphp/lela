//! php's two profiler outputs (`jit-design.md` §4.5 *Observability*),
//! under `opcache.jit_debug`'s `PERF` (`1<<4`) and `PERF_DUMP` (`1<<5`)
//! bits (`Z/ext/opcache/jit/zend_jit.h:59-60`), written by the publish
//! that places a trace's code, with php's bytes:
//!
//! - **The map line**, under either bit: `<start> <size> <name>` in hex
//!   to `/tmp/perf-<pid>.map`, `ir_perf_map_register`
//!   (`Z/ext/opcache/jit/ir/ir_perf.c:256-270`), the name
//!   `TRACE-<n>$<where>$<line>` (`zend_jit_trace_name`).
//! - **The jitdump record**, under `PERF_DUMP` where php writes one, on
//!   Linux, whose executable is an ELF file: a `JIT_CODE_LOAD` record with
//!   the name and the code's bytes appended to `/tmp/jit-<pid>.dump`
//!   (`ir_perf_jitdump_register`), after the header the engine's start
//!   wrote and the mapping `perf inject` finds the file by
//!   (`ir_perf_jitdump_open`).
//!
//! php opens each file once per process, truncating it, numbers its
//! records from one per process and closes the jitdump at shutdown; here
//! the engine's start truncates them ([`Outputs::start`]), each epoch's
//! table appends, one write per line or record, counting on the engine's
//! one `code_id`, and the engine's end writes the `JIT_CODE_CLOSE`
//! record.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{TraceId, TraceName, code};
use crate::policy::JitDebug;

/// `IR_PERF_JITDUMP_HEADER_MAGIC`, `JiTD`.
const MAGIC: u32 = 0x4A69_5444;
/// `IR_PERF_JITDUMP_HEADER_VERSION`.
const VERSION: u32 = 1;
/// `IR_PERF_JITDUMP_RECORD_LOAD`.
const RECORD_LOAD: u32 = 0;
/// `IR_PERF_JITDUMP_RECORD_CLOSE`.
const RECORD_CLOSE: u32 = 3;
/// The header's size, `sizeof(ir_perf_jitdump_header)`.
const HEADER: u32 = 40;
/// A load record's size before its name, `sizeof(ir_perf_jitdump_load_record)`.
const LOAD: u32 = 56;

/// The ELF machine of this target, php's `elf_hdr.machine` of its own
/// executable: `EM_X86_64` or `EM_AARCH64`.
const ELF_MACHINE: u32 = if cfg!(target_arch = "aarch64") {
    183
} else {
    62
};

/// Where a table's `PERF` lines and `PERF_DUMP` records go: nowhere for
/// a bit that is clear.
pub(crate) struct Perf {
    map: Option<Box<dyn Write + Send>>,
    dump: Option<Dump>,
}

/// The jitdump the records go to, and the engine's next `code_id`.
struct Dump {
    to: Box<dyn Write + Send>,
    code_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for Perf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Perf")
            .field("map", &self.map.is_some())
            .field("dump", &self.dump.is_some())
            .finish()
    }
}

/// `/tmp/perf-<pid>.map`.
fn map_path() -> String {
    format!("/tmp/perf-{}.map", std::process::id())
}

/// `/tmp/jit-<pid>.dump`.
fn dump_path() -> String {
    format!("/tmp/jit-{}.dump", std::process::id())
}

/// Whether php writes a jitdump on this target: where its executable is
/// an ELF file, which `ir_perf_jitdump_open` reads for its machine.
const DUMPS: bool = cfg!(target_os = "linux");

/// The engine's `PERF` outputs, php's per-process ones: whether the
/// map file and the jitdump were started, and the jitdump's next
/// `code_id`, which every epoch's table counts on as php counts on its
/// one static. Dropped at the engine's end, it closes the jitdump with
/// php's `JIT_CODE_CLOSE` record (`ir_perf_jitdump_close`).
#[derive(Debug)]
pub(crate) struct Outputs {
    map: bool,
    dump: bool,
    code_id: Arc<AtomicU64>,
}

impl Outputs {
    /// None started.
    pub(crate) fn none() -> Outputs {
        Outputs {
            map: false,
            dump: false,
            code_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The engine's start, under `debug`: the map file emptied where
    /// either bit asks for lines, and the jitdump emptied, given php's
    /// header and mapped once, which is the event `perf inject` finds it
    /// by, where `PERF_DUMP` asks and php writes one. What does not open
    /// is not started, as php leaves it.
    pub(crate) fn start(debug: JitDebug) -> Outputs {
        let map =
            debug.any(JitDebug::PERF | JitDebug::PERF_DUMP) && File::create(map_path()).is_ok();
        let dump = DUMPS
            && debug.any(JitDebug::PERF_DUMP)
            && File::create(dump_path())
                .and_then(|mut file| {
                    file.write_all(&header(std::process::id(), code::monotonic_ns()))?;
                    code::map_once(&file)
                })
                .is_ok();
        Outputs {
            map,
            dump,
            code_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl Drop for Outputs {
    fn drop(&mut self) {
        if self.dump
            && let Ok(mut file) = OpenOptions::new().append(true).open(dump_path())
        {
            // A write that fails is dropped, as php's `ir_perf_jitdump_close` drops it.
            drop(file.write_all(&close_record(code::monotonic_ns())));
        }
    }
}

impl Perf {
    /// Nowhere: no line and no record.
    #[cfg(test)]
    pub(crate) fn none() -> Perf {
        Perf {
            map: None,
            dump: None,
        }
    }

    /// A table's writers, appending to the files `outputs` started;
    /// one that does not open writes nothing.
    pub(crate) fn of(outputs: &Outputs) -> Perf {
        let append = |started: bool, path: String| -> Option<Box<dyn Write + Send>> {
            let file = OpenOptions::new()
                .append(true)
                .open(path)
                .ok()
                .filter(|_| started)?;
            Some(Box::new(file))
        };
        Perf {
            map: append(outputs.map, map_path()),
            dump: append(outputs.dump, dump_path()).map(|to| Dump {
                to,
                code_id: Arc::clone(&outputs.code_id),
            }),
        }
    }

    /// Lines to `map` and records to `dump`, each where given, counting
    /// `code_id`s from one.
    #[cfg(test)]
    pub(crate) fn to(
        map: Option<Box<dyn Write + Send>>,
        dump: Option<Box<dyn Write + Send>>,
    ) -> Perf {
        Perf {
            map,
            dump: dump.map(|to| Dump {
                to,
                code_id: Arc::new(AtomicU64::new(1)),
            }),
        }
    }

    /// Trace `id`, named `name`, placed at `entry` with `bytes`: its map
    /// line and its jitdump record, each where asked. A write that fails
    /// is dropped, as php drops it.
    pub(crate) fn published(
        &mut self,
        id: TraceId,
        name: &TraceName,
        entry: NonNull<u8>,
        bytes: &[u8],
    ) {
        let symbol = symbol(id, name);
        let start = entry.as_ptr().addr();
        if let Some(map) = &mut self.map {
            drop(map.write_all(&map_line(start, bytes.len(), &symbol)));
        }
        if let Some(dump) = &mut self.dump {
            let at = Loaded {
                pid: std::process::id(),
                tid: code::thread_id(),
                time: code::monotonic_ns(),
                code_id: dump.code_id.fetch_add(1, Ordering::Relaxed),
            };
            drop(dump.to.write_all(&load_record(&at, &symbol, start, bytes)));
        }
    }
}

/// `TRACE-<n>$<where>$<line>`, php's `zend_jit_trace_name`.
pub(crate) fn symbol(id: TraceId, name: &TraceName) -> Vec<u8> {
    let mut symbol = format!("TRACE-{}$", id.get()).into_bytes();
    symbol.extend_from_slice(&name.at);
    symbol.extend_from_slice(format!("${}", name.line).as_bytes());
    symbol
}

/// `ir_perf_map_register`'s line: `"%zx %zx %s\n"`.
pub(crate) fn map_line(start: usize, size: usize, symbol: &[u8]) -> Vec<u8> {
    let mut line = format!("{start:x} {size:x} ").into_bytes();
    line.extend_from_slice(symbol);
    line.push(b'\n');
    line
}

/// `ir_perf_jitdump_open`'s header: magic, version, its size, the
/// machine, a reserved word, the process, the time and no flags, in the
/// machine's byte order.
pub(crate) fn header(pid: u32, time: u64) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER as usize);
    for word in [MAGIC, VERSION, HEADER, ELF_MACHINE, 0, pid] {
        header.extend_from_slice(&word.to_ne_bytes());
    }
    header.extend_from_slice(&time.to_ne_bytes());
    header.extend_from_slice(&0_u64.to_ne_bytes());
    header
}

/// Who loaded a record's code, when, and the record's `code_id`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Loaded {
    pub(crate) pid: u32,
    pub(crate) tid: u32,
    pub(crate) time: u64,
    pub(crate) code_id: u64,
}

/// `ir_perf_jitdump_close`'s `JIT_CODE_CLOSE` record: its header alone.
pub(crate) fn close_record(time: u64) -> Vec<u8> {
    let mut record = Vec::with_capacity(16);
    record.extend_from_slice(&RECORD_CLOSE.to_ne_bytes());
    record.extend_from_slice(&16_u32.to_ne_bytes());
    record.extend_from_slice(&time.to_ne_bytes());
    record
}

/// `ir_perf_jitdump_register`'s `JIT_CODE_LOAD` record: its header, the
/// process and thread, the code's address twice and its size, the
/// `code_id`, then the name with its NUL and the code's bytes.
pub(crate) fn load_record(at: &Loaded, symbol: &[u8], start: usize, bytes: &[u8]) -> Vec<u8> {
    let size = LOAD as usize + symbol.len() + 1 + bytes.len();
    let mut record = Vec::with_capacity(size);
    record.extend_from_slice(&RECORD_LOAD.to_ne_bytes());
    record.extend_from_slice(&u32::try_from(size).unwrap_or(u32::MAX).to_ne_bytes());
    record.extend_from_slice(&at.time.to_ne_bytes());
    record.extend_from_slice(&at.pid.to_ne_bytes());
    record.extend_from_slice(&at.tid.to_ne_bytes());
    let address = start as u64;
    let length = bytes.len() as u64;
    for word in [address, address, length, at.code_id] {
        record.extend_from_slice(&word.to_ne_bytes());
    }
    record.extend_from_slice(symbol);
    record.push(0);
    record.extend_from_slice(bytes);
    record
}

/// A sink a test reads back: what was written, shared with the test.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct Captured(pub(crate) std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Captured {
    /// What was written so far.
    pub(crate) fn taken(&self) -> Vec<u8> {
        self.0.lock().map(|bytes| bytes.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut bytes) = self.0.lock() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
