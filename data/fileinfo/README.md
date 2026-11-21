# Fileinfo transaction and storage evidence — #2827

This is bounded prerequisite evidence for #2836, under the confirmed #2823
contract, engine §7.3 and ADR-0015. It is **not a replacement**, a new approval,
a shipping switch, or proof that fileinfo's native closure has disappeared.
The baseline is main `06eb2a8662e4e277f19a5c53ceb602ed9a45ce2e`.

## Reproduction and standing

`python3 scripts/fileinfo-proof.py --scratch <approved-existing-directory>` reproduces
`oracle.json`. `--accept` captures observations, not rustdoc witness verdicts.
The runner verifies the pinned PHP's digest, the SHA-256 where `[oracle]` carries
one and the MD5 where it carries only that, and the php-src commit, invokes the existing
single-extension witness to assert the complete pinned keg closure, checks vendored
algorithm/database inputs against that tree, records SHA-256/size/source lines,
and records the pin digest, recipe and before/after closure assertions. Arguments,
ini, cleared/rebuilt environment (including private HOME/TMPDIR) and timeout are
the pin's; raw streams keep their trailing newline rather than adopting witness
trim semantics. The 71 bounded cases each have their own
process and scratch directory. Only those directories and probe paths are
normalised, including their JSON-escaped spellings; warning text, order,
statuses, memory readings and cursor effects are not normalised. Malformed
database exceptions also retain a hex representation of the exact message
bytes, alongside the readable UTF8-substituted JSON text. Fatal memory
refusal is an observation, not a passing classification.

`python3 -m unittest discover -s scripts/tests -p test_fileinfo_proof.py` holds
the pin check to either digest and to a pin with neither, and runs
three retained negative controls: changed defined behaviour fails comparison,
undefined sampled bytes do not become an expected value, and identity/pin
changes still fail. The CLI comparison has been exercised with an intentionally
altered receipt as well as the reproducible capture.

The checked-in PHP files are input data for this extraction. The tiny compiled
fixtures are generated into scratch by a **fixture writer**, not a production
magic compiler or a second PHP executor. Their headers and populated records
are explicit, pinned `file.h` format fields; both endian fixtures classify as
`compiled`. Inputs, their digests, raw observations and source accounting are
accessible in this directory. No runtime downloads or ambient `file(1)`
database are used. The experiment runs on aarch64 macOS only; the four-target
shipping proof remains #2833's shared collection authority, not a claim here.

Seams tested: the existing PHP `finfo`/`mime_content_type` interface, PCRE's PHP
capture/error interface, and the already locked Rust native-face `Emalloc`,
request-state/lifecycle and `Capture` interfaces. This is the confirmed bounded
proof scope, not a request for a new test seam.

## Domain distinctions

An architectural ruling assigns responsibility; a bounded proof establishes
facts or falsifies a representation; #2836 selects exact storage/signatures and
budgets; a later implementation performs the port; completed-scope certification
requires that implementation's removal/correctness/performance evidence.

The **loaded snapshot** is immutable selected rule content. A **live session**
also owns mutable flags, continuation/search state and retained output. A
**classification transaction** observes the live session at PHP's points, not a
frozen copy of it. A held active session keeps storage alive; it is not a Rust
mutable borrow across PHP. A descriptor's open-file offset is not necessarily
the PHP stream's cached logical position. No glossary or design decision is
changed by these distinctions.

## 1. Live transactions: observed and source-delimited

`transactions.php` records open/stat/read/cast/close/error-handler/deprecation
callbacks, nested classification, `set_flags`, outer return and a later default
classification. Both zero and MIME-type (`16`) overrides are used; callback
throws are isolated in separate cases.

- In open/stat callbacks a nested default call sees the outer nonzero override
  (`image/gif`), not the object's prior description flags. A callback changing
  flags to MIME encoding changes outer classification to `us-ascii`.
- A zero override leaves callback changes installed; a nonzero override
  restores the saved flags even after the observed callback throw. A close
  callback sees classification already finished, can change flags, and occurs
  before restoration. It does not retroactively reformat the returned text.
- Nested buffer calls during read/cast share scratch: the observed outer output
  is `us-asciius-ascii`. This is not permission to isolate/freeze the job or
  silently remove shared-session effects. No Rust state borrow spans these calls.
- The buffer-context deprecation occurs **before** fetching the active magic
  pointer. Reconstruction there is defined: the replacement remains installed,
  outer MIME-type classification returns `image/gif`, and the later call returns
  `us-ascii`. Throwing that deprecation aborts before temporary flags are set.
- Wrapper/open and identification diagnostics occur while temporary flags are
  live. File close precedes restoration. Buffer's final `Failed identify data`
  warning follows restoration. Source: pinned `fileinfo.c:237-381`; resource
  rewind/classify/failure-warning/restore/close: `:429-452`.
- Identification failure, match-time regex compilation warnings and buffer's
  final failure warning have separate mutate/throw cases. File identification's
  handler sees `image/gif` during a nonzero override, then close precedes restore.
  Regex compilation's handler sees temporary MIME flags and its mutations can
  change the outer retained output. The missing-use compiled rule produces
  buffer's final failure warning: even with outer override 16, the handler's
  empty-buffer classification is `empty`, **not** `application/x-empty`, proving
  flags were restored before that warning. A later empty-buffer call shows
  nonzero outer restoration versus zero-override persistence independently of
  the failing rule; callback throws retain the same operation-specific ordering.

Current `ext/fileinfo/src/lib.rs:265-280` takes `Magic` out of the object; same
object callback `set_flags`/classification therefore reaches `Invalid finfo
object`, rather than the live session the oracle exposes. Reinstallation when
the slot is still empty can revive extracted state after failed reconstruction.
This is a **source-delimited baseline deficiency**, not new desired behaviour.
`:413-440` loops prefix reads and drains diagnostics after C returns;
`:542-554` restores flags before draining C diagnostics. `shim.c:134-155` retains
only sixteen diagnostics; `ffi/mod.rs:192-213` truncates diagnostic buffers.
Delayed/bounded publication cannot establish the oracle's callback point/order.
No production fix is smuggled into this evidence commit.

### Cases the proof does not compare

Reconstruction is captured in the deprecation case only, before the magic is
acquired; no case reconstructs from a stream callback.

The `stat-fail/mime` receipt is kept as evidence and is not compared: its
standing retains its status/streams but excludes those values from reproduction
equality; only its identity/standing are compared. `stat-fail/file` is compared:
first stat failure closes and returns false without classification. The cases
left out are filed separately for a person's ruling and block the concrete
approval, not the bounded proof. No registry reason is authored here.

## 2. Stream and descriptor effects

`streams.php` exercises plain, filtered, short-reading, failed-stat/read,
nonseekable, failed-rewind/restore and throwing-close streams, for file and
resource transactions. `descriptors.php` supplies a real file/socket cast target
and records its release and **next bytes**, not only cached `ftell`.

- PHP makes one logical prefix `php_stream_read` (`magic.c:248-256`). A wrapper
  asked for 8192 returns two bytes; fileinfo returns `text/plain` and does not
  fill again to obtain the rest of `GIF89a`. The current Rust loop is different.
- The resource transaction attempts rewind, stat, read, descriptor cast and
  restore in that order; a cast can add seek/tell callbacks. The restore-fail
  receipt ends at 600 instead of its initial 3. A socket emits two seek warnings
  and ends at 6. Neither result promises cursor neutrality.
- Read failure yields `Failed identify data 0:cannot read '(null)'`, then close
  for file; for resource it attempts restoration before the caller's close.
  A throwing file close suppresses the ordinary return. A throwing caller close
  after resource classification occurs after its answer/position observation.
- Tail rule `-4 string TAIL` recognises a file longer than the 7 MiB prefix,
  whereas classifying the complete buffer does not use a file tail and returns
  text. Tail acquisition is lazy and bounded: `buffer.c:65-99` uses one `lseek`
  followed by one exact-length `read`; errors mark `FILE_BADSIZE`. No `pread`
  substitute is equivalent.
- Real descriptor tail success leaves next bytes empty although target `ftell`
  reports 0. A short backing file (declared regular, actual smaller) fails the
  exact read; a socket fails seek and still has `TAIL` queued. Missing cast leaves
  backing bytes `78787878` untouched. The default recognizer also may inspect
  tail data, so `plain` is not an assertion of zero descriptor reads.
- CDF uses the pinned PowerPoint fixture. Prefix-backed reads use memory when
  available; lazy sectors otherwise seek/read the descriptor (`cdf.c:338-363`).
  The observed next bytes `43007500` demonstrate a moved open-file offset;
  descriptor ownership ends once at wrapper close. The fixture SHA-256 is in
  the receipt. Truncated CDF/DER cases are retained in `recognition.php`.

The cast request in `funcs.c:330-339` is `PHP_STREAM_AS_FD` **without** INTERNAL.
Current Rust `identify_stream` requests `Cast::Internal`; both flush/seek, but
only `Cast::Fd` carries PHP's non-internal buffered-byte diagnostics. Existing
`Stream::dup_fd` is not a transparent replacement: it duplicates and has its own
flush/keep semantics. A hold must cover the actual cast target, not merely an
integer or an unrelated outer wrapper.

## 3. Actual allocation/storage proof

The oracle owns actual populated storage; `storage.php` observes usage after
open, output creation, dropping the returned PHP string, resetting output, and
dropping the session. These are **whole PHP observation deltas**, not an exact
allocation tracer. Positive residuals include harness/cache/array work and are
not claimed leaks; first-use differences between native and swapped cases are
not claimed endian overhead. Values are pinned to this build/recipe.

| profile | open delta | with output | after output drop | after reset | after session drop |
|---|---:|---:|---:|---:|---:|
| built-in | 736 | 1176 | 1120 | 1016 | 216 |
| custom text | 1440 | 1496 | 1464 | 1464 | 216 |
| custom native compiled | 1792 | 2128 | 2088 | 2072 | 376 |
| custom opposite-endian compiled | 1632 | 1968 | 1928 | 1912 | 216 |

Thirty-two built-in sessions increase usage by 24,464 bytes, not 32 copies of
the 10 MiB database, and still do real per-session work. A 121-byte output
retains 760 bytes after its PHP string is dropped; reset reduces that delta to
272. The final negative delta releases storage allocated before that probe's
baseline, not a claim of negative allocation. The 8 MiB limit case prints
`before-open`, then refuses the **actual 17,280,432-byte compiled database**;
`after-open` is absent and exit is 255.

Load-time `MAGIC`, current custom contents and loaded snapshots are observed:
old text session answers `first`, a newly opened session answers `second`,
changing `MAGIC` selects `compiled`, and reconstruction reads that current
selection. `custom:absent` procedurally warns yet preserves the valid loaded
database; constructor EH_THROW and malformed text/compiled paths have distinct
partial-failure outcomes. Version/count/truncation failures can fall back to
text parsing and report invalid offsets; they must not be rewritten as neat
compiled-only errors.

Actual source sites (pinned `libmagic/apprentice.c`):

- `:546-592`: zeroed session, flags, ten allocated continuation slots (initialised
  at use) and
  optional C locale; `:530-543`: lists, output buffers, continuation, locale and
  session release. `:615-708`: real list heads/maps and partial-load cleanup.
- `:3210-3344`: custom stream stat, exact database allocation/read, header and
  counts, byte swapping and all partial cleanup. The normal built-in points at
  immutable data; only opposite-endian **built-in** loading copies the full
  database (`:3293-3299`). That branch is source-demonstrated, not executed on
  this little-endian supported host. Both **custom** endian forms were executed.
- `magic.c:204-261`: `bytes_max + 1 + sizeof(VALUETYPE)` prefix storage, zeroed
  slop and cleanup. `encoding.c:96-173`: populated codepoint scratch, optional
  EBCDIC scratch and real release. `funcs.c:523-645`: retained output reset,
  printable allocation `4*strlen+1`, continuation growth `20+level`.

`ext/fileinfo/tests/storage.rs` proved an already permitted safe byte-storage
route (#3099 removed it; `crates/php_engine/src/tests/storage.rs` holds the
request heap's storage now): actual record fields and `GIF89a` bytes, decoded
continuation offsets, grown/zeroed storage, 121 output bytes plus NUL,
eight-byte alignment, explicit reclamation after each partial stage, real
memory-limit refusal after populated initialisation, and three successive
request-end releases. No system-owned duplicate content/shadow charges or raw
typed casts exist. The negative control intentionally drops the holder without
freeing: the leak sensor reports three blocks/736 bytes. The successful holder
instead retains blocks in request state and explicitly frees them in rshutdown
after ending the state borrow.

This proves **byte storage**, not a complete typed live-session representation,
GC integration, a stable layout on four targets or instruction-level allocation
identity. `Emalloc` has no typed destructor contract. Structured session fields
need either actual encoded bytes with private accessors, or a separately proved
typed storage proposal. Ordinary `Vec<Rule>` plus duplicate charges is rejected.
Individual session destruction cannot be postponed to request end just because
the request-state example is useful for retained scratch. Exact ownership,
failure and cleanup points return to #2836.

## 4. Narrow existing seams: concrete proposals, not exports

PHP's `config.m4:44` declares PCRE dependency; `php_pcre.h:49-57` is the authority.
Current fileinfo instead links pcre2-sys and `shim.c:381-475` has a duplicate
per-thread cache, fixed limits and ignored locale policy. Current
`php_bundle`'s `fileinfo` feature does not activate `pcre` itself.

| need/source | existing operation / concrete proposed addition |
|---|---|
| FILE_REGEX, `softmagic.c:2278-2349` | use `Regex::compile` and existing `Regex::match_impl(req, haystack, false, PREG_OFFSET_CAPTURE, 0)`; construct real PHP haystack, keep/destroy full PHP return/capture owners, then read global match/offset |
| text-load validation, `apprentice.c:2814-2827` | `Regex::compile` at load with converter options 0; do not precompile every compiled rule or use match-time multiline/caseless validation |
| locale-unaware output replacement, `funcs.c:655-688` | proposed `Regex::compile_locale_unaware(req: &mut Request, pattern: &[u8]) -> Option<Regex>`, header `_cache_ex(..., false)` |
| raw-subject replacement, same source | proposed `Regex::replace_raw(&self, req: &mut Request, subject: &[u8], replacement: &[u8], limit: i64, count: &mut usize) -> Result<Option<Owner<PhpString>>, Thrown>`, header `subject_str == NULL`; no extra PHP subject allocation |
| stream cast/lifetime/direct seek/read | proposed `Stream::cast_held(self, req: &mut Request, cast: Cast, warn: bool) -> Result<Option<StreamDescriptor>, Thrown>` with opaque, request-owned target hold, not a direct extension/process edge |
| descriptor calls/cleanup | proposed `StreamDescriptor::{seek(&mut self, offset: i64, whence: Whence) -> io::Result<i64>, read(&mut self, into: &mut [u8]) -> io::Result<usize>, release(self, req: &mut Request)}`; internally existing process/layer seam gains safe `BorrowedFd` seek/read operations, one OS call each, preserving shared offsets/errno and exact short reads |

The descriptor shape is a **proposal requiring lifetime proof/approval**, not
permission to duplicate blindly, keep a stale number, or ignore target-close
callbacks. No new public generic allocator/matcher/callback framework is needed.
Surface estimate: two PCRE items (7 → 9); one acquisition, one opaque type,
three operations (5 stream/native-face items), two process operations mirrored
through the layer (4 locked items total). Bundle proposal:
`fileinfo = ["dep:php_ext_fileinfo", "pcre"]`, required PCRE dependency on the
extension declaration and a ruled fileinfo → PCRE consumer edge. No fileinfo →
php_process edge. Existing process/stream homes suffice for OS calls; exact lock
lines/home permissions/representation are #2836's, with shared authority #2833.

`regex-storage.php` demonstrates load-versus-match diagnostics: malformed
compiled regex loads successfully, fails compilation on match and leaves
`preg_last_error() == 1`; malformed text fails at load. Backtrack/recursion
limits yield 2/3, not a generic boolean. Warm zero-capture versus fifty-capture
matches have observed peak deltas 416 versus 123,800 bytes; whole transaction
peaks do not attribute every byte, but falsify allocation-free offsets as a
default replacement. All participating PHP captures/offsets are retained as
evidence. C and en_US.UTF-8 locale matches run; they do not by themselves prove
locale-unaware cache identity. `pattern.rs:113-155` keys the current cache with
locale, so the header-backed false-policy operation is genuinely missing.
Raw-subject behaviour is mapped to the precise header/source allocation sites,
not claimed implemented or PHP-callable without that approval.

Rust proposal invoice: runtime/compile-time/binary costs are **unmeasured**;
private byte accessors are rung 1, reversible, without new generics. The five
descriptor and two PCRE public items would be permanent API until a lock change;
they foreclose treating raw offsets or duplicated fd numbers as equivalent.
Their concrete consumer is fileinfo's observed transactions. No speed/binary
claim accompanies that invoice.

## 5. Complete source/data scope and counted estimate

The extraction inventories every configured source, all vendored headers and
shim separately. There are 14,815 physical libmagic C lines (including disabled
compression and conditional strcasestr), 663 substitute-shim lines, 1,555 header
lines including shim headers, and 10,117 **data-representation** lines in
`data_file.c` (20,506,735 source bytes). These are measured input counts, not
Rust ceilings. The current counted fileinfo reading is 454/1000, workspace
461,726/480,000; test-only inputs do not expand the shipping implementation.
The new-code constraint is still unmeasured until inherited classification is
settled; scripts have no manifest ceiling and that is not a completion claim.

Decoded actual built-in bytes are 10,355,712; used format bytes 10,355,472 plus
240 zero padding. Format 19, 432-byte records, 16,771 binary and 7,199 text
rules; 3,979 top-level records, continuation levels through 30, 351 regex
records. Type/level histograms and the binary digest are in `oracle.json`.
The extractor rejects unknown escapes/declarations/record types, bad counts and
nonzero trailing data instead of silently omitting inputs. All 59 non-invalid
type IDs remain grammar scope even when absent from the built-in histogram.

Recognition order is `funcs.c:330-518`: descriptor/stat facts, empty/one byte,
encoding, conditional OS2/compression, tar, JSON, CSV, SIMH, CDF, conditional ELF,
ordered binary softmagic, text properties and text softmagic, fallback,
separator trim and MIME encoding. `PHP_FILEINFO_UNCOMPRESS` and `BUILTIN_ELF`
are not enabled by the pinned fileinfo configuration; `__EMX__` is outside the
approved targets. Compiling compress.c is not enabling decompression. Gzip
signature recognition stays enabled. Conditional strcasestr fallback remains
inventoried; a Rust replacement must preserve its locale/byte algorithm where
reachable, not an ambient libc implementation.

The rule/loader scope includes numeric signedness/endian/masks/relations,
float/date/MSDOS/varint/offset, strings/pstrings/search/regex/UTF16/GUID/DER,
name/use/indirect/clear/default, strength ordering, continuation levels and
offset arithmetic, bounds, directory/path-separator loading, custom text and
compiled format, byte swapping, MIME/apple/ext formatting and escaping,
warnings/errors/limits. Encoding scope includes ASCII/control-byte tables,
UTF7, UTF8 with/without BOM, UTF16/32 both endian, Latin1, extended bytes and
EBCDIC, not `char` or UTF8-only conversion. Representative inputs exercise
those families, malformed UTF8 (observably Latin1), malformed JSON/CDF/DER,
regex limits, captures, continuation strength and malformed custom data.
The 62 PHPTs are a floor, not exhaustive recognizer/security coverage.

### Estimate basis for #2836

These are **review estimates**, not selected numerical manifest values. Their
unit is counted handwritten Rust lines in private implementation, independent
of whether translated from C. Group bounds reflect complete source families
and their operations, not a uniform C-to-Rust ratio:

| complete responsibility | physical C input | estimated counted Rust |
|---|---:|---:|
| load/grammar/strength/compiled format (`apprentice`) | 3673 | 3000–5000 |
| rule/continuation matching (`softmagic`) | 2519 | 2300–3800 |
| CDF/date/sector recognition (`cdf`, `cdf_time`, `readcdf`) | 2522 | 2000–3300 |
| text/encoding/output/date/DER (`encoding`, `ascmagic`, `funcs`, `print`, `der`) | 2756 | 2200–3700 |
| tar/JSON/CSV/SIMH, filesystem/buffer/transaction (`is_*`, `fsmagic`, `buffer`, `magic`) | 1858 | 1500–2500 |
| actual session/storage/holds/cleanup | overlaps above | 500–1000 |
| immutable data validation/build-output generator | separate data | 100–200 |
| PCRE/stream/process/layer items, bundle/declaration | header backed | 120–260 |
| new retained proof/regression cases | separate tests | 400–800 |

Total estimated counted shipping handwritten implementation: 11,720–19,760
lines; with proposed retained proof cases: 12,120–20,560. The estimate counts
maintained algorithm code, not disabled decompression/OS2 as secretly enabled
features. Tests/witnesses and generated implementation accounting must follow
#2833, not an exemption invented here. Existing fileinfo 1000 does not authorise
this increment. A whole-workspace switch forecast subtracts the measured 454
thin Rust lines only once, producing approximately 472,992–481,032 shipping
lines before unrelated concurrent growth. Its high end exceeds the current
workspace ceiling: the person must decide from actual port increments.

Package estimate: 10,355,712 authoritative database bytes plus licensed source
lineage/update receipts; Rust recognizer/source and debug/release binary sizes
are **unmeasured**, not equated to C source size. Proposed data embedding uses
one immutable byte input with build-output representation, not a committed
20 MiB generated C/Rust listing. No maintained third-party Rust classifier,
compression backend, new package or prebuilt tool is selected by this proof.
PCRE2 remains native until #2841's separate route completes; OS/runtime helpers
and allocation/locale/formatting algorithms remain in the shared census.

### Provenance, update and exact reporting proposal

Authoritative **current** input is php-src's pinned `data_file.c`, checked
byte-identical to the vendored copy, plus its libmagic LICENSE and patched
sources. `generate_patch.sh:3-16` names file 5.46 and its algorithm patch;
`create_data_file.php:3,38-48` consumes a precompiled magic database and pads
1024-byte C strings. `magicdata.patch` is additional data provenance; raw
upstream magic source/compiler receipts are **not supplied by that C listing**.
Reproducing an independently rebuilt database is therefore not claimed.

Bounded external primary fact verified: the tagged PHP LICENSE at
https://raw.githubusercontent.com/php/php-src/php-8.5.9/ext/fileinfo/libmagic/LICENSE
requires preserved source notices and reproduced binary-distribution notices;
translation does not erase those obligations. This is research evidence, not a
person's release-compliance judgement. #2836 should approve the authoritative
compiled-byte input initially or require separately pinned original inputs,
compiler/tool digest and extraction receipt before changing that update route.

Proposed update receipt names algorithm source commit/file digests, database
source and decoded digests/format/type census, generator identity, input/output
digests and licenses. Re-run extraction (reject unknown input), all transaction/
allocation/corpus proofs, adversarial parser/offset/regex/recursion cases and
CI-only full/fuzz/native-closure lanes. Classify source, data and orchestration
changes separately; route security fixes for person review independently of an
oracle bump. No runtime downloads or installed-library version selects data.

Exact informational proposal for a later completed implementation:

```
fileinfo support => enabled
fileinfo implementation => dax-php Rust recognizer
libmagic compatibility target => 5.46
libmagic algorithm lineage => php-src php-8.5.9, file 5.46, PHP patches
magic database format => 19
magic database SHA-256 => d9545ebe36da3ad435a915022374bd8f0715050e526680ccdc1dfb4b96badb9f
```

The implementation row must additionally carry the actual landed implementation
commit/version, **not invented now**. Existing `libmagic => 5.46` claims actual
linkage today; removing/replacing it is a particular informational rendering
decision/registry row on #2836. PHP extension version remains its compatibility
target. No absent library will silently be reported linked; no reporting row or
divergence reason is changed by this commit.

## 6. Workloads and blocked CI measurements

`docs/perf/workloads/fileinfo.php` contains four real inputs: warm 4096-byte
buffer, plain file, custom wrapper, and 256 newly opened custom text databases.
Each emits a nonempty SHA-256 of all classification answers; recipe-run
correctness outputs and the content digest are in the receipt. No clock,
speedup or local bench invocation occurs.

The CI measurement proposal is four separate A/B bars (current transition
binary versus eventual candidate) and each arm's identical A/A control, under
measurement-protocol version 2: content pins before/after including input data,
correctness comparison **every rep**, two discarded warmups, twelve measured
reps, alternating order, load averages, paired median/Q1/Q3/IQR and the unsigned
NOISE-SWAMPED rule. Retain raw readings, binary/source/configuration/host pins
and the person's control sign-off. `bench --from` currently marks custom bars
as controls; its status is not an approved replacement-speed threshold.

Timings/baseline-of-record are **blocked pending the permitted CI collection
and person-set workload/tolerance authority**, not zero, skipped-as-green or
predicted. #2836 already owns those acts and #2833 the shared measurement
authority; no new benchmark ticket offloads an unperformed local obligation.
There is no shipping candidate to compare. The workload correctness baseline
is complete and the no-material-regression decision is still the person's.

## Validation boundaries

Rust test/Clippy, witness, extension-only PHPT/no-regression, crosscheck,
api-check, budget, unsafe-audit, vocabulary, regions and done readings accompany
the judged branch. Kernel/native bodies, public locks, manifests, oracle and
divergence registry are untouched. No new runtime native needs a witness: the
storage adapter is test-only. The final ticket comment records exact readings
and review receipts rather than treating this document as their replacement.
