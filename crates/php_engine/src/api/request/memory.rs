//! The memory area of a request: the heap reached through the
//! request, interned and request strings, references, arrays, the
//! memory limit and its verdict, `memory_get_usage()` and the garbage
//! collector's natives. Its state is the heap itself, which every area
//! reaches, so it stays on the request.

use std::sync::Arc;

/// A heap apart from the request's, `zend_mm_startup`'s: started, swept
/// and shut down whole by whoever holds it, and never a place a value of
/// the request's is allocated from (`zend_test_gh18756`).
pub use php_heap::StandaloneHeap;
pub use php_heap::{Allocation, AllocationObserverId, ObserveAllocations};
use php_heap::{Arr, Fatal, Heap, Intern, Key, Ref, SortEntry, Str, Zval, ZvalRef};
pub use php_heap::{ecalloc, efree, emalloc, erealloc};

use crate::api::abi::{Beneath, chain};
use crate::api::raising::Thrown;
use crate::api::values::{Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, adopted};

use super::{Hooks, Ini, Request};

/// The request's memory facet (design §4.4): what its heap has served
/// and lends to C, the collector's roots, the objects it keeps for a C
/// library (ADR-0013) and its resources. [`Request::memory`] reads it
/// and [`Request::memory_mut`] changes it; either is the whole request,
/// since nearly every operation can serve PHP code.
pub struct Memory<R>(pub(crate) R);

impl Request {
    /// The memory facet, to read.
    #[must_use]
    pub fn memory(&self) -> Memory<&Request> {
        Memory(self)
    }

    /// The memory facet, to change.
    pub fn memory_mut(&mut self) -> Memory<&mut Request> {
        Memory(self)
    }
}

/// The home a unit this request alone serves makes its constant strings
/// in, its compile's `zend_new_interned_string` as opcache installs it,
/// `accel_new_interned_string_for_php` (`Z/ext/opcache/ZendAccelerator.c:577`):
/// the engine's string where the request has taken its [`SharedHold`] and
/// the engine's table, opcache's buffer, holds the bytes, which costs the
/// request nothing, and else a string of the request's that the unit owns
/// and gives up with its op arrays ([`super::unit_strings`]), one per
/// time the compile asks, as php's scanner makes one per occurrence and
/// nothing finds it again. The empty string and one byte are the
/// process's, as the scanner makes them (`ZVAL_STRINGL_FAST`). So a
/// constant an `eval` folds away or reads once is gone when the `eval`
/// returns, one a variable keeps is the variable's, and two written
/// alike are two strings:
///
/// ```php
/// $m = memory_get_usage(); eval('strlen("abcdef");'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('strlen("abcdef");'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('echo "zzzzzzzzqq";'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('$y = "abcdefgh";'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('$y = "abcdefgh";'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('$a = "xxxxxxxx"; $b = "xxxxxxxx";'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('$c = "yyyyyyyy"; $d = "yyyyyyyy"; $e = "yyyyyyyy";'); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect regime=jit-off
/// 0
/// 0
/// zzzzzzzzqq0
/// 40
/// 0
/// 80
/// 120
/// ```
///
/// It is opcache's in every regime (design §5.2, revision 150). Without
/// opcache php's compile interns through `zend_new_interned_string_request`
/// into `CG(interned_strings)`, kept to the request's end, so under the
/// recipe's `php -n` an `eval` reads other than php's, a by-design
/// divergence (#3224, landed by #3233): less by a class's runtime
/// definition key php keeps, the engine reading `256 576 512` (the file
/// is written where the key's length, which names it, stands still):
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3233-key.inc';
/// file_put_contents("$f." . getmypid(), <<<'PHP'
/// <?php
/// trait T9 {} interface I2 { function i(); }
/// $m = memory_get_usage(); eval('class Q9 { use T9; }'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('class CI implements I2 { function i() {} }'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('interface I3 extends I2 {}'); echo memory_get_usage() - $m, "\n";
/// PHP);
/// rename("$f." . getmypid(), $f);
/// include $f;
/// ```
/// ```expect exit=0
/// 368
/// 688
/// 624
/// ```
///
/// and more where two constants alike are one string in php's table and
/// two here, less by a constant php keeps, the engine reading `80` and
/// `0`:
///
/// ```php
/// $m = memory_get_usage(); eval('$a = "xxxxxxxx"; $b = "xxxxxxxx";'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('echo "zzzzzzzzqq";'); echo "\n", memory_get_usage() - $m, "\n";
/// ```
/// ```expect exit=0
/// 40
/// zzzzzzzzqq
/// 40
/// ```
pub(super) struct RequestStrings<'a> {
    pub(super) shared: &'a crate::engine::Shared,
    pub(super) hold: SharedHold,
    pub(super) heap: &'a mut Heap,
    /// The strings and arrays the compile has made, which the unit
    /// takes.
    pub(super) made: &'a mut super::unit_strings::Compiling,
    /// Whether what it lays is interned ([`Intern::interning`]).
    pub(super) interning: bool,
}

impl RequestStrings<'_> {
    /// Whether a string asked for is looked for in the engine's table
    /// first: where it is interned and the request holds opcache's
    /// buffer.
    fn asks_table(&self) -> bool {
        self.interning && self.hold == SharedHold::Taken
    }
}

impl Intern for RequestStrings<'_> {
    fn intern_marked(&mut self, bytes: &[u8], valid_utf8: bool) -> Result<Str, Fatal> {
        if bytes.len() <= 1
            && let Some(frozen) = self.shared.permanent(bytes)
        {
            return Ok(frozen);
        }
        if self.asks_table()
            && let Some(held) = self.shared.existing_interned(bytes)
        {
            return Ok(held);
        }
        let s = self.heap.string(bytes)?;
        if valid_utf8 {
            s.mark_valid_utf8();
        }
        self.made.add(Zval::string(s));
        Ok(s)
    }

    /// `zend_string_init` of `room` bytes cut to `bytes`, and
    /// `zval_make_interned_string` over it where it is interned, which
    /// finds the engine's string where the request has taken its
    /// [`SharedHold`].
    fn intern_in(&mut self, bytes: &[u8], room: usize) -> Result<Str, Fatal> {
        if self.asks_table()
            && let Some(held) = self.shared.existing_interned(bytes)
        {
            return Ok(held);
        }
        let mut laid = self.heap.string_fresh(room.max(bytes.len()))?;
        laid.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
        laid.set_len(bytes.len());
        let s = laid.finish();
        self.made.add(Zval::string(s));
        Ok(s)
    }

    fn interning(&mut self, on: bool) -> bool {
        std::mem::replace(&mut self.interning, on)
    }

    /// A declaration's own string, kept as what its place in the
    /// compile's walk names when the unit is taken
    /// ([`Request::hold_compiled`]).
    fn hand_name(&mut self, name: Option<Str>) {
        self.made.hand(name);
    }

    /// A constant the request defined, substituted where an `eval`
    /// names it: one more owner of its value, `ZVAL_COPY_OR_DUP`, which
    /// the unit keeps as it keeps a constant its compile made, so the
    /// unit costs nothing for it and gives the owner back with its op
    /// array ([`super::unit_strings`]); a value the request's heap holds
    /// is always one this home can hold.
    fn hold(&mut self, value: Zval) -> bool {
        if value.is_refcounted() {
            self.heap.addref(value);
            self.made.add(value);
        }
        true
    }

    /// A string the fold that made it has read, given up, as
    /// `zend_eval_const_expr` frees an operand once the value it made stands; one
    /// the engine's table holds is no string of the compile's. An array
    /// stays the unit's, with the owners of its entries it took over, and
    /// goes with its op arrays.
    fn give_back(&mut self, made: php_heap::Made) {
        let Some(string) = made.value().as_str() else {
            return;
        };
        if self.made.take(Zval::string(string)) {
            self.heap.release_string(string);
        }
    }

    /// A constant array of a unit this request alone serves, as php's
    /// compile leaves it where no cache keeps the unit
    /// (`zend_try_ct_eval_array`, `Z/Zend/zend_compile.c:10085`): a plain
    /// array of the request's, `array_init_size` and each entry written
    /// in order, which the unit owns and gives up with its op arrays as
    /// `destroy_op_array` frees its literals ([`super::unit_strings`]); a
    /// value copied out of it takes an owner of its own (`ZVAL_COPY`), so
    /// what a variable keeps outlives the unit and what nothing keeps is
    /// gone with it. A value the compile made for the unit is moved into
    /// the array, as php's fold moves the zval it evaluated, and any
    /// other is one more owner; a key the compile made is the array's
    /// alone once written. So 100 `eval`s of one array cost nothing
    /// past the variable that keeps the last:
    ///
    /// ```php
    /// eval('$x = ["alpha" => "beta", "gamma" => [1, 2, "delta"]];');
    /// $m = memory_get_usage();
    /// for ($i = 0; $i < 100; $i++) { eval('$x = ["alpha" => "beta", "gamma" => [1, 2, "delta"]];'); }
    /// echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage();
    /// for ($i = 0; $i < 100; $i++) { eval('$y = ["k1" => "zzzzzzzzzz", "k2" => [1, 2, "qqqqqqqqqqq"]]; unset($y);'); }
    /// echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$z = ["k3" => "wwwwwwwwwwww"];'); echo memory_get_usage() - $m, "\n";
    /// eval('debug_zval_dump(["k4" => "vvvvvvvvvvvv"]);');
    /// eval('$w = ["k5" => "uuuuuuuuuuuu"]; debug_zval_dump($w);');
    /// debug_zval_dump($w);
    /// ```
    /// ```expect regime=jit-off
    /// 0
    /// 0
    /// 448
    /// array(1) refcount(2){
    ///   ["k4"]=>
    ///   string(12) "vvvvvvvvvvvv" refcount(1)
    /// }
    /// array(1) refcount(3){
    ///   ["k5"]=>
    ///   string(12) "uuuuuuuuuuuu" refcount(1)
    /// }
    /// array(1) refcount(2){
    ///   ["k5"]=>
    ///   string(12) "uuuuuuuuuuuu" refcount(1)
    /// }
    /// ```
    fn constant_array(&mut self, entries: &[(Key, Zval)], size: usize) -> Result<Arr, Fatal> {
        if entries.is_empty() {
            return Ok(Arr::empty());
        }
        let size = size.max(entries.len());
        let a = (self.heap).array_sized(u32::try_from(size).unwrap_or(u32::MAX))?;
        self.made.add(Zval::array(a));
        for &(key, z) in entries {
            let moved = self.made.take(z);
            if !moved {
                self.heap.addref(z);
            }
            if let Err(fatal) = self.heap.array_add_new(a, key, z) {
                if moved {
                    self.made.add(z);
                }
                return Err(fatal);
            }
            if let Key::Str(s) = key
                && self.made.take(Zval::string(s))
            {
                self.heap.release_string(s);
            }
        }
        Ok(a)
    }
}

/// opcache's `ZCG(counted)`: whether the request holds opcache's shared
/// memory, taken by `accel_activate_add` in `persistent_compile_file`
/// once a file to compile is open (`Z/ext/opcache/ZendAccelerator.c:2106`)
/// and given back at the request's end. Until then
/// `accel_init_interned_string_for_php` and
/// `accel_new_interned_string_for_php` ask the buffer nothing, so what a
/// request interns before its first file compiles, `$_SERVER`'s names
/// and a constant a host registers, is a string of its own whatever the
/// engine holds; an `eval` and `-r`'s code compile no file and take no
/// hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedHold {
    /// No file compiled yet: nothing is looked up.
    Untaken,
    /// A file compiled: the engine's table answers.
    Taken,
}

impl Request {
    /// The request's own memory (design §5.2, §5.13), which every value
    /// of the request's is allocated out of.
    pub(crate) fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// `zend_string_init_interned` and
    /// `zend_string_init_existing_interned` as opcache installs them,
    /// both `accel_init_interned_string_for_php`
    /// (`Z/ext/opcache/ZendAccelerator.c:610`): the engine's string for
    /// `bytes` where the request has taken its [`SharedHold`] and the engine's
    /// table, opcache's buffer, holds them, which costs the request
    /// nothing, and otherwise a string of the request's own,
    /// `zend_string_init`, whose one owner is the caller's. Nothing is
    /// kept to be found again, so two asks are two strings, and the
    /// empty string and one byte have no shortcut of their own: opcache's
    /// buffer holds `ZSTR_CHAR`'s strings (`accel_copy_permanent_strings`,
    /// `:650-652`), so they answer from the table once the hold is taken,
    /// and before it they are strings of the request's as any other. The
    /// `Err` is the memory limit.
    ///
    /// `php_hash_environment` names `$_SERVER`'s rows before the primary
    /// file compiles, so a variable of one byte costs what one of two
    /// does:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/first' . getmypid() . '.php';
    /// file_put_contents($f, '<?php echo memory_get_usage();');
    /// $read = function (array $env) use ($f) {
    ///     $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', $f], [1 => ['pipe', 'w']], $pipes, null, $env);
    ///     $out = stream_get_contents($pipes[1]); proc_close($p); return (int) $out;
    /// };
    /// $none = $read([]);
    /// echo $read(['A' => '1']) - $none, " ", $read(['AB' => '1']) - $none, "\n";
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off process capture
    /// 32 32
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn interned(&mut self, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
        if self.shared_hold == SharedHold::Taken
            && let Some(held) = self.shared.existing_interned(bytes)
        {
            return Ok(Owner::adopt(PhpString::of(held)));
        }
        self.string_init(bytes).map(Owner::adopt)
    }

    /// The request's [`SharedHold`] taken, as `persistent_compile_file` takes
    /// it on the first file the request compiles, and kept to the
    /// request's end.
    pub(crate) fn take_shared_hold(&mut self) {
        self.shared_hold = SharedHold::Taken;
    }

    /// A string of `bytes` out of this request's heap, what
    /// [`PhpString::new`] is: php's `ZVAL_STRINGL_FAST`, the process's
    /// interned string for the empty string and a string of one byte
    /// (`ZSTR_EMPTY_ALLOC`, `ZSTR_CHAR`), `zend_string_init` otherwise.
    /// The memory limit is the fatal that ends the request, recorded
    /// here where `zend_mm` leaves the request, and the `Thrown` that
    /// carries no throwable ends the native.
    pub(crate) fn string(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        if bytes.len() <= 1
            && let Some(frozen) = self.shared.permanent(bytes)
        {
            return Ok(PhpString::of(frozen));
        }
        let allocated = self.heap.string(bytes);
        self.allocated(allocated)
    }

    /// A string of `bytes` out of this request's heap whatever their
    /// length, `zend_string_init`, where [`Request::string`] hands out the
    /// interned string for the empty string and one byte.
    pub(crate) fn string_init(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string(bytes);
        self.allocated(allocated)
    }

    /// A string of `len` bytes written in place, what
    /// [`PhpString::filled`] is: `zend_string_alloc`, then the caller's
    /// write into `ZSTR_VAL`.
    pub(crate) fn string_with(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_with(len, fill);
        self.allocated(allocated)
    }

    /// A string of `n * m + l` bytes written in place, what
    /// [`PhpString::product`] is: `zend_string_safe_alloc`.
    pub(crate) fn string_product(
        &mut self,
        n: usize,
        m: usize,
        l: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_safe_alloc(n, m, l, fill);
        self.allocated(allocated)
    }

    /// `s` with `tail`'s bytes after its own, `zend_string_extend` and
    /// the `memcpy` after it ([`php_heap::Heap::string_append`]): one
    /// owner of `s` handed over, one of the longer string handed back, grown
    /// where it stands when that owner was the only one.
    pub(crate) fn string_append(
        &mut self,
        s: PhpString,
        tail: PhpString,
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_append(s.heap(), tail.heap());
        self.allocated(allocated)
    }

    /// What the heap answered, as the native sees it: the string, or the
    /// memory limit as php's fatal, which `zend_mm` raises where the
    /// allocation was asked for and which ends the request from there.
    fn allocated(&mut self, allocated: Result<php_heap::Str, Fatal>) -> Result<PhpString, Thrown> {
        self.fatal_of(allocated).map(PhpString::of)
    }

    /// One more owner of what `value` holds: `Z_TRY_ADDREF_P`, a refcount
    /// for a string, an array, an object, a resource or a reference of
    /// the request's, nothing for a scalar, an interned string or an
    /// immutable array. The engine's own, where php copies a value it
    /// holds into a second place (`zend_string_copy`, `ZVAL_COPY`); a
    /// native takes an [`Owner`](crate::api::values::Owner) instead, which
    /// only the engine names without taking (design §5.1, #2239).
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn addref(&self, value: &Value) {
        self.heap.addref(value.to_zval());
    }

    /// One owner fewer of what `value` holds, `zval_ptr_dtor`: the
    /// string, the array, the object, the resource or the reference is
    /// freed when its last owner gives it up, an array's elements and a
    /// reference's value released with it, an object's destructor and a
    /// resource's per-type destructor served first through seam 1, and
    /// an interned string or an immutable array never is. The request
    /// is the hooks over itself, so the heap a destructor allocates from
    /// is this one, handed back for the duration ([`Hooks`]). The
    /// engine's own; a native gives up an
    /// [`Owner`](crate::api::values::Owner) with its `give_up`.
    /// A value that counts no owner, a scalar, an interned string or an
    /// immutable array, is tested here, where the caller stands, and
    /// gives up nothing, as `zval_ptr_dtor`'s `Z_REFCOUNTED_P` test is
    /// inline in php's handlers; the release of a counted one is a call.
    // Taking the value is the point: the owner is given up here, and a
    // `Value` that is still in hand after this call names what may be
    // freed.
    #[allow(clippy::needless_pass_by_value)]
    #[cfg_attr(debug_assertions, track_caller)]
    #[inline]
    pub(crate) fn release(&mut self, value: Value) {
        self.release_zval(value.to_zval());
    }

    /// `GC_DELREF` and `gc_check_possible_root` over a value the caller
    /// holds that another owner of stands ([`Heap::release_held`]):
    /// whether an owner was given up, which is never the last.
    #[allow(clippy::needless_pass_by_value)]
    #[must_use = "the last owner kept is the caller's InternalError"]
    pub(crate) fn release_held(&mut self, value: Value) -> bool {
        Heap::release_held(&mut Hooks(self), value.to_zval())
    }

    /// One owner fewer as `zval_ptr_dtor_nogc` gives it up: what
    /// [`Request::release`] does, and never a possible root of the cycle
    /// collector (design §5.12). What `FREE_OP` does to a temporary
    /// operand and `zend_vm_stack_free_args` to an internal call's
    /// arguments.
    /// A value that counts no owner is tested inline, as for
    /// [`Request::release`].
    #[allow(clippy::needless_pass_by_value)]
    #[cfg_attr(debug_assertions, track_caller)]
    #[inline]
    pub(crate) fn release_nogc(&mut self, value: Value) {
        self.release_zval_nogc(value.to_zval());
    }

    /// [`Request::release`] over a zval a slot held, the owner it held
    /// given up with no `Value` made of it, so no kind is asked before the
    /// decrement: what `UNSET_CV` gives up of the variable it empties.
    #[cfg_attr(debug_assertions, track_caller)]
    #[inline]
    pub(crate) fn release_zval(&mut self, z: php_heap::Zval) {
        if z.is_refcounted() {
            Heap::release(&mut Hooks(self), z);
        }
    }

    /// [`Request::release_nogc`] over a zval a slot or a call's record
    /// held, the owner it held given up with no `Value` made of it: what
    /// a frameless call's record gives up of its operands as the call
    /// ends, `FREE_OP*`.
    #[cfg_attr(debug_assertions, track_caller)]
    #[inline]
    pub(crate) fn release_zval_nogc(&mut self, z: php_heap::Zval) {
        if z.is_refcounted() {
            Heap::release_nogc(&mut Hooks(self), z);
        }
    }

    /// `ZVAL_NEW_REF`, what [`PhpRef::new`] is: a cell of the request's
    /// heap holding `value`, one owner of the cell, `value` the cell's
    /// from here, and the memory limit as the fatal that ends the
    /// request.
    pub(crate) fn reference(&mut self, value: Value) -> Result<PhpRef, Thrown> {
        let allocated = self.heap.reference(value.into_zval());
        self.fatal_of(allocated).map(PhpRef::of)
    }

    /// `ZVAL_MAKE_REF`: the cell `place` holds, or a new one over what it
    /// held, `null` for what nothing wrote, written back into the place,
    /// which gives the cell the owner the place had of the value. No
    /// owner more is taken; a caller that keeps the cell adds its own.
    pub(crate) fn make_ref(&mut self, place: php_heap::ZvalRef) -> Result<PhpRef, Thrown> {
        let held = Value::of_slot(place.read());
        if let Some(cell) = held.as_php_ref() {
            return Ok(cell);
        }
        let cell = self.reference(held.undef_as_null())?;
        place.write(cell.as_value().to_zval());
        Ok(cell)
    }

    /// The reference's half of `zend_assign_to_variable`, what
    /// [`PhpRef::set`] and `ASSIGN` over a CV holding a reference are:
    /// `value` is the cell's from here, and what the cell held is
    /// released last, after the write.
    pub(crate) fn reference_set(&mut self, r: Ref, value: Value) {
        Heap::reference_set(&mut Hooks(self), r, value.into_zval());
    }

    /// An empty array out of this request's heap, what [`PhpArray::new`]
    /// is: php's `array_init`, and the memory limit as the fatal that
    /// ends the request.
    pub(crate) fn array(&mut self) -> Result<PhpArray, Thrown> {
        let allocated = self.heap.array();
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_new_array(size)`, what `INIT_ARRAY` lays down for the
    /// elements it was told to expect, and `zend_hash_real_init_mixed`
    /// over it where the opline says the array cannot be packed
    /// (`ZEND_ARRAY_NOT_PACKED`); the memory limit as the fatal that ends
    /// the request.
    pub(crate) fn array_sized(&mut self, size: u32, packed: bool) -> Result<PhpArray, Thrown> {
        let allocated = if packed {
            self.heap.array_sized(size)
        } else {
            self.heap.array_mixed(size)
        };
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_hash_update` or `zend_hash_index_update` over `a`, which
    /// has one owner: `value` is the array's from here, what `key` held
    /// is released, and a string key is one more owner.
    pub(crate) fn array_update(
        &mut self,
        a: Arr,
        key: php_heap::Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let z = value.into_zval();
        let written = Heap::array_update(&mut Hooks(self), a, key, z);
        self.fatal_of(written)
    }

    /// `zend_hash_str_update` over `a`, which has one owner: `value` is
    /// the array's from here, what the key held is released, and a key
    /// the array lacks is made after the table has room.
    pub(crate) fn array_str_update(
        &mut self,
        a: Arr,
        name: &[u8],
        value: Value,
    ) -> Result<(), Thrown> {
        let z = value.into_zval();
        let written = Heap::array_str_update(&mut Hooks(self), a, name, z);
        self.fatal_of(written)
    }

    /// `zend_hash_add_new` or `zend_hash_index_add_new` over `a`, which
    /// has one owner: `value` is the array's from here and a string key
    /// one more owner, appended without the lookup.
    pub(crate) fn array_add_new(
        &mut self,
        a: Arr,
        key: php_heap::Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let added = self.heap.array_add_new(a, key, value.into_zval());
        self.fatal_of(added)
    }

    /// `zend_hash_next_index_insert` over `a`, which has one owner:
    /// `false`, and `value` given up, when the next element is taken.
    pub(crate) fn array_append(&mut self, a: Arr, value: Value) -> Result<bool, Thrown> {
        let z = value.into_zval();
        let appended = self.heap.array_append(a, z);
        if appended == Ok(false) {
            Heap::release(&mut Hooks(self), z);
        }
        self.fatal_of(appended)
    }

    /// `zend_hash_lookup` over `a`, which has one owner: the entry at
    /// `key` in place, laid down holding `null` where the table has none,
    /// which is what a `FETCH_DIM_W` and an `ASSIGN_DIM` write through.
    pub(crate) fn array_lookup(&mut self, a: Arr, key: php_heap::Key) -> Result<ZvalRef, Thrown> {
        let entry = self.heap.array_lookup(a, key);
        self.fatal_of(entry)
    }

    /// `zend_hash_next_index_insert` of `null` over `a`, which has one
    /// owner: the entry appended, in place; `None` when the next element
    /// is taken.
    pub(crate) fn array_next_entry(&mut self, a: Arr) -> Result<Option<ZvalRef>, Thrown> {
        let entry = self.heap.array_next_entry(a);
        self.fatal_of(entry)
    }

    /// `zend_hash_del` over `a`, which has one owner: `true` when the key
    /// was there, its value released.
    pub(crate) fn array_del(&mut self, a: Arr, key: php_heap::Key) -> bool {
        Heap::array_del(&mut Hooks(self), a, key)
    }

    /// `zend_hash_clean` over `a`, which has one owner
    /// ([`Heap::array_clean`]).
    pub(crate) fn array_clean(&mut self, a: Arr) {
        Heap::array_clean(&mut Hooks(self), a);
    }

    /// `zend_array_dup`: a copy of `a` with one owner.
    pub(crate) fn array_dup(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let copied = self.heap.array_dup(a);
        self.fatal_of(copied)
    }

    /// [`Heap::dup_value`], the one rule a copy of a table holds an
    /// element by (`zend_array_dup_value`): `value` with one more owner,
    /// or the value of a reference nothing else holds with one more
    /// owner of that, unless that value is `source`, the table copied.
    pub(crate) fn dup_value(&self, value: Value, source: Option<Arr>) -> Value {
        Value::of_zval(self.heap.dup_value(value.to_zval(), source))
    }

    /// [`Heap::symtable_to_proptable`]: an array's table as an object's,
    /// `a` itself with one more owner where its keys are all strings.
    pub(crate) fn symtable_to_proptable(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let table = Heap::symtable_to_proptable(&mut Hooks(self), a);
        self.fatal_of(table)
    }

    /// [`Heap::proptable_to_symtable`]: an object's table as an array's,
    /// `a` itself with one more owner where no name reads as an integer
    /// and nothing asks for a copy.
    pub(crate) fn proptable_to_symtable(
        &mut self,
        a: Arr,
        always_duplicate: bool,
    ) -> Result<Arr, Thrown> {
        let table = Heap::proptable_to_symtable(&mut Hooks(self), a, always_duplicate);
        self.fatal_of(table)
    }

    /// `SEPARATE_ARRAY`: `a` itself with one owner, else a copy of it
    /// with one, `a` one owner fewer.
    pub(crate) fn array_separate(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let separated = self.heap.separate(a);
        self.fatal_of(separated)
    }

    /// `zend_hash_sort` over `a`, which has one owner: the comparison
    /// carried through seam 1, since a `usort` callback runs PHP code,
    /// and the keys dropped under `renumber`.
    pub(crate) fn array_sort(
        &mut self,
        a: Arr,
        renumber: bool,
        compare: &mut dyn FnMut(&mut Request, &SortEntry, &SortEntry) -> std::cmp::Ordering,
    ) -> Result<(), Thrown> {
        let sorted = Heap::array_sort(&mut Hooks(self), a, renumber, &mut |hooks, x, y| {
            compare(hooks.0, x, y)
        });
        self.fatal_of(sorted)
    }

    /// `SEPARATE_ZVAL_NOREF` on what `r` holds, what
    /// [`PhpRef::separate_array`] is: the array in the cell with one
    /// owner, a copy written into the cell where it had more.
    pub(crate) fn reference_separate(&mut self, r: Ref) -> Result<Option<Arr>, Thrown> {
        let separated = self.heap.reference_separate(r);
        self.fatal_of(separated)
    }

    /// What the heap answered, as the native sees it: the value, or the
    /// fatal the allocator raised, recorded here where `zend_mm` leaves
    /// the request, as the `Thrown` carrying no throwable that ends the
    /// native.
    pub(crate) fn fatal_of<T>(&mut self, answered: Result<T, Fatal>) -> Result<T, Thrown> {
        match answered {
            Ok(value) => Ok(value),
            // A fallible ask's refusal ends nothing: it is carried out to
            // its scope ([`Memory::fallible`]).
            Err(fatal) if *fatal.reason() == php_heap::FatalReason::Refused => {
                Err(Thrown::of(None))
            }
            Err(fatal) => {
                self.end_in(&fatal);
                Err(Thrown::of(None))
            }
        }
    }

    /// The limit the heap holds a request to from its start: what
    /// `php_module_startup` left, `memory_limit` where it is under
    /// `max_memory_limit` and the maximum's otherwise, since
    /// `OnChangeMaxMemoryLimit` runs first and set it. `-1` is no limit.
    pub(super) fn set_memory_limit(&mut self) {
        let limit = self
            .limit_bytes(b"memory_limit")
            .min(self.limit_bytes(b"max_memory_limit"));
        self.heap.set_memory_limit(limit.min(usize::MAX >> 1));
        // `memory_hard_limit_percent`, which follows the soft limit;
        // php's `NULL`, its default, is the heap's own `-1`.
        let percent = self.ini().get(b"memory_hard_limit_percent");
        if let Some(percent) = percent.filter(|p| !p.is_empty()).map(Ini::parse_bytes) {
            self.heap.set_memory_hard_limit_percent(percent);
        }
    }

    /// `OnChangeMemoryHardLimitPercent` (`Z/main/main.c:391` at 8.5.12)
    /// over `value`, `None` for php's `NULL`, which is `-1`: a percent
    /// under 100 other than `-1` is php's warning and refused, and so is
    /// one the heap refuses while the soft limit is exceeded,
    /// `zend_set_memory_hard_limit_percent`'s `FAILURE`; whether the
    /// entry takes it is the answer.
    ///
    /// ```php
    /// var_dump(ini_set('memory_hard_limit_percent', '50'), ini_set('memory_hard_limit_percent', '150'), ini_get('memory_hard_limit_percent'));
    /// ini_set('memory_limit', '2M');
    /// ini_set('memory_hard_limit_percent', '300');
    /// $a = str_repeat('x', 5 << 20);
    /// echo "unreached\n";
    /// ```
    /// ```expect exit=255
    /// Warning: memory_hard_limit_percent must be >= 100, or -1 in %s on line 2
    /// bool(false)
    /// string(0) ""
    /// string(3) "150"
    ///
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 5259264 bytes) in %s on line 5
    /// ```
    pub(crate) fn memory_hard_limit_percent_taken(&mut self, value: Option<&[u8]>) -> bool {
        let percent = value.map_or(-1, Ini::parse_bytes);
        if percent < 100 && percent != -1 {
            crate::api::raising::warning(self, "memory_hard_limit_percent must be >= 100, or -1");
            return false;
        }
        if !self.heap.set_memory_hard_limit_percent(percent) {
            crate::api::raising::warning(
                self,
                &format!("Failed to set memory_hard_limit_percent to {percent}"),
            );
            return false;
        }
        true
    }

    /// An entry `zend_ini_parse_uquantity` reads, `-1` the largest size.
    fn limit_bytes(&self, name: &[u8]) -> usize {
        Request::limit_of(self.ini().get(name).unwrap_or_default())
    }

    /// `value` as `zend_ini_parse_uquantity` reads a limit, as `size_t`.
    fn limit_of(value: &[u8]) -> usize {
        let (bytes, _) = Ini::parse_uquantity(value);
        usize::try_from(bytes).unwrap_or(usize::MAX)
    }

    /// `OnChangeMemoryLimit` (`Z/main/main.c:333-367`) over `value` at
    /// runtime or at startup: a value past `max_memory_limit` is php's
    /// warning (silent for `-1`) and the maximum's text in its place,
    /// the heap left as it was; otherwise `zend_set_memory_limit`, whose
    /// refusal of a limit under what the chunks hold is php's other
    /// warning and the entry kept. The text the entry takes is the
    /// answer.
    ///
    /// ```php
    /// $a = str_repeat("0", 5 * 1024 * 1024);
    /// var_dump(ini_set("memory_limit", "3M"), ini_get("memory_limit"));
    /// ```
    /// ```expect
    /// Warning: Failed to set memory limit to 3145728 bytes (Current memory usage is 7356416 bytes) in %s on line 3
    /// bool(false)
    /// string(4) "128M"
    /// ```
    pub(crate) fn memory_limit_verdict(&mut self, value: &[u8]) -> Option<Vec<u8>> {
        let wanted = Request::limit_of(value);
        let max = self.limit_bytes(b"max_memory_limit");
        if wanted > max {
            if wanted != usize::MAX {
                crate::api::raising::warning(
                    self,
                    &format!(
                        "Failed to set memory_limit to {wanted} bytes. Setting to max_memory_limit instead (currently: {max} bytes)"
                    ),
                );
            }
            return self.ini().get(b"max_memory_limit").map(<[u8]>::to_vec);
        }
        if !self.heap.set_memory_limit(wanted.min(usize::MAX >> 1)) {
            let usage = self.heap.memory_usage(true);
            crate::api::raising::warning(
                self,
                &format!(
                    "Failed to set memory limit to {wanted} bytes (Current memory usage is {usage} bytes)"
                ),
            );
            return None;
        }
        Some(value.to_vec())
    }

    /// `memory_reset_peak_usage()`.
    pub(crate) fn reset_memory_peak(&mut self) {
        self.heap.reset_peak();
    }

    /// `gc_collect_cycles()` through php's pointer: the chain the
    /// bundle supplied entered from the top
    /// ([`ExtensionBuilder::gc_collect`]), with `zend_gc_collect_cycles`
    /// over the request's heap beneath every link (design §5.12); the
    /// values the collection freed, as php 8.5 counts them.
    ///
    /// [`ExtensionBuilder::gc_collect`]: crate::api::abi::ExtensionBuilder::gc_collect
    pub(crate) fn gc_collect(&mut self) -> i64 {
        let shared = Arc::clone(&self.shared);
        Beneath::<chain::GcCollect>::top(shared.supplied.gc_collect.links(), ()).serve(self)
    }

    /// `zend_gc_collect_cycles`, the heap's own collection beneath every
    /// link of the pointer.
    pub(crate) fn gc_collect_own(&mut self) -> i64 {
        Heap::gc_collect_cycles(&mut Hooks(self))
    }

    /// One entry of the collector's destructor phase served
    /// ([`Heap::gc_call_destructor`]), for the fiber the phase is served
    /// in: the object whose destructor was served.
    pub(crate) fn gc_call_destructor(&mut self, idx: u32) -> Option<PhpObject> {
        Heap::gc_call_destructor(&mut Hooks(self), idx).map(PhpObject::of)
    }

    /// `gc_enable()` and `gc_disable()`: `zend.enable_gc` altered, whose
    /// `OnUpdateGCEnabled` is `gc_enable`, so `ini_get` reads what the
    /// call left.
    ///
    /// The value is `zend_alter_ini_entry_chars`'s new string of one
    /// byte (`Z/Zend/zend_builtin_functions.c:184-206`), which the entry
    /// keeps in place of the one it held:
    ///
    /// ```php
    /// ini_set('from', 'x');
    /// $m = memory_get_usage(); gc_disable(); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); gc_enable(); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 32
    /// 0
    /// ```
    pub(crate) fn gc_enable(&mut self, enabled: bool) {
        let value: &[u8] = if enabled { b"1" } else { b"0" };
        // The memory limit's fatal is recorded and ends the request.
        let string = self
            .string_with(value.len(), |bytes| bytes.copy_from_slice(value))
            .ok();
        self.ini_hold_string(b"zend.enable_gc", Some(value), string);
        self.heap.gc_enable(enabled);
    }

    /// `gc_enabled()`.
    #[must_use]
    pub(crate) fn gc_enabled(&self) -> bool {
        self.heap.gc_enabled()
    }

    /// `gc_status()`, `zend_gc_get_status` in php 8.5's twelve fields, the
    /// times in seconds as php divides its nanoseconds, the keys strings
    /// of the request's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn gc_status(&mut self) -> Result<Owner<PhpArray>, Thrown> {
        adopted(self.gc_status_made())
    }

    /// What [`gc_status`](Self::gc_status) makes, its one owner not yet named.
    pub(crate) fn gc_status_made(&mut self) -> Result<PhpArray, Thrown> {
        let status = self.heap.gc_status();
        let mut array = self.array()?;
        for (name, value) in [
            ("running", Value::bool(status.running)),
            ("protected", Value::bool(status.protected)),
            ("full", Value::bool(status.full)),
            ("runs", Value::int(i64::from(status.runs))),
            ("collected", Value::int(i64::from(status.collected))),
            ("threshold", Value::int(i64::from(status.threshold))),
            ("buffer_size", Value::int(i64::from(status.buffer_size))),
            ("roots", Value::int(i64::from(status.roots))),
            (
                "application_time",
                Value::float(status.application_time.as_secs_f64()),
            ),
            (
                "collector_time",
                Value::float(status.collector_time.as_secs_f64()),
            ),
            (
                "destructor_time",
                Value::float(status.destructor_time.as_secs_f64()),
            ),
            ("free_time", Value::float(status.free_time.as_secs_f64())),
        ] {
            let key = PhpString::new(self, name.as_bytes())?.into_key();
            array.set(self, key, Owner::take(self, value))?;
        }
        Ok(array)
    }

    /// `ZVAL_STR_COPY` of a name a declaration or an op array holds, with
    /// one owner for the caller: what php copies out of `ce->name`,
    /// `function_name`, `prop_info->name`, `op_array->vars`,
    /// `op_array->filename` or `ZSTR_KNOWN`. The engine's declarations
    /// keep their names as bytes, so the string is the engine's where
    /// its startup or a published unit's compile interned the bytes
    /// (design §5.2), which counts nothing, else the string the
    /// request-only unit that carries the name holds, one more owner
    /// ([`Request::hold_compiled`]), else a string of the request's made
    /// for the caller, which no declaration of a living unit names. So
    /// a throwable made in an `eval`'d unit costs nothing past the unit's
    /// end once it is gone, as php's costs nothing, and an `eval` that
    /// throws nothing costs what php's does:
    ///
    /// ```php
    /// $m = memory_get_usage(); eval('try { intdiv(1,0); } catch (Error $e) {} unset($e);'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('try { intdiv(1,0); } catch (Error $e) {} unset($e);'); echo memory_get_usage() - $m, "\n";
    /// $f = eval('return function () { try { intdiv(1, 0); } catch (Error $e) {} };');
    /// $m = memory_get_usage(); $f(); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$e = new Exception;'); unset($e); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$x = 1;'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$x = 1;'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// ```
    ///
    /// A class's and a method's names are `ce->name` and
    /// `function_name`, so only the array of names is counted:
    ///
    /// ```php
    /// $o = new ArrayObject; $i = new ArrayIterator;
    /// $m = memory_get_usage();
    /// $a = get_class($o); $b = $o::class; $c = get_parent_class($i); $d = get_class_methods('Countable');
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(216)
    /// ```
    ///
    /// A name an `eval`'d class or variable holds is its unit's string,
    /// which what copies it keeps once the unit is gone:
    ///
    /// ```php
    /// eval('class Kept { public $propname = 1; protected $other = 2; }');
    /// $m = memory_get_usage(); eval('$varname = get_class(new Kept);'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$o = new Kept; $a = (array) $o; $b = get_object_vars($o);'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); foreach (new Kept as $k => $v) { $keys[] = $k; } echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($varname, $o, $a, $b, $keys); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 0
    /// 1208
    /// 216
    /// -1424
    /// ```
    pub(crate) fn declared_string(&mut self, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
        match self.held_name(bytes) {
            Some(held) => Ok(Owner::take(self, held)),
            None => PhpString::new(self, bytes),
        }
    }

    /// The string a declaration that lives holds for `bytes`, a view with
    /// no owner taken, as php reads `ce->name` or `prop_info->name` where
    /// nothing keeps it: the engine's, else the one a request-only unit
    /// holds for the name ([`Request::hold_compiled`]); `None` where
    /// neither holds one.
    pub(crate) fn held_name(&self, bytes: &[u8]) -> Option<PhpString> {
        if let Some(held) = self.shared.existing_interned(bytes) {
            return Some(PhpString::of(held));
        }
        self.unit_strings.find(bytes)
    }

    /// [`Request::held_name`], else a string the request keeps to its
    /// end for `bytes`, made on the first ask and the same at every ask
    /// after: a view with no owner taken. What a name a class that lives
    /// declares is read as where no unit holds it, a name the class's
    /// link made, as php's `zend_declare_typed_property` mangles a
    /// trait's private property under the name of the class that uses
    /// it and the class owns it to the request's end; and what a native
    /// that keeps a name for the rest of the request reads
    /// ([`PhpString::interned`]). The `Err` is the memory limit.
    pub(crate) fn kept_string(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        if let Some(held) = self.held_name(bytes) {
            return Ok(held);
        }
        let made = self.string(bytes)?;
        self.unit_strings.keep(bytes, made);
        Ok(made)
    }

    /// A name the program wrote, which php hands on as the string its
    /// cached unit holds, interned by opcache's persist, a class named
    /// in an attribute or a type among them: [`Request::kept_string`]
    /// of the bytes, with one owner for the caller.
    pub(crate) fn standing_string(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        let kept = self.kept_string(bytes)?;
        self.addref(&Value::string(kept));
        Ok(kept)
    }

    /// `ZSTR_KNOWN(known)`: [`Request::declared_string`] of a known name,
    /// read off the engine's table by its position, with one owner for
    /// the caller.
    pub(crate) fn known(
        &mut self,
        known: crate::engine::Known,
    ) -> Result<Owner<PhpString>, Thrown> {
        match self.shared.known(known) {
            Some(held) => Ok(Owner::adopt(PhpString::of(held))),
            None => self.declared_string(known.bytes()),
        }
    }

    /// What a compile made for the request left php's compiler stacks
    /// holding room for ([`Pushed`]): `zend_stack_push` grows a stack's
    /// elements sixteen at a time with `safe_erealloc`, and nothing
    /// shrinks them before `shutdown_compiler`, so the request holds them
    /// from the first compile that pushed one to its end.
    /// `CG(loop_var_stack)`'s, 12-byte `zend_loop_var`s, a loop's or a
    /// function body's:
    ///
    /// ```php
    /// $m = memory_get_usage(); eval('$x = 1;'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('while (false) {}'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('switch (1) {}'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { for (;;) { break 17; } } } } } } } } } } } } } } } } }'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 0
    /// 192
    /// 0
    /// 192
    /// ```
    ///
    /// And `CG(delayed_oplines_stack)`'s, 32-byte `zend_op`s, one a fetch
    /// a chain of dimensions and properties held back: the first compile
    /// that reads an element lays sixteen, and one that holds back more
    /// than sixteen at once grows them, with opcache and without:
    ///
    /// ```php
    /// $m = memory_get_usage(); eval('$a = $argv;'); $plain = memory_get_usage() - $m;
    /// $m = memory_get_usage(); eval('$a = $argv[0];'); echo memory_get_usage() - $m - $plain, "\n";
    /// $m = memory_get_usage(); eval('$a = $argv[0];'); echo memory_get_usage() - $m - $plain, "\n";
    /// $a = 1; $deep = '$b = $a' . str_repeat('[0]', 18) . ';';
    /// $shallow = str_pad('$b = $a[0]', strlen($deep) - 1) . ';';
    /// @eval($shallow); @eval($shallow);
    /// $m = memory_get_usage(); @eval($deep); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 512
    /// 0
    /// 512
    /// ```
    ///
    /// ```php
    /// $m = memory_get_usage(); eval('$a = $argv;'); $plain = memory_get_usage() - $m;
    /// $m = memory_get_usage(); eval('$a = $argv[0];'); echo memory_get_usage() - $m - $plain, "\n";
    /// $m = memory_get_usage(); eval('$o = new stdClass; $o->p = 1;'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 512
    /// 416
    /// ```
    ///
    /// Two programs of one length, one of which reads an element, apart
    /// by the stack under opcache, where the program's own op array is
    /// opcache's:
    ///
    /// ```php
    /// $dir = sys_get_temp_dir();
    /// $usage = function ($code) use ($dir) {
    ///     file_put_contents("$dir/p.php", "<?php $code echo memory_get_usage();");
    ///     return (int) shell_exec(escapeshellarg(PHP_BINARY) . ' -n -d opcache.enable_cli=1 -d opcache.jit=off -d opcache.file_update_protection=0 ' . escapeshellarg("$dir/p.php"));
    /// };
    /// echo $usage('$x = $argv[0];') - $usage('$x = $argv;   '), "\n";
    /// ```
    /// ```expect process
    /// 512
    /// ```
    pub(crate) fn note_pushed(&mut self, pushed: Pushed) {
        let stacks = &mut self.compiler_stacks;
        stacks
            .loop_vars
            .grow(&mut self.heap, pushed.loop_vars, LOOP_VAR);
        (stacks.delayed_oplines).grow(
            &mut self.heap,
            pushed.delayed_fetches,
            super::declared_bytes::OPLINE,
        );
    }

    /// `shutdown_compiler`'s `zend_stack_destroy` of each stack.
    pub(crate) fn free_compiler_stacks(&mut self) {
        let stacks = std::mem::take(&mut self.compiler_stacks);
        for stack in [stacks.loop_vars, stacks.delayed_oplines] {
            if let Some((held, _)) = stack.0 {
                self.heap.free_block(held);
            }
        }
    }

    /// The heap's memory limit in bytes, `PG(memory_limit)` as
    /// `zend_set_memory_limit` set it.
    pub(crate) fn heap_memory_limit(&self) -> usize {
        self.heap.memory_limit()
    }
}

impl Memory<&Request> {
    /// `ZEND_HAS_OOM()` (`Z/Zend/zend_alloc.h:63` at 8.5.12): whether the
    /// memory limit was met in this request, its fatal pending or being
    /// raised, until `php_request_shutdown` begins. php 8.5.12's natives
    /// read it to stop a loop whose allocations were served past the
    /// limit, as `RETURN_OOM` stops a fallible one, and leave the fatal
    /// to the interrupt check after the call. One load of the heap's
    /// state.
    #[must_use]
    #[inline]
    pub fn has_oom(self) -> bool {
        self.0.has_oom()
    }

    /// `memory_get_usage()`.
    #[must_use]
    pub fn usage(self, real: bool) -> usize {
        self.0.heap.memory_usage(real)
    }

    /// `is_zend_ptr`: whether `address` lies inside the request heap's
    /// chunks or huge blocks, which reads no memory, so any number is
    /// a question it answers.
    #[must_use]
    pub fn heap_holds(self, address: usize) -> bool {
        self.0.heap.holds(address)
    }

    /// `memory_get_peak_usage()`: the chunks' high-water mark for
    /// `real == true`, the bin accounting's for `real == false`, the two
    /// the heap keeps apart (design §5.13).
    #[must_use]
    pub fn peak(self, real: bool) -> usize {
        if real {
            self.0.heap.memory_real_peak()
        } else {
            self.0.heap.memory_peak()
        }
    }
}

impl Memory<&mut Request> {
    /// `ask` served with the request's heap fallible, php's
    /// `zend_mm_set_fallible(true)` around one allocation, as php 8.5.12's
    /// `femalloc`, `zend_string_safe_falloc` and their kin ask
    /// (`ZEND_MM_FALLIBLE_FORWARD`, `Z/Zend/zend_alloc.c:3060`): an
    /// allocation the memory limit has no room for is refused rather than
    /// served, the delayed OOM recorded, and the answer is `None`, php's
    /// `NULL`, for the native to end on as `RETURN_OOM` ends it, with
    /// nothing pending; the next safepoint, the interrupt check after the
    /// call, raises the limit's fatal. A fallible ask is held to the soft
    /// limit alone, never to the hard one. The heap's fallibility is put
    /// back as `ask` returns, so scopes nest as php's saved word does.
    ///
    /// The scope is the allocation's, as php's forward wraps the one call:
    /// a native asks fallibly for the string it returns and nothing else,
    /// so no PHP code it serves is served fallibly.
    ///
    /// # Errors
    ///
    /// What `ask` threw, the request's end among it: a fatal, the hard
    /// limit an allocation outside the scope met, or a throwable.
    pub fn fallible<T>(
        self,
        ask: impl FnOnce(&mut Request) -> Result<T, Thrown>,
    ) -> Result<Option<T>, Thrown> {
        let was = self.0.heap.set_fallible(true);
        let answered = ask(self.0);
        self.0.heap.set_fallible(was);
        let refused = self.0.heap.take_refused();
        match answered {
            Ok(value) => Ok(Some(value)),
            // The refusal carried out as an `Err` with nothing thrown and
            // the request not ending; a throwable or a fatal met after a
            // refusal is `ask`'s to answer.
            Err(thrown) if refused && thrown.exception().is_none() && self.0.ending().is_none() => {
                Ok(None)
            }
            Err(thrown) => Err(thrown),
        }
    }

    /// `call` served with the request's heap lent to C for its length
    /// (ADR-0014, design §5.14): a C library compiled against
    /// `php_heap`'s `emalloc`, `ecalloc`, `erealloc`, `estrdup`,
    /// `estrndup` and `efree` (timelib, through its `timelib_config.h`)
    /// allocates on this request's heap while `call` runs, as php's
    /// library does, and a block of the heap it frees goes back to it.
    /// A library whose allocator callbacks take an argument of their own
    /// (zlib's `zalloc`, uriparser's memory manager) is pointed at
    /// [`emalloc`], [`ecalloc`], [`erealloc`] and [`efree`] as php points
    /// it at its own, and each call into it is made inside `call`.
    /// `call` reaches nothing of the request, which is borrowed for its
    /// length, and the lend ends when it returns. Every free of a block
    /// a lend made is made under a lend of the same request; outside one
    /// `efree` frees nothing, and the block is the request's end's. The
    /// `Err` is the memory limit's fatal, recorded where `zend_mm` leaves
    /// the request: the library ran to its return on the system
    /// allocator's memory, and what `call` answered is dropped.
    ///
    /// # Errors
    ///
    /// The thrown that carries the limit's fatal.
    pub fn lend_heap<R>(self, call: impl FnOnce() -> R) -> Result<R, Thrown> {
        let lent = self.0.heap.lend(call);
        self.0.fatal_of(lent)
    }

    /// `observer` told of every allocation, reallocation and free the
    /// request's heap serves from here on, and of what
    /// `gc_mem_caches()` gives back (design §7.1): what stands in the
    /// place of php's `zend_mm_set_custom_handlers`, which no extension
    /// is given. The heap serves each one still, so `memory_get_usage()`
    /// reads what it reads with no observer, and with none put in an
    /// allocation pays one test of an empty list, as php's pays its
    /// `use_custom_heap` test. An observer is told mid-allocation and is
    /// handed nothing of the request; what it keeps is its own, and it is
    /// dropped with the request's heap where it is not taken out before.
    ///
    /// With none put in, what the heap reads across small and large
    /// allocations, grown and freed, is php's:
    ///
    /// ```php
    /// $m = memory_get_usage(); memory_reset_peak_usage();
    /// $a = []; for ($i = 0; $i < 1000; $i++) { $a[] = "x$i"; }
    /// $c = 'y'; for ($i = 0; $i < 12; $i++) { $c .= $c; }
    /// $s = ''; for ($i = 0; $i < 100; $i++) { $s .= $c; }
    /// unset($a);
    /// var_dump(memory_get_usage() - $m, memory_get_peak_usage() - $m, memory_get_usage(true));
    /// ```
    /// ```expect
    /// int(421888)
    /// int(474424)
    /// int(2097152)
    /// ```
    pub fn observe_allocations(
        self,
        observer: Box<dyn ObserveAllocations>,
    ) -> AllocationObserverId {
        self.0.heap.observe_allocations(observer)
    }

    /// The observer put in at `at` taken out and handed back, so the
    /// extension that put it in reads what it kept; `None` where it was
    /// taken out already.
    pub fn unobserve_allocations(
        self,
        at: AllocationObserverId,
    ) -> Option<Box<dyn ObserveAllocations>> {
        self.0.heap.unobserve_allocations(at)
    }

    /// `gc_check_possible_root`: an array or an object, or the one a
    /// reference holds, offered to the collector as a possible root where
    /// it is not one already, as a release that leaves it owners does;
    /// any other value is never a root and is left as it is. An
    /// extension calls it where php's does, on a cycle a temporary's
    /// release would otherwise leave unseen, since php's
    /// `zval_ptr_dtor_nogc` offers nothing (ext/dom's `children`,
    /// `unserialize`'s answer).
    pub fn gc_possible_root(self, value: &Value) {
        Heap::gc_check_possible_root(&mut Hooks(self.0), value.to_zval());
    }
}

/// `sizeof(zend_loop_var)`.
const LOOP_VAR: usize = 12;

/// The most entries a compile pushed onto each of php's compiler stacks
/// that a request keeps: `CG(loop_var_stack)`'s
/// ([`php_unit::Unit::loop_vars`]) and `CG(delayed_oplines_stack)`'s
/// ([`php_unit::Unit::delayed_fetches`]); none of either for a compile
/// that never ran, a hit's.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pushed {
    loop_vars: u32,
    delayed_fetches: u32,
}

impl Pushed {
    /// What `unit`'s compile pushed.
    pub(crate) fn of(unit: &php_unit::Unit) -> Pushed {
        Pushed {
            loop_vars: unit.loop_vars,
            delayed_fetches: unit.delayed_fetches,
        }
    }
}

/// php's compiler stacks a request keeps from the first compile that
/// pushed onto one to its end.
#[derive(Default)]
pub(crate) struct CompilerStacks {
    loop_vars: CompilerStack,
    delayed_oplines: CompilerStack,
}

/// One `zend_stack`'s elements on the request's heap and how many they
/// hold room for.
#[derive(Default)]
struct CompilerStack(Option<(php_heap::Block, u32)>);

impl CompilerStack {
    /// Room for `entries` of `element` bytes each, grown sixteen at a
    /// time as `zend_stack_push` grows it; a refusal is the memory
    /// limit's fatal, which ends the request.
    fn grow(&mut self, heap: &mut php_heap::Heap, entries: u32, element: usize) {
        let wanted = entries.div_ceil(16).saturating_mul(16);
        if wanted <= self.0.as_ref().map_or(0, |(_, held)| *held) {
            return;
        }
        let len = wanted as usize * element;
        let grown = match self.0.take() {
            Some((held, _)) => heap.block_resize(held, len),
            None => heap.block(len),
        };
        self.0 = grown.ok().map(|grown| (grown, wanted));
    }
}
