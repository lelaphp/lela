//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3),
//! the observer's order and pairing (seam 9), the frames its handlers
//! read, keep and write and the op arrays they read,
//! its notices and the handlers added while a request is served, the
//! opcodes an extension observes, a periodic interrupt as an extension
//! sees it, the pool's drain, a handler that blocks with the
//! time limit suspended, allocations as an extension observes
//! them (design §7.1), the statements a Zend extension is told of
//! under extended info, and a debug build's freed-use guard
//! and request leak sensor as a program meets them,
//! the leaks that sensor found in the engine, the owner type
//! as one meets it, a table copied as php copies it,
//! the callback walks as a native meets them, the owners the
//! face takes for a native and gives up, and the frames
//! a fatal leaves, whose annexes leave with them, an
//! extension's optimizer passes where the unit cache runs them,
//! the running execution's trace number across a fiber switch and a
//! generator's resume, and the request's storage as a native
//! holds it, an `Emalloc` and a `FreshString` (design §5.13), and a
//! call's closure taken for the closure `CALLABLE_CONVERT` writes,
//! php's typed throws held to their typed functions across
//! the workspace, and compiled code's guard on a method's pair
//! held to the interpreter's read-back of it.

use crate::api;

mod allocations;
mod body;
mod cache_guard;
mod call_closure;
mod dropped_frames;
mod frame_writes;
mod frames;
#[cfg(debug_assertions)]
mod freed_use;
#[cfg(debug_assertions)]
mod given_up;
mod hand_off;
#[cfg(debug_assertions)]
mod leak_causes;
#[cfg(debug_assertions)]
mod leak_sensor;
mod lifecycle;
#[cfg(debug_assertions)]
mod native_owners;
mod observer;
mod observer_notices;
mod observer_traits;
mod op_arrays;
mod opcodes;
mod optimizer_passes;
mod owners;
mod periodic;
mod pool;
mod statements;
pub(crate) mod stepped;
#[cfg(debug_assertions)]
mod storage;
#[cfg(debug_assertions)]
mod table_copy;
pub(crate) mod test_ext;
mod tier_setting;
mod time_limit;
mod trace_num;
pub(crate) mod unit_view;
#[cfg(debug_assertions)]
mod variadic;
#[cfg(debug_assertions)]
mod walks;
#[cfg(debug_assertions)]
mod while_pending;
