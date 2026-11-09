//! Compiled code's guard on a method's pair held to the interpreter's
//! read-back of it (`jit-design.md` §4.3, #4326). Compiled code tests a
//! slot's tag and that the class past it is the object's (`php_jit`'s
//! `cache_guard` over [`PLACE`]'s `pair` and `pair_class`), where
//! [`Request::cached_callee`] also tests that the frame reads the slot
//! back, by its function or by its scope. The table walks every tag a
//! slot holds, the slot's class against the object's and the scope the
//! reading frame stands in, and holds that a passing guard is a hit
//! wherever the recorder takes the frame: a frame of the function that
//! wrote the slot, its op array's scope fixed (`tier::view::scoped_code`),
//! under a fake scope or none, which [`Request::cached_callee`] does not
//! test. Outside that, a passing guard misses only where the slot was
//! written from another scope, which is why the recorder takes no op
//! array whose scope is not fixed.
//!
//! A slot's bytes are read as compiled code reads them only where
//! `unsafe` may stand, so the guard here reads the variant RFC 2195's
//! mirror lays at [`PLACE`]'s offsets, as `exec.rs`'s layout asserts
//! hold it: the tag, then the pair's class.

use std::sync::Arc;

use php_heap::FrameRef;
use php_unit::decl::Bare;
use php_unit::{CacheSlot, OpArray, Opcode, Opline, Unit};

use crate::api::call::Function;
use crate::api::request::{PropertyInfo, Request};
use crate::api::values::{ClassId, Value};
use crate::engine::{Engine, RequestDescription};
use crate::exec::{
    self, CacheAt, CacheBase, Cached, CallInfo, PrimedCall, PrimedFrom, PropertyOffset,
    PropertyPlace,
};
use crate::tier::PLACE;

/// The tag compiled code reads at `PLACE.tag()`, by the name [`PLACE`]
/// gives it; none for a variant compiled code names no tag of, whose
/// tag the compiler holds apart from every named one.
fn tag(slot: Cached) -> Option<u32> {
    match slot {
        Cached::Unprimed => Some(PLACE.unprimed()),
        Cached::ClassAndCallee { .. } => Some(PLACE.pair()),
        Cached::Property(_) => Some(PLACE.property()),
        Cached::Call(_) => Some(PLACE.call()),
        Cached::Constant(_) => Some(PLACE.constant()),
        Cached::StaticProperty { .. } => Some(PLACE.static_property()),
        Cached::Function(_)
        | Cached::Class(_)
        | Cached::ClassConstant { .. }
        | Cached::Constructed { .. }
        | Cached::Miss
        | Cached::Hit
        | Cached::Undefined(_) => None,
    }
}

/// `cache_guard` over `(PLACE.pair(), PLACE.pair_class())`: the slot's
/// tag is the pair's, tested first, and the class past it is `object`.
fn guard_passes(slot: Cached, object: ClassId) -> bool {
    tag(slot) == Some(PLACE.pair())
        && matches!(slot, Cached::ClassAndCallee { class, .. } if class == object)
}

/// The scope the frame reading the slot stands in, against the frame
/// that wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// A frame of the function that wrote the slot: every frame of an op
    /// array whose scope is fixed is that to every other, and it is the
    /// one frame the recorder takes.
    Own,
    /// The same, with a fake scope standing, as a resumed generator's or
    /// fiber's frame may stand under one, as php's does.
    Faked,
    /// A frame of the scope the slot was written from but of another
    /// function: the slot a closure's frame wrote.
    Closure,
    /// A frame of another function in another scope.
    Other,
}

/// One row of the table: the scope the frame stands in, whether the
/// slot's class is the object's, and the slot's tag.
type Row = (Scope, bool, Option<u32>);

/// The rows the guard passes, and those of them the interpreter misses.
#[derive(Debug, Default)]
struct Walked {
    guarded: Vec<Row>,
    missed: Vec<Row>,
}

/// The slot the table writes and reads, the frame that reads it and the
/// object's class, and what every slot of it is built over: the callee,
/// its record and the request's count of constants.
struct Table {
    frame: FrameRef,
    slot: CacheSlot,
    /// The object's class, `A`.
    object: ClassId,
    /// Another class, `B`, and the fake scope a [`Scope::Faked`] row
    /// stands under.
    other: ClassId,
    callee: Function,
    record: crate::readable::CalleeAt,
    count: crate::api::request::ConstantCount,
}

impl Table {
    /// The rows of `scope`, the slot written from `primed`: every tag,
    /// its class the object's and another, read back by the interpreter
    /// and tested by the guard, a passing guard held to a hit on a row
    /// the recorder takes.
    fn walk(&self, req: &mut Request, (scope, primed): (Scope, PrimedFrom), walked: &mut Walked) {
        let (frame, slot, object) = (self.frame, self.slot, self.object);
        for class in [object, self.other] {
            for cached in self.every_tag(class, primed) {
                req.prime(frame, slot, cached);
                let read =
                    |req: &mut Request| req.cached_callee(frame, slot, Some(object)).is_some();
                let hit = match scope {
                    Scope::Faked => req.in_fake_scope(Some(self.other), read),
                    Scope::Own | Scope::Closure | Scope::Other => read(req),
                };
                let guarded = guard_passes(cached, object);
                let row = (scope, class == object, tag(cached));
                if scope == Scope::Own {
                    assert!(
                        !guarded || hit,
                        "the guard passes and the interpreter misses on {row:?}: {cached:?}"
                    );
                }
                if guarded {
                    walked.guarded.push(row);
                }
                if guarded && !hit {
                    walked.missed.push(row);
                }
            }
        }
    }

    /// One slot of every variant, `class` its class where it holds one
    /// and `primed` the frame it was written from where it names one.
    fn every_tag(&self, class: ClassId, primed: PrimedFrom) -> [Cached; 13] {
        let call = PrimedCall {
            function: self.callee,
            room: 0,
            callee: self.record,
            info: CallInfo::default(),
            cache: CacheAt::default(),
            base: CacheBase::NONE,
            opline: 0,
        };
        [
            Cached::Unprimed,
            Cached::Constant(Value::int(1)),
            Cached::Function(self.callee),
            Cached::Class(class),
            Cached::ClassAndCallee {
                class,
                primed,
                function: self.callee,
                instance: true,
                record: self.record,
            },
            Cached::StaticProperty {
                class,
                primed,
                info: PropertyInfo {
                    declaring: class,
                    at: 0,
                },
                place: 0,
            },
            Cached::ClassConstant {
                class,
                primed,
                value: Value::int(1),
            },
            Cached::Property(PropertyPlace {
                class,
                offset: PropertyOffset::Dynamic,
                primed,
            }),
            Cached::Call(call),
            Cached::Miss,
            Cached::Hit,
            Cached::Undefined(self.count),
            Cached::Constructed { class, call },
        ]
    }
}

/// A frame of `function` activated as a callback's is, over a main op
/// array pushed to call it from.
fn activated(req: &mut Request, function: Function) -> FrameRef {
    let main = Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::Return,
                ..Opline::default()
            }]
            .into(),
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let main = req.pushed(main);
    let mut header = exec::Header::call(function, 0);
    header.info.set_serving();
    let call = req.push_call(header).expect("the call is pushed");
    let shape = req.call_shape(call).expect("the call's shape");
    req.activate_user(call, shape, None, exec::Caller::Opline(main))
        .expect("the frame is activated")
        .frame
}

#[test]
fn a_passing_guard_on_a_method_s_pair_is_a_hit_wherever_the_recorder_takes_the_frame() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    assert!(
        req.code_mut()
            .eval_stringl(
                b"class A { function m($o) { return $o->m(); } } \
                  class B { function m($o) { return $o->m(); } }",
                b"declared"
            )
            .is_ok(),
        "the classes are declared"
    );
    let class = |req: &Request, name: &[u8]| {
        req.class_named(Bare::resolved(name))
            .expect("the class is declared")
    };
    let (a, b) = (class(&req, b"a"), class(&req, b"b"));
    let method = |req: &Request, class: ClassId| {
        req.symbols()
            .method_named(class, b"m")
            .expect("the class declares m")
    };
    let (a_m, b_m) = (method(&req, a), method(&req, b));
    let frame = activated(&mut req, a_m);
    let slot = CacheSlot(0);
    req.prime_callee(frame, slot, a, (a_m, true));
    let Cached::ClassAndCallee {
        primed: own,
        record,
        ..
    } = req.cached(frame, slot)
    else {
        panic!("the pair is primed: {:?}", req.cached(frame, slot));
    };
    assert_eq!(
        own,
        PrimedFrom {
            function: Some(a_m),
            scope: Some(a)
        },
        "A::m's frame writes its own function and scope"
    );
    let table = Table {
        frame,
        slot,
        object: a,
        other: b,
        callee: a_m,
        record,
        count: req.constant_count(),
    };
    let mut walked = Walked::default();
    for (scope, primed) in [
        (Scope::Own, own),
        (Scope::Faked, own),
        (
            Scope::Closure,
            PrimedFrom {
                function: None,
                scope: Some(a),
            },
        ),
        (
            Scope::Other,
            PrimedFrom {
                function: Some(b_m),
                scope: Some(b),
            },
        ),
    ] {
        table.walk(&mut req, (scope, primed), &mut walked);
    }
    let pair = Some(PLACE.pair());
    assert_eq!(
        walked.guarded,
        [
            (Scope::Own, true, pair),
            (Scope::Faked, true, pair),
            (Scope::Closure, true, pair),
            (Scope::Other, true, pair),
        ],
        "the guard passes on the pair of the object's class alone, whatever the scope"
    );
    assert_eq!(
        walked.missed,
        [(Scope::Other, true, pair)],
        "outside the frames the recorder takes, a passing guard misses where another scope wrote the slot"
    );
}
