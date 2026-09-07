# api.lock v0 — the census behind it

Drafted 2026-09-07 from Mahout at `1b3104a8` (19 `mahout_ext_*` crates, 105k lines, test modules excluded) by
`scripts/census.sh`; joined to the lock in `data/port-map.v0.toml` (651 rows: 165 same, 336 renamed, 150 gone).

## Counts per source: script vs the r2 census (`data/r2-mahout-ext-seam-census.txt`, HEAD d6460ef0)

| Source | r2 census | census.sh at 1b3104a8 | Why they differ |
|---|---|---|---|
| mahout_value | 89 of 283 pub fns | 82 methods (of 162 inherent names on the public value types; 188 inherent, 92 free, 15 trait fns) + 9 free fns + 43 `Type::name` associated fns and variants | r2 matched every pub fn name on any receiver; the script matches only names the public value types define, drops 135 sites with a literal/`String`/`Vec` receiver (`as_bytes` 126 of them), and adds the constructors r2 could not see (`Value::null` 372, `PhpArray::new` 266, `PhpString::from_bytes` 265, `Key::Int`/`Str` 357). r2's `instance_of` (47) and `lookup` (44) are `ctx.classes.*`, ClassTable members, not mahout_value |
| RequestCtx | 83 of 413 | 115 methods (of 380 `pub fn`; 453 with `pub(crate)`) + 59 public-field members (`ctx.classes.descriptor` 119, `ctx.ini.get` 20, …) | the script counts the `r.ctx().` and `self.ctx.` receivers too (`Reentrant::ctx` is called 1,209 times); r2 counted `ctx.` only. `note_alloc`: 327 on any receiver (194 on `ctx.`), the figure §4.6 uses |
| semantics:: | 135 | 171 (155 fns, 16 types/consts): 125 by `semantics::` path outside tests (135 with tests, r2's number) + names imported with `use mahout_runtime::semantics::{..}` and reflection's one glob | `Raised` alone appears at 1,614 sites |
| mahout_native_abi | 38 pub items | 38 pub items, 18 named by an extension | the other 20 (`NativeCtx`, `NativeMethod`, `ArgCursor`, `UnionArm`, …) are the macro's, never a body's |
| mahout_native_macros | — | 28 attribute forms: `sig` on 438 sites, `tentative` on 133 (2 crates), `raw` on 21 | `sig_tentative` does not exist at 1b3104a8 (a doc comment only); the rename is `#[php_method(tentative)] → #[tentative]` |
| mahout_linker / code_meta / native | 12 / 12 crates (Cargo) | 13 / 27 / 4 items; 7 / 8 / 5 crates reference them in source (9 distinct) | dom, pdo, session and tokenizer depend on them in Cargo.toml and never name them |
| other mahout_runtime | — | 82 items (`Reentrant` 164 + its 8 methods, `EngineBuilder` 81, `ext_state_mut` 33 in 7 crates, `ExtState` 11, native_marshal 6, resources 18, settings 12, output 5, gpc 4, codecs 4) | needed for the port-map's `ExtState`, `Reentrant` and state rows |

## The lock: 301 items (ceiling 400)

values 86 · raising 17 · coerce 22 · call 29 · request 98 · decl 17 · abi 28 · attr 4. Raising + coercion = 39, §4.4's "≈ 40".
30 items have no census row: the §4.4 additions (`instantiate`, `autoload`, `Cache` and its two methods, `Extension`, the builder's
`constants`/`ini`/`engine_state`/`rinit`/`build`, `StartupIni`, `FromArgument`, `IntoReturn`, `Request::engine`, `decl::Name`,
`decl::FunctionDecl`, `Resources::{insert,get,remove}`, `BinaryOp`) plus type rows whose methods are the census's (`PhpObject`, `PhpRef`,
`ArrayIter`, `ClosureInfo`) and `PhpString::{len,is_empty,append}`, whose sites the port-map credits to the `PhpArray` twin of the same name.
The most-collapsed targets: `Request::ini_get` (23 Mahout items), `attr::php_class` (15), `PhpObject::set_prop` (10), `PhpObject::closure` (9).

## Gone: 150 rows, 2,899 sites, 92 rules (the full list is in the port-map; grouped by rule, ≥ 4 sites)

- 1,373 · `Reentrant`, `Reentrant::ctx` — re-entry is a parameter (principle 9): every native receives `&mut Request`; no Plain/Reentrant split
- 438 · `php_method(sig)`, `php_function(sig)` — the signature is derived from the Rust fn; the stub cross-check verifies it
- 327 · `RequestCtx::note_alloc` — nothing; the allocator accounts
- 163 · `php_class(registered)`, `php_class(id)` — extension() pairs declarations with entries; class identities are the linker's
- 156 · `FoldedName`, `ExactName`, `NativeFnDecl`, `build_linked_descriptor`, `ClassInput`, `TraitAdaptations`, `NameScope`, `strip_native_ids`, `assert_sig`, `parse_function_sig` — not needed once natives declare classes through the macros; the macro emits the declaration builder; lookups fold names themselves
- 51 · `True`, `Nullable`, `php_function(raw)`, `php_method(raw)` — `bool`, `Option<T>`, typed parameters and unions; `Variadic<Value>` covers the rest
- 49 · `ArrayOrString`, `type_mode`, `TypeMode` — an `array|string` parameter is an `abi::Union` enum; `coerce_param` reads the caller's `strict_types` itself
- 46 · `PhpObject::addr`, `guard_alloc`, `reserve_or_raise`, `ARRAY_ELEM_BYTES`, `engine_limitation`, `msgs::ENGINE_LIMITATION` — identity is the handle; the allocator raises the memory-limit fatal; no engine-limitation raise, unported behaviour is a divergence in the oracle registry
- 28 · `spl_array_view`, `is_spl_array_family`, `spl_array_flags`, `SplArrayState`, `set_spl_array_flags`, `ensure_spl_array_storage`, `spl_array_*_payload`, `spl_dual_it_receiver` — ArrayObject's storage is ext/spl's own `NativeClass` payload; the engine reaches it through `ObjectHandlers`
- 31 · `LazyDumpView`, `dump_view`, `mark_ghost_debug_viewed`, `observe`, `LazyOp`, `lazy_objects_is_empty`, `lazy_pinned`, `note_props_materialized`, `unset_dying_count` — lazy initialisation on access and its bookkeeping are the engine's handlers'
- 21 · `UnitId`, `classes.unit_of` — units are not addressed by natives; `current_file` and the declaration views carry the file
- 18 · `render`, `messages::render`, `msgs`, `Reflection*_to_string` (8 fns) — messages are rendered at the raising site; `__toString` is ext/reflection's rendering over `class_decl`
- 22 · `drain_handler_calls_sync`, `handler_routes`, `queue_handler_call`, `errors.has_pending`, `errors.pending_is_exception`, `suppress_*`, `suppressed`, `silence_cancel`, the 5 tick items — the engine runs the error handler at the raise; a `Thrown` in hand is the pending exception; `@` and `declare(ticks)` are the engine's
- 17 · `php_class(prototypes)`, `codecs::*`, `resources::{Backend,Link,SelectKind,WriteOutcome}`, `scheme_of` — declared like any method; codecs are ext/standard's; a stream's backing is inside `request::Stream`
- 16 · `native_call_by_name`, `native_early_bound`, `native_discard`, `NoDiscardNote`, `take_native_named_args`, `native_marshal::*`, byref helpers (4) — the ABI knows the call shape, matches named arguments and owns the byref table; `#[\NoDiscard]` is a declaration fact
- 12 · `adopt_iters_from`, `fixup_iters`, `TypeTag::Closure`, `php_stable_sort_by`, `reserved_scope_word`, `ScopeWord` — iterator fixup is php_heap's; a closure is an object of class Closure; std's sort is stable; scope words resolve in `lookup_class`
- 31 · the remaining 39 rows of ≤ 3 sites each (`ScratchValue`, `RawValue`, `strtok_*`, `engine_handle`, `RequestCtx::new`, `declare_class`, `instantiate_user_filter`, `wind_*_chain`, `ProcPipe`, `host_note`, `CaptureBackend`, …), each with its one-sentence rule in the port-map

## The 20 most-called items kept by name

`len` 748 · `as_bytes` 700 · `get` 527 · `This` 520 · `#[php_function]` 495 · `Value::null` 372 · `set` 359 · `OrFalse` 347 · `Value::bool` 288 ·
`PhpArray::new` 266 · `PhpString::from_bytes` 265 · `is_empty` 220 · `Value::array` 209 · `Value::int` 205 · `as_object` 190 · `Key::Int` 186 ·
`Key::Str` 171 · `contains` 154 · `Value::string` 153 · `append` 151 — then `snapshot` 145, `as_array` 141, `as_ref` 140, `class` 121, `tag` 117.
Of §4.4's own top-20, 18 are kept by name; `instance_of` becomes `values::PhpObject::instance_of` and `lookup` becomes `call::lookup_class`.

## Linker leakage: what the crates imported and what covers it

| Crate | mahout_linker / mahout_code_meta items | Covered by |
|---|---|---|
| curl, random, date (declaring) | `ClassDescriptor`, `ClassFlags`, `ClassKind`, `MethodDesc`, `MethodSig`, `ConstRow`, `PropDefault`, `Visibility`; `NameScope`, `UnitId` | not needed once natives declare classes through the macros (`build_linked_descriptor`, 49 sites, is the whole use) |
| json | `ClassKind`, `Visibility` | not needed (declaration only) |
| spl | the above + `PropDesc`, `PropHooks`; `Deprecation`, `NativeMethodId`, `ParamTy`, `ScalarType`, `TypeMode` | declaration: not needed; `coerce_param` takes `decl::Type` and reads `strict_types` itself |
| reflection (reading) | `ClassDescriptor`, `ConstRow`, `EnumBacking`, `MethodDesc`, `PropDesc`, `PropDefault`, `Visibility`, `ClassKind`, `ClassFlags`; `AttrSpec`, `AttrArg`, `ClassId`, `render` | `request::Request::class_decl` → `decl::{ClassDecl, MethodDecl, PropertyDecl, ClassConstDecl, ConstExpr, EnumBacking, Visibility, ClassKind, ClassFlags, Attribute, AttributeArgument}`, `values::ClassId` |
| standard | `ClassDescriptor`, `ClassInput`, `TraitAdaptations`, `EnumBacking`, `MethodSig`, `PropDesc`; 21 code_meta items (`msgs::*`, `byref_*`, `ScopeWord`, `StaticVarLit`, `PhpErrorKind`, `ErrorLevel`, …) | `class_decl`, `declared_classes` for get_class_*; `declare_class` gone; messages and byref helpers gone; `StaticVarLit` → `decl::ConstExpr`; `ErrorLevel` → `raising::Level` |
| ctype, pcre, date | `ErrorLevel`, `msgs::ENGINE_LIMITATION` | `raising::Level`; the limitation raise is gone |
| dom, pdo, session, tokenizer | Cargo dependency only | nothing to port |

## Where the census contradicts §4.4

1. "345 distinct engine items": the script finds 389 over the four census sources alone (134 value, 115 RequestCtx, 171 semantics, 18 abi) and 651 with RequestCtx's public fields, the macro forms, the linker family and the rest of mahout_runtime. The surface is wider than §4.4 sized it; the lock still lands at 301.
2. §4.4's "83 RequestCtx items" is the `ctx.`-receiver count. 12 of 19 crates reach the request through `Reentrant::ctx` (1,209 sites), and 59 members are reached as public fields (`ctx.classes.descriptor` at 119 sites is the second-largest single RequestCtx use after `note_alloc`). The Request group is 98 items, a third of the lock; output, headers, ini, handlers and streams are most of it.
3. `ext_state_mut` is at 33 sites in 7 crates, not 35 in 6. `throw_internal` is at 181 sites (128 counted on `ctx.`), `raise_text_warning` 116. `sig_tentative` is not an item at this commit.
4. "203 sites hand-type the native's name": 176 `b"name(): …"` message literals plus 84 raiser calls passing a `func`/`fname` argument (32 of them `check_fs_access`) — 260 by this count; the collapse rule stands either way.
5. `engine_limitation` (15 sites, 7 crates) has no §4.4 item. **Resolved 2026-09-07: gone.** php has no such error, so no witness can pin one. Ten sites go with the C bindings (curl, iconv, mbstring, dom), two exist only because Mahout could not re-enter user code where `call::call` and the object handlers now can (SplPriorityQueue::compare override, PhpToken `__get` override), and three are unported behaviour (RegexIterator modes, `unpack` non-integer codes, `htmlspecialchars` charsets) whose natives fail their own witnesses until ported; the failed witness proposes the open divergence row, and `done` fails while it is open. No interim raise exists, so a native either does what php does or is visibly not done.
6. `instance_of` and `lookup` in §4.4's value top-20 are ClassTable members (see above), and `mahout_native`'s `build_linked_descriptor` (49 sites, 4 crates) is the actual linker leak, not the `use mahout_linker` lines: the descriptors are constructed by a helper the census did not census.
