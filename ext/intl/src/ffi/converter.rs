//! `UConverter` (`ucnv.h`, `ucnv_err.h`), the C API php's converter
//! calls. ICU calls a converter's two callbacks from inside a conversion,
//! for its reset and each character in error; [`Callbacks`] serves them
//! for the span of one such call, and ICU's own are put back after it, so
//! a converter at rest holds ICU's defaults and no pointer into a frame.
//! What ICU tells the callbacks as it closes or clones a converter, with
//! nothing in error, the caller tells them itself ([`Callbacks::told`]),
//! so a close or a clone never runs with a frame's pointer installed.
//! The converter itself is the request's ([`Converter`], ADR-0013): a
//! conversion that lends its callbacks to PHP code holds it, so code that
//! replaces or frees the object's converter meanwhile leaves the close to
//! the conversion's end.

use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;

use php_ext::request::{Foreign, ForeignId, Held, Request};

use super::cpp::{opaque, owner};
use super::{Code, length};
use sys::ucnv_close;

mod sys {
    use std::ffi::{c_char, c_void};
    use std::ptr::NonNull;

    use super::super::Code;
    pub(crate) use super::UConverter;

    /// `UConverterToUnicodeArgs`.
    #[repr(C)]
    pub(crate) struct ToUArgs {
        pub(crate) size: u16,
        pub(crate) flush: i8,
        pub(crate) converter: *mut UConverter,
        pub(crate) source: *const c_char,
        pub(crate) source_limit: *const c_char,
        pub(crate) target: *mut u16,
        pub(crate) target_limit: *const u16,
        pub(crate) offsets: *mut i32,
    }

    /// `UConverterFromUnicodeArgs`.
    #[repr(C)]
    pub(crate) struct FromUArgs {
        pub(crate) size: u16,
        pub(crate) flush: i8,
        pub(crate) converter: *mut UConverter,
        pub(crate) source: *const u16,
        pub(crate) source_limit: *const u16,
        pub(crate) target: *mut c_char,
        pub(crate) target_limit: *const c_char,
        pub(crate) offsets: *mut i32,
    }

    /// `UConverterToUCallback`.
    pub(crate) type ToU = unsafe extern "C" fn(
        context: *const c_void,
        args: *mut ToUArgs,
        units: *const c_char,
        length: i32,
        reason: i32,
        status: *mut Code,
    );

    /// `UConverterFromUCallback`.
    pub(crate) type FromU = unsafe extern "C" fn(
        context: *const c_void,
        args: *mut FromUArgs,
        units: *const u16,
        length: i32,
        code_point: i32,
        reason: i32,
        status: *mut Code,
    );

    icu! {
        fn ucnv_open(name: *const c_char, status: &mut Code) -> *mut UConverter;
        fn ucnv_close(cnv: NonNull<UConverter>);
        fn ucnv_clone(cnv: *const UConverter, status: &mut Code) -> *mut UConverter;
        fn ucnv_getName(cnv: *const UConverter, status: &mut Code) -> *const c_char;
        fn ucnv_getType(cnv: *const UConverter) -> i32;
        fn ucnv_getSubstChars(cnv: *const UConverter, chars: *mut c_char, len: &mut i8, status: &mut Code);
        fn ucnv_setSubstChars(cnv: *mut UConverter, chars: *const c_char, len: i8, status: &mut Code);
        fn ucnv_toUChars(cnv: *mut UConverter, dest: *mut u16, capacity: i32, src: *const c_char, len: i32, status: &mut Code) -> i32;
        fn ucnv_fromUChars(cnv: *mut UConverter, dest: *mut c_char, capacity: i32, src: *const u16, len: i32, status: &mut Code) -> i32;
        fn ucnv_setToUCallBack(cnv: *mut UConverter, new: Option<ToU>, context: *const c_void, old: &mut Option<ToU>, old_context: &mut *const c_void, status: &mut Code);
        fn ucnv_setFromUCallBack(cnv: *mut UConverter, new: Option<FromU>, context: *const c_void, old: &mut Option<FromU>, old_context: &mut *const c_void, status: &mut Code);
        fn ucnv_countAvailable() -> i32;
        fn ucnv_getAvailableName(n: i32) -> *const c_char;
        fn ucnv_countAliases(alias: *const c_char, status: &mut Code) -> u16;
        fn ucnv_getAlias(alias: *const c_char, n: u16, status: &mut Code) -> *const c_char;
        fn ucnv_countStandards() -> u16;
        fn ucnv_getStandard(n: u16, status: &mut Code) -> *const c_char;
    }
}

/// `UConverterCallbackReason`: why ICU called a callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    Unassigned,
    Illegal,
    Irregular,
    Reset,
    Close,
    Clone,
}

impl Reason {
    /// The reason numbered so, `UConverter::REASON_*`; `None` past
    /// `UCNV_CLONE`.
    pub(crate) fn of(number: i64) -> Option<Reason> {
        Some(match number {
            0 => Reason::Unassigned,
            1 => Reason::Illegal,
            2 => Reason::Irregular,
            3 => Reason::Reset,
            4 => Reason::Close,
            5 => Reason::Clone,
            _ => return None,
        })
    }

    /// Its number, as ICU gives it.
    pub(crate) fn number(self) -> i64 {
        match self {
            Reason::Unassigned => 0,
            Reason::Illegal => 1,
            Reason::Irregular => 2,
            Reason::Reset => 3,
            Reason::Close => 4,
            Reason::Clone => 5,
        }
    }

    /// Its constant's name, as `UConverter::reasonText` answers it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Reason::Unassigned => "REASON_UNASSIGNED",
            Reason::Illegal => "REASON_ILLEGAL",
            Reason::Irregular => "REASON_IRREGULAR",
            Reason::Reset => "REASON_RESET",
            Reason::Close => "REASON_CLOSE",
            Reason::Clone => "REASON_CLONE",
        }
    }

    /// A character in error, which a callback substitutes, rather than
    /// news of a reset, close or clone (`reason <= UCNV_IRREGULAR`).
    pub(crate) fn is_error(self) -> bool {
        matches!(
            self,
            Reason::Unassigned | Reason::Illegal | Reason::Irregular
        )
    }
}

/// The room a callback writes its substitute into, `args->target` up to
/// `args->targetLimit`; what it wrote moves `args->target` on.
pub(crate) struct Target<'a, T> {
    room: &'a mut [T],
    at: usize,
}

impl<T> Target<'_, T> {
    /// No room, `args->target` and `args->targetLimit` both null.
    fn none() -> Self {
        Target {
            room: &mut [],
            at: 0,
        }
    }
}

impl<T: Copy> Target<'_, T> {
    /// `args->targetLimit - args->target`.
    pub(crate) fn available(&self) -> usize {
        self.room.len() - self.at
    }

    /// The units written at `args->target`, where they fit.
    pub(crate) fn write(&mut self, units: &[T]) {
        let end = (self.at + units.len()).min(self.room.len());
        let n = end - self.at;
        self.room[self.at..end].copy_from_slice(&units[..n]);
        self.at = end;
    }
}

/// What serves a converter's callbacks for the span of one call: php's
/// `php_converter_to_u_callback` and `php_converter_from_u_callback`,
/// each with ICU's status to read and set.
pub(crate) trait Callbacks {
    /// The source to Unicode: what is left of the source, the code units
    /// in error and the room for their substitute.
    fn decoding(
        &mut self,
        reason: Reason,
        source: &[u8],
        units: &[u8],
        status: &mut Code,
        target: &mut Target<'_, u16>,
    );

    /// Unicode to the target: the code units in error, their code point
    /// and the room for their substitute.
    fn encoding(
        &mut self,
        reason: Reason,
        units: &[u16],
        code_point: i32,
        status: &mut Code,
        target: &mut Target<'_, u8>,
    );

    /// What `ucnv_close` and `ucnv_safeClone` tell a converter's callbacks
    /// of itself (`UCNV_CLOSE`, `UCNV_CLONE`): the to-Unicode one, then the
    /// from-Unicode one, each with nothing in error, no room and its own
    /// status, where the callbacks are not ICU's defaults.
    fn told(&mut self, reason: Reason) {
        let (mut to_u, mut from_u) = (Code::ZERO, Code::ZERO);
        self.decoding(reason, &[], &[], &mut to_u, &mut Target::none());
        self.encoding(reason, &[], 0, &mut from_u, &mut Target::none());
    }
}

/// The slice ICU hands between two pointers, empty where it hands none.
///
/// # Safety
/// Where `start` is not null, `start..limit` is a live range ICU holds
/// for the span of the callback.
unsafe fn between<'a, T>(start: *const T, limit: *const T) -> &'a [T] {
    if start.is_null() || limit <= start {
        return &[];
    }
    // SAFETY: both point into the one buffer ICU hands, limit past start.
    let n = unsafe { limit.offset_from(start) };
    // SAFETY: the caller's range, n elements from start.
    unsafe { std::slice::from_raw_parts(start, n.unsigned_abs()) }
}

/// The writable room ICU hands, as [`between`].
///
/// # Safety
/// As [`between`], and nothing else reads or writes the range meanwhile.
unsafe fn room<'a, T>(start: *mut T, limit: *const T) -> &'a mut [T] {
    if start.is_null() || limit <= start.cast_const() {
        return &mut [];
    }
    // SAFETY: both point into the one target buffer, limit past start.
    let n = unsafe { limit.offset_from(start) };
    // SAFETY: the caller's range, n elements from start, ICU's alone.
    unsafe { std::slice::from_raw_parts_mut(start, n.unsigned_abs()) }
}

/// The handler [`served`] lent, from the pointer ICU passes back.
///
/// # Safety
/// `context` is the `*mut &mut dyn Callbacks` [`served`] installed, alive
/// for the span of the ICU call that calls back.
unsafe fn lent<'a>(context: *const c_void) -> &'a mut dyn Callbacks {
    // SAFETY: the caller's pointer, to a handler nothing else reaches
    // while ICU calls back.
    unsafe { &mut **context.cast_mut().cast::<&mut dyn Callbacks>() }
}

/// The `UConverterToUCallback` [`served`] installs.
unsafe extern "C" fn to_unicode(
    context: *const c_void,
    args: *mut sys::ToUArgs,
    units: *const c_char,
    len: i32,
    reason: i32,
    status: *mut Code,
) {
    let Some(reason) = Reason::of(i64::from(reason)) else {
        return;
    };
    // SAFETY: ICU passes back the context `served` gave it.
    let callbacks = unsafe { lent(context) };
    // SAFETY: ICU hands its live arguments for the span of the call.
    let args = unsafe { &mut *args };
    // SAFETY: ICU's live status.
    let status = unsafe { &mut *status };
    // SAFETY: what is left of the source, ICU's.
    let source = unsafe { between(args.source.cast::<u8>(), args.source_limit.cast()) };
    let len = usize::try_from(len).unwrap_or(0);
    // SAFETY: the code units in error, len bytes ICU holds.
    let units = unsafe { between(units.cast::<u8>(), units.cast::<u8>().wrapping_add(len)) };
    // SAFETY: the target's room, ICU's until the callback returns.
    let room = unsafe { room(args.target, args.target_limit) };
    let mut target = Target { room, at: 0 };
    callbacks.decoding(reason, source, units, status, &mut target);
    args.target = args.target.wrapping_add(target.at);
}

/// The `UConverterFromUCallback` [`served`] installs.
unsafe extern "C" fn from_unicode(
    context: *const c_void,
    args: *mut sys::FromUArgs,
    units: *const u16,
    len: i32,
    code_point: i32,
    reason: i32,
    status: *mut Code,
) {
    let Some(reason) = Reason::of(i64::from(reason)) else {
        return;
    };
    // SAFETY: ICU passes back the context `served` gave it.
    let callbacks = unsafe { lent(context) };
    // SAFETY: ICU hands its live arguments for the span of the call.
    let args = unsafe { &mut *args };
    // SAFETY: ICU's live status.
    let status = unsafe { &mut *status };
    let len = usize::try_from(len).unwrap_or(0);
    // SAFETY: the code units in error, len UChars ICU holds.
    let units = unsafe { between(units, units.wrapping_add(len)) };
    // SAFETY: the target's room, ICU's until the callback returns.
    let room = unsafe { room(args.target.cast::<u8>(), args.target_limit.cast()) };
    let mut target = Target { room, at: 0 };
    callbacks.encoding(reason, units, code_point, status, &mut target);
    args.target = args.target.wrapping_add(target.at);
}

/// The two callbacks ICU held before [`install`], to put back.
#[derive(Clone, Copy)]
struct Before {
    to_u: Option<sys::ToU>,
    to_u_context: *const c_void,
    from_u: Option<sys::FromU>,
    from_u_context: *const c_void,
}

/// The trampolines installed on `cnv` with `context`, what was there
/// answered.
fn install(cnv: *mut sys::UConverter, context: *const c_void) -> Before {
    let mut held = Before {
        to_u: None,
        to_u_context: std::ptr::null(),
        from_u: None,
        from_u_context: std::ptr::null(),
    };
    let mut status = Code::ZERO;
    let (to_u, to_ctx) = (&mut held.to_u, &mut held.to_u_context);
    // SAFETY: cnv is open; ICU keeps the pointers until they are replaced.
    unsafe { sys::ucnv_setToUCallBack(cnv, Some(to_unicode), context, to_u, to_ctx, &mut status) };
    let (from_u, from_ctx) = (&mut held.from_u, &mut held.from_u_context);
    // SAFETY: as above.
    unsafe {
        sys::ucnv_setFromUCallBack(
            cnv,
            Some(from_unicode),
            context,
            from_u,
            from_ctx,
            &mut status,
        );
    };
    held
}

/// What [`install`] replaced put back on `cnv`.
fn restore(cnv: *mut sys::UConverter, held: Before) {
    let (mut status, mut to_u, mut from_u) = (Code::ZERO, None, None);
    let mut context = std::ptr::null();
    // SAFETY: cnv is open; the callback and context are ICU's own again.
    unsafe {
        sys::ucnv_setToUCallBack(
            cnv,
            held.to_u,
            held.to_u_context,
            &mut to_u,
            &mut context,
            &mut status,
        );
    };
    // SAFETY: as above.
    unsafe {
        sys::ucnv_setFromUCallBack(
            cnv,
            held.from_u,
            held.from_u_context,
            &mut from_u,
            &mut context,
            &mut status,
        );
    };
}

/// `op` over `cnv` with its callbacks served by `callbacks`, where there
/// are any, and ICU's own put back after.
fn served<R>(
    cnv: *mut sys::UConverter,
    callbacks: Option<&mut dyn Callbacks>,
    op: impl FnOnce() -> R,
) -> R {
    let Some(mut callbacks) = callbacks else {
        return op();
    };
    let context: *mut &mut dyn Callbacks = &raw mut callbacks;
    let held = install(cnv, context.cast_const().cast());
    let answer = op();
    restore(cnv, held);
    answer
}

opaque!(
    /// `UConverter`.
    UConverter
);

owner!(
    /// One open `UConverter`, which the request keeps for the object
    /// that opened it and `ucnv_close` closes once, with ICU's own
    /// callbacks on it.
    Open(UConverter),
    ucnv_close
);

/// One `UConverter` the request keeps ([`Memory::adopt`], ADR-0013):
/// what an object's payload stores, given up by [`Converter::close`]. A
/// call that lends the converter's callbacks to PHP code holds it for
/// its span ([`ConverterId::held`]), so code that replaces or frees the
/// object's converter meanwhile leaves the close to that call's end.
pub(crate) struct Converter(Foreign<Open>);

/// The name of a [`Converter`], copied freely: it reaches the converter
/// while its owner has not given it up, and nothing after.
#[derive(Clone, Copy)]
pub(crate) struct ConverterId(ForeignId<Open>);

/// A converter held for the span `'h` of one [`ConverterId::held`], which
/// it cannot outlive, so the pointer it carries is open while it lives.
pub(crate) struct Live<'h> {
    raw: NonNull<UConverter>,
    span: PhantomData<Held<'h, Open>>,
}

impl<'h> Live<'h> {
    /// The converter `held` holds, as the request reaches it.
    fn of(req: &mut Request, held: &Held<'h, Open>) -> Option<Live<'h>> {
        let raw = held.get(req)?.0;
        Some(Live {
            raw,
            span: PhantomData,
        })
    }

    /// `ucnv_toUChars` into `dest`, its capacity all of it: the length the
    /// whole answer needs and ICU's code.
    pub(crate) fn decode(
        &self,
        dest: &mut [u16],
        src: &[u8],
        callbacks: Option<&mut dyn Callbacks>,
    ) -> (i32, Code) {
        let mut status = Code::ZERO;
        let (Ok(capacity), Ok(len)) = (length(dest.len()), length(src.len())) else {
            return (0, Code::BUFFER_OVERFLOW);
        };
        let (cnv, out, from) = (self.raw.as_ptr(), dest.as_mut_ptr(), src.as_ptr().cast());
        // SAFETY: the converter is open for the hold `self` lives in;
        // dest holds capacity UChars, src len bytes.
        let op = || unsafe { sys::ucnv_toUChars(cnv, out, capacity, from, len, &mut status) };
        let n = served(cnv, callbacks, op);
        (n, status)
    }

    /// `ucnv_fromUChars` into `dest`, as [`Live::decode`].
    pub(crate) fn encode(
        &self,
        dest: &mut [u8],
        src: &[u16],
        callbacks: Option<&mut dyn Callbacks>,
    ) -> (i32, Code) {
        let mut status = Code::ZERO;
        let (Ok(capacity), Ok(len)) = (length(dest.len()), length(src.len())) else {
            return (0, Code::BUFFER_OVERFLOW);
        };
        let (cnv, out, from) = (self.raw.as_ptr(), dest.as_mut_ptr().cast(), src.as_ptr());
        // SAFETY: as in `decode`; dest holds capacity bytes, src len UChars.
        let op = || unsafe { sys::ucnv_fromUChars(cnv, out, capacity, from, len, &mut status) };
        let n = served(cnv, callbacks, op);
        (n, status)
    }
}

impl Converter {
    /// `ucnv_open`: the converter, kept by the request, or none beside a
    /// failure ([`Code::built`]), and ICU's code, a warning
    /// (`U_AMBIGUOUS_ALIAS_WARNING`) kept.
    pub(crate) fn open(req: &mut Request, name: &CStr) -> (Option<Converter>, Code) {
        let mut status = Code::ZERO;
        // SAFETY: name is NUL-terminated.
        let raw = unsafe { sys::ucnv_open(name.as_ptr(), &mut status) };
        let open = status.built(NonNull::new(raw).map(Open));
        (
            open.map(|open| Converter(req.memory_mut().adopt(open))),
            status,
        )
    }

    /// Its name.
    pub(crate) fn id(&self) -> ConverterId {
        ConverterId(self.0.id())
    }

    /// Given up: closed now, or as the last call that holds it ends.
    pub(crate) fn close(self, req: &mut Request) {
        drop(req.memory_mut().disown(self.0));
    }
}

impl ConverterId {
    /// The converter while its owner keeps it; what a call that runs no
    /// PHP code reads it through, for the span of its borrow.
    pub(crate) fn get(self, req: &mut Request) -> Option<&mut Open> {
        req.memory_mut().foreign(self.0)
    }

    /// `body` served with the converter held ([`Memory::holding`]), for
    /// a call whose callbacks serve PHP code; `None` where its owner gave
    /// it up before.
    pub(crate) fn held<R>(
        self,
        req: &mut Request,
        body: impl for<'h> FnOnce(&mut Request, &Live<'h>) -> R,
    ) -> Option<R> {
        req.memory_mut()
            .holding(self.0, |req, held| {
                let live = Live::of(req, &held)?;
                Some(body(req, &live))
            })
            .flatten()
    }

    /// `ucnv_clone`, the clone kept by the request. ICU tells the
    /// original's callbacks of it after (`UCNV_CLONE`), which its caller
    /// does as [`Callbacks::told`] tells them; the clone holds ICU's own.
    pub(crate) fn try_clone(self, req: &mut Request) -> Result<Converter, Code> {
        let open = self.get(req).ok_or(Code::INVALID_STATE)?;
        let copy = open.try_clone()?;
        Ok(Converter(req.memory_mut().adopt(copy)))
    }
}

impl Open {
    fn raw(&self) -> *mut sys::UConverter {
        self.0.as_ptr()
    }

    /// `ucnv_clone`.
    fn try_clone(&self) -> Result<Open, Code> {
        let mut status = Code::ZERO;
        // SAFETY: the converter is open.
        let raw = unsafe { sys::ucnv_clone(self.raw(), &mut status) };
        status.check()?;
        NonNull::new(raw).map(Open).ok_or(Code::MEMORY_ALLOCATION)
    }

    /// `ucnv_getName`.
    pub(crate) fn name(&self) -> Result<Vec<u8>, Code> {
        let mut status = Code::ZERO;
        // SAFETY: the converter is open.
        let name = unsafe { sys::ucnv_getName(self.raw(), &mut status) };
        status.check()?;
        Ok(bytes(name))
    }

    /// `ucnv_getType`, `UConverterType`.
    pub(crate) fn kind(&self) -> i32 {
        // SAFETY: the converter is open.
        unsafe { sys::ucnv_getType(self.raw()) }
    }

    /// `ucnv_getSubstChars` into php's 127 bytes.
    pub(crate) fn subst_chars(&self) -> Result<Vec<u8>, Code> {
        let (mut chars, mut len, mut status) = ([0u8; 127], 127i8, Code::ZERO);
        // SAFETY: the buffer holds len bytes.
        unsafe {
            sys::ucnv_getSubstChars(self.raw(), chars.as_mut_ptr().cast(), &mut len, &mut status);
        };
        status.check()?;
        Ok(chars[..usize::try_from(len).unwrap_or(0).min(chars.len())].to_vec())
    }

    /// `ucnv_setSubstChars`, the length as C narrows it to `int8_t`.
    pub(crate) fn set_subst_chars(&self, chars: &[u8]) -> Code {
        let len = i8::from_le_bytes([chars.len().to_le_bytes()[0]]);
        let mut status = Code::ZERO;
        // SAFETY: the converter is open; ICU reads len bytes, no more than
        // the slice holds, and refuses a negative one.
        unsafe { sys::ucnv_setSubstChars(self.raw(), chars.as_ptr().cast(), len, &mut status) };
        status
    }
}

/// A NUL-terminated name ICU keeps, copied; empty for a null one.
fn bytes(name: *const c_char) -> Vec<u8> {
    if name.is_null() {
        return Vec::new();
    }
    // SAFETY: a non-null name ICU keeps in static or converter storage.
    unsafe { CStr::from_ptr(name) }.to_bytes().to_vec()
}

/// `ucnv_countAvailable` names by `ucnv_getAvailableName`.
pub(crate) fn available() -> Vec<Vec<u8>> {
    // SAFETY: the call takes nothing.
    let count = unsafe { sys::ucnv_countAvailable() };
    // SAFETY: n is below the count.
    (0..count)
        .map(|n| bytes(unsafe { sys::ucnv_getAvailableName(n) }))
        .collect()
}

/// `ucnv_countAliases`.
pub(crate) fn count_aliases(name: &CStr) -> Result<u16, Code> {
    let mut status = Code::ZERO;
    // SAFETY: name is NUL-terminated.
    let count = unsafe { sys::ucnv_countAliases(name.as_ptr(), &mut status) };
    status.check().map(|_| count)
}

/// `ucnv_getAlias`.
pub(crate) fn alias(name: &CStr, n: u16) -> Result<Vec<u8>, Code> {
    let mut status = Code::ZERO;
    // SAFETY: name is NUL-terminated.
    let alias = unsafe { sys::ucnv_getAlias(name.as_ptr(), n, &mut status) };
    status.check().map(|_| bytes(alias))
}

/// `ucnv_countStandards`.
pub(crate) fn count_standards() -> u16 {
    // SAFETY: the call takes nothing.
    unsafe { sys::ucnv_countStandards() }
}

/// `ucnv_getStandard`.
pub(crate) fn standard(n: u16) -> Result<Vec<u8>, Code> {
    let mut status = Code::ZERO;
    // SAFETY: the call takes an index and a status.
    let name = unsafe { sys::ucnv_getStandard(n, &mut status) };
    status.check().map(|_| bytes(name))
}
