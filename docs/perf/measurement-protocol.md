# The measurement protocol

Every perf number this engine states is taken under this document
(`third-engine-design.md` §9, §14; `harness-design.md` §3, §4.2). It is
inherited from the second engine, whose own protocol stands at
`mahout_base/docs/perf/measurement-protocol.md` at 726 lines; what is copied
here is the part that binds a run, and §10 says what was left behind and why.
`xtask bench` is the one instrument that runs it.

Version `3`: the ancestor's `1`, a request-shaped arm under the same
paired rule (§3, §5; #2351), which made `2`, and that arm's concurrent
form, whose rep is a rate (§3, §4.2; #2660). A change to §3, §4 or §5
bumps it; adding a statement or a recorded field does not.

## 1. What it refuses

1. **No number without a content pin.** A commit is not a binary. Each arm's
   program is digested at both ends of the run, and a disagreement refuses
   rather than reports (§6).
2. **No signed number whose sign is not established.** When the paired
   delta's inter-quartile range straddles zero the row is `NOISE-SWAMPED`
   and is reported unsigned, whatever the medians say (§4.3).
3. **No timing before correctness.** The two arms' bytes are compared inside
   every rep, and a disagreement stops the run before a timing is read (§5).

## 2. The unit is a paired A/B run

One measurement is two arms, each run once per rep, close together in time.
The paired per-rep delta is the statistic that survives a loaded machine;
absolute medians are reported beside it and never carry the verdict. The
second engine established this by running it: deltas forty times the noise
floor kept their sign through a load average of thirteen on a twelve-core
box, where the medians did not.

**The A/A control.** A bar whose two arms are the same call. Its bar is the
protocol's own verdict: the rig must be unable to tell the arm from itself,
so the row must come back `NOISE-SWAMPED`. A resolved row is a systematic
order effect the run failed to cancel, which is a defect of the instrument
and not a finding about the arm.

This is the amended sentence and not the original one. §4.3 is a quartile
rule with no confidence level, so it resolves some unmeasured fraction of
the time on a null effect; the second engine measured one resolved row in
seventeen. **A single resolved A/A row is evidence; a pattern of them is the
defect.** An instrument that refused on the first resolved row would be red
for no reason about once a run in seventeen, which is why `bench` records
the verdict and exits 0 (`harness-design.md` §4.2).

**An A/A control over a set** of programs (§10) is held to a count, as
Mitchell ruled on #3127. It fails when at least k of its n rows resolve,
k the smallest count whose chance of being reached, each row resolving
at the rate above, one in seventeen, is at most 1%: the binomial's upper
tail. For n = 8, P(≥ 2) ≈ 7.7% and P(≥ 3) ≈ 0.9%, so k = 3; a set of one
has no such k and fails on none. k is computed from n and the rate,
never typed in, and the reading keeps how many rows resolved, so §12's
series can show a pattern across nights. One pair is held to its one
row, as above. Holding each row of a set as one pair would read `fail`
on about 38% of a set of eight's runs with nothing wrong.

## 3. The knobs

| knob | value |
|---|---|
| warm-up reps, discarded | 2 |
| measured reps per arm | 12 |
| order within a rep | alternated: even reps run arm A first, odd reps arm B first |
| processes per rep | one per arm, back to back |
| requests per rep, a request-shaped arm | one per arm, back to back, to a server started once for the bar |
| clients per rep, a concurrent request-shaped arm | 8 per arm, the arms back to back, each client asking until 3 s have passed and then finishing the request it has in flight |
| what a concurrent rep measures | the requests a second its clients were answered at, over the time its last client took to end |

Twelve paired deltas give a stable interpolated inter-quartile range. The
response to `NOISE-SWAMPED` is more reps, a quieter machine or a larger
workload, in that order; re-labelling the verdict is not a response.
Alternating the order is what keeps a penalty on whichever process runs
second from being charged to one arm.

A request-shaped arm times a request as its client pays for it: the
connection, the request and the response read to its end, against a server
the bar starts before its first rep and ends after its last, the server's
binary pinned as a program is (§6). It is how the production server is
measured against php-fpm (#2351); the two arms reach their hosts as each is
reached, so the bar states its clients, and a server's own A/A control
stands beside it.

Its concurrent form measures throughput where the one request measures
latency (#2660, `third-engine-design.md` §9.2's "8 threads, one process ≥
php-fpm with 8 workers"). Each arm's rep is 8 clients at once, each
sending the request again as soon as it is answered, one connection a
request as the one-request arm sends it (HTTP/1.0 over loopback TCP to
the server, `FastCGI` over a Unix socket to php-fpm, so the bar times
each host as it is reached), until 3 seconds have passed;
each then finishes the request in flight, and the rep is the requests
answered over the time from the first sent to the last answered. The
servers are the bar's, started before its first rep: a server of 8
request threads, and php-fpm with `pm = static` and `pm.max_children =
8` behind the same `FastCGI` client. The warm-up is the two discarded
reps, which reach every request thread and child before a rep is kept.

## 4. The statistics

### 4.1 Quantiles

Median, Q1 and Q3 by linear interpolation on nearest ranks over the sorted
samples `a`, one-indexed:

```
idx = p * (n - 1) + 1        p in { 0.25, 0.5, 0.75 }
lo  = floor(idx)             fr = idx - lo
q   = (lo >= n) ? a[n] : a[lo] + fr * (a[lo + 1] - a[lo])
```

`IQR = Q3 - Q1`. Median rather than mean and IQR rather than standard
deviation, because the failure mode on a shared machine is a few very slow
reps. No trimmed mean, no confidence interval, no bootstrap, and no
percentile: a percentile is a per-arm description and is never signed.

### 4.2 The paired delta

`delta[r] = a[r] - b[r]` for each measured rep, in the bar's unit: the
milliseconds a run took, or a concurrent rep's requests a second (§3).
Reported: the median, Q1, Q3 and IQR of the delta sample, and
`paired_pct = median(delta) / median(b) * 100`, which is the number to
quote. A rate's loss is a negative delta, so what reads a delta as better
or worse, the distance to go and §12's rule, reads a rate's turned: more
is worse of a time, and less of a rate. The reading keeps each figure
under its unit, `_ms` or `_rps`.

### 4.3 The verdict

```
NOISE-SWAMPED  when  n < 4  or  (Q1(delta) <= 0 and Q3(delta) >= 0)
resolved       otherwise
```

The `n < 4` term is a repair, not a taste: at one rep Q1 and Q3 are the
sample itself, so any nonzero delta cleared zero and a binary compared with
a copy of itself came back signed. `<=` and `>=` and not `<` and `>`: a
quartile exactly at zero still straddles it. `NOISE-SWAMPED` is reported and
not signalled, and it is not a claim that the two arms are equal.

## 5. Correctness before timing

Per rep, never once at the end. The two arms' standard streams, or a
request-shaped arm's response body, its head being what each host is, are
compared byte for byte inside the rep that produced them, and a
disagreement stops the run. A concurrent rep holds every response its
clients were answered with to one body, and an answer of another stops
the run as two arms that disagree do. How the two compared is named in the reading,
in four values:

| value | meaning |
|---|---|
| `stdout-bytes` | both arms wrote the same non-empty bytes |
| `body-bytes` | both arms' responses carried the same non-empty body |
| `vacuous` | both arms wrote nothing, so nothing was compared |
| `differ` | the arms disagreed; the run stopped |

A vacuous row is named out loud. Its timings stand; its agreement proves
nothing. The second engine added this value after running the instrument
found it: its startup rows printed nothing, both sides hashed the empty
string, and it was reported as agreement.

## 6. The content pin

Each arm's program is read once at the start of the run and once at the end,
and the digest and byte count of that one read are compared. A program that
moved underneath the run refuses the numbers rather than reporting them,
and the refusal takes precedence over a correctness disagreement, which it
explains. There is no override.

## 7. Hardware and load

Every run records the 1-, 5- and 15-minute load average at its start, and
every rendering of a number prints the load beside it. A number taken under
load is not void and not automatically valid; the load is recorded, not
controlled.

A machine held for the bench is controlled, as Mitchell ruled on
#3767 for dax-build: `xtask bench --machine dedicated` takes every seat
before it builds, so no other instrument builds there while it measures,
and starts a bar's reps only once the 1-minute load is below 2. Above it
the run waits, looking again every 15 s, and the reading names how long it
waited and the highest load it waited out; a load still at 2 or above after
15 minutes leaves the bar unmeasured, a skip that names the load. Every
rendering and reading also names the machine: its processor, its cores and
its kernel.

## 8. Status labels

What standing the numbers are offered with, carried by the reading:

| label | what it says |
|---|---|
| `UNLABELLED` | the default; a number with no standing claimed |
| `SMOKE` | a run made to see the instrument work |
| `PROVISIONAL` | a number offered with a written reservation |
| `BASELINE-OF-RECORD` | a number offered as one later runs are held against |

`PROVISIONAL` and `BASELINE-OF-RECORD` owe a note, and a run that gives one
without a note refuses: a standing with no written reservation is a word,
not a label. Which label a run deserves is part of the sign-off, and the
sign-off is a person's (`harness-design.md` §4.2, §11). What a nightly bar
is held against is not a label but the ratchet's reading, the bar's
baseline that §12 keeps in the store and moves; the label says what
standing a person gives a run, and moves no baseline.

## 9. Honest limitations

The verdict is wall time alone. A makala bar also states what each arm's
server retired per request, instructions, cycles, CPU and runnable time,
read with `proc_pid_rusage`, as evidence beside the rate and never as the
verdict (#3062). A code-layout confound of the kind Mytkowicz described would not be
caught here. The `NOISE-SWAMPED` rule is a quartile rule and not a
hypothesis test: it has no confidence level, no *p* and no power analysis.
That two arms are comparable at all is asserted by whoever declared the bar
and is held by nothing here except §5.

## 10. What this engine's copy leaves out

The ancestor carries, and this engine has no use for yet: a corpus manifest
and its twenty-three workloads, a group selection over them and the
self-timed `TOTAL`/`CHECKSUM` contract their PHP prints; an on-disk unit
cache switch and a second executor, neither of which this engine has; a
binary-vintage tripwire over build products under the repository root; the
ancestor's regression gate, one run held against a baseline of record with
quiet, shared and hosted hardware tiers, which §12's two-night rule over a
ratcheted baseline replaces; and the ancestor's request-shaped
kind, whose per-arm latency percentiles are descriptive and never signed.
Each returns here when the engine has the thing it measures, as the
request-shaped arm has, paired under §2 to §5 rather than as percentiles
(§3, #2351), and as a geomean over workloads has (#2948): a bar over a
set of programs pairs each program under §2 to §5, states each one's
ratio as `1 + paired_pct / 100`, and their geometric mean, which inherits
the floor — one `NOISE-SWAMPED` constituent and the aggregate is
published unsigned. A geomean has a sign only when every row resolves
the same way, as Mitchell ruled on #3127: one whose rows resolve in
opposite directions is published unsigned as `mixed`, with the count
each way, since +20% on one program and −17% on another make a geomean
near 1.00, and signing it would claim no change while hiding a
regression. A bar held to a bound is held row by row and not by its
geomean. It fails where any row resolves slower (§4.3) with its ratio
beyond one plus the tolerance, and passes otherwise. A `NOISE-SWAMPED`
row is never a regression (§1.2) and holds nothing. The geomean, signed,
`mixed` or unsigned, is published and holds nothing.

## 11. Where a workload lives

A bench workload a ticket names is committed under `docs/perf/workloads/`,
beside this document, and a ticket that asks for a reading against a named
workload cites its committed path (#2491). A bar file stays on the machine
that ran it, since its arms name that machine's binaries. A workload kept
out of the repository is lost: #1808's `calls.php` was.

Each workload is byte-identical to the copy its first reading ran over, so
a header comment the file did not carry then is not added now. What a file
cannot say of itself is declared once in `docs/perf/workloads.toml`, beside
the directory: its role, the arguments of each of its runs, the status they
end on and the ticket that first read it (#4335, #4344). `xtask bench`,
`xtask dispatch-census` and `xtask trace-dump` reach a program only through
that declaration, a program under the directory with no row is refused, and
the table below is rendered from it. A program the dispatch census counts
that lives elsewhere, php-src's `Zend/micro_bench.php` or a counting loop
given whole, is declared there too. A role is a person's to give.

<!-- computed exec cmd="cargo xtask status --section workloads --tracked" inputs=workloads.toml,workloads/*.php,../../xtask/src/workloads.rs,../../xtask/src/status/workloads.rs name=workloads | do not edit; run computed -->

| workload | role | each run's arguments | ends on | first read by | what it is |
|---|---|---|---|---|---|
| `fib.php` | kernel | none | status 0 | #2740 | one of the seven kernels, Mahout's `benchmarks/src/fib.php` with `$N` at the size `third-engine-design.md` §9.2 names |
| `nbody.php` | kernel | none | status 0 | #2740 | one of the seven kernels, Mahout's `benchmarks/src/nbody.php` with `$N` at §9.2's size |
| `mandelbrot.php` | kernel | none | status 0 | #2740 | one of the seven kernels, Mahout's `benchmarks/src/mandelbrot.php` with `$N` at §9.2's size |
| `spectralnorm.php` | kernel | none | status 0 | #2740 | one of the seven kernels, Mahout's `benchmarks/src/spectralnorm.php` with `$N` at §9.2's size |
| `binary_trees.php` | kernel | none | status 0 | #2740 | one of the seven kernels, Mahout's `benchmarks/src/binary_trees.php` with `$N` at §9.2's size |
| `fannkuch.php` | kernel | none | status 0 | #2740 | one of the seven kernels, Mahout's `benchmarks/src/fannkuch.php` with `$N` at §9.2's size |
| `string_munge.php` | kernel | none | status 0 | #2740 | one of the seven kernels, Mahout's `benchmarks/src/string_munge.php` with `$N` at §9.2's size |
| `bench_quiet.php` | bench | none | status 0 | #1808 | php-src's `Zend/bench.php` with `end_test` and `total` printing names only, so its bytes are deterministic |
| `wordpress_shapes.php` | shape | none | status 0 | #4329 | original code, written from the opline counts, its side panels' sizes and sort orders following WordPress's widget defaults, with the mix a counting build of the loop met over the four WordPress bench pages; #4329's last comment sets their top twenty variants side by side |
| `calls.php` | probe | none | status 0 | #2011 | #1808's `calls.php` rebuilt from #1808's comment, not #1809's file, so its medians are not comparable to #1809's rows |
| `frame_chain.php` | probe | none | status 0 | #1301 | recursive user calls, then a loop of native calls: the bar #1384 holds the readable frame header to |
| `natives.php` | probe | none | status 0 | #2209 | a loop of native calls, each return kind #2209 names |
| `ucall.php` | probe | none | status 0 | #1901 | micro.php's `ucall` kernel with nothing timed in the bytes |
| `Zend/micro_bench.php` of the pinned php-src | probe | none | status 0 | #3285 | php-src's `Zend/micro_bench.php` at the pin, run whole |
| `loop-empty`, its code in the declaration | probe | none | status 0 | #3273 | #3273's counting loop, a million rounds of an empty body |
| `loop-add`, its code in the declaration | probe | none | status 0 | #3273 | #3273's counting loop, a million rounds of `$s = $i + 1;` |
| `loop-add-assign`, its code in the declaration | probe | none | status 0 | #3273 | #3273's counting loop, a million rounds of `$s += $i;` |
| `loop-mul`, its code in the declaration | probe | none | status 0 | #3273 | #3273's counting loop, a million rounds of `$s = $s * 1.0000001;` |
| `compression.php` | native | `small 1`, `large 1`, `incompressible 1`, `incremental 1`, `gzip 1`, `zip 1`, `mysql 1` | status 0 | #2826 | zlib's raw deflate, incremental deflate, gzip, zip and mysql payload over generated text and bytes, the mode and an iteration count its arguments |
| `fileinfo.php` | native | `buffer`, `file`, `wrapper`, `cold` | status 0 | #2827 | `finfo` over a buffer, a file, a stream wrapper and a cold database, 256 times each, the profile its one argument |
| `mbstring_profiles_2831.php` | native | `convert small`, `convert large`, `mime small`, `mime large`, `cut small`, `cut large`, `http small`, `http large` | status 0 | #2831 | mbstring's conversion, MIME header encoding, `mb_strcut` and output handler over a small and a large input, the mode and the size its arguments |
| `uri-rfc-read-update.php` | native | none | status 0 | #2832 | `Uri\Rfc3986\Uri` raw, normalised, cloned with an update and resolved |
| `uri-rfc-unicode.php` | native | none | status 0 | #2832 | `Uri\Rfc3986\Uri` byte and percent normalisation, not WHATWG or IDNA |
| `uri-rfc-validation.php` | native | none | status 0 | #2832 | `Uri\Rfc3986\Uri` over valid and malformed inputs |
| `uri-whatwg-read-update.php` | native | none | status 0 | #2832 | `Uri\WhatWg\Url` ordered reads, immutable updates and a relative base |
| `uri-whatwg-unicode.php` | native | none | status 0 | #2832 | `Uri\WhatWg\Url` host mapping, NFC and deviation inputs at the pin, not modern IDNA |
| `uri-whatwg-validation.php` | native | none | status 0 | #2832 | `Uri\WhatWg\Url` special, opaque, file, IP and malformed inputs |

<!-- /computed in=8ed51a69c1e1b98cfa2f3c172a5607b63880630e14a22baee951f65b1da2452f out=ea437975d9d77a0258c689c9e32a2e5a3b805f5a7f6fef6e842783d710fe336d -->

## 12. The tolerance a nightly bar is held to

The rule every perf bar is held by, as Mitchell ruled on #1902 and #2336
(#2659). It replaces a bound a person writes for each bar: the numbers come
from the bar's own series by this rule, and no person sets them one by one.
A night is one reading of the bar: its paired delta's median, Q1 and Q3
(§4.2) as a share of the second arm's median, in percent.

1. **A night fails on an established regression.** A night is an
   established regression when its paired delta's whole inter-quartile
   range lies worse than the bar's baseline by more than its tolerance,
   Q1 above the baseline plus the tolerance, a rate's night turned so
   that more is worse (§4.2). A `NOISE-SWAMPED` night (§4.3)
   is never one: §1.2 applied to the bar.
2. **Two nights in a row fail the bar.** One such night is a warning on its
   reading. A second in a row fails the reading and the nightly lane, and
   files a ticket naming the bar, both nights and the commits from the last
   night that held to the first that did not.
3. **The tolerance is the bar's own A/A spread, floored at 2%.** It is the
   75th percentile of the bar's A/A control's nightly |paired delta| over
   the bar's first two weeks of readings, and never below 2 points
   (`third-engine-design.md` §9.2's revisit line for G1). The derived
   number and the dates it came from land in `seams.toml` as one
   `[[bench]]` row per bar, through `xtask steer`. A bar without a row is a
   series and fails nothing. Until the bar's A/A control has its two
   weeks of nights, the row carries the 2% floor alone and names no
   nights, as Mitchell ruled for G1 on #3130. A bar held to a bound and
   not a baseline, G1, whose door should cost nothing, is held to one
   plus its row's tolerance: each row's ratio at most 1.02 at the
   floor. The row is the one place that bound is written.
4. **The baseline ratchets, and is a reading.** The baseline is a reading
   of the store, `<bar>/baseline`, never a manifest value. The first is the
   median of the bar's nights over the tolerance's dates. It moves only
   when two nights in a row each beat it as an established improvement,
   the inter-quartile range wholly better by more than the tolerance, and
   then to the worse of those two nights' medians; each move is a reading
   of its own naming the two nights, so the history says when and why a
   bar tightened. The nightly lane's store starts empty, so the lane reads
   a bar's earlier nights and its baseline from the published history.
5. **The distance to go gates nothing.** §9.2's absolute targets (1.5×,
   1.2×, at least php-fpm) stay in each reading as the distance to go, as
   #1902 ruled.
