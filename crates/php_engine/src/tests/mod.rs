//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3),
//! the observer's order and pairing (seam 9), the frames its handlers
//! read, keep and write and the op arrays they read (#1828, #1829),
//! its notices and the handlers added while a request is served, the
//! opcodes an extension observes, a periodic interrupt as an extension
//! sees it (#1806), the pool's drain, a handler that blocks with the
//! time limit suspended (#1814), allocations as an extension observes
//! them (design §7.1), the statements a Zend extension is told of
//! under extended info (#1823), and a debug build's freed-use guard
//! (#1990) and request leak sensor (#1992) as a program meets them,
//! the leaks that sensor found in the engine (#2057), the owner type
//! as one meets it (#1995), a table copied as php copies it (#2005),
//! the callback walks as a native meets them (#1980), the owners the
//! face takes for a native and gives up (#2059, #2080), and the frames
//! a fatal leaves, whose annexes leave with them (#2554), and an
//! extension's optimizer passes where the unit cache runs them (#2386).

use crate::api;

mod allocations;
mod body;
mod dropped_frames;
mod frame_writes;
mod frames;
#[cfg(debug_assertions)]
mod freed_use;
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
mod stepped;
#[cfg(debug_assertions)]
mod table_copy;
pub(crate) mod test_ext;
mod time_limit;
#[cfg(debug_assertions)]
mod walks;
#[cfg(debug_assertions)]
mod while_pending;
