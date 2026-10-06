//! The callback walks (#1980 candidate 3): a native that runs code of
//! the program for each row of a table, a callback, a magic method or a
//! handler a diagnostic reaches, walks the table here, and the walk owns
//! what it walks while that code runs. Each walk ports one of php's
//! iteration modes once:
//!
//! - [`held`] holds the table for the walk where the program could
//!   give it up, php's `ZVAL_COPY` or `GC_ADDREF` of it, and
//!   [`held_guarded`] guards its recursion beside, php's
//!   `GC_PROTECT_RECURSION`. The default for a body that needs the
//!   request mutably: `extract`, `serialize`, `print_r`.
//!
//! A walk takes the table and lends the body each row's key and value
//! for that step. The body answers a [`Step`]: std's `ControlFlow` to go
//! on or stop early with a value, and `Err` for a throwable, which `?`
//! hands on. The walk owns the loop and calls the body, so what it took
//! is given back on every way out, `?` included, by the walk itself and
//! never by a `Drop` (design §5.1: nothing runs implicitly on drop). A
//! body keeps a row past its step only by taking an owner of its own
//! ([`crate::api::values::Owner::take`]).

use std::ops::ControlFlow;

use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, Owner, PhpArray, PhpRef, Value};

/// What a walk's body answers for one row: `Continue` to go on to the
/// next, `Break` to end the walk there with what the walk answers, and
/// `Err`, what `?` hands on, a throwable that ends the walk as php's
/// loops end on `EG(exception)`.
pub type Step<B = ()> = Result<ControlFlow<B>, Thrown>;

/// A table as a held walk's caller came by it, which says whether the
/// walk must hold it ([`held`]).
#[derive(Debug, Clone, Copy)]
pub enum Reached {
    /// Held by what the caller reached it from, which cannot be written
    /// in place while the walk runs: the call's argument, a table the
    /// native owns, or a row of a table a held walk is lending, which is
    /// written only after the table holding it separates. The walk takes
    /// nothing, so the refcounts the body's code sees are php's.
    Held(PhpArray),
    /// Through a reference the program shares, which code the body runs
    /// may assign over and so give the table its last owner up: the
    /// walk holds one owner of the table the reference holds as it
    /// starts, and under that owner what writes the table separates it.
    Through(PhpRef, PhpArray),
}

impl Reached {
    /// How a value lent as a row, or passed as an argument, reaches the
    /// table it names: an array as it stands, a reference through the
    /// reference; `None` for anything that names no array.
    #[must_use]
    pub fn of(value: &Value) -> Option<Reached> {
        if let Some(cell) = value.as_php_ref() {
            return cell
                .get()
                .as_array()
                .map(|table| Reached::Through(*cell, *table));
        }
        value.as_array().map(|table| Reached::Held(*table))
    }

    /// The table walked.
    #[must_use]
    pub fn table(&self) -> PhpArray {
        match self {
            Reached::Held(table) | Reached::Through(_, table) => *table,
        }
    }

    /// The owner the walk takes: one where the program could give the
    /// table up, none otherwise.
    fn hold(self, req: &Request) -> Option<Owner<PhpArray>> {
        match self {
            Reached::Held(_) => None,
            Reached::Through(_, table) => Some(Owner::take(req, table)),
        }
    }
}

/// How a walk that guards its recursion ended ([`held_guarded`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a walk inside the table already walked nothing, which the caller shows"]
pub enum Guarded<B> {
    /// The rows were walked: `Some` what the body broke with, `None`
    /// past the last row.
    Walked(Option<B>),
    /// A guarded walk is inside the table already, `GC_IS_RECURSIVE`,
    /// and nothing was walked: what a dumper prints `*RECURSION*` for.
    Recursive,
}

/// php's walk over a table it holds: the rows of the table `reached`
/// names in order, each lent to `body` with its key, a reference row
/// lent as the reference. A table reached through a reference is held
/// by one owner until the walk ends ([`Reached::Through`]), as php's
/// `extract` copies its array first (`ZVAL_COPY`, "The array might be
/// stored in a local variable that will be overwritten") and
/// `zend_array_sort_ex` holds what it sorts, so the rows it lends stand
/// while the body runs. The answer is what `body` broke with, `None`
/// where the walk went past the last row.
///
/// ```php
/// function f() { $k = 'kkkkk'; $v = 'vvvvv'; $z = 'ZZZZZ';
///   $arr = ['arr' => 1, $k . $k => $v . $v];
///   echo extract($arr), ','; $x = $z . $z; echo $arr, ',', $kkkkkkkkkk, "\n"; }
/// f();
/// ```
/// ```expect
/// 2,1,vvvvvvvvvv
/// ```
pub fn held<B>(
    req: &mut Request,
    reached: Reached,
    mut body: impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let owner = reached.hold(req);
    let walked = rows(req, reached.table(), &mut body);
    if let Some(owner) = owner {
        owner.give_up(req);
    }
    walked
}

/// [`held`] guarding its recursion, as php's dumpers and encoders guard
/// a table they descend into: where a guarded walk is inside the table
/// already (`GC_IS_RECURSIVE`) nothing is walked and the answer says
/// so; otherwise the table is marked for the walk
/// (`GC_PROTECT_RECURSION`) and unmarked as it ends, on every way out,
/// inside the walk's owner, so the mark is never cleared on a freed
/// table. An immutable table is marked by neither, as php's is not,
/// since nothing can make one hold itself.
///
/// ```php
/// $a = [1]; $a[] = &$a;
/// print_r($a); echo "\n";
/// ```
/// ```expect
/// Array
/// (
///     [0] => 1
///     [1] => Array
///  *RECURSION*
/// )
///
/// ```
pub fn held_guarded<B>(
    req: &mut Request,
    reached: Reached,
    mut body: impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Guarded<B>, Thrown> {
    let table = reached.table();
    if table.is_recursive() {
        return Ok(Guarded::Recursive);
    }
    let owner = reached.hold(req);
    table.protect();
    let walked = rows(req, table, &mut body);
    table.unprotect();
    if let Some(owner) = owner {
        owner.give_up(req);
    }
    walked.map(Guarded::Walked)
}

/// The rows of `table` from its first bucket, each lent to `body`, until
/// the last or the first that breaks or throws. The table is read again
/// at each step, so a row stands as long as the bucket that holds it.
fn rows<B>(
    req: &mut Request,
    table: PhpArray,
    body: &mut impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let mut pos = 0;
    while let Some((at, key, value)) = table.live_from(pos) {
        pos = at.saturating_add(1);
        if let ControlFlow::Break(broke) = body(req, &key, &value)? {
            return Ok(Some(broke));
        }
    }
    Ok(None)
}
