//! The recorder (`jit-design.md` §4.1), so far where a recording starts,
//! [`Start`], and why one stops, [`Stop`], which `policy` thresholds and
//! settles on and the dump names in php's words. The loop over `step`,
//! `TraceRecord` and `Recorded` land with it (#2951), extending these
//! rather than redefining them.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "nothing records until the recorder lands (#2951)")
)]

use crate::table::TraceId;
use crate::types::ExitId;

/// Where a recording starts: php's `ZEND_JIT_TRACE_START_LOOP`, `_ENTER`,
/// `_RETURN` and `_SIDE` (`Z/ext/opcache/jit/zend_jit_internal.h:317-320`),
/// a side trace naming the trace and exit it grows from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    Loop,
    Enter,
    Return,
    Side { parent: TraceId, exit: ExitId },
}

impl Start {
    /// `zend_jit_trace_star_desc`, the word a root's `TRACE start` line
    /// names it by; a side trace's line names its parent instead.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Start::Loop => "loop",
            Start::Enter => "enter",
            Start::Return => "return",
            Start::Side { .. } => "side",
        }
    }
}

/// Why a recording or a compile stopped: php's `ZEND_JIT_TRACE_STOP`
/// list (`Z/ext/opcache/jit/zend_jit_internal.h:261-293`) with the two of
/// this engine's own, `MovedFrame` and `RequestUnit` (`jit-design.md`
/// §4.1). The first nine end a trace that compiles; the rest are
/// failures, and from `TooManyTraces` on php blacklists at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Loop,
    RecursiveCall,
    RecursiveRet,
    Return,
    Link(TraceId),
    Interpreter,
    Trampoline,
    PropHookCall,
    BadFunc,
    NotSupported,
    Exception,
    TooLong,
    TooDeep,
    TooDeepRet,
    DeepRecursion,
    LoopUnroll,
    LoopExit,
    RecursionExit,
    InnerLoop,
    CompiledLoop,
    CompilerError,
    MovedFrame,
    RequestUnit,
    TooManyTraces,
    TooManyChildren,
    TooManyExits,
}

impl Stop {
    /// `zend_jit_trace_stop_description`, the words a `TRACE stop` or
    /// `abort` line gives the stop in; this engine's two in the same
    /// voice. A link is named by its trace, which the line writes itself.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Stop::Loop => "loop",
            Stop::RecursiveCall => "recursive call",
            Stop::RecursiveRet => "recursive return",
            Stop::Return => "return",
            Stop::Link(_) => "link to another trace",
            Stop::Interpreter => "exit to VM interpreter",
            Stop::Trampoline => "trampoline call",
            Stop::PropHookCall => "property hook call",
            Stop::BadFunc => "bad function call",
            Stop::NotSupported => "not supported instructions",
            Stop::Exception => "exception",
            Stop::TooLong => "trace too long",
            Stop::TooDeep => "trace too deep",
            Stop::TooDeepRet => "trace too deep return",
            Stop::DeepRecursion => "deep recursion",
            Stop::LoopUnroll => "loop unroll limit reached",
            Stop::LoopExit => "exit from loop",
            Stop::RecursionExit => "return from recursive function",
            Stop::InnerLoop => "inner loop",
            Stop::CompiledLoop => "compiled loop",
            Stop::CompilerError => "JIT compilation error",
            Stop::MovedFrame => "frame moved to the heap",
            Stop::RequestUnit => "call into a request-scoped op array",
            Stop::TooManyTraces => "too many traces",
            Stop::TooManyChildren => "too many side traces",
            Stop::TooManyExits => "too many side exits",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Start, Stop};
    use crate::table::TraceId;
    use crate::types::ExitId;

    /// Every stop in php's order with php's words, and the engine's two
    /// after `COMPILER_ERROR`, where they recover as php's failures do.
    #[test]
    fn every_stop_is_named_in_phps_words() {
        let link = TraceId::new(2).map_or(Stop::Loop, Stop::Link);
        let named: Vec<&str> = [
            Stop::Loop,
            Stop::RecursiveCall,
            Stop::RecursiveRet,
            Stop::Return,
            link,
            Stop::Interpreter,
            Stop::Trampoline,
            Stop::PropHookCall,
            Stop::BadFunc,
            Stop::NotSupported,
            Stop::Exception,
            Stop::TooLong,
            Stop::TooDeep,
            Stop::TooDeepRet,
            Stop::DeepRecursion,
            Stop::LoopUnroll,
            Stop::LoopExit,
            Stop::RecursionExit,
            Stop::InnerLoop,
            Stop::CompiledLoop,
            Stop::CompilerError,
            Stop::MovedFrame,
            Stop::RequestUnit,
            Stop::TooManyTraces,
            Stop::TooManyChildren,
            Stop::TooManyExits,
        ]
        .into_iter()
        .map(Stop::describe)
        .collect();
        assert_eq!(named[4], "link to another trace");
        assert_eq!(named[20], "JIT compilation error");
        assert_eq!(named[25], "too many side exits");
        let starts = [Start::Loop, Start::Enter, Start::Return];
        let side = TraceId::new(1).map(|parent| Start::Side {
            parent,
            exit: ExitId(0),
        });
        let words: Vec<&str> = starts
            .into_iter()
            .chain(side)
            .map(Start::describe)
            .collect();
        assert_eq!(words, ["loop", "enter", "return", "side"]);
    }
}
