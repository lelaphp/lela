//! What becomes of a [`TraceIr`] (`jit-design.md` §4.3): the seam is the
//! value, and two modules read it. [`dump`] renders it as text, which
//! `types` is held to before code is made; [`Backend`] compiles it to
//! final bytes over Cranelift, a [`Compiled`], which `table` publishes.
//! Nothing but `lower` names a Cranelift type, so a Cranelift bump is a
//! change here alone.
//!
//! **What compiled code is.** A trace is one function,
//! `extern "C" fn(cx: *mut ExitCtx, frame: *mut u8) -> u32`, entered with
//! the crossing's `ExitCtx` and the room of the frame it was recorded in,
//! php's `execute_data`, and answering the [`ExitId`] it left through.
//! It runs on the interpreter's frames: a slot is a load and a store at
//! `room + ROOM_BYTES + 16 * var` with the zval layout `php_heap` defines,
//! so an exit leaves every frame as the interpreter reads it once it has
//! stored the scalars it kept in registers with their type words. What
//! compiled code does to the frame chain, and to anything the engine
//! keeps behind its frames, is a call of a kernel through the one
//! emitter; [`Kernel`] lists them and [`Symbols`] holds their addresses.
//! Every kernel answers the crossing's status word, zero to go on, and on
//! anything else compiled code leaves through the opline's exit, or,
//! where the opline names none, answers [`ABANDONED`], for `enter` to
//! read what the kernel kept.
//!
//! **The interrupt test** (`jit-design.md` §4.3). A trace that loops loads
//! the interrupt pointer at offset [`INTERRUPT_AT`] of its `ExitCtx` once
//! as it enters, and at its header loads the byte it points to and leaves
//! through the header's interrupt exit when it is not zero: one load and
//! one branch per iteration, php's cost (`rfc-jit.md:118`).
//!
//! [`ExitId`]: crate::types::ExitId

mod clif;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the goldens' text; no host prints a trace's IR yet"
    )
)]
pub(crate) mod dump;
#[cfg(test)]
mod tests;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::{Function, SourceLoc};
use cranelift_codegen::isa::{self, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, Final, MachSrcLoc};
use php_engine::tier::{FuncRef, MapPtrRaw};
use php_unit::OplineIdx;

use crate::policy::{JitDebug, Policy};
use crate::record::Stop;
use crate::table::TraceName;
use crate::types::{Abort, ExitSpec, Op, TraceIr};

/// A trace in final bytes (`jit-design.md` §4.3), what the `Backend` makes
/// and `table::TraceTable::publish` copies into the code arena once:
/// the bytes, where they are entered, the alignment their first byte
/// needs, one [`ExitSpec`] per exit in [`ExitId`](crate::types::ExitId)
/// order, the runtime-cache slots the code reads, where each recorded
/// opline's code begins, and what a profiler names the trace by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Compiled {
    pub(crate) bytes: Vec<u8>,
    /// The offset in `bytes` the trace is entered at.
    pub(crate) entry_offset: usize,
    /// A power of two.
    pub(crate) align: usize,
    pub(crate) exits: Box<[ExitSpec]>,
    /// The slots of the requests' runtime caches the code reads, which
    /// `enter` lays down on a request's first entry.
    pub(crate) map_ptrs: Box<[MapPtrRaw]>,
    pub(crate) marks: Box<[Mark]>,
    /// php's `zend_jit_trace_name` but the number, which the table
    /// assigns: the job's, named where the trace's `TRACE start` line is.
    pub(crate) name: TraceName,
}

/// Where one recorded opline's code begins: the op array, named with
/// its function since a trace inlines calls, the opline, and the offset
/// in the trace's bytes. What a profiler and a disassembly read; it costs
/// nothing in the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) func: FuncRef,
    pub(crate) opline: OplineIdx,
    pub(crate) offset: u32,
}

/// Why a host compiles no trace (`jit-design.md` §4.3): Cranelift has no
/// ISA for it, or none this build carries, or it is not a 64-bit
/// host. The tier then runs with `Mode::Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unsupported(pub(crate) &'static str);

/// What compiled code answers where a kernel answered other than zero
/// and the opline that called it names no exit: past every
/// [`ExitId`](crate::types::ExitId), which is sixteen bits. `enter` reads
/// what the kernel kept in the `ExitCtx`, a fatal or a panic to resume.
pub(crate) const ABANDONED: u32 = u32::MAX;

/// Where compiled code reads the interrupt pointer in the `ExitCtx`
/// (`jit-design.md` §4.6): the request at offset zero, the pointer to its
/// interrupt byte at offset eight, the field `enter` lays down with the
/// first trace it enters (#2957).
pub(crate) const INTERRUPT_AT: i32 = 8;

/// A kernel compiled code calls (`jit-design.md` §4.6, §5.2), by the
/// wrapper `enter`'s `kernels!` list makes of it. Every wrapper is
/// `extern "C"` on the host's convention, takes the `ExitCtx`, the room
/// of the frame the opline runs in and the opline's index first, and
/// answers the crossing's status word; where it writes a value, the
/// pointer it writes through is its last argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kernel {
    /// `step(cx, frame, opline: u32, stood: *mut Stood)`: the opline
    /// served whole through `php_engine::step` (§5.2), and where the
    /// interpreter stands after it written to `stood`.
    Step,
    /// `push_call(cx, frame, opline: u32, callee: u32, stood: *mut
    /// Stood)`: an inline `INIT_*`, served as its handler serves it where
    /// its callee resolves to the op array the engine numbers `callee`;
    /// where it resolves to another, nothing is pushed and `stood.call`
    /// is null.
    PushCall,
    /// `release(cx, frame, opline: u32, word: u64, type_info: u32)`:
    /// php's `zval_ptr_dtor` of the counted value the zval names, a
    /// destructor's throw answered as one.
    Release,
    /// `copy(cx, frame, opline: u32, word: u64, type_info: u32, out: *mut
    /// Zval)`: php's `ZVAL_COPY_DEREF` of the value the zval names into
    /// `out`.
    Copy,
}

impl Kernel {
    /// Every kernel, in the order [`Symbols`] keeps them.
    pub(crate) const ALL: [Kernel; 4] = [
        Kernel::Step,
        Kernel::PushCall,
        Kernel::Release,
        Kernel::Copy,
    ];

    /// The kernel's place in [`Kernel::ALL`].
    const fn index(self) -> usize {
        self as usize
    }
}

/// The kernel symbol table (`jit-design.md` §4.3): the address of each
/// kernel's wrapper, which compiled code calls as a 64-bit constant, so
/// its bytes are final. `enter`'s `kernels!` list makes the one the
/// engine's `Backend` is built with (#2957).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Symbols([usize; Kernel::ALL.len()]);

impl Symbols {
    /// The table of the addresses `address` names, one per kernel.
    pub(crate) fn new(address: impl Fn(Kernel) -> usize) -> Symbols {
        Symbols(Kernel::ALL.map(address))
    }

    /// The address of `kernel`'s wrapper.
    pub(crate) fn address(&self, kernel: Kernel) -> usize {
        self.0[kernel.index()]
    }
}

/// Where the interpreter stands after a kernel served an opline whole
/// ([`Kernel::Step`], [`Kernel::PushCall`]): the room of the frame that
/// runs, php's `EX`; the room of the call under way in it, `EX(call)`,
/// null where none is; the op array it serves, by the engine offset that
/// numbers it (`FuncRef`); and the opline it goes on at. A local of
/// compiled code the kernel writes, by which compiled code follows the
/// frame chain: the call it enters, the return it lands from, the call
/// its sends write, the caller php's IP guard holds, and the way a
/// stepped jump went.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stood {
    pub(crate) frame: *mut u8,
    pub(crate) call: *mut u8,
    pub(crate) func: u32,
    pub(crate) opline: u32,
}

/// The host's `Backend` (`jit-design.md` §4.3): its ISA with the flags the
/// [`Policy`] asks for, and the kernel table compiled code calls. It holds
/// no state of one compile, so one serves every compile of the engine.
pub(crate) struct Backend {
    isa: OwnedTargetIsa,
    symbols: Symbols,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("isa", &self.isa.triple().to_string())
            .finish_non_exhaustive()
    }
}

// The worker compiles on its own thread with the engine's one backend
// (`jit-design.md` §4.3, §4.8), which the design left unverified.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<Backend>();
};

/// How a host's ISA is found: `cranelift-native`'s lookup on a real host.
type Host = fn() -> Result<isa::Builder, &'static str>;

impl Backend {
    /// The host's `Backend`, its ISA found through `cranelift-native`, with the
    /// flags `policy` asks for, calling the kernels `symbols` names; or
    /// `Unsupported` where the host has none, which runs the tier with
    /// `Mode::Off` (`jit-design.md` §4).
    pub(crate) fn new(policy: &Policy, symbols: Symbols) -> Result<Backend, Unsupported> {
        Backend::on(cranelift_native::builder, policy, symbols)
    }

    /// The `Backend` of the ISA `host` finds.
    fn on(host: Host, policy: &Policy, symbols: Symbols) -> Result<Backend, Unsupported> {
        let builder = host().map_err(Unsupported)?;
        let isa = builder
            .finish(flags(policy)?)
            .map_err(|_| Unsupported("the host's ISA refused the tier's flags"))?;
        if isa.pointer_bits() != 64 {
            return Err(Unsupported("a 32-bit host"));
        }
        Ok(Backend { isa, symbols })
    }

    /// `ir` in final bytes, named `name`: the function [`clif::lower`]
    /// builds, compiled for the host, with a [`Mark`] per recorded
    /// opline. Bytes that would need relocating are php's
    /// `COMPILER_ERROR`, as is any refusal of Cranelift's: kernel
    /// addresses are constants, so a trace's bytes work wherever the arena
    /// places them.
    pub(crate) fn compile(&self, ir: &TraceIr, name: TraceName) -> Result<Compiled, Abort> {
        let func = self.clif(ir)?;
        let mut ctx = Context::for_function(func);
        let code = ctx
            .compile(&*self.isa, &mut ControlPlane::default())
            .map_err(|_| refused("Cranelift refused the trace's function"))?;
        if !code.buffer.relocs().is_empty() {
            return Err(refused("the trace's code needs relocating"));
        }
        let bytes = code.code_buffer().to_vec();
        let align = usize::try_from(code.buffer.alignment.max(1))
            .map_err(|_| refused("an alignment past the host's"))?;
        let marks = marks(ir, code.buffer.get_srclocs_sorted(), bytes.len());
        Ok(Compiled {
            bytes,
            entry_offset: 0,
            align,
            exits: ir.exits.clone().into_boxed_slice(),
            map_ptrs: ir.map_ptrs.clone().into_boxed_slice(),
            marks,
            name,
        })
    }

    /// The function `ir` lowers to, before Cranelift's passes: the CLIF
    /// the goldens hold.
    fn clif(&self, ir: &TraceIr) -> Result<Function, Abort> {
        clif::lower(ir, self.isa.frontend_config(), &self.symbols)
    }
}

/// The ISA's flags (`jit-design.md` §4.3): optimised for speed, the
/// verifier where assertions are, and the frame pointers a profiler walks
/// where `opcache.jit_debug` asks for `PERF` or `PERF_DUMP`.
fn flags(policy: &Policy) -> Result<settings::Flags, Unsupported> {
    let mut flags = settings::builder();
    let mut set = |name: &str, value: &str| {
        flags
            .set(name, value)
            .map_err(|_| Unsupported("Cranelift lacks a flag the tier sets"))
    };
    set("opt_level", "speed")?;
    set(
        "enable_verifier",
        if cfg!(debug_assertions) {
            "true"
        } else {
            "false"
        },
    )?;
    if policy.debug().any(JitDebug::PERF | JitDebug::PERF_DUMP) {
        set("preserve_frame_pointers", "true")?;
    }
    Ok(settings::Flags::new(flags))
}

/// php's `COMPILER_ERROR`, for a trace Cranelift would not compile.
fn refused(why: &'static str) -> Abort {
    Abort {
        stop: Stop::CompilerError,
        why,
    }
}

/// Where each recorded opline's code begins: the lowest offset of the
/// code its op left, and where Cranelift folded an op's code away, where
/// the next op's begins.
fn marks(ir: &TraceIr, srclocs: &[MachSrcLoc<Final>], len: usize) -> Box<[Mark]> {
    let end = u32::try_from(len).unwrap_or(u32::MAX);
    let mut starts = vec![None::<u32>; ir.ops.len()];
    for loc in srclocs {
        if loc.loc.is_default() {
            continue;
        }
        if let Some(start) = starts.get_mut(loc.loc.bits() as usize) {
            *start = Some(start.map_or(loc.start, |s| s.min(loc.start)));
        }
    }
    let mut next = end;
    for start in starts.iter_mut().rev() {
        next = *start.get_or_insert(next);
    }
    ir.ops
        .iter()
        .zip(starts)
        .filter_map(|(op, start)| match op {
            Op::Line(line) => Some(Mark {
                func: ir.frames.get(usize::from(line.frame.0))?.func,
                opline: line.opline,
                offset: start.unwrap_or(end),
            }),
            _ => None,
        })
        .collect()
}

/// The source location compiled code carries for the op at `at`, which
/// [`marks`] reads back.
fn located(at: usize) -> SourceLoc {
    SourceLoc::new(u32::try_from(at).unwrap_or(u32::MAX - 1))
}
