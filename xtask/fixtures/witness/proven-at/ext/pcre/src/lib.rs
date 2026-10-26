//! A negative control (harness-design.md §4.1, §4.3; #3365, #3369): an
//! extension whose first four witnesses a `trace:` row of the fixture's
//! `docs/divergence.toml` covers, the row naming `proven_at =
//! "optimizer-off"`. Each fence pins the linked engine's side and the
//! engine meets it. The first prints what the engine prints whatever the
//! oracle's optimizer does, and the second a pid its fence's wildcard
//! meets, so the oracle under the regime agrees and each is covered; the
//! third prints `oracle` under any php without
//! `zend_test`, which `php-linked`'s bundle carries and the oracle does
//! not load, so the oracle under the regime still differs and the fence
//! fails, naming its id. The fourth is #3366's snippet, which the oracle
//! under opcache prints `1` of, its SCCP folding a root away, and `2`
//! with the optimizer off, as the engine does: the fence the regime
//! exists for, covered only because the regime's ini is applied.
//!
//! The last two pin the oracle's side under `witness:` rows naming the
//! same `proven_at`, so the engine's miss is diverged: #3366's snippet
//! again, where the oracle under the regime prints the engine's `2` and
//! the fence stays diverged, and a `zend_test` probe, where it still
//! prints `oracle` and the fence fails.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness the proven row covers and the oracle agrees on.
///
/// ```php
/// echo 1;
/// ```
/// ```expect
/// 1
/// ```
pub fn preg_quote() {}

/// A native whose witness the proven row covers and whose bytes move with
/// the process: the oracle's run under the regime meets the fence as the
/// engine's does, its wildcard over the pid, so it is covered.
///
/// ```php
/// echo getmypid();
/// ```
/// ```expect
/// %d
/// ```
pub fn preg_split() {}

/// A native whose witness the proven row covers and the oracle differs on
/// under the regime too.
///
/// ```php
/// echo extension_loaded('zend_test') ? 'engine' : 'oracle';
/// ```
/// ```expect
/// engine
/// ```
pub fn preg_match() {}

/// A native whose witness the proven row covers and the oracle meets only
/// under the regime, its optimizer off (#3366).
///
/// ```php
/// class D { private array $p = [];
///   function a() { parse_str('', $this->p); }
///   function c() { $this->p += ['x' => [str_repeat('a', 2)]]; } }
/// $d = new D; $d->a(); $d->c();
/// echo gc_status()['roots'], "\n";
/// ```
/// ```expect regime=immutable-literals
/// 2
/// ```
pub fn preg_replace() {}

/// A native whose witness pins the oracle's side, which the engine misses
/// and a `witness:` row accepts, the oracle under the regime printing the
/// engine's bytes.
///
/// ```php
/// class E { private array $p = [];
///   function a() { parse_str('', $this->p); }
///   function c() { $this->p += ['x' => [str_repeat('a', 2)]]; } }
/// $e = new E; $e->a(); $e->c();
/// echo gc_status()['roots'], "\n";
/// ```
/// ```expect regime=immutable-literals
/// 1
/// ```
pub fn preg_grep() {}

/// A native whose witness pins the oracle's side, which the engine misses
/// and a `witness:` row accepts, the oracle under the regime still not
/// printing the engine's bytes.
///
/// ```php
/// echo extension_loaded('zend_test') ? 'linked' : 'oracle';
/// ```
/// ```expect
/// oracle
/// ```
pub fn preg_last_error() {}
