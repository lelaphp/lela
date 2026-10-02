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

use php_ext::request::Request;

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
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let mut owned = bytes.get(..end).unwrap_or_default().to_vec();
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
        (status == 0).then_some(Signature {
            cif,
            args: arg_types,
            nodes,
            lists,
            arity: args.len(),
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
/// `ret`. `slot` holds the request while C runs, for a callback C makes
/// back into PHP code, and what it held before once C returns.
pub(crate) fn call(
    req: &mut Request,
    slot: usize,
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
    let before = read::<usize>(slot);
    write(slot, std::ptr::from_mut(req) as usize);
    // SAFETY: the cif was prepared for exactly these argument values,
    // each standing across the call, and `ret` holds at least
    // `sizeof(ffi_arg)` and the return type's size; `target` is the
    // function the declaration names, as php calls it.
    unsafe {
        sys::ffi_call(
            &raw mut *sig.cif,
            target as *const c_void,
            ret.as_mut_ptr().cast(),
            values,
        );
    }
    write(slot, before);
    let _ = (&sig.args, &sig.nodes, &sig.lists);
}

/// What a closure's code hands back to PHP code: the request the call
/// into C came from, read off `slot`, the callback's own number, where
/// to write the return, and each argument's address.
pub(crate) type Callback = fn(&mut Request, usize, usize, &[usize]);

/// What a closure's code reaches through `user_data`.
struct Thunk {
    slot: usize,
    callback: Callback,
    data: usize,
    arity: usize,
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
    /// A closure over `sig` whose code calls `callback` with `data`.
    pub(crate) fn new(
        sig: Signature,
        slot: usize,
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
        let mut closure = Closure {
            writable: writable as usize,
            code: code as usize,
            sig,
            thunk: Box::new(Thunk {
                slot,
                callback,
                data,
                arity,
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
}

impl Drop for Closure {
    fn drop(&mut self) {
        // SAFETY: the writable address is `ffi_closure_alloc`'s, freed
        // once, here.
        unsafe { sys::ffi_closure_free(self.writable as *mut c_void) };
    }
}

/// `zend_ffi_callback_trampoline`'s entry from C: the thunk, the
/// argument addresses, and the request the call into C was made from.
extern "C" fn enter(
    _cif: *mut sys::Cif,
    ret: *mut c_void,
    args: *mut *mut c_void,
    user: *mut c_void,
) {
    // SAFETY: `user` is the thunk the closure was prepared with, which
    // stands as long as the closure's code can be entered.
    let thunk = unsafe { &*user.cast::<Thunk>() };
    let addrs: Vec<usize> = if thunk.arity == 0 || args.is_null() {
        Vec::new()
    } else {
        // SAFETY: libffi hands one argument address per argument of the
        // cif the closure was prepared with.
        unsafe { std::slice::from_raw_parts(args, thunk.arity) }
            .iter()
            .map(|addr| *addr as usize)
            .collect()
    };
    let request = read::<usize>(thunk.slot);
    if request == 0 {
        return;
    }
    // SAFETY: the slot holds the request whose call into C is on this
    // thread's stack, set by `call` around `ffi_call`; the request is
    // reached through nothing else until C returns.
    let req = unsafe { &mut *(request as *mut Request) };
    (thunk.callback)(req, thunk.data, ret as usize, &addrs);
}

/// `ZEND_FFI_SIZEOF_ARG`: the room each argument value takes.
pub(crate) const ARG_SIZE: usize = 16;
