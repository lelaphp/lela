---
status: accepted
date: 2026-10-09
narrows: third-engine-design.md revision 50 (its rejection of "a thread-local heap for timelib's `emalloc`")
---

# A request's heap is lent to C for the length of one call

php builds timelib with its allocator macros pointed at the request's heap. `config0.m4` writes a `timelib_config.h` that defines `timelib_malloc`, `timelib_calloc`, `timelib_realloc`, `timelib_strdup`, `timelib_strndup` and `timelib_free` as `emalloc`, `ecalloc`, `erealloc`, `estrdup`, `estrndup` and `efree`. So `memory_get_usage()` and `memory_get_peak_usage()` count every `timelib_time`, `timelib_tzinfo` and error container php's date code holds, and everything timelib allocates while it parses. The macros take no argument that could name a heap, and `emalloc` finds php's through `AG(mm_heap)`, a per-thread global. Revision 50 (#1170) rejected "a thread-local heap for timelib's `emalloc`, which §5.14 forbids", and §5.13 kept those allocations as the recorded approximation.

The cost showed up as a measurement. With the date classes' structs modelled on the request's heap (#1655, afbc6814), every `memory_get_usage()` delta read the oracle's, and every peak read short:

| | oracle peak | engine peak |
|---|---|---|
| `new DateTime` | 624 | 320 |
| `new DateTime('2020-01-01 10:00')` | 640 | 320 |
| `new DateInterval('P1D')` | 768 | 176 |
| `new DatePeriod(...)` | 1184 | 976 |
| `strtotime('next monday')` | 632 | 0 |

Mitchell ruled on #1658 (2026-10-03), option 2: a scoped lend of the heap, in `php_heap`. The ruling named the record ADR-0009. That number was taken by then (branch tracks, 2026-10-03), so the record is this one.

## Decision

- **The slot is `php_heap`'s, and a lend fills it for one call.** `Heap::lend(call)` puts the heap's allocator in a per-thread slot, serves `call`, and puts back what stood there before, on every path out, unwinding among them. The heap is borrowed for the length of `call`, so no Rust code reaches it meanwhile. `Request::lend_heap(call)` is the same thing on the face (`api.lock`). Nothing outlives the call, so a pooled thread carries no heap from one request to the next. Revision 50's reason holds, and a heap that outlives a call stays rejected.
- **Six C symbols reach it, under php's names.** `php_heap` defines `emalloc`, `ecalloc`, `erealloc`, `estrdup`, `estrndup` and `efree` as `extern "C"`. The ruling named five; `estrndup` is the sixth because php's header maps `timelib_strndup` to it, and `parse_posix.c` calls that. `timelib_sys` compiles its vendored sources, unchanged, with `HAVE_TIMELIB_CONFIG_H` and an `include/timelib_config.h` that declares the six and maps timelib's macros onto them, as php's does. The symbols resolve at link time, timelib's undefined references against `php_heap`'s definitions. `timelib_sys` depends on no crate, which is the manifest's edge rule. The never-inlined `enter` and `leave` that every lend calls share an object file with the six. Whatever object pulls in a lend therefore pulls in the definitions too, whichever order the linker reads the archives in.
- **Outside a lend, allocation goes to the system allocator.** `emalloc` and its kin call `malloc` and its kin there, never a stale request's heap.
- **Under a lend, the heap tells its own blocks apart.** `efree` and `erealloc` ask `Mm::owns`: a chunk-aligned address is the heap's if it is one of its huge blocks, and any other address is the heap's if its chunk is one of the heap's chunks. This is php's `chunk->heap == heap`, read from the heap's own lists and never from the memory at the address. A chunk is the heap's own 2 MiB and a huge block its own mapping, so no system block lies inside either. A system block freed under a lend therefore goes to `free`, and a heap block to the heap, through `Mm::free`. That way the debug quarantine and the freed-use sensors see C's frees too.
- **Outside a lend, `efree` frees nothing.** There is no heap to ask, and telling a heap's block from the system's would mean reading memory that may already be gone. A block of a heap goes back with that heap at the request's end. ext/date keeps every free of a heap block under a lend of the same request:
  - a `Lent` proof is taken by every timelib call that can allocate or free, and only a lend makes one;
  - each owner of timelib's memory (`Time`, a zone, an error container) is dropped under a lend;
  - `rshutdown` frees `DATEG(tzcache)` and `DATEG(last_errors)` under one, as php's `RSHUTDOWN` does.

  A drop that misses one leaves a block the debug build's request leak sensor names.
- **The memory limit is served, then raised.** php's `emalloc` bails out of timelib with a jump. A Rust frame cannot be jumped over, so the allocation the heap refuses is served by the system allocator instead, the call runs to its return, and the lend answers the refusal. `Request::lend_heap` then ends the request in the limit's fatal, as `zend_mm` does. That block is the system's, and a later `efree` under a lend sends it there.

## Considered options

- **Keep revision 50 (option 1 on #1658).** Rejected by the ruling: the peaks stay short by what timelib allocates, which is most of each constructor's peak.
- **Model the peaks (option 3).** Charge each timelib entry point's allocations at php's sizes, after the fact. Rejected: a model drifts whenever timelib changes, and php's number is timelib's own allocations.
- **A heap that outlives the call, per thread.** Still rejected, for revision 50's reason: a pooled thread would carry it into the next request.
- **An allocator that names its heap.** Rejected: timelib's macros take no argument, and patching the vendored sources breaks the byte-for-byte copy of `php-src/ext/date/lib`.
- **`efree` outside a lend through `free`, the ruling's fallback read for frees as well.** Rejected: a block a lend made and an owner dropped outside one would go to `free`, which is undefined behaviour. The fallback stays for allocation, where it is safe.
- **A process-wide record of the chunks every heap maps, so `efree` could tell blocks apart without a lend.** Rejected: it adds a lock on every chunk mapping and state that outlives every call, for a free that a type can keep under a lend instead.
- **Frees deferred to a per-thread list that the next lend drains.** Rejected: the list outlives the call, and a chunk freed with one request can be mapped again by the next. A deferred pointer could then name a live block of another request.

## Consequences

- `heap.lock` gains `Heap::lend`, and `api.lock` gains `request::Request::lend_heap`. `php_heap` gains `lend.rs`, plus `Mm::owns` and `Mm::usable` beside the allocator.
- ext/date's `timelib_time` values, its zone cache's `timelib_tzinfo`s and `DATEG(last_errors)`'s container are timelib's own, on the request's heap. The modelled block (`held.rs`) is left only for what ext/date keeps as Rust values: a `DateInterval`'s and a `DatePeriod`'s `timelib_rel_time`, and a `DateTimeZone`'s abbreviation. A body of `php_date.c` that works on `dateobj->time` works on it where it stands (`with_time`, `lent_time`), not on a copy.
- The five rows of #1655 read the oracle's peak and usage, each pinned by a witness beside its constructor.
- The cache's own table is still a Rust `HashMap` where php's `DATEG(tzcache)` is a `HashTable` on the request's heap, so the first zone a request parses leaves 424 bytes fewer in `memory_get_usage()` than php's.
- lexbor and libmbfl, which allocate with `malloc` today, may take the same lend, each on its own ticket.
- A future reader will want `efree` outside a lend to call `free`, "as the fallback says". Do not: the block may be a heap's. This file is the answer.
