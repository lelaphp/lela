//! The C side of `FFI`, bound by hand (design §7.2, revision 75; #1573):
//! libffi as the platform ships it, which the oracle links as
//! `/usr/lib/libffi.dylib` and whose declarations are the SDK's `ffi.h`;
//! the dynamic loader for `FFI::cdef($code, $lib)` and `FFI::load`; and
//! the raw reads and writes every `FFI\CData` makes of C memory. Every
//! address here is one PHP code handed FFI, which php holds to no bound
//! either: FFI's contract is the C program's, and a wrong address faults
//! here as it faults in php. The one home of this crate's `unsafe`
//! (`unsafe.homes`' `ext/*/src/ffi/`), one operation per block.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, c_char, c_uint, c_ushort, c_void};
use std::num::NonZeroUsize;
use std::sync::Arc;

use php_ext::request::{DEFERRED_WORDS, Deferred, Lent, Reentry, Request};

mod sys {
    use std::ffi::{c_int, c_uint, c_ushort, c_void};

    /// `ffi_type`: a C type as libffi lays its arguments out.
    #[repr(C)]
    pub(super) struct Type {
        pub(super) size: usize,
        pub(super) alignment: c_ushort,
        pub(super) kind: c_ushort,
        pub(super) elements: *mut *mut Type,
    }

    /// `ffi_cif`, with room past it for `FFI_EXTRA_CIF_FIELDS`
    /// (`aarch64_nfixedargs` on Apple's arm64), which `ffi_prep_cif`
    /// writes and nothing here reads.
    #[repr(C)]
    pub(super) struct Cif {
        pub(super) abi: c_int,
        pub(super) nargs: c_uint,
        pub(super) arg_types: *mut *mut Type,
        pub(super) rtype: *mut Type,
        pub(super) bytes: c_uint,
        pub(super) flags: c_uint,
        pub(super) extra: [c_uint; 4],
    }

    /// The function a closure's code enters, `fun` of `ffi_prep_closure_loc`.
    pub(super) type Handler = extern "C" fn(*mut Cif, *mut c_void, *mut *mut c_void, *mut c_void);

    unsafe extern "C" {
        pub(super) static ffi_type_void: Type;
        pub(super) static ffi_type_uint8: Type;
        pub(super) static ffi_type_sint8: Type;
        pub(super) static ffi_type_uint16: Type;
        pub(super) static ffi_type_sint16: Type;
        pub(super) static ffi_type_uint32: Type;
        pub(super) static ffi_type_sint32: Type;
        pub(super) static ffi_type_uint64: Type;
        pub(super) static ffi_type_sint64: Type;
        pub(super) static ffi_type_float: Type;
        pub(super) static ffi_type_double: Type;
        pub(super) static ffi_type_pointer: Type;
        pub(super) fn ffi_prep_cif(
            cif: *mut Cif,
            abi: c_int,
            nargs: c_uint,
            rtype: *mut Type,
            atypes: *mut *mut Type,
        ) -> c_int;
        pub(super) fn ffi_prep_cif_var(
            cif: *mut Cif,
            abi: c_int,
            nfixed: c_uint,
            ntotal: c_uint,
            rtype: *mut Type,
            atypes: *mut *mut Type,
        ) -> c_int;
        pub(super) fn ffi_call(
            cif: *mut Cif,
            target: *const c_void,
            rvalue: *mut c_void,
            avalue: *mut *mut c_void,
        );
        pub(super) fn ffi_closure_alloc(size: usize, code: *mut *mut c_void) -> *mut c_void;
        pub(super) fn ffi_closure_free(closure: *mut c_void);
        pub(super) fn ffi_prep_closure_loc(
            closure: *mut c_void,
            cif: *mut Cif,
            fun: Handler,
            user: *mut c_void,
            code: *mut c_void,
        ) -> c_int;
    }
}

/// `FFI_DEFAULT_ABI`: `FFI_SYSV` on arm64, `FFI_UNIX64` on x86-64.
#[cfg(target_arch = "aarch64")]
pub(crate) const DEFAULT_ABI: i32 = 1;
/// `FFI_DEFAULT_ABI`: `FFI_SYSV` on arm64, `FFI_UNIX64` on x86-64.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const DEFAULT_ABI: i32 = 2;

/// `FFI_TYPE_STRUCT`.
const STRUCT: c_ushort = 13;

/// `sizeof(ffi_closure)` with room to spare on every target libffi
/// lays one out on.
const CLOSURE_SIZE: usize = 64;

/// A value C code holds in memory, read and written as php's `*(T*)ptr`.
pub(crate) trait Plain: Copy {}

impl Plain for u8 {}
impl Plain for i8 {}
impl Plain for u16 {}
impl Plain for i16 {}
impl Plain for u32 {}
impl Plain for i32 {}
impl Plain for u64 {}
impl Plain for i64 {}
impl Plain for f32 {}
impl Plain for f64 {}
impl Plain for usize {}

/// `*(T*)addr`, unaligned as a packed field is laid out.
pub(crate) fn read<T: Plain>(addr: usize) -> T {
    // SAFETY: `addr` is C memory a `FFI\CData` names or a C call handed
    // back, which FFI reads wherever PHP code points it, as php does.
    unsafe { std::ptr::read_unaligned(addr as *const T) }
}

/// `*(T*)addr = value`, unaligned as a packed field is laid out.
pub(crate) fn write<T: Plain>(addr: usize, value: T) {
    // SAFETY: `addr` is C memory a `FFI\CData` names, which FFI writes
    // wherever PHP code points it, as php does.
    unsafe { std::ptr::write_unaligned(addr as *mut T, value) }
}

/// `memcpy(dst, src, len)`, overlapping ranges allowed.
pub(crate) fn copy(dst: usize, src: usize, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: both ranges are C memory PHP code named, as `FFI::memcpy`
    // takes them; `copy` allows them to overlap.
    unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) }
}

/// `memset(dst, byte, len)`.
pub(crate) fn fill(dst: usize, byte: u8, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: the range is C memory PHP code named, as `FFI::memset`
    // takes it.
    unsafe { std::ptr::write_bytes(dst as *mut u8, byte, len) }
}

/// `memcmp(a, b, len)`'s sign.
pub(crate) fn compare(a: usize, b: usize, len: usize) -> i32 {
    // SAFETY: both ranges are memory PHP code named, as `FFI::memcmp`
    // takes them.
    unsafe { libc::memcmp(a as *const c_void, b as *const c_void, len) }
}

/// The `len` bytes at `addr`, copied out.
pub(crate) fn bytes(addr: usize, len: usize) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    // SAFETY: the range is C memory PHP code named, as `FFI::string`
    // with a size reads it.
    unsafe { std::slice::from_raw_parts(addr as *const u8, len) }.to_vec()
}

/// The NUL-terminated string at `addr`, copied out without its NUL.
pub(crate) fn c_string(addr: usize) -> Vec<u8> {
    // SAFETY: `addr` names a C string, as php's `ZVAL_STRING(rv,
    // *(char**)ptr)` reads one.
    unsafe { CStr::from_ptr(addr as *const c_char) }
        .to_bytes()
        .to_vec()
}

/// Memory of the process's: `calloc`'s, zeroed as `FFI::new` zeroes
/// it, aligned for any C type, and freed when the owner goes.
#[derive(Debug)]
pub(crate) struct Block {
    addr: NonZeroUsize,
}

impl Block {
    /// `size` zeroed bytes, at least one; none where the allocator has
    /// none to give.
    pub(crate) fn zeroed(size: usize) -> Option<Block> {
        // SAFETY: `calloc` takes any count and size and answers null or
        // memory of its own.
        let addr = unsafe { libc::calloc(1, size.max(1)) } as usize;
        NonZeroUsize::new(addr).map(|addr| Block { addr })
    }

    /// Where the block stands.
    pub(crate) fn addr(&self) -> usize {
        self.addr.get()
    }

    /// The block given up unfreed: its address, which `FFI::free` frees.
    pub(crate) fn release(self) -> usize {
        let addr = self.addr.get();
        #[expect(
            clippy::mem_forget,
            reason = "the block is given up unfreed, and its address is `FFI::free`'s to free"
        )]
        std::mem::forget(self);
        addr
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        free(self.addr.get());
    }
}

/// `free(addr)`: what `FFI::free` hands back of the process's memory.
pub(crate) fn free(addr: usize) {
    // SAFETY: `addr` is memory `calloc` gave a block, or one PHP code
    // names as such, which `FFI::free` frees as php's `pefree` does.
    unsafe { libc::free(addr as *mut c_void) }
}

/// A library `dlopen` loaded, `DL_LOAD(lib)`, closed with its last
/// owner as `DL_UNLOAD` closes it.
#[derive(Debug)]
pub(crate) struct Library {
    handle: NonZeroUsize,
}

impl Library {
    /// `dlopen(path, RTLD_LAZY | RTLD_GLOBAL)`, php's `DL_LOAD`, or
    /// `dlerror()`'s sentence.
    pub(crate) fn open(path: &[u8]) -> Result<Library, String> {
        let path = c_bytes(path);
        // SAFETY: `path` is NUL-terminated and stands across the call.
        let handle =
            unsafe { libc::dlopen(path.as_ptr().cast(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
        match NonZeroUsize::new(handle as usize) {
            Some(handle) => Ok(Library { handle }),
            None => Err(last_error()),
        }
    }

    /// `dlsym(handle, name)`.
    pub(crate) fn symbol(&self, name: &[u8]) -> Option<usize> {
        lookup(self.handle.get() as *mut c_void, name)
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the handle is `dlopen`'s, closed once, here.
        unsafe { libc::dlclose(self.handle.get() as *mut c_void) };
    }
}

/// `dlsym(RTLD_DEFAULT, name)`: a symbol of the process, php's lookup
/// for a declaration with no library.
pub(crate) fn default_symbol(name: &[u8]) -> Option<usize> {
    lookup(libc::RTLD_DEFAULT, name)
}

fn lookup(handle: *mut c_void, name: &[u8]) -> Option<usize> {
    let name = c_bytes(name);
    // SAFETY: `name` is NUL-terminated and stands across the call;
    // `handle` is `dlopen`'s or `RTLD_DEFAULT`.
    let addr = unsafe { libc::dlsym(handle, name.as_ptr().cast()) } as usize;
    (addr != 0).then_some(addr)
}

/// `dlerror()`, `(null)` where it has none, as php's `%s` prints it.
fn last_error() -> String {
    // SAFETY: `dlerror` answers null or a string of its own.
    let text = unsafe { libc::dlerror() };
    if text.is_null() {
        return String::from("(null)");
    }
    // SAFETY: a non-null `dlerror` is a NUL-terminated string standing
    // until the next call into the loader.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

/// `ZSTR_VAL(s)` as C reads it: up to the first NUL, with one after.
fn c_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut owned = php_ext::coerce::c_str(bytes).to_vec();
    owned.push(0);
    owned
}

/// How libffi passes one value: `zend_ffi_get_type`'s answer, a struct
/// as `zend_ffi_make_fake_struct_type` flattens it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Layout {
    Void,
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    Float,
    Double,
    Pointer,
    Struct {
        size: usize,
        align: u16,
        elements: Vec<Layout>,
    },
}

impl Layout {
    /// The bytes a value of the layout takes, `ffi_type.size`.
    fn size(&self) -> usize {
        match self {
            Layout::Void => 0,
            Layout::U8 | Layout::I8 => 1,
            Layout::U16 | Layout::I16 => 2,
            Layout::U32 | Layout::I32 | Layout::Float => 4,
            Layout::U64 | Layout::I64 | Layout::Double | Layout::Pointer => 8,
            Layout::Struct { size, .. } => *size,
        }
    }
}

/// A call interface libffi prepared, `ffi_cif`, with the types it
/// points at kept beside it for as long as it stands.
#[allow(
    clippy::vec_box,
    reason = "each node's address is in libffi's element lists, so it must not move"
)]
pub(crate) struct Signature {
    cif: Box<sys::Cif>,
    args: Box<[*mut sys::Type]>,
    nodes: Vec<Box<sys::Type>>,
    lists: Vec<Box<[*mut sys::Type]>>,
    arity: usize,
    /// Each argument's size: what a deferred call copies.
    sizes: Box<[usize]>,
    /// What a deferred call zeroes of the return: a struct's size, a
    /// scalar's widened to `ffi_arg` as libffi's buffer holds it, and
    /// nothing for void.
    returns: usize,
}

// SAFETY: every pointer a signature holds is into its own boxes or at
// libffi's immutable `ffi_type_*` statics, which no thread writes.
unsafe impl Send for Signature {}

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signature")
            .field("arity", &self.arity)
            .finish_non_exhaustive()
    }
}

impl Signature {
    /// `ffi_prep_cif`, or `ffi_prep_cif_var` with `fixed` arguments
    /// before the variadic ones; none where libffi refuses it.
    pub(crate) fn prepare(
        abi: i32,
        ret: &Layout,
        args: &[Layout],
        fixed: Option<usize>,
    ) -> Option<Signature> {
        let mut nodes = Vec::new();
        let mut lists = Vec::new();
        let rtype = place(ret, &mut nodes, &mut lists);
        let mut arg_types: Box<[*mut sys::Type]> = args
            .iter()
            .map(|layout| place(layout, &mut nodes, &mut lists))
            .collect();
        let mut cif = Box::new(sys::Cif {
            abi: 0,
            nargs: 0,
            arg_types: std::ptr::null_mut(),
            rtype: std::ptr::null_mut(),
            bytes: 0,
            flags: 0,
            extra: [0; 4],
        });
        let total = c_uint::try_from(args.len()).ok()?;
        let atypes = if arg_types.is_empty() {
            std::ptr::null_mut()
        } else {
            arg_types.as_mut_ptr()
        };
        let status = match fixed {
            Some(fixed) => {
                let fixed = c_uint::try_from(fixed).ok()?;
                // SAFETY: the cif, the return type and the argument types
                // stand in the signature for as long as the cif does.
                unsafe { sys::ffi_prep_cif_var(&raw mut *cif, abi, fixed, total, rtype, atypes) }
            }
            // SAFETY: as above.
            None => unsafe { sys::ffi_prep_cif(&raw mut *cif, abi, total, rtype, atypes) },
        };
        let returns = match ret {
            Layout::Void => 0,
            Layout::Struct { size, .. } => *size,
            scalar => scalar.size().max(8),
        };
        (status == 0).then_some(Signature {
            cif,
            args: arg_types,
            nodes,
            lists,
            arity: args.len(),
            sizes: args.iter().map(Layout::size).collect(),
            returns,
        })
    }
}

/// The `ffi_type` a layout is: one of libffi's statics, or a struct's
/// laid down in `nodes` with its element list in `lists`.
#[allow(
    clippy::vec_box,
    reason = "each node's address is in libffi's element lists, so it must not move"
)]
fn place(
    layout: &Layout,
    nodes: &mut Vec<Box<sys::Type>>,
    lists: &mut Vec<Box<[*mut sys::Type]>>,
) -> *mut sys::Type {
    let fixed = match layout {
        Layout::Void => &raw const sys::ffi_type_void,
        Layout::U8 => &raw const sys::ffi_type_uint8,
        Layout::I8 => &raw const sys::ffi_type_sint8,
        Layout::U16 => &raw const sys::ffi_type_uint16,
        Layout::I16 => &raw const sys::ffi_type_sint16,
        Layout::U32 => &raw const sys::ffi_type_uint32,
        Layout::I32 => &raw const sys::ffi_type_sint32,
        Layout::U64 => &raw const sys::ffi_type_uint64,
        Layout::I64 => &raw const sys::ffi_type_sint64,
        Layout::Float => &raw const sys::ffi_type_float,
        Layout::Double => &raw const sys::ffi_type_double,
        Layout::Pointer => &raw const sys::ffi_type_pointer,
        Layout::Struct {
            size,
            align,
            elements,
        } => {
            let mut list: Box<[*mut sys::Type]> = elements
                .iter()
                .map(|element| place(element, nodes, lists))
                .chain(std::iter::once(std::ptr::null_mut()))
                .collect();
            let mut node = Box::new(sys::Type {
                size: *size,
                alignment: *align,
                kind: STRUCT,
                elements: list.as_mut_ptr(),
            });
            let at = &raw mut *node;
            lists.push(list);
            nodes.push(node);
            return at;
        }
    };
    fixed.cast_mut()
}

/// `ffi_call(&cif, target, ret, args)`: the C function at `target`
/// called with the argument values at `args`, the return written into
/// `ret`. The request is lent while C runs ([`Limits::blocking`]), for
/// a callback C makes back into PHP code, and taken back once C returns.
pub(crate) fn call(
    req: &mut Request,
    sig: &mut Signature,
    target: usize,
    args: &[usize],
    ret: &mut [u64],
) {
    let mut values: Vec<*mut c_void> = args.iter().map(|addr| *addr as *mut c_void).collect();
    let values = if values.is_empty() {
        std::ptr::null_mut()
    } else {
        values.as_mut_ptr()
    };
    let cif = &raw mut *sig.cif;
    let ret = ret.as_mut_ptr().cast();
    req.limits_mut().blocking(|| {
        // SAFETY: the cif was prepared for exactly these argument values,
        // each standing across the call, and `ret` holds at least
        // `sizeof(ffi_arg)` and the return type's size; `target` is the
        // function the declaration names, as php calls it.
        unsafe { sys::ffi_call(cif, target as *const c_void, ret, values) };
    });
    let _ = (&sig.args, &sig.nodes, &sig.lists);
}

/// What a closure's code hands back to PHP code: the request lent to
/// it, the callback's own number, where to write the return (zero where
/// nobody reads it), and each argument's address.
pub(crate) type Callback = fn(&mut Request, usize, usize, &[usize]);

/// What a closure's code reaches through `user_data`: the request's
/// [`Reentry`], the thread the request is served on, and where each
/// argument goes in a [`Deferred`] call, in words.
struct Thunk {
    reentry: Arc<Reentry>,
    thread: usize,
    callback: Callback,
    data: usize,
    arity: usize,
    sizes: Box<[usize]>,
    offsets: Box<[usize]>,
    words: usize,
    returns: usize,
}

/// The calling thread, `pthread_self()`.
fn thread() -> usize {
    // SAFETY: `pthread_self` takes nothing and answers the calling
    // thread, from any context.
    unsafe { libc::pthread_self() as usize }
}

/// A C function pointer libffi made of a PHP callable,
/// `ffi_closure_alloc` and `ffi_prep_closure_loc`, freed with its owner.
pub(crate) struct Closure {
    writable: usize,
    code: usize,
    sig: Signature,
    thunk: Box<Thunk>,
}

impl std::fmt::Debug for Closure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Closure")
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

/// Why a closure could not be made: `ffi_closure_alloc` had none, or
/// `ffi_prep_closure_loc` refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClosureError {
    Alloc,
    Prepare,
}

impl Closure {
    /// A closure over `sig` whose code calls `callback` with `data`,
    /// made on the thread its request is served on.
    pub(crate) fn new(
        sig: Signature,
        reentry: Arc<Reentry>,
        callback: Callback,
        data: usize,
    ) -> Result<Closure, ClosureError> {
        let mut code: *mut c_void = std::ptr::null_mut();
        // SAFETY: `ffi_closure_alloc` writes the executable address into
        // `code` and answers the writable one or null.
        let writable = unsafe { sys::ffi_closure_alloc(CLOSURE_SIZE, &raw mut code) };
        if writable.is_null() {
            return Err(ClosureError::Alloc);
        }
        let arity = sig.arity;
        let sizes = sig.sizes.clone();
        let mut offsets = Vec::with_capacity(sizes.len());
        let mut words = 0;
        for size in &sizes {
            offsets.push(words);
            words += size.div_ceil(8);
        }
        let returns = sig.returns;
        let mut closure = Closure {
            writable: writable as usize,
            code: code as usize,
            sig,
            thunk: Box::new(Thunk {
                reentry,
                thread: thread(),
                callback,
                data,
                arity,
                sizes,
                offsets: offsets.into(),
                words,
                returns,
            }),
        };
        let user = std::ptr::from_ref::<Thunk>(&closure.thunk)
            .cast_mut()
            .cast::<c_void>();
        // SAFETY: the closure, its cif and its thunk stand together until
        // the closure is dropped, which frees the code first.
        let status = unsafe {
            sys::ffi_prep_closure_loc(writable, &raw mut *closure.sig.cif, enter, user, code)
        };
        if status != 0 {
            return Err(ClosureError::Prepare);
        }
        Ok(closure)
    }

    /// The executable address C calls.
    pub(crate) fn code(&self) -> usize {
        self.code
    }

    /// Each argument's address in `deferred`, where [`enter`] copied it.
    pub(crate) fn args_in(&self, deferred: &mut Deferred) -> Vec<usize> {
        let base = std::ptr::from_mut(&mut deferred.words).expose_provenance();
        self.thunk
            .offsets
            .iter()
            .map(|offset| base + offset * 8)
            .collect()
    }
}

impl Drop for Closure {
    fn drop(&mut self) {
        // SAFETY: the writable address is `ffi_closure_alloc`'s, freed
        // once, here.
        unsafe { sys::ffi_closure_free(self.writable as *mut c_void) };
    }
}

/// `zend_ffi_callback_trampoline`'s entry from C: the thunk and the
/// argument addresses. Where the request is lent, during a call into C
/// or a native that blocks, the callback takes it and calls PHP code at
/// once, as php does. At any other time the request runs PHP code, and
/// C called it from a signal handler that interrupted the request's own
/// thread, or from another thread. The first is deferred, its arguments
/// copied and the return zeroed for C, and served at the request's
/// interrupt check; nothing in it allocates, so a signal handler may
/// enter it. The second is posted with its arguments where C passed
/// them and the thread held until the request serves it at its check,
/// or lends itself, or ends, when C reads zero (#1640): php calls the
/// callable on that thread at once, concurrently with the request, and
/// a call served at the request's next check is one interleaving that
/// race allows.
extern "C" fn enter(
    _cif: *mut sys::Cif,
    ret: *mut c_void,
    args: *mut *mut c_void,
    user: *mut c_void,
) {
    // SAFETY: `user` is the thunk the closure was prepared with, which
    // stands as long as the closure's code can be entered.
    let thunk = unsafe { &*user.cast::<Thunk>() };
    let addrs: &[*mut c_void] = if thunk.arity == 0 || args.is_null() {
        &[]
    } else {
        // SAFETY: libffi hands one argument address per argument of the
        // cif the closure was prepared with.
        unsafe { std::slice::from_raw_parts(args, thunk.arity) }
    };
    if let Some(lent) = thunk.reentry.take_lent() {
        serve_lent(&lent, thunk.callback, thunk.data, ret, addrs);
        thunk.reentry.give_back(lent);
        return;
    }
    if thunk.returns != 0 {
        // SAFETY: libffi's return buffer holds a struct's size, or a
        // scalar widened to `ffi_arg`, which `returns` is.
        unsafe { std::ptr::write_bytes(ret.cast::<u8>(), 0, thunk.returns) };
    }
    if thread() != thunk.thread {
        // The thunk may be freed with the request while this thread
        // waits, so nothing past here reads it.
        let reentry = Arc::clone(&thunk.reentry);
        let waiting = Waiting {
            callback: thunk.callback,
            data: thunk.data,
            ret: ret as usize,
            args: addrs.iter().map(|addr| *addr as usize).collect(),
        };
        if let Some(call) = NonZeroUsize::new(std::ptr::from_ref(&waiting).expose_provenance()) {
            reentry.wait(call, |lent| {
                serve_lent(lent, waiting.callback, waiting.data, ret, addrs);
            });
        }
        return;
    }
    if thunk.words > DEFERRED_WORDS {
        return;
    }
    thunk.reentry.defer(|deferred| {
        deferred.callback = thunk.data;
        let base = std::ptr::from_mut(&mut deferred.words).cast::<u8>();
        for ((addr, size), offset) in addrs.iter().zip(&thunk.sizes).zip(&thunk.offsets) {
            // SAFETY: libffi's argument address holds a value of the
            // argument's size; it lands at its offset in the record's
            // words, which `words`, checked above, says it fits in.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    addr.cast::<u8>(),
                    base.wrapping_add(offset * 8),
                    *size,
                );
            }
        }
    });
}

/// The callback called with the request a lend exposed, on the thread
/// C called it on.
fn serve_lent(
    lent: &Lent,
    callback: Callback,
    data: usize,
    ret: *mut c_void,
    addrs: &[*mut c_void],
) {
    let addrs: Vec<usize> = addrs.iter().map(|addr| *addr as usize).collect();
    // SAFETY: the lend exposed the request's address from the `&mut
    // Request` its waiter holds and uses no further until it takes the
    // request back, which waits for `give_back`. Taking it handed it to
    // this callback alone, so this is the one reference to the request
    // until then.
    let req = unsafe { &mut *std::ptr::with_exposed_provenance_mut::<Request>(lent.at().get()) };
    callback(req, data, ret as usize, &addrs);
}

/// A call C made from another thread while the request ran PHP code,
/// on that thread's stack while it waits in [`Reentry::wait`]: the
/// callback, where C reads its return, and each argument's address,
/// all C's own until the thread returns to it.
struct Waiting {
    callback: Callback,
    data: usize,
    ret: usize,
    args: Vec<usize>,
}

/// The call another thread waits on at `call`, served on the request's
/// thread at its interrupt check ([`Reentry::serve_waiting`]).
pub(crate) fn serve_waiting(req: &mut Request, call: NonZeroUsize) {
    // SAFETY: `call` is the address of a `Waiting` its thread exposed
    // and keeps standing, unchanged, until `Reentry::wait` returns,
    // which is not before this call is answered.
    let waiting = unsafe { &*std::ptr::with_exposed_provenance::<Waiting>(call.get()) };
    (waiting.callback)(req, waiting.data, waiting.ret, &waiting.args);
}

/// `ZEND_FFI_SIZEOF_ARG`: the room each argument value takes.
pub(crate) const ARG_SIZE: usize = 16;
