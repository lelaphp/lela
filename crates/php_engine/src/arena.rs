//! The engine's published arena (design §5.14, *Identities*): the rows
//! every request shares of the classes and functions published past the
//! engine's own, the preload's first. A [`ClassId`] or a [`Function`]
//! whose top bit is clear names one of these rows, or one of the
//! engine's below them; one whose top bit is set names a row of the
//! request's own.
//!
//! Each kind of row is a [`Column`]: spans of chunks of one length,
//! each laid down on first use and never moved, of slots that are each
//! written once. A publisher takes a number with
//! [`Column::reserve`] before it builds the row, so rows that name each
//! other (a class and its methods) are built knowing each other's
//! numbers, and fills it with [`Column::fill`]. A request reads a row
//! with [`Column::get`]: the number's high bits held to the spans, its
//! middle bits a chunk in the span and its low bits a slot in the
//! chunk, with no bound to hold, three acquire loads and nothing taken, as php reads a `zend_class_entry *` from
//! shared memory. A row is found once it is filled and only then, and a
//! publisher hands a number on only after its row is filled, so whatever
//! carried the number to a request (the preload's tables, the unit
//! cache) carried the row with it. A number reserved by a publication
//! that failed is never filled: a hole no request is handed, which the
//! restart counts as waste. No row is written after its number is
//! handed on, nor freed or reused while the arena stands: what a class
//! builds once and keeps (its placed properties) its publisher builds
//! before handing the number on.
//!
//! An opcache restart takes rows back by epochs ([`Epochs`]): it hands
//! every request that starts after it a new, empty arena at once, and
//! the arena it replaced is dropped when the last request holding it
//! ends, as design §5.14 clears an arena "once no request holds its
//! epoch". Nothing renumbers a row: an epoch's numbers name its rows
//! alone, and everything that carries a number from one request to
//! another (the unit cache's files, the inheritance cache) is the
//! epoch's own ([`Arena::files`], [`Arena::inheritance`]), so it reaches
//! only requests of the same epoch. The preload's rows are the one
//! thing every epoch shares: once the preload has published, its arena
//! is laid below every epoch after ([`Epochs::lay_base`]) and an epoch
//! numbers its own rows past it, so the preload's classes and functions
//! keep their numbers across every restart, as php keeps what the
//! preload put in shared memory (`zend_shared_alloc_save_state` after
//! preloading, restored to by `accel_activate`'s restart). The tier's
//! words for the preload's op arrays are not shared: each epoch lays its
//! own, as php's restart resets them (`zend_jit_restart`), since the
//! traces they name are the epoch's.
//!
//! Nothing here is `unsafe`: the slots are `OnceLock`s, so a row is
//! `Sync` where its type is, and the chunks never reallocate, so a
//! row borrowed from the arena lives as long as the arena.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use crate::api::call::{ClassNative, Function};
use crate::api::request::{Declared, UserFunction};
use crate::api::values::ClassId;
use crate::tier::Laid;
use php_link::LinkedClass;
use php_unit::PublishedUnit;

/// The rows of one chunk, `1 << CHUNK_BITS` of them: a number's low
/// bits are its slot in the chunk.
const CHUNK_BITS: u32 = 9;
const CHUNK: usize = 1 << CHUNK_BITS;

/// The chunks of one span, `1 << SPAN_BITS` of them: a number's middle
/// bits are its chunk's place in the span.
const SPAN_BITS: u32 = 8;
const SPAN: usize = 1 << SPAN_BITS;

/// The spans a column holds, so the most rows it holds is
/// `SPANS * SPAN * CHUNK`, eight million: past the classes and methods
/// any program publishes into one epoch, and past the map pointers the
/// engine numbers op arrays by (`tier::published::Funcs`), which every
/// publication of an epoch advances, a unit published again after its
/// file changed and a trait's copied method among them; a column whose
/// numbers could name more is held to it ([`Column::below`]). A number's
/// high bits are its span. The spans are the column's own, so a column
/// is made with nothing laid but their sixty-four empty cells, and a
/// span and a chunk are each laid where a row first needs them: a
/// directory of every chunk laid whole where the column was made put a
/// million instructions and a megabyte of written pages on every
/// engine's start.
const SPANS: usize = 1 << 6;

/// The most rows a column holds.
pub(crate) const COLUMN_ROWS: usize = SPANS * SPAN * CHUNK;

/// One chunk of a column, laid down on first use and never moved: a
/// fixed-length array, so a number's slot in it is its low bits with no
/// bound to test.
type Chunk<T> = Box<[OnceLock<T>; CHUNK]>;

/// One span of a column's chunks, laid down on first use and never
/// moved, a fixed-length array as a chunk is.
type Span<T> = Box<[OnceLock<Chunk<T>>; SPAN]>;

/// One kind of row of the arena, numbered from zero in the order its
/// numbers were reserved.
#[derive(php_macros::EngineState)]
pub(crate) struct Column<T> {
    /// The spans, each laid on first use.
    spans: [OnceLock<Span<T>>; SPANS],
    /// How many numbers were reserved, which is the next one's.
    reserved: AtomicU32,
    /// The number no row reaches.
    limit: u32,
}

/// What `cell` holds, laid where it held nothing: `N` empty cells made
/// on the heap, never built on the stack, as the fixed-length array a
/// span or a chunk is, and made before they are offered, so a thread
/// that laid the cell first keeps its own and these are dropped. The
/// conversion from the `Vec` holds, its length being `N`; where it would
/// not, the fill that asked fails rather than panics.
fn lay<U, const N: usize>(cell: &OnceLock<Box<[OnceLock<U>; N]>>) -> Option<&[OnceLock<U>; N]> {
    if let Some(held) = cell.get() {
        return Some(held);
    }
    let cells: Vec<OnceLock<U>> = (0..N).map(|_| OnceLock::new()).collect();
    let fresh = cells.try_into().ok()?;
    Some(cell.get_or_init(|| fresh))
}

impl<T> Column<T> {
    /// An empty column whose numbers stay below `limit`, and below the
    /// rows its spans hold.
    pub(crate) fn below(limit: u32) -> Column<T> {
        let held = u32::try_from(SPANS * SPAN * CHUNK).unwrap_or(u32::MAX);
        Column {
            spans: std::array::from_fn(|_| OnceLock::new()),
            reserved: AtomicU32::new(0),
            limit: limit.min(held),
        }
    }

    /// `count` numbers taken together, the first answered: theirs alone
    /// whichever thread reserves beside this one. None where the column
    /// has no room for them.
    pub(crate) fn reserve(&self, count: u32) -> Option<u32> {
        self.reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(count).filter(|end| *end <= self.limit)
            })
            .ok()
    }

    /// The row a reserved number names, written once: `Err` gives the row
    /// back where the number was not reserved or its row was filled.
    pub(crate) fn fill(&self, at: u32, row: T) -> Result<(), T> {
        if at >= self.reserved() {
            return Err(row);
        }
        let Some(span) = self
            .spans
            .get((at >> (CHUNK_BITS + SPAN_BITS)) as usize)
            .and_then(lay)
        else {
            return Err(row);
        };
        let Some(chunk) = lay(&span[(at >> CHUNK_BITS) as usize & (SPAN - 1)]) else {
            return Err(row);
        };
        chunk[at as usize & (CHUNK - 1)].set(row)
    }

    /// The row at a number its publisher chose rather than reserved,
    /// written once: every number below it counts as reserved, its row
    /// unfilled where no publisher places one. `Err` gives the row back
    /// past the limit and where the slot was filled.
    pub(crate) fn place(&self, at: u32, row: T) -> Result<(), T> {
        let Some(end) = at.checked_add(1).filter(|end| *end <= self.limit) else {
            return Err(row);
        };
        self.reserved.fetch_max(end, Ordering::AcqRel);
        self.fill(at, row)
    }

    /// The row at `at`, where it was filled.
    pub(crate) fn get(&self, at: u32) -> Option<&T> {
        let span = self
            .spans
            .get((at >> (CHUNK_BITS + SPAN_BITS)) as usize)?
            .get()?;
        let chunk = span[(at >> CHUNK_BITS) as usize & (SPAN - 1)].get()?;
        chunk[at as usize & (CHUNK - 1)].get()
    }

    /// How many numbers were reserved: every row a request can have been
    /// handed stands below it.
    pub(crate) fn reserved(&self) -> u32 {
        self.reserved.load(Ordering::Acquire)
    }
}

/// The arena of one epoch: the classes, the user functions and methods,
/// and the natives a class lays under its own scope published past the
/// engine's own and the preload's, each numbered past what is numbered
/// below them, with the caches that name them.
pub(crate) struct Arena {
    /// The preload, which every epoch after it is laid over
    /// ([`Epochs::lay_base`]); none in the preload's own arena and where
    /// nothing was preloaded.
    below: Option<Base>,
    classes: Column<Declared>,
    functions: Column<UserFunction>,
    class_natives: Column<(ClassId, ClassNative)>,
    /// The classes below, the engine's and the preload's, which the
    /// arena's are numbered past.
    first_class: u32,
    /// The functions below, the engine's natives and native methods and
    /// the preload's user functions, which the arena's are numbered past.
    first_function: u32,
    /// The preload's class natives, which the arena's are numbered past.
    first_class_native: u32,
    /// php's inheritance cache over the classes published here, dropped
    /// with them ([`crate::inheritance_cache`]).
    pub(crate) inheritance: crate::inheritance_cache::InheritanceCache,
    /// The unit cache's files, whose early-bound classes and functions
    /// are rows here, php's `ZCSG(hash)` and what is counted of the
    /// memory under it ([`crate::unit_cache::Files`]).
    pub(crate) files: crate::unit_cache::Files,
    /// Where compiled code finds the landing words of the epoch's rows,
    /// its own in `files` and its rows of the preload's op arrays in
    /// `below`, which a trace's escape reads
    /// ([`crate::tier::published::Landings`]).
    landings: crate::tier::published::Landings,
    /// The engine's own classes' linked records, which every epoch
    /// shares, so what holds an epoch reads a class of the engine's as it
    /// reads one the epoch published ([`Arena::linked_class`]).
    natives: Arc<[LinkedClass<ClassId>]>,
    /// The compiled tier's value for the epoch, its trace table
    /// (`jit-design.md` §4.5): made by the tier the first time it asks,
    /// read by every request of the epoch with nothing taken, and
    /// dropped with the epoch, so a restart's epoch starts with none
    /// ([`crate::tier::epoch_state`]). Engine state, as every request of
    /// the epoch reads it.
    pub(crate) tier: OnceLock<php_heap::AnyState>,
    /// The engine's interned strings, which a held unit's constants and
    /// names and the engine's classes' names are: kept by the epoch so
    /// what holds it reads them after the engine that made them is
    /// dropped. Last, so the units go before the strings they name.
    interned: Interned,
}

/// The engine's two tables of interned strings (design §5.2): the
/// permanent names its startup interned, frozen after, and the table a
/// unit's constant strings enter as it is published, grown by one
/// publisher at a time. Shared by the engine and every epoch it hands out, so neither
/// outlives the strings the other's units name.
#[derive(Clone, Default)]
pub(crate) struct Interned {
    pub(crate) names: Arc<php_heap::Strings>,
    pub(crate) strings: Arc<Mutex<php_heap::Strings>>,
}

impl Interned {
    /// The table a unit's constant strings enter, its one publisher at a
    /// time. The table is append-only and every string in it was
    /// finished before it was published, so what a thread that panicked
    /// holding the `Mutex` left is consistent, and a poisoned guard is
    /// taken as it stands.
    pub(crate) fn strings(&self) -> MutexGuard<'_, php_heap::Strings> {
        self.strings.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Arena {
    /// An empty arena over an engine of `classes` classes and
    /// `functions` natives and native methods, holding `files`, for a
    /// test that links no class.
    #[cfg(test)]
    pub(crate) fn past(classes: usize, functions: usize, files: crate::unit_cache::Files) -> Arena {
        let first_class = u32::try_from(classes).unwrap_or(ClassId::LOCAL);
        let first_function = u32::try_from(functions).unwrap_or(Function::CLASS_NATIVE);
        let natives = Arc::from([]);
        Arena::numbered(
            None,
            [first_class, first_function, 0],
            files,
            natives,
            Interned::default(),
        )
    }

    /// An empty arena over an engine whose classes linked to `natives`,
    /// past them and past `functions` natives and native methods,
    /// holding `files` and keeping `interned`.
    pub(crate) fn of_engine(
        natives: Arc<[LinkedClass<ClassId>]>,
        functions: usize,
        files: crate::unit_cache::Files,
        interned: Interned,
    ) -> Arena {
        let first_class = u32::try_from(natives.len()).unwrap_or(ClassId::LOCAL);
        let first_function = u32::try_from(functions).unwrap_or(Function::CLASS_NATIVE);
        Arena::numbered(
            None,
            [first_class, first_function, 0],
            files,
            natives,
            interned,
        )
    }

    /// An empty arena numbered from `first` (its classes, functions and
    /// class natives) over `below`, holding `files`.
    fn numbered(
        below: Option<Base>,
        first: [u32; 3],
        files: crate::unit_cache::Files,
        natives: Arc<[LinkedClass<ClassId>]>,
        interned: Interned,
    ) -> Arena {
        let [first_class, first_function, first_class_native] = first;
        let preloaded = below.as_ref().map(|below| &below.funcs);
        let landings = crate::tier::published::Landings::over(&files.funcs, preloaded);
        Arena {
            below,
            landings,
            classes: Column::below(ClassId::LOCAL.saturating_sub(first_class)),
            functions: Column::below(Function::CLASS_NATIVE.saturating_sub(first_function)),
            class_natives: Column::below(Function::CLASS_NATIVE.saturating_sub(first_class_native)),
            first_class,
            first_function,
            first_class_native,
            inheritance: crate::inheritance_cache::InheritanceCache::default(),
            files,
            natives,
            tier: OnceLock::new(),
            interned,
        }
    }

    /// An empty arena laid over `below`, whose arena is laid over
    /// nothing, numbered past every number that arena reserved.
    fn over(below: Base) -> Arena {
        let base = &below.arena;
        let [classes, functions, class_natives] = base.counts();
        let first = [
            base.first_class.saturating_add(classes),
            base.first_function.saturating_add(functions),
            base.first_class_native.saturating_add(class_natives),
        ];
        let files = base.files.emptied();
        let natives = Arc::clone(&base.natives);
        let interned = base.interned.clone();
        Arena::numbered(Some(below), first, files, natives, interned)
    }

    /// The arena a restart hands on: empty, over what this one is laid
    /// over, with the preload's words laid afresh ([`Base::again`]), and
    /// numbered as it is, which no request of this epoch sees.
    fn successor(&self) -> Arena {
        let first = [
            self.first_class,
            self.first_function,
            self.first_class_native,
        ];
        let natives = Arc::clone(&self.natives);
        let interned = self.interned.clone();
        Arena::numbered(
            self.below.as_ref().map(Base::again),
            first,
            self.files.emptied(),
            natives,
            interned,
        )
    }

    /// The arena a number below this one's first names a row of: the
    /// base, which is laid over nothing ([`Epochs::lay_base`]), so a
    /// lookup goes down one arena at most.
    fn base(&self) -> Option<&Arena> {
        Some(&self.below.as_ref()?.arena)
    }

    /// The published class `class` names; none for the engine's own and
    /// for a request's.
    pub(crate) fn class(&self, class: ClassId) -> Option<&Declared> {
        let number = class.published_number()?;
        if let Some(at) = number.checked_sub(self.first_class) {
            return self.classes.get(at);
        }
        let base = self.base()?;
        base.classes.get(number.checked_sub(base.first_class)?)
    }

    /// The linked record of a class every request shares: one of the
    /// engine's, or one the epoch published; none for a request's.
    pub(crate) fn linked_class(&self, class: ClassId) -> Option<&LinkedClass<ClassId>> {
        let number = usize::try_from(class.published_number()?).ok()?;
        match self.natives.get(number) {
            Some(native) => Some(native),
            None => self.class(class)?.record(),
        }
    }

    /// The held op array at engine map-pointer offset `offset`, this
    /// epoch's or the preload's below it, with this epoch's words for it
    /// ([`crate::tier::published`]).
    pub(crate) fn func(&self, offset: u32) -> Option<&crate::tier::published::Func> {
        (self.files.funcs.get(offset)).or_else(|| self.below.as_ref()?.funcs.get(offset))
    }

    /// The landing word of opline `at` of the held op array at `offset`,
    /// this epoch's or the preload's below it ([`Arena::func`]), for a
    /// tier to arm, named in the landing index compiled code reads
    /// ([`crate::tier::published::Landings::lay`]).
    pub(crate) fn lay_landing(
        &self,
        offset: u32,
        at: php_unit::OplineIdx,
    ) -> Option<&crate::tier::Word> {
        self.landings.lay(self.func(offset)?, offset, at)
    }

    /// Where compiled code finds the landing words of this epoch's op
    /// arrays and its rows of the preload's (`jit-design.md` §4.9).
    pub(crate) fn landing_index(&self) -> crate::tier::LandingIndex {
        self.landings.index()
    }

    /// The published user function or method `function` names; none for
    /// a native and for a request's.
    pub(crate) fn function(&self, function: Function) -> Option<&UserFunction> {
        let number = function.published_number()?;
        if let Some(at) = number.checked_sub(self.first_function) {
            return self.functions.get(at);
        }
        let base = self.base()?;
        base.functions.get(number.checked_sub(base.first_function)?)
    }

    /// Whether every request of the epoch calls `function` by its number:
    /// one of the engine's natives or native methods, numbered below every
    /// arena's, or a published user function this arena or the preload's
    /// below it holds; never a request's own.
    pub(crate) fn shares_function(&self, function: Function) -> bool {
        let Some(number) = function.published_number() else {
            return false;
        };
        let natives = self
            .base()
            .map_or(self.first_function, |base| base.first_function);
        number < natives || self.function(function).is_some()
    }

    /// The published class native `function` names, and its class.
    pub(crate) fn class_native(&self, function: Function) -> Option<(ClassId, ClassNative)> {
        let at = function.published_class_native_at()?;
        if let Some(own) = at.checked_sub(self.first_class_native) {
            return self.class_natives.get(own).copied();
        }
        let base = self.base()?;
        let row = base
            .class_natives
            .get(at.checked_sub(base.first_class_native)?);
        row.copied()
    }

    /// The number of a class to publish, its row to [`Arena::publish_class`].
    pub(crate) fn reserve_class(&self) -> Option<ClassId> {
        let at = self.classes.reserve(1)?;
        Some(ClassId(self.first_class.checked_add(at)?))
    }

    /// The numbers of `count` user functions to publish, in order.
    pub(crate) fn reserve_functions(&self, count: u32) -> Option<std::ops::Range<u32>> {
        let at = self.functions.reserve(count)?;
        let first = self.first_function.checked_add(at)?;
        Some(first..first.checked_add(count)?)
    }

    /// The number of a class native to publish.
    pub(crate) fn reserve_class_native(&self) -> Option<Function> {
        let at = self.class_natives.reserve(1)?;
        Function::published_class_native(self.first_class_native.checked_add(at)?)
    }

    /// The row of a reserved class, written once; `false` where the
    /// number was not reserved or its row was written.
    pub(crate) fn publish_class(&self, class: ClassId, row: Declared) -> bool {
        let Some(at) = class
            .published_number()
            .and_then(|number| number.checked_sub(self.first_class))
        else {
            return false;
        };
        self.classes.fill(at, row).is_ok()
    }

    /// The row of a reserved user function, written once.
    pub(crate) fn publish_function(&self, function: Function, row: UserFunction) -> bool {
        let Some(at) = function
            .published_number()
            .and_then(|number| number.checked_sub(self.first_function))
        else {
            return false;
        };
        self.functions.fill(at, row).is_ok()
    }

    /// The row of a reserved class native, written once.
    pub(crate) fn publish_class_native(
        &self,
        function: Function,
        row: (ClassId, ClassNative),
    ) -> bool {
        let Some(at) = function
            .published_class_native_at()
            .and_then(|at| at.checked_sub(self.first_class_native))
        else {
            return false;
        };
        self.class_natives.fill(at, row).is_ok()
    }

    /// How many classes, functions and class natives this arena reserved,
    /// what is below it not among them.
    pub(crate) fn counts(&self) -> [u32; 3] {
        [
            self.classes.reserved(),
            self.functions.reserved(),
            self.class_natives.reserved(),
        ]
    }
}

/// The arena's epochs (design §5.14, *Identities*): the arena a request
/// starting now is handed, which it holds until it ends. A restart
/// (`accel_activate`'s, under `ZCSG(restart_pending)`) replaces it at the
/// next request's start with an empty one over the same base, however
/// many requests of the old epoch are still served; the old arena, its
/// rows and its caches go with the last of them. php waits instead for a
/// moment no process is inside (`accel_is_inactive`) and kills the ones
/// that hold it past `opcache.force_restart_timeout`; a thread cannot be
/// killed, and an epoch per request makes the wait needless.
pub(crate) struct Epochs {
    current: Mutex<Arc<Arena>>,
}

impl Epochs {
    pub(crate) fn of(arena: Arena) -> Epochs {
        Epochs {
            current: Mutex::new(Arc::new(arena)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Arc<Arena>> {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The arena of the epoch a request starting now would enter, for
    /// what reads or fills the cache outside a request; no restart is
    /// made here.
    pub(crate) fn current(&self) -> Arc<Arena> {
        Arc::clone(&self.lock())
    }

    /// The arena for a request starting now: where `restart` finds a
    /// restart scheduled for the current one (and counts it), a new
    /// arena in its place, both while no other request can start, so
    /// one restart replaces an epoch once.
    ///
    /// Served by the built-in server with a preload, whose function's
    /// loop compiled as trace 1 before `opcache_reset()`: the restart's
    /// epoch publishes its own trace 1 first, and the preloaded loop,
    /// counting again, records as trace 2 rather than entering the new
    /// table's trace 1, as php's `zend_jit_restart` resets a preloaded op
    /// array (#3564):
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/jit-preload-' . getmypid(); mkdir($dir);
    /// file_put_contents("$dir/preload.php", '<?php function hot() { $s = 0; for ($i = 0; $i < 4; $i++) { $s += $i; } return $s; }');
    /// file_put_contents("$dir/index.php", '<?php if ($_GET["step"] == "a") { echo hot(), "\n"; var_dump(opcache_reset()); } else { $t = 0; for ($j = 0; $j < 3; $j++) { $t += $j; } echo $t, " ", hot(), "\n"; }');
    /// touch("$dir/preload.php", time() - 60); touch("$dir/index.php", time() - 60);
    /// $ini = ['opcache.enable=1', 'opcache.jit=tracing', 'opcache.jit_buffer_size=64M', 'opcache.jit_debug=28672', 'opcache.jit_hot_loop=1', 'opcache.jit_hot_func=1', 'opcache.jit_hot_return=1', 'opcache.jit_hot_side_exit=100', "opcache.preload=$dir/preload.php"];
    /// $cmd = [PHP_BINARY, '-n']; foreach ($ini as $set) { $cmd[] = '-d'; $cmd[] = $set; }
    /// $p = proc_open([...$cmd, '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// $log = ''; $port = 0; $ready = [$pipes[2]]; $none = null;
    /// while (!$port && stream_select($ready, $none, $none, 30) && ($line = fgets($pipes[2])) !== false) { if (preg_match('/:(\d+)\) started$/', rtrim($line), $m)) { $port = $m[1]; } else { $log .= $line; } $ready = [$pipes[2]]; }
    /// foreach (['a', 'b', 'b'] as $step) { echo file_get_contents("http://127.0.0.1:$port/?step=$step"); }
    /// proc_terminate($p); $log .= stream_get_contents($pipes[2]); proc_close($p);
    /// foreach (explode("\n", $log) as $line) { if (str_contains($line, 'TRACE')) { echo str_replace($dir, 'DIR', $line), "\n"; } }
    /// unlink("$dir/preload.php"); unlink("$dir/index.php"); rmdir($dir);
    /// ```
    /// ```expect regime=jit-tracing process
    /// 6
    /// bool(true)
    /// 3 6
    /// 3 6
    /// ---- TRACE 1 start (enter) $main() DIR/index.php:1
    /// ---- TRACE 1 start (loop) hot() DIR/preload.php:1
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    /// ---- TRACE 1 start (enter) $main() DIR/index.php:1
    /// ---- TRACE 1 start (loop) $main() DIR/index.php:1
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    /// ---- TRACE 2 start (enter) hot() DIR/preload.php:1
    /// ---- TRACE 2 start (loop) hot() DIR/preload.php:1
    /// ---- TRACE 2 stop (loop)
    /// ---- TRACE 2 compiled
    /// ---- TRACE 3 start (enter) $main() DIR/index.php:1
    /// ---- TRACE 3 stop (link to 1)
    /// ---- TRACE 3 compiled
    /// ---- TRACE 4 start (enter) hot() DIR/preload.php:1
    /// ---- TRACE 4 stop (link to 2)
    /// ---- TRACE 4 compiled
    /// ```
    pub(crate) fn enter(&self, restart: impl FnOnce(&Arena) -> bool) -> Arc<Arena> {
        let mut current = self.lock();
        if restart(&current) {
            let next = current.successor();
            *current = Arc::new(next);
        }
        Arc::clone(&current)
    }

    /// `zend_shared_alloc_save_state` after preloading: the preload's
    /// `arena`, which it has published into, laid below every epoch from
    /// now on, with `units`, the files it preloaded, whose words are laid
    /// as `laid` says, as php's `zend_jit_script` lays the preload's
    /// units' words as it persists them; the first laid stands, and
    /// an arena laid over one is never laid. The preload is served before
    /// any other request, and publishes nothing into it after; a row it
    /// did would be numbered among the next epoch's own and reached by no
    /// request of it.
    pub(crate) fn lay_base(&self, arena: &Arc<Arena>, units: Arc<[PublishedUnit]>, laid: Laid) {
        let mut current = self.lock();
        if arena.below.is_some() || current.below.is_some() {
            return;
        }
        let below = Base::of(Arc::clone(arena), units, laid);
        *current = Arc::new(Arena::over(below));
    }
}

/// The preload as an epoch laid over it holds it ([`Epochs::lay_base`]):
/// its arena, whose rows every epoch shares at their numbers; the files
/// it preloaded and how their words are laid; and the epoch's own words
/// for their op arrays. A preloaded op array runs in every epoch, while
/// the trace a word names is of one epoch's table and goes with it, so
/// each epoch lays its own words for them, as php's restart resets every
/// preloaded op array's (`zend_jit_restart`) and an epoch still served
/// keeps its own.
struct Base {
    arena: Arc<Arena>,
    units: Arc<[PublishedUnit]>,
    laid: Laid,
    funcs: crate::tier::published::Funcs,
}

impl Base {
    fn of(arena: Arc<Arena>, units: Arc<[PublishedUnit]>, laid: Laid) -> Base {
        let funcs = crate::tier::published::Funcs::preloaded(&units, laid);
        Base {
            arena,
            units,
            laid,
            funcs,
        }
    }

    /// The same preload for a restart's epoch, its words laid afresh.
    fn again(&self) -> Base {
        Base::of(Arc::clone(&self.arena), Arc::clone(&self.units), self.laid)
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::Arc;

    use super::{Arena, CHUNK, Column, Epochs, Laid, SPAN, SPANS};
    use crate::api::call::ClassNative;
    use crate::api::values::ClassId;
    use crate::enums::EnumMethod;

    /// The preload's rows laid below every epoch: each epoch numbers its
    /// own past them and reads them at their numbers, and a restart
    /// hands on an empty epoch that reads them still.
    #[test]
    fn the_preloads_rows_keep_their_numbers_across_restarts() {
        let epochs = Epochs::of(Arena::past(10, 20, crate::unit_cache::Files::new(16)));
        let preload = epochs.enter(|_| false);
        assert_eq!(preload.reserve_class(), Some(ClassId(10)));
        let method = preload.reserve_class_native();
        let row = (ClassId(10), ClassNative::Enum(EnumMethod::Cases));
        assert!(method.is_some_and(|method| preload.publish_class_native(method, row)));
        epochs.lay_base(&preload, Arc::from([]), Laid::COLD);
        let first = epochs.enter(|_| false);
        assert_eq!(
            first.reserve_class(),
            Some(ClassId(11)),
            "numbered past the preload's"
        );
        let own = first.reserve_class_native();
        let mine = (ClassId(11), ClassNative::Enum(EnumMethod::From));
        assert!(own.is_some_and(|own| first.publish_class_native(own, mine)));
        assert_ne!(own, method);
        let second = epochs.enter(|_| true);
        assert!(!Arc::ptr_eq(&first, &second), "a new epoch");
        assert_eq!(second.counts(), [0, 0, 0]);
        for epoch in [&first, &second] {
            assert_eq!(
                method.and_then(|method| epoch.class_native(method)),
                Some(row)
            );
        }
        assert_eq!(own.and_then(|own| first.class_native(own)), Some(mine));
        assert_eq!(
            own.and_then(|own| second.class_native(own)),
            None,
            "not the new epoch's"
        );
        assert_eq!(second.reserve_class(), Some(ClassId(11)));
        assert!(Arc::ptr_eq(&second, &epochs.enter(|_| false)));
    }

    /// A restart after a preloaded op array's trace compiled (#3564):
    /// the restart's epoch reads that op array's words as php's
    /// `zend_jit_restart` leaves them, counting from where they were laid
    /// with no landing armed, never the number of a trace of the table
    /// the restart dropped, while the epoch still served keeps its own;
    /// the preload's own arena, which publishes no words of them, holds
    /// none.
    #[test]
    fn a_restarts_epoch_lays_the_preloads_words_afresh() {
        use std::sync::atomic::Ordering::Relaxed;

        use php_unit::OplineIdx;

        use crate::tier::SafepointIdx;
        let epochs = Epochs::of(Arena::past(0, 0, crate::unit_cache::Files::new(16)));
        let preload = epochs.enter(|_| false);
        let unit = crate::tier::published::tests::published(5, 2);
        epochs.lay_base(&preload, Arc::from([unit]), Laid::Counting([3, 0]));
        assert!(preload.func(5).is_none());
        let first = epochs.enter(|_| false);
        let halves = |epoch: &Arena| {
            let word = epoch.func(5).map(|func| func.word(SafepointIdx(1)));
            word.map(|word| word.halves().each_ref().map(|half| half.load(Relaxed)))
        };
        assert_eq!(halves(&first), Some([3, 0]), "laid as the preload said");
        // The tier compiled trace 1 of the first epoch's table at the
        // loop, and armed a landing that a return roots.
        let word = first.func(5).map(|func| func.word(SafepointIdx(1)));
        word.inspect(|word| word.halves()[1].store((1 << 8) | 2, Relaxed));
        let entry = first.func(5).map(|func| func.word(SafepointIdx(0)));
        entry.inspect(|word| word.halves()[1].store(u32::MAX, Relaxed));
        let landing = first.lay_landing(5, OplineIdx(1));
        landing.inspect(|word| word.halves()[1].store(0, Relaxed));
        let second = epochs.enter(|_| true);
        assert_eq!(halves(&second), Some([3, 0]), "counting again");
        let entry = second.func(5).map(|func| func.word(SafepointIdx(0)));
        let cleared = entry.map(|word| word.halves()[1].load(Relaxed));
        assert_eq!(cleared, Some(0), "no longer blacklisted");
        let unarmed = second.func(5).map(|func| func.landing(OplineIdx(1)));
        assert!(unarmed.is_some_and(|word| word.halves()[1].load(Relaxed) == u32::MAX));
        assert_eq!(
            halves(&first),
            Some([3, (1 << 8) | 2]),
            "the old epoch's own"
        );
        let armed = first.func(5).map(|func| func.landing(OplineIdx(1)));
        assert!(armed.is_some_and(|word| word.halves()[1].load(Relaxed) == 0));
        assert!(second.func(4).is_none() && second.func(6).is_none());
    }

    /// A landing armed in a preloaded op array is named in the landing
    /// index of the epoch that armed it, as one of the epoch's own is
    /// (#3568), and a restart's epoch, which
    /// lays the preload's words afresh, names none of the old epoch's,
    /// and its own where it arms one.
    #[test]
    fn an_epochs_landing_index_names_its_landings_of_the_preload() {
        use php_unit::OplineIdx;

        let epochs = Epochs::of(Arena::past(0, 0, crate::unit_cache::Files::new(16)));
        let preload = epochs.enter(|_| false);
        let unit = crate::tier::published::tests::published(5, 2);
        epochs.lay_base(&preload, Arc::from([unit]), Laid::Counting([3, 0]));
        let first = epochs.enter(|_| false);
        let words = |epoch: &Arena| {
            let func = epoch.func(5)?;
            Some(std::ptr::from_ref(func.landing(OplineIdx(0))).addr())
        };
        assert_eq!(first.landings.named(5), None, "none armed yet");
        assert!(first.lay_landing(5, OplineIdx(1)).is_some());
        let named = first.landings.named(5);
        assert!(named.is_some() && named == words(&first), "{named:?}");
        let second = epochs.enter(|_| true);
        assert_eq!(second.landings.named(5), None, "not the old epoch's");
        assert_eq!(
            second.landing_index().preloaded,
            first.landing_index().preloaded
        );
        assert!(second.lay_landing(5, OplineIdx(1)).is_some());
        let renamed = second.landings.named(5);
        assert!(renamed.is_some() && renamed == words(&second) && renamed != named);
        assert_eq!(first.landings.named(5), named, "the old epoch's own");
    }

    /// The tier's value is the epoch's (`jit-design.md` §4.5): what the
    /// tier made for one epoch every request of it reads, a restart's
    /// epoch starts with none, and the old value goes with the old epoch.
    #[test]
    fn a_restarts_epoch_starts_with_no_tier_value() {
        let epochs = Epochs::of(Arena::past(0, 0, crate::unit_cache::Files::new(16)));
        let first = epochs.enter(|_| false);
        let made = first
            .tier
            .get_or_init(|| php_heap::AnyState::new(Arc::new(7_u32)));
        let held = made.downcast_ref::<Arc<u32>>().map(Arc::downgrade);
        assert!(Arc::ptr_eq(&first, &epochs.enter(|_| false)));
        let second = epochs.enter(|_| true);
        assert!(second.tier.get().is_none(), "a new epoch's has none");
        assert!(held.as_ref().is_some_and(|held| held.upgrade().is_some()));
        drop(first);
        assert!(
            held.is_some_and(|held| held.upgrade().is_none()),
            "gone with its epoch"
        );
    }

    /// A number's low bits are its slot in a chunk, its middle bits the
    /// chunk's place in a span and its high bits the span's: rows on
    /// either side of a chunk's edge and of a span's are each their own,
    /// a column is held to the rows its spans hold, and a number past
    /// them names nothing.
    #[test]
    fn each_number_has_its_own_slot_across_the_chunks_and_spans() {
        let column: Column<u32> = Column::below(u32::MAX);
        let held = u32::try_from(SPANS * SPAN * CHUNK).unwrap_or(u32::MAX);
        assert_eq!(column.limit, held, "held to the spans");
        let chunk = u32::try_from(CHUNK).unwrap_or(u32::MAX);
        let span = u32::try_from(SPAN * CHUNK).unwrap_or(u32::MAX);
        let edges = [0, chunk - 1, chunk, span - 1, span];
        assert_eq!(column.reserve(span + 1), Some(0));
        for at in edges {
            assert_eq!(column.fill(at, at + 7), Ok(()));
        }
        for at in edges {
            assert_eq!(column.get(at), Some(&(at + 7)));
        }
        assert_eq!(column.get(1), None, "reserved, never filled");
        assert_eq!(column.get(held), None);
        assert_eq!(column.get(u32::MAX), None);
        assert!(
            column.spans.iter().skip(2).all(|span| span.get().is_none()),
            "laid where a row needs it"
        );
    }

    #[test]
    fn a_row_is_found_once_filled_and_never_written_again() {
        let column: Column<u64> = Column::below(1000);
        let at = column.reserve(3).unwrap_or(u32::MAX);
        assert_eq!(at, 0);
        assert_eq!(column.get(1), None);
        assert_eq!(column.fill(1, 11), Ok(()));
        assert_eq!(column.get(1), Some(&11));
        assert_eq!(column.fill(1, 12), Err(12));
        assert_eq!(column.fill(3, 13), Err(13), "not reserved");
        assert_eq!(column.reserve(998), None, "past the limit");
        assert_eq!(column.reserved(), 3);
    }

    /// Two threads reserving and filling at once: each number is one
    /// thread's, each row is what that thread filled, and a row read
    /// while the other thread grows the column stays where it was.
    #[test]
    fn rows_filled_on_two_threads_stay_where_they_were_filled() {
        let column: Column<(u8, u32)> = Column::below(1 << 20);
        let taken: Vec<Vec<u32>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0u8..2)
                .map(|thread| {
                    let column = &column;
                    scope.spawn(move || {
                        let mut mine = Vec::new();
                        for n in 0..5000 {
                            let Some(at) = column.reserve(1) else { break };
                            assert_eq!(column.fill(at, (thread, n)), Ok(()));
                            mine.push(at);
                            let first = mine.first().copied().unwrap_or(0);
                            assert_eq!(column.get(first), Some(&(thread, 0)));
                        }
                        mine
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("a worker runs to its end"))
                .collect()
        });
        for (thread, numbers) in (0u8..).zip(&taken) {
            for (n, at) in (0u32..).zip(numbers) {
                assert_eq!(column.get(*at), Some(&(thread, n)));
            }
        }
        assert_eq!(column.reserved(), 10_000);
    }
}
