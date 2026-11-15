# php2shell in Dax PHP

php fixed two remote-code-execution bugs, together called php2shell, in 8.5.12, 8.4.27, 8.3.36 and 8.2.35 on 2026-10-21:

- CVE-2026-109618, OOM corruption ([GHSA-w8hw-w9fx-j79w](https://github.com/php/php-src/security/advisories/GHSA-w8hw-w9fx-j79w)).
- CVE-2026-109619, stale global state ([GHSA-6wmf-4rv5-p3rp](https://github.com/php/php-src/security/advisories/GHSA-6wmf-4rv5-p3rp)).

Neither bug is in the C that Dax PHP compiles from php-src (timelib, libmbfl, lexbor, uriparser).

A first reading of the source decided that neither bug carries over to Dax PHP. The work this document reports turned that decision into two tests, so a later change that breaks either property fails a test. This document gives what the tests showed, why each CVE does or does not carry over, what the tests leave out, and the five defects found on the way.

Each `path:line` below is as of the commit that last changed this file, and the commits it names are on the branch that added the two tests.

In short: on the evidence of these tests, Dax PHP was not vulnerable to either CVE as published. The work did find five places where a request that met its memory limit left state wrong for the code that runs after the fatal. Two of them could reach memory that was no longer what it claimed to be: an array written past its data, and an iterator moving through a freed array. These are the general form of CVE-2026-109618, and neither had a known route to code execution. All five are fixed on this branch.

## The two tests

### Every place the heap can refuse an allocation

`crates/php_heap/src/refusals.rs` holds the tests. The miri lane runs them as part of php_heap's suite (`cargo xtask miri`). Natively they are part of `cargo xtask test --crate php_heap`.

- `each_arm_of_the_allocator_the_limit_reaches_refuses_and_leaves_the_block_it_had` drives each arm of the allocator to the real memory limit. The test first fills the heap's only chunk, so the next allocation that needs a page has to meet the limit. The arms are a bin with no free slot, a `run` of pages, a huge block, a block cut to a lower bin, a block grown past its bin, a block grown from a bin to a `run`, a `run` grown past the pages after it, and a huge block cut to a `run`. Each refusal must be the limit's fatal and must leave the allocation count where it stood. A block the call was asked to resize must keep its bytes.
- `every_ask_of_every_entry_point_refused_in_turn_leaves_what_it_touched_whole` covers 48 entry points above the allocator. They are strings, blocks, arrays and the sort, objects and the object store, table copies, references, resources and the resource list, the compiler's arena, the VM stack, iterators, and C code under a lend. Each operation is refused at its first allocation, then at its second, and so on until it completes. A full heap refuses whichever allocation finds no room, and a test cannot fill a heap so that exactly the nth allocation of an operation is the one refused. So a test-only switch in `Mm::alloc` and `Mm::realloc` refuses that one allocation the way the limit does (`crates/php_heap/src/mm.rs`, `Mm::refuse_after`). After each refusal the test does what a shutdown function could do. It writes every array it holds at every row the array's size claims, then reads every value back through the value's own lookup. It checks that each array's data really holds the room its size claims, runs the operation again, releases everything, and asks the debug build's freed-use sensors what they saw.

The three tests in the file, and `a_huge_string_grown_past_the_limit_lifts_it_until_the_fatal_is_rendered` in `crates/php_heap/src/lib.rs`, pass natively and under Miri. The last miri lane over the branch passed php_heap's suite, 122 tests under Tree Borrows with permissive provenance and 6 ignored. The three smaller tests take 15 s of wall time under Miri. The sweep takes 109 s of Miri's own clock, 47 s of it for the 1,023 objects that fill the object store.

Miri's view inside the heap has a limit. A 2 MiB chunk is a single allocation to Miri, so a read of a bin slot after the slot is freed stays inside a live allocation, and Miri does not flag it. The debug build's freed-use sensors catch that case. Miri adds what it sees in full: reads of uninitialised memory, huge blocks and system blocks, which it tracks one by one, and the Rust payloads it checks for leaks.

### A second request on the same worker

`sapi/makala/tests/worker.rs` starts makala with one request thread, so both requests run on one worker, as a php-fpm child serves its requests one after another.

The ticket for the two tests asked for this one in php_engine. php_engine sits below the extensions and cannot load ext/session, so it cannot see the session's progress state. A php_engine test that called `Engine::serve` twice would also only imitate a worker. makala's request thread is a real one.

- The first request posts a multipart body. The body holds the session's upload-progress field, a file, and then a 5 MB form field, under `memory_limit=4M`. The request ends in `Allowed memory size of 4194304 bytes exhausted (tried to allocate 5000032 bytes) in Unknown on line 0`. That fatal comes while the body is read, before the program starts and after the progress hook has written the file into the session.
- Before the second request, the test reads the session's file: it holds the first file's progress, not done, so the first request left progress behind to carry over.
- The second request posts the progress field and one file, on the same thread. The test reads three things from it:
  - **The session's progress state.** Under `session.upload_progress.cleanup=0` the progress array lists the second request's file alone, with its own content length, and is marked done. Progress carried over from the first request would list the first file before it.
  - **The diagnostics' carried error state.** `error_get_last()` reads `NULL` on the program's first line. A carried error would read the first request's fatal and its backtrace.
  - **The heap's lend slot.** ext/date's `rshutdown` lends the heap to timelib at the end of every request, the first request included. A debug build now records a heap that is created while a lend is still in place on its thread (`crates/php_heap/src/mm.rs:366`), and the engine ends that request in an `InternalError` instead of running its program.

The test passes. As a negative control, `Restore::drop` in `crates/php_heap/src/lend.rs:80` was changed to leave the slot filled, and the test failed: the second request read the `InternalError` naming the lend slot where it should have read `NULL`. The control was then reverted.

## CVE-2026-109618, OOM corruption

**In php:** when the memory limit refuses an allocation, `zend_bailout` leaves the request with `longjmp()`. Request memory the refused path had published but not finished stays reachable, and request shutdown reads it. The advisory reaches the limit through multipart parsing: a body with no declared length, part headers with no bound, and empty file controls that `max_file_uploads` does not count.

**In Dax PHP:** the memory limit is a value, `Fatal` (`crates/php_heap/src/hooks.rs:38`), passed up with `?`. Every Rust frame it passes through runs its drops. C code under a lend is never cut off part way. The system allocator serves the allocation the heap refused, the library runs to its return, and `Restore` empties the lend slot on every path out (`crates/php_heap/src/lend.rs:78-84`, `:127`). The multipart parser returns the fatal out of `parts` (`crates/php_engine/src/rfc1867.rs:557`).

The `longjmp` does not carry over. What was left to check is the general form of the bug: does a path publish state before an allocation that can fail, so that what runs after the fatal finds memory that disagrees with its header? The refusal sweep answers that question for php_heap, and it found three such paths. Before this branch, each could leave state that a shutdown function could reach. php's own code has the same ordering in all three places:

- **A full list written past its room.** `write_long` doubled the list's `table_size` before it allocated the larger hash (`Z/Zend/zend_hash.c:1150-1154` does the same). When the limit refused the allocation, the list claimed 16 slots over data for 8. The next write at index 9 went past its data. `packed_to_hash` now takes the new size and writes it together with the new data (`crates/php_heap/src/array.rs:1464`, `:2118`).
- **An iterator moved onto a shared array.** `iterator_pos_ex` copies the shared array first and moves the iterator's count after the copy (`crates/php_heap/src/iterators.rs:427`), so a copy the limit refuses leaves the iterator counted on the array it points at, and destroying that array poisons it.
- **A sort that renumbers.** When the packed data for a renumbered sort was refused, the buckets were left renumbered under an empty hash index, so no key could be found again. The refused path now links the buckets as a hash (`crates/php_heap/src/array.rs:2635`).

All three now leave the array consistent with its memory, and each one failed the sweep natively before its fix.

## CVE-2026-109619, stale global state

**In php:** worker globals (`EG`, `PG`, `PS`, the error backtraces) keep pointers into a request's heap after that request has freed it. Code that runs before the next request has set itself up reads them. The advisory's two consumers are an INI diagnostic raised per request and the session's upload progress.

**In Dax PHP:** request state lives only in the `Request` that `Request::new` builds for each request from the engine's shared state (`crates/php_engine/src/api/request.rs:1041`). The request's heap is created there too (`:1070`).

- The last error, the fatal error's backtrace and the log guard are fields of the request's diagnostics (`crates/php_engine/src/api/request/diagnostics.rs:124`, `:137`, `:164`). The only state carried on purpose is `in_error_log`. It is a `bool` that php's `longjmp` also leaves set, it holds no pointer, and a new request starts with it cleared.
- The session's `PS(rfc1867_progress)` is `State::progress` in the session's request state (`ext/session/src/lib.rs:166-167`). The upload hook takes `&mut Request` (`ext/session/src/progress.rs:72`), so it only ever sees its own request's state.
- The lend slot is the one piece of per-thread state, and a lend fills it for the length of one call.

The worker test reads all three across two requests on one thread.

## What the tests do not cover

- **The hand-written `Send` and `Sync`.** `Zval` is `unsafe impl Send` and `Sync` (`crates/php_heap/src/zval.rs:308`, `:311`). `Arr`, `Obj`, `Str`, `Ref` and `Res` are `Send` (`array.rs:554`, `object.rs:137`, `string.rs:678`, `reference.rs:59`, `resource.rs:145`). The compiler would therefore not stop a request's value from being stored in engine-wide state. ADR-0005 and the manifest's `state.no-thread-local` and `ext.no-locks` rules stop it. The worker test checks the three places the first reading named, and it would miss a value stored anywhere else.
- **The multipart limits.** Dax PHP follows php 8.5.9's limits on multipart bodies. The advisory's route to the limit, through a body with no declared length and part headers with no bound, is a denial-of-service matter here, because meeting the limit corrupts nothing. The limits will change when the oracle moves to 8.5.12, which also brings php's new soft OOM at the next safe point and a hard `_exit()` at a second threshold.
- **Three allocator arms the limit cannot reach.** The system allocator alone can refuse a new chunk or huge mapping (`Out of memory`), or the bin slot for a huge block's list node, which is asked for after the mapping while at least a chunk of the limit is still free. The sweep does not reach these three.
- **Builds without debug assertions.** The lend-slot reading uses a debug-build sensor, and a release build compiles none of it.

## Two more defects after the fatal

Two more defects turned up while the sweep's fences were pinned against the oracle.

- **A huge block grown past the limit.** `realloc_huge` returned the limit's fatal without lifting the limit while the fatal was rendered, and it ignored the lifted limit while another fatal was being rendered. php's `zend_mm_safe_error` does both (`Z/Zend/zend_alloc.c:1640-1651`). It now refuses as `alloc_huge` does (`crates/php_heap/src/mm.rs:961`).
- **A `.=` the limit refused.** The append to a string emptied the variable before growing the string, and when the grow was refused the variable was left `null`. On a class's property, the refused path released the read's owner a second time, which only balanced because the string's own owner had been dropped without a release. php grows the string while the variable still holds it, so a shutdown function reads the string whole. The variable, property or array element now gets its string back (`crates/php_engine/src/ops/mod.rs:3015`, `crates/php_engine/src/handlers.rs:14410`). Five fences beside `concat_assign` and one beside `realloc_huge` pin the oracle's output.

## Commits

- `1c91d1545` php_engine ops: a `.=` the limit refuses leaves the left string where it stood.
- `ebe1a08eb` php_engine ops: concat_assign's second closure is `reinstate`, the name of the helper every caller hands it.
- `747095983` php_heap: every place the request's heap can refuse an allocation is driven to the refusal, and each leaves what it touched whole.
- `4af1f9ee5` php_heap: a debug build senses a heap created while a lend is still in place.
- `5f9ce0be9` sapi/makala tests: a request after one the limit ended in its multipart body starts fresh on the same request thread.
- `f2af82a25` sapi/makala tests: the worker test reads that the first request's upload progress reached its session before the limit met it.
