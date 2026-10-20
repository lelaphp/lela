---
status: accepted
date: 2026-10-08
supplements: ADR-0005 (what an extension may hold, and what it may not)
---

# A foreign library's object that PHP code can free during a call is the request's, and the call holds it

A native often keeps a C library's object in an object's payload and calls into the library, and the library calls back into PHP code with the `&mut Request` the payload lives behind. That code may replace or free the payload's object while the library is still inside it: `setSourceEncoding()` called from `toUCallback()` closes the converter ICU is converting with, and php does not finish that conversion. ext/intl's `UConverter` (#1623) kept the converter alive with a second `std::rc::Rc` share held for the call, behind `unsafe impl Send`, whose argument was that every share stays with the request that opened it. That is a discipline, not a type: safe code could send one share to another thread and keep the other. Mitchell ruled on #2277 (2026-10-08), option (c): the engine offers one typed way to keep such an object alive across a call into PHP code, `UConverter` moves onto it, and `ext.no-locks` then names `std::rc::Rc` under `ext/*`. #2313 built it.

## Decision

- **The request owns the object.** `Request::adopt(value)` keeps a `T: Send + 'static` in a table on the request and answers its one owner, `request::Foreign<T>`, which the payload stores. `Foreign` is neither `Copy` nor `Clone`, so the object is given up in exactly one place, `Request::disown`.
- **A name reaches it.** `Foreign::id` answers a `ForeignId<T>`, copied freely. `Request::foreign(id)` reaches the object while its owner keeps it, and answers nothing once it was given up or its row reused (each row carries a generation).
- **A call holds it for a span the compiler checks.** `Request::holding(id, body)` serves `body(req, held)` with a hold on the object: a `disown` inside the span marks it and the drop waits for the last hold to end. `held` is a `Held<'h, T>` whose lifetime is the span, higher-ranked in `body`, so nothing built from it (ext/intl's `Live<'h>`, the converter's pointer) outlives the hold; a `compile_fail` doctest holds that. `Held::get` reaches the object for the whole span, including after its owner gave it up.
- **The request's end drops what remains.** The table is a field of the request and drops with it, after `post_deactivate`, as request state does (ADR-0005, ADR-0010): `T`'s `Drop` touches no engine.
- **An extension holds no shared ownership.** With this in place an extension has no need of `Rc` under `ext/*`; the manifest's `ext.no-locks` is to name `std::rc::Rc` beside the cells and locks it already denies (the widening is a person's edit, put to Mitchell from #2313).

## Considered options

- **Keep the `Rc` share behind `unsafe impl Send` (option (a) on #2277).** Rejected by the ruling: the guarantee rests on no one sending a share elsewhere.
- **`Arc` in place of `Rc`.** Rejected: an ICU converter or a sqlite connection is not `Sync`, so `Arc<T>` is not `Send` without the same `unsafe impl`, and ADR-0005 denies the atomics under `ext/*` besides.
- **A pin of the object's payload for the call, as `GC_ADDREF` pins the object.** Rejected: it keeps the payload's memory, not the C object in one of its fields, which is what PHP code replaces; `setSourceEncoding()` overwrites the field whatever holds the object.
- **Move the C object out of the payload for the span of the call.** Rejected: the callback's own PHP code reads it (`parent::toUCallback()` reads the source converter's substitute, `getSourceEncoding()` its name), and php answers those mid-call.
- **A hold as a value the caller releases by hand.** Rejected: the release needs the request, so it cannot be `Drop`, and a forgotten release would keep the object to the request's end unseen; the closure ends the hold on every path `body` returns by.
- **A table each extension keeps in its own request state.** Rejected: every extension with the shape (sqlite, curl, libxml) would write the same table, and the request's end, which only the engine sees whole, would be each extension's to get right.

## Consequences

- The `request` group of `api.lock` gains `Foreign`, `Foreign::id`, `ForeignId`, `Held`, `Held::get` and `Request::{adopt, foreign, holding, disown}`.
- ext/intl's converter is `Foreign<Open>`, where `Open` is the crate's single-owner `owner!` type; `ffi/converter.rs` holds no `Rc` and no `unsafe impl Send`. The engine runs the callback that frees its own converter to its end, where php does not finish the conversion; that difference is a divergence row a person writes.
- The other extensions with this shape keep their discipline until each moves, one ticket each: pdo_sqlite's counted `Db` with its `*mut Request`, curl's `Arc` handles behind `unsafe impl Send + Sync`, and the like.
- A future reader will want to put an `Rc` back "because the object never leaves the request". Do not; the request owns it and a call holds it. This file is the answer.
- *Renamed 2026-10-20 by #696 (#3103):* `Request::{adopt, foreign, holding, disown}` are `Memory::{adopt, foreign, holding, disown}`, reached through `req.memory_mut()`, and `api.lock` lists them under `request::Memory`. The decision is unchanged.
