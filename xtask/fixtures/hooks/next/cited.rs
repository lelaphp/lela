//! A recorded trace's numbering (jit-design.md §6.4,
//! revisions 23 and NEXT; design §5.11, revision 52). What the guard
//! clears on its drop is the request's own (jit-design.md revision
//! NEXT-2), as php's JIT_G(tracing) is.

pub fn numbered() {}
