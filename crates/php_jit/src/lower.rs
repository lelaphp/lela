//! What becomes of a [`TraceIr`](crate::types::TraceIr) (`jit-design.md`
//! §4.3): the seam is the value, and two modules read it. [`dump`]
//! renders it as text, which `types` is held to before code is made; the
//! `Backend` that compiles it to final bytes lands with #2955.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the pipeline renders a trace under jit_debug with #2958"
    )
)]

pub(crate) mod dump;
