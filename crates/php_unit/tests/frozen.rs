//! `Frozen::new` and the walk it reads through (#4649): a unit or a class
//! declaration is admitted only where every value it holds is the engine
//! table's, every index its constants hold names an entry of its own
//! tables, and every op array is numbered by the engine.

use php_heap::{Heap, Intern, Key, Strings, Zval};
use php_unit::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstDecl,
    ConstExpr, EnumBacking, HookKind, MethodDecl, Name, NameKind, NativeParameter, Parameter,
    ParameterDefault, Parameters, PropertyDecl, PropertyHooks, Type, UnitValue, Visibility,
};
use php_unit::frozen::{Frozen, Holder, Slot, Stray, unit_values};
use php_unit::{
    BoundVar, HookBody, MapPtrOffset, MethodBody, OpArray, PublishedUnit, Unit, UnitCell,
};

/// What the engine's table lays for a unit it publishes: a string, and a
/// constant array holding it under a string key of the table's.
struct Table {
    strings: Strings,
    string: Zval,
    array: Zval,
}

fn table() -> Table {
    let mut strings = Strings::new();
    let string = strings.intern(b"the table's").expect("the table has room");
    let key = strings.intern(b"key").expect("the table has room");
    let entries = [
        (Key::Str(key), Zval::string(string)),
        (Key::Long(1), Zval::long(2)),
    ];
    let array = strings.constant_array(&entries, 2).expect("an array");
    Table {
        strings,
        string: Zval::string(string),
        array: Zval::array(array),
    }
}

fn held(z: Zval) -> ConstExpr {
    ConstExpr::UnitValue(UnitValue::new(z).expect("an array or a counted string"))
}

fn attribute(value: ConstExpr) -> Attribute {
    Attribute {
        name: Name::from("At"),
        arguments: vec![AttributeArgument { name: None, value }].into(),
        line: 1,
        strict: false,
        own_name: false,
    }
}

/// A class whose one constant is `value`.
fn class(value: ConstExpr) -> ClassDecl {
    ClassDecl {
        name: Name::from("K"),
        kind: ClassKind::Class,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: Vec::new().into(),
        implementors: Vec::new().into(),
        aliases: Vec::new().into(),
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: EnumBacking::Pure,
        constants: vec![ClassConstDecl {
            name: Name::from("C"),
            visibility: Visibility::Public,
            is_final: false,
            is_case: false,
            ty: None,
            value,
            attributes: Vec::new().into(),
            doc_comment: None,
        }],
        properties: Vec::new().into(),
        methods: Vec::new(),
        attributes: Vec::new().into(),
        doc_comment: None,
        file: Name::from("k.php"),
        line: 1,
        end_line: 1,
        object_size: 0,
        object_beside: Vec::new().into(),
        attribute_validator: None,
    }
}

fn code(literals: Vec<Zval>) -> OpArray {
    OpArray {
        literals: literals.into(),
        ..OpArray::default()
    }
}

/// A unit holding every kind of slot the walk reaches, each value the
/// table's: the main op array's constants, among them an index of each
/// kind below its bound, a bound variable, a constant expression, a
/// function's attribute, a dynamic definition, a class's constant, a
/// `const`, and a method's op array.
fn published(table: &Table) -> Unit {
    let mut main = code(vec![
        table.string,
        table.array,
        Zval::long(3),
        Zval::const_expr(0),
        Zval::ptr(0),
    ]);
    main.const_exprs = vec![held(table.array)];
    main.bound_vars = vec![BoundVar {
        name: Name::from("s"),
        value: table.string,
        lexical: false,
    }];
    let mut function = php_unit::decl::native_function(b"f");
    function.attributes = vec![attribute(held(table.array))].into();
    Unit {
        main,
        functions: vec![(function, code(vec![table.string]))],
        dynamic: vec![(
            php_unit::decl::native_function(b"g"),
            code(vec![table.array]),
        )],
        classes: vec![class(held(table.array))],
        constants: vec![ConstDecl {
            name: Name::from("Z"),
            value: held(table.array),
            attributes: vec![attribute(held(table.array))].into(),
        }],
        methods: vec![MethodBody {
            class: 0,
            method: 0,
            body: code(vec![table.string]),
        }],
        ..Unit::default()
    }
}

fn frozen(unit: Unit) -> bool {
    Frozen::new(unit.published(None)).is_ok()
}

#[test]
fn frozen_admits_a_unit_the_engine_published() {
    let table = table();
    let unit = Frozen::new(published(&table).published(None));
    let unit = unit.expect("every value is the table's");
    assert_eq!(unit.functions.len(), 1, "read through the frozen unit");
    let class = Frozen::new(class(held(table.array)));
    assert!(class.is_ok(), "a declaration of the table's values");
    assert!(
        table.strings.find(b"key").is_some(),
        "the table holds what they name"
    );
}

#[test]
fn frozen_refuses_a_unit_with_a_requests_string() {
    let table = table();
    let mut heap = Heap::new();
    let string = Zval::string(heap.string(b"made in the request's memory").expect("room"));
    let mut unit = published(&table);
    unit.main = code(vec![string]);
    assert!(!frozen(unit), "a request's string as a constant");
    let mut unit = published(&table);
    unit.functions[0].0.attributes = vec![attribute(held(string))].into();
    assert!(
        !frozen(unit),
        "a request's string as a function's attribute"
    );
}

#[test]
fn frozen_refuses_a_unit_with_a_refcounted_array() {
    let table = table();
    let mut heap = Heap::new();
    let array = Zval::array(heap.array().expect("the heap has room"));
    let mut unit = published(&table);
    unit.dynamic[0].1 = code(vec![array]);
    assert!(!frozen(unit), "a refcounted array as a constant");
    let mut unit = published(&table);
    unit.main.const_exprs = vec![held(array)];
    assert!(!frozen(unit), "a refcounted array in a constant expression");
}

#[test]
fn frozen_refuses_a_unit_with_an_op_array_a_request_numbered() {
    let table = table();
    let mut unit = published(&table);
    unit.methods[0].body.runtime_cache = MapPtrOffset::Request(1);
    assert_eq!(
        unit_values(&unit, &mut |_, _, _| {}),
        Err(Stray::Request(1))
    );
    assert!(!frozen(unit), "an op array numbered by a request");
    let mut unit = published(&table);
    unit.main.runtime_cache = MapPtrOffset::Engine(7);
    assert!(frozen(unit), "an op array numbered by the engine");
}

#[test]
fn frozen_refuses_a_declaration_with_a_requests_constant() {
    let mut heap = Heap::new();
    let string = Zval::string(heap.string(b"a request's").expect("room"));
    let array = Zval::array(heap.array().expect("the heap has room"));
    for z in [string, array] {
        assert!(
            Frozen::new(class(held(z))).is_err(),
            "{:?} as a constant",
            z.kind()
        );
        let table = table();
        let mut unit = published(&table);
        unit.classes = vec![class(held(z))];
        assert!(!frozen(unit), "a class of the unit holding {:?}", z.kind());
    }
}

/// The constants the walk hands on from `unit`'s main op array, and what
/// it refused.
fn constants(unit: &Unit) -> (Vec<Zval>, Result<(), Stray>) {
    let mut found = Vec::new();
    let walked = unit_values(unit, &mut |holder, slot, z| {
        if (holder, slot) == (Holder::Main, Slot::Constant) {
            found.push(z);
        }
    });
    (found, walked)
}

#[test]
fn the_walk_decides_a_constant_expressions_index_by_its_op_arrays_bound() {
    let table = table();
    let mut unit = published(&table);
    unit.main.literals = vec![Zval::const_expr(0)].into();
    assert_eq!(constants(&unit), (Vec::new(), Ok(())), "below the bound");
    assert!(frozen(unit.clone()), "below the bound");
    unit.main.literals = vec![Zval::const_expr(1)].into();
    assert_eq!(constants(&unit).1, Err(Stray::PastConstExprs(1)));
    assert!(!frozen(unit.clone()), "past the bound");
    unit.main.const_exprs = Vec::new();
    unit.main.literals = vec![Zval::const_expr(0)].into();
    assert_eq!(constants(&unit).1, Err(Stray::PastConstExprs(0)));
    // Another op array's table is not this one's.
    unit.functions[0].1.const_exprs = vec![held(table.array)];
    assert_eq!(constants(&unit).1, Err(Stray::PastConstExprs(0)));
}

#[test]
fn the_walk_decides_an_is_ptrs_index_by_the_units_constants() {
    let table = table();
    let mut unit = published(&table);
    unit.main.literals = vec![Zval::ptr(0)].into();
    assert_eq!(constants(&unit), (Vec::new(), Ok(())), "below the bound");
    assert!(frozen(unit.clone()), "below the bound");
    unit.main.literals = vec![Zval::ptr(1)].into();
    assert_eq!(constants(&unit).1, Err(Stray::PastConstants(1)));
    assert!(!frozen(unit.clone()), "past the bound");
    unit.main.literals = vec![Zval::ptr(0)].into();
    unit.constants = Vec::new();
    assert_eq!(constants(&unit).1, Err(Stray::PastConstants(0)));
    assert!(!frozen(unit), "a unit with no constant");
}

#[test]
fn the_walk_refuses_an_undef_constant() {
    let table = table();
    let mut unit = published(&table);
    unit.main.literals = vec![Zval::long(1), Zval::UNDEF].into();
    assert_eq!(constants(&unit), (vec![Zval::long(1)], Err(Stray::Undef)));
    assert!(!frozen(unit), "an `UNDEF` constant");
}

#[test]
fn a_frozen_unit_is_engine_state() {
    fn held<T: php_heap::EngineState>() {}
    held::<Frozen<PublishedUnit>>();
    held::<Frozen<ClassDecl>>();
}

#[test]
fn the_walk_hands_each_declared_value_beside_its_holder() {
    let table = table();
    let unit = published(&table);
    let mut declared = Vec::new();
    let walked = unit_values(&unit, &mut |holder, slot, _| {
        if slot == Slot::Declared {
            declared.push(holder);
        }
    });
    assert_eq!(walked, Ok(()));
    // The function's attribute, then the class's constant; the `const`'s
    // value and attribute are views of its `DECLARE_CONST`.
    assert_eq!(declared, [Holder::Unit, Holder::Unit]);
}

/// A native's one parameter, as its entry spells it.
const ENTRIES: &[NativeParameter] = &[NativeParameter {
    name: b"flags",
    ty: || Some(Type::Int),
    by_ref: false,
    prefer_ref: false,
    variadic: false,
    passed: true,
    default: Some(|| ConstExpr::long(0)),
    attributes: &[],
}];

/// The walk reads a native's slots only where a reader has built them, so
/// a declaration whose method is a native's leaves the slots unbuilt.
#[test]
fn the_walk_leaves_a_natives_unbuilt_slots_unbuilt() {
    let table = table();
    let mut function = php_unit::decl::native_function(b"m");
    function.parameters = Parameters::native(ENTRIES);
    let mut declared = class(held(table.array));
    declared.methods = vec![MethodDecl {
        function,
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }];
    let frozen = Frozen::new(declared).expect("every value is the table's");
    let parameters = &frozen.methods[0].function.parameters;
    assert!(parameters.unbuilt().is_some(), "the walk built no slot");
    assert_eq!(parameters.len(), 1, "a reader builds them after");
}

/// A string a request's compile made in the request's memory, counted.
fn requests(heap: &mut Heap) -> Zval {
    Zval::string(heap.string(b"made in the request's memory").expect("room"))
}

/// A method named `m` whose declaration carries `attributes`.
fn method(attributes: Vec<Attribute>) -> MethodDecl {
    let mut function = php_unit::decl::native_function(b"m");
    function.attributes = attributes.into();
    MethodDecl {
        function,
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// A property named `p` with nothing in it, for a test to lay one value in.
fn property() -> PropertyDecl {
    PropertyDecl {
        name: Name::from("p"),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
        is_promoted: false,
        is_virtual: false,
        ty: None,
        default: None,
        attributes: Vec::new().into(),
        doc_comment: None,
        hooks: None,
    }
}

/// A parameter named `a` with nothing in it.
fn parameter() -> Parameter {
    Parameter {
        name: Name::from("a"),
        ty: None,
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new().into(),
    }
}

/// Whether the table's class, with `change` made to it, is frozen.
fn declaration(table: &Table, change: impl FnOnce(&mut ClassDecl)) -> bool {
    let mut declared = class(held(table.array));
    change(&mut declared);
    Frozen::new(declared).is_ok()
}

#[test]
fn frozen_refuses_a_requests_value_as_a_propertys_default() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    let mut p = property();
    p.default = Some(held(table.array));
    assert!(declaration(&table, |c| c.properties = vec![p.clone()].into()));
    p.default = Some(held(s));
    assert!(!declaration(&table, |c| c.properties = vec![p].into()));
}

#[test]
fn frozen_refuses_a_requests_value_as_a_propertys_attribute() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    let mut p = property();
    p.attributes = vec![attribute(held(s))].into();
    assert!(!declaration(&table, |c| c.properties = vec![p].into()));
}

#[test]
fn frozen_refuses_a_requests_value_as_a_property_hooks_attribute() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    for (get, set) in [
        (Some(method(vec![attribute(held(s))])), None),
        (None, Some(method(vec![attribute(held(s))]))),
    ] {
        let mut p = property();
        p.hooks = Some(Box::new(PropertyHooks {
            get,
            set,
            is_abstract: false,
        }));
        assert!(!declaration(&table, |c| c.properties = vec![p].into()));
    }
}

#[test]
fn frozen_refuses_a_requests_value_in_a_property_hooks_op_array() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    let hook = |literals| HookBody {
        class: 0,
        property: 0,
        kind: HookKind::Get,
        body: code(literals),
    };
    let mut unit = published(&table);
    unit.hooks = vec![hook(vec![table.string])];
    assert!(frozen(unit), "the table's string in a hook's op array");
    let mut unit = published(&table);
    unit.hooks = vec![hook(vec![s])];
    assert!(!frozen(unit), "a request's string in a hook's op array");
}

#[test]
fn frozen_refuses_a_requests_value_as_a_classs_attribute() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    assert!(declaration(&table, |c| c.attributes =
        vec![attribute(held(table.array))].into()));
    assert!(!declaration(&table, |c| c.attributes = vec![attribute(held(s))].into()));
}

#[test]
fn frozen_refuses_a_requests_value_as_a_methods_attribute() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    assert!(!declaration(&table, |c| c.methods =
        vec![method(vec![attribute(held(s))])]));
}

/// A unit of the table's whose function `f` has `slot` for its one
/// parameter.
fn with_parameter(table: &Table, slot: Parameter) -> Unit {
    let mut unit = published(table);
    unit.functions[0].0.parameters = Parameters::from(vec![slot]);
    unit
}

#[test]
fn frozen_refuses_a_requests_value_as_a_parameters_default() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    let mut slot = parameter();
    slot.default = ParameterDefault::Expr(held(table.array));
    assert!(frozen(with_parameter(&table, slot.clone())));
    slot.default = ParameterDefault::Expr(held(s));
    assert!(!frozen(with_parameter(&table, slot)));
}

#[test]
fn frozen_refuses_a_requests_value_as_a_parameters_attribute() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    let mut slot = parameter();
    slot.attributes = vec![attribute(held(s))].into();
    assert!(!frozen(with_parameter(&table, slot)));
}

#[test]
fn frozen_refuses_a_requests_value_as_a_consts_value() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    let mut unit = published(&table);
    unit.constants[0].value = held(s);
    assert!(!frozen(unit), "a request's string as a `const`'s value");
    let mut unit = published(&table);
    unit.constants[0].attributes = vec![attribute(held(s))].into();
    assert!(!frozen(unit), "a request's string as a `const`'s attribute");
}

#[test]
fn frozen_refuses_a_requests_value_as_a_bound_variables_value() {
    let (table, mut heap) = (table(), Heap::new());
    let s = requests(&mut heap);
    let mut unit = published(&table);
    unit.dynamic[0].1.bound_vars = vec![BoundVar {
        name: Name::from("s"),
        value: s,
        lexical: false,
    }];
    assert!(
        !frozen(unit),
        "a request's string a bound variable starts with"
    );
}

/// A `static function () {}` node and a `strlen(...)` node, each holding
/// a share of `cell`, as a compile lays them (`UnitCell::share`).
fn closures(cell: &mut UnitCell) -> [ConstExpr; 2] {
    [
        ConstExpr::Closure {
            at: 0,
            name: Name::from("{closure:k.php:1}"),
            unit: cell.share(),
        },
        ConstExpr::FunctionCallable {
            name: Name::from("strlen"),
            name_kind: NameKind::NotFullyQualified,
            line: 1,
            at: 0,
            unit: cell.share(),
        },
    ]
}

/// Whether the table's class is frozen with `change` made to it for each
/// closure node holding a share of a unit's cell, each answer in turn.
fn with_cell(table: &Table, change: impl Fn(&mut ClassDecl, ConstExpr)) -> [bool; 2] {
    closures(&mut UnitCell::default()).map(|closure| declaration(table, |c| change(c, closure)))
}

#[test]
fn frozen_refuses_a_declaration_with_a_cell_in_a_class_constant() {
    let table = table();
    let refused = with_cell(&table, |c, closure| c.constants[0].value = closure);
    assert_eq!(refused, [false, false]);
}

#[test]
fn frozen_refuses_a_declaration_with_a_cell_in_a_propertys_default() {
    let table = table();
    let refused = with_cell(&table, |c, closure| {
        let mut p = property();
        p.default = Some(closure);
        c.properties = vec![p].into();
    });
    assert_eq!(refused, [false, false]);
}

#[test]
fn frozen_refuses_a_declaration_with_a_cell_in_an_attributes_argument() {
    let table = table();
    let refused = with_cell(&table, |c, closure| {
        c.attributes = vec![attribute(closure)].into();
    });
    assert_eq!(refused, [false, false]);
}

#[test]
fn frozen_refuses_a_declaration_with_a_cell_in_a_methods_parameter_default() {
    let table = table();
    let refused = with_cell(&table, |c, closure| {
        let mut slot = parameter();
        slot.default = ParameterDefault::Expr(closure);
        let mut m = method(Vec::new());
        m.function.parameters = Parameters::from(vec![slot]);
        c.methods = vec![m];
    });
    assert_eq!(refused, [false, false]);
}

/// A cell is refused by what it is, not by whether it is bound: one
/// bound by a publish is refused as one not yet bound is.
#[test]
fn frozen_refuses_a_declaration_with_a_cell_its_unit_bound() {
    let table = table();
    let mut unit = published(&table);
    let [closure, _] = closures(&mut unit.cell);
    unit.classes[0].constants[0].value = closure;
    let unit = Frozen::new(unit.published(None)).expect("a unit's own cell names it");
    let ConstExpr::Closure { unit: cell, .. } = &unit.classes[0].constants[0].value else {
        unreachable!("the closure laid above");
    };
    assert!(cell.unit().is_some(), "bound to the published unit");
    assert!(Frozen::new(unit.classes[0].clone()).is_err());
}

#[test]
fn frozen_admits_a_declaration_whose_expressions_hold_no_cell() {
    let table = table();
    assert!(declaration(&table, |_| {}), "no closure node");
    let closure = ConstExpr::Closure {
        at: 0,
        name: Name::from("{closure:k.php:1}"),
        unit: UnitCell::default(),
    };
    assert!(
        declaration(&table, |c| c.constants[0].value = closure),
        "a closure node whose cell is the empty one"
    );
}

#[test]
fn frozen_admits_a_unit_whose_class_holds_a_closure() {
    let table = table();
    for at in 0..2 {
        let mut unit = published(&table);
        let closure = closures(&mut unit.cell)[at].clone();
        unit.classes[0].constants[0].value = closure;
        assert_eq!(unit_values(&unit, &mut |_, _, _| {}), Ok(()));
        assert!(frozen(unit), "the cell names the unit walked");
    }
}
