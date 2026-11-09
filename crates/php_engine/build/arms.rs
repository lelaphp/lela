//! Where each variant's arm sits in the loop (design §5.8): the seven
//! lists below, folded by [`place`] into one [`Placement`] per variant,
//! which every point of the dispatch's generation reads and none of the
//! lists, and held to each other and to the variants the dispatch serves
//! by [`held`]. The lists stay the input; what a placement says follows
//! from them alone.
//!
//! One file for `php_engine`'s `build.rs` and its test `tests/arms.rs`,
//! which asks the placement of one variant of each kind.

/// Where one variant's arm sits, and so what the loop's arm for it
/// serves: [`place`]'s answer, one per variant the dispatch serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Out of the loop's body: the handler is called out of line, through
    /// the dispatch's `aside`, and the loop reads its answer where the
    /// arms meet. Every variant no list places in the loop, and one of
    /// [`PATH_IN_THE_LOOP`] whose handler has no inline path.
    Cold,
    /// The handler's inline path alone in the loop's body
    /// ([`PATH_IN_THE_LOOP`]): a hit goes on as the handler's answer
    /// would, and a miss calls the handler out of line, as [`Cold`] does.
    ///
    /// [`Cold`]: Placement::Cold
    PathInTheLoop(InlinePath),
    /// In the loop's body ([`IN_THE_LOOP`]): the handler is called there,
    /// first by its inline path where it has one ([`INLINE_PATHS`]), and
    /// its answer is read as `answer` says.
    InTheLoop {
        path: Option<InlinePath>,
        answer: Answer,
    },
}

impl Placement {
    /// Whether the handler itself is called in the loop's body, not out of
    /// line.
    pub fn in_the_loop(self) -> bool {
        matches!(self, Placement::InTheLoop { .. })
    }
}

/// An inline path the loop's arm serves first: a function of
/// `crate::handlers::inline_path`, one name of [`INLINE_PATHS`], and what
/// it reads beside the frame's slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlinePath {
    pub name: &'static str,
    pub reads: Reads,
}

/// What an inline path reads beside the frame's slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reads {
    /// The slots alone.
    Slots,
    /// The `ASSIGN` after its opline, which it serves too, unseen, so an
    /// engine whose observers declared `ASSIGN` serves the opline through
    /// its handler instead ([`ASSIGNED`]).
    Assign,
    /// The call being built, php's `EX(call)`, taken up with the slots
    /// from one look at the frame table ([`CALL_PATHS`]).
    Call,
}

/// How an arm in the loop's body reads its handler's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// As the loop reads every arm's, where the arms meet.
    Cold,
    /// As a way, in the arm: a call edge's ([`WAY_IN_THE_ARM`]).
    Way,
    /// The push first, whether the handler's `INIT` flags the callee and
    /// php's name of the opcode, then as a way ([`ENTERS`]).
    Pushed { flagged: bool, opcode: &'static str },
}

/// The placement of `variant`, served by the handler `handler` (its
/// identifier, `r#` kept), `assigned` where it is the variant fused to
/// the `ASSIGN` after it ([`ASSIGNED`]), whose inline path is the
/// handler's `_assign_cv` one.
pub fn place(variant: &str, handler: &str, assigned: bool) -> Placement {
    let path = || {
        let name = if assigned {
            format!("{handler}_assign_cv")
        } else {
            handler.to_owned()
        };
        let &name = INLINE_PATHS.iter().find(|path| **path == name)?;
        let reads = if assigned {
            Reads::Assign
        } else if CALL_PATHS.contains(&name) {
            Reads::Call
        } else {
            Reads::Slots
        };
        Some(InlinePath { name, reads })
    };
    if IN_THE_LOOP.contains(&variant) {
        let answer = match ENTERS.iter().find(|(name, ..)| *name == variant) {
            Some(&(_, flagged, opcode)) => Answer::Pushed { flagged, opcode },
            None if WAY_IN_THE_ARM.contains(&variant) => Answer::Way,
            None => Answer::Cold,
        };
        return Placement::InTheLoop {
            path: path(),
            answer,
        };
    }
    match path() {
        Some(path) if PATH_IN_THE_LOOP.contains(&variant) => Placement::PathInTheLoop(path),
        _ => Placement::Cold,
    }
}

/// Whether `php_unit` resolves the opcode `opcode`, php's name, over
/// whether the opline after it is the `ASSIGN` of its temporary to a CV,
/// so that it has a fused variant beside each of its own ([`ASSIGNED`]).
pub fn fuses(opcode: &str) -> bool {
    ASSIGNED.contains(&opcode)
}

/// The lists held to each other and to `served`, every variant a handler
/// of `bodies` serves: a name of [`IN_THE_LOOP`] is none of
/// [`PATH_IN_THE_LOOP`], a name of [`ENTERS`] is one of
/// [`WAY_IN_THE_ARM`] and a name of that one of [`IN_THE_LOOP`], and a
/// name of either list that places a variant in the loop's body is a
/// variant in `served`. Each is a build error.
pub fn held(served: &[&str], bodies: &std::path::Path) -> Result<(), String> {
    if let Some(twice) = PATH_IN_THE_LOOP
        .iter()
        .find(|name| IN_THE_LOOP.contains(name))
    {
        return Err(format!(
            "`{twice}` is named by both `IN_THE_LOOP` and `PATH_IN_THE_LOOP`, which say where its handler is called two ways"
        ));
    }
    if let Some((stray, ..)) = ENTERS
        .iter()
        .find(|(name, ..)| !WAY_IN_THE_ARM.contains(name))
    {
        return Err(format!(
            "`{stray}` is named by `ENTERS` and not by `WAY_IN_THE_ARM`, whose arms alone read the push"
        ));
    }
    if let Some(stray) = WAY_IN_THE_ARM
        .iter()
        .find(|name| !IN_THE_LOOP.contains(name))
    {
        return Err(format!(
            "`{stray}` is named by `WAY_IN_THE_ARM` and not by `IN_THE_LOOP`, whose arms alone are in the loop's body"
        ));
    }
    let lists = [
        ("IN_THE_LOOP", IN_THE_LOOP),
        ("PATH_IN_THE_LOOP", PATH_IN_THE_LOOP),
    ];
    for (list, names) in lists {
        if let Some(stray) = names.iter().find(|name| !served.contains(name)) {
            return Err(format!(
                "`{stray}` is in the loop's body by `{list}` and is no variant a handler of {} serves",
                bodies.display()
            ));
        }
    }
    Ok(())
}

/// The arithmetic `php_unit` resolves over whether the opline after it
/// is the `ASSIGN` of its temporary to a CV, which its `build.rs` names
/// in the same list: each such variant's call is its unfused one's, so
/// that `dispatch`, which a stepping host and the tier's recorder serve
/// an opline through, serves the arithmetic alone and the `ASSIGN` after
/// it as php does, and the loop's arm serves both by the fused inline
/// path where [`IN_THE_LOOP`] names the variant ([`Reads::Assign`]).
const ASSIGNED: [&str; 4] = ["ADD", "SUB", "MUL", "DIV"];

/// The variants whose arms are in the loop's body (design §5.8): every
/// variant met in more than a quarter of a percent of the oplines
/// dispatched by any program of `docs/perf/workloads`, by php-src's
/// `Zend/micro_bench.php` or by the four counting loops of #3273, as
/// a counting build of the loop told them over each (#3285). The arm of
/// every other variant calls its handler out of line, through a call the
/// optimiser reads as cold, so that the loop's body is the arms programs
/// meet and the register allocator, which weighs every arm of the
/// `match` alike, places the loop's values for them: with all 526
/// generic and `#[inline(always)]` arms in its body, `serve_frame` was
/// 270 KB and the arms programs meet paid for the ones they do not.
/// A name that is no variant the dispatch serves is a build error.
const IN_THE_LOOP: &[&str] = &[
    "AddArrayElement",
    "AddConstVar",
    "AddCvConst",
    "AddCvCv",
    "AddCvCvAssignCv",
    "AddTmpConst",
    "AddTmpCv",
    "AddTmpCvAssignCv",
    "AddTmpTmp",
    "AddTmpTmpAssignCv",
    "AddTmpVar",
    "AddVarVar",
    "ArrayKeyExists",
    "AssignCvConstRetvalUnused",
    "AssignCvCvRetvalUnused",
    "AssignCvTmpRetvalUnused",
    "AssignCvVarRetvalUnused",
    "AssignDimCvConstOpDataTmp",
    "AssignDimCvConstOpDataVar",
    "AssignDimCvCvOpDataConst",
    "AssignDimCvCvOpDataCv",
    "AssignDimCvCvOpDataTmp",
    "AssignDimCvTmpOpDataCv",
    "AssignDimOpCvCv",
    "AssignObjOp",
    "AssignObjUnusedConstOpDataConst",
    "AssignOpCvConst",
    "AssignOpCvCv",
    "AssignOpCvTmp",
    "AssignOpCvVar",
    "AssignStaticProp",
    "Bool",
    "BoolNot",
    "BwAnd",
    "Cast",
    "CheckFuncArg",
    "Coalesce",
    "ConcatConstTmp",
    "ConcatCvCv",
    "ConcatTmpConst",
    "ConcatTmpTmp",
    "Div",
    "DivAssignCv",
    "DoFcall",
    "DoFcallByName",
    "DoIcall",
    "DoUcall",
    "Echo",
    "FeFetchRVar",
    "FeFree",
    "FeResetR",
    "FetchClass",
    "FetchClassConstant",
    "FetchConstant",
    "FetchDimFuncArgCvConst",
    "FetchDimIs",
    "FetchDimRCvConst",
    "FetchDimRCvCv",
    "FetchDimRCvTmp",
    "FetchDimRTmpCv",
    "FetchDimRwCvCv",
    "FetchObjRCvConst",
    "FetchObjRUnusedConst",
    "FetchR",
    "FetchStaticPropR",
    "FetchThis",
    "FramelessIcall1",
    "FramelessIcall2",
    "FramelessIcall3",
    "Free",
    "FuncNumArgs",
    "InArray",
    "InitArray",
    "InitDynamicCall",
    "InitFcallByName",
    "InitFcallConst",
    "InitMethodCall",
    "InitStaticMethodCall",
    "IsEqualCvConstJmpz",
    "IsIdenticalCvConst",
    "IsIdenticalCvCv",
    "IsIdenticalTmpConst",
    "IsNotIdentical",
    "IssetIsemptyPropObj",
    "IssetIsemptyStaticProp",
    "IsSmallerConstCv",
    "IsSmallerConstTmpJmpz",
    "IsSmallerCvConst",
    "IsSmallerCvConstJmpnzBack",
    "IsSmallerCvConstJmpz",
    "IsSmallerCvCvJmpnzBack",
    "IsSmallerCvCvJmpz",
    "IsSmallerCvTmpJmpz",
    "IsSmallerOrEqualJmpnzBack",
    "IsSmallerTmpConst",
    "Jmp",
    "JmpnzConst",
    "JmpnzEx",
    "JmpnzTmp",
    "JmpSet",
    "JmpzCv",
    "JmpzEx",
    "JmpzTmp",
    "Mod",
    "MulCvConst",
    "MulCvCv",
    "MulCvCvAssignCv",
    "MulCvTmp",
    "MulCvTmpAssignCv",
    "MulCvVar",
    "MulTmpConst",
    "MulTmpTmp",
    "MulVarTmp",
    "New",
    "PreDecCvRetvalUnused",
    "PreDecObj",
    "PreDecVarRetvalUnused",
    "PreIncCvRetvalUnused",
    "PreIncObj",
    "QmAssign",
    "ReturnConst",
    "ReturnCv",
    "ReturnTmp",
    "RopeAddTmpConst",
    "RopeAddTmpCv",
    "RopeEndTmpConst",
    "RopeInitUnusedConst",
    "SendFuncArg",
    "SendRef",
    "SendValConstUnused",
    "SendValExConstUnused",
    "SendValExTmpUnused",
    "SendValTmpUnused",
    "SendVarCvUnused",
    "SendVarEx",
    "Strlen",
    "SubCvConst",
    "SubCvCv",
    "SubTmpConst",
    "SubTmpTmp",
    "SubTmpTmpAssignCv",
    "TypeCheck",
];

/// The variants whose arms serve their handler's inline path in the
/// loop's body and leave it as an arm outside [`IN_THE_LOOP`] does: a
/// miss calls the handler out of line, and a hit lets the slots go and
/// goes on as such a call's answer does, the slots taken up again for
/// the frame ([`Placement::PathInTheLoop`]). So the path is one test in
/// the loop, and the loop's own way on from an inline path, which the
/// optimiser copies into the arms that take it while few enough do, gains
/// no arm. `VERIFY_RETURN_TYPE`'s mask test taking that way stopped those
/// copies (48 to 7 in `serve_frame`) and read fannkuch, which serves it
/// once, at 1.08 of the base's cycles; taking this one, 0.99 (#3978). A
/// name here is no name of `IN_THE_LOOP`, and one that is no variant the
/// dispatch serves is a build error.
const PATH_IN_THE_LOOP: &[&str] = &["VerifyReturnType"];

/// The variants of the call edges, whose arms read their handler's answer
/// as a way in the arm ([`Answer::Way`]): a call's push and a return's
/// pop, php's `ZEND_VM_ENTER` and `ZEND_VM_LEAVE`. The way is a constant
/// of each path through the handler, so the loop goes from the push or
/// the pop straight to its take-up of the callee or the caller, as php's
/// handler goes straight to the callee's or the caller's next opline;
/// read where the arms meet, the answer was one word of every arm, made
/// and searched again (#3995). A name here is a name of [`IN_THE_LOOP`],
/// and one that is not is a build error.
const WAY_IN_THE_ARM: &[&str] = &[
    "DoFcall",
    "DoFcallByName",
    "DoUcall",
    "ReturnConst",
    "ReturnCv",
    "ReturnTmp",
];

/// The calls of [`WAY_IN_THE_ARM`] whose arm serves the push first,
/// `crate::handlers::inline_path::enter`, each with whether its handler
/// enters a callee whose `INIT` flagged it and php's name of its opcode:
/// a handler that answers the push or another edge on its two paths makes
/// one answer of them where it returns, so its arm reads the push apart,
/// as an inline path, and serves what the push stands aside for past it
/// (`crate::handlers::inline_path::past_the_push`), the handler's own two
/// halves, so no call is pushed twice.
const ENTERS: &[(&str, bool, &str)] = &[
    ("DoFcall", true, "ZEND_DO_FCALL"),
    ("DoFcallByName", true, "ZEND_DO_FCALL_BY_NAME"),
    ("DoUcall", false, "ZEND_DO_UCALL"),
];

/// The handlers with an inline path, a function of
/// `crate::handlers::inline_path` named as the handler and generic over
/// what it is generic over but its tier, which the loop serves over the
/// slots it holds ([`InlinePath`]). A name here that has no such function
/// is a build error of the crate, an unresolved path; a function there
/// that no name here reaches is one only where its handler does not
/// serve it too, an unused item (`div`, `jmp`), so a name left out of
/// the list leaves every other path served by its handler alone.
const INLINE_PATHS: &[&str] = &[
    "add",
    "add_assign_cv",
    "assign",
    "assign_dim",
    "assign_dim_op",
    "assign_op",
    "check_func_arg",
    "div",
    "div_assign_cv",
    "fetch_dim_func_arg",
    "fetch_dim_r",
    "is_equal",
    "is_identical",
    "is_not_equal",
    "is_not_identical",
    "is_smaller",
    "is_smaller_or_equal",
    "jmp",
    "jmpnz",
    "jmpnz_ex",
    "jmpz",
    "jmpz_ex",
    "mul",
    "mul_assign_cv",
    "pre_dec",
    "pre_inc",
    "qm_assign",
    "send_func_arg",
    "send_val",
    "send_val_ex",
    "send_var",
    "send_var_ex",
    "sub",
    "sub_assign_cv",
    "type_check",
    "verify_return_type",
];

/// The inline paths of [`INLINE_PATHS`] that read and write the call
/// being built as well as the frame's slots, php's `EX(call)`: each takes
/// the call after the slots (`&mut crate::exec::HeapFrame`), which its
/// arm takes up with the slots from one look at the frame table
/// (`Request::slots_and_call`) and lets go at the arm's end, so the call
/// an arm is handed is the one on top of the stack whatever `INIT_*`
/// pushed or `DO_*CALL` popped before it. Held across the loop's arms
/// instead, refreshed where an arm took the request, it put the frame's
/// number out of its register: single-stepped, `DO_UCALL` 103 to 111,
/// `RETURN` 128 to 133 and `INIT_FCALL` 101 to 105 instructions (#4099).
/// A name here that is not in [`INLINE_PATHS`] is served by its handler
/// alone.
const CALL_PATHS: &[&str] = &[
    "check_func_arg",
    "fetch_dim_func_arg",
    "send_func_arg",
    "send_val",
    "send_val_ex",
    "send_var",
    "send_var_ex",
];
