//! A corpus reading's scope, `<subset>[@control][@<lane>][@against]`:
//! the subset, and what the run was beside it. The writers render it here
//! and every reader parses it here, so `status`, `done`'s stamp and the
//! comparisons all read one subset off `X@control@against` (#1744).
//! `compile-identity` writes its control's scope through it as well, the
//! same grammar over a corpus of its own.

use std::fmt;
use std::str::FromStr;

use anyhow::{Error, bail};
use clap::ValueEnum;

use super::Lane;

/// What separates the subset from each suffix after it.
const AT: char = '@';

/// One reading's scope, decoded once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The subset: a crate's name or a directory of the corpus.
    pub subset: String,
    /// The run was the runner's control, the oracle in the engine's place,
    /// so it keeps a reading of its own and never displaces the engine's.
    pub control: bool,
    /// The compiled tier's lane the run was under (jit-design.md §7).
    pub lane: Option<Lane>,
    /// The comparison of the run against a kept commit's (#664).
    pub against: bool,
}

impl Scope {
    /// The word of the control's suffix, and of its column in `status`.
    pub(crate) const CONTROL: &str = "control";
    /// The word of the comparison's suffix, and of its column in `status`.
    pub(crate) const AGAINST: &str = "against";

    /// The engine's own run over `subset`, beside nothing.
    pub(crate) fn of(subset: &str) -> Scope {
        Scope {
            subset: subset.to_owned(),
            control: false,
            lane: None,
            against: false,
        }
    }

    /// The same run's comparison against a kept commit's.
    pub(crate) fn held_against(&self) -> Scope {
        Scope {
            against: true,
            ..self.clone()
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.subset)?;
        if self.control {
            write!(f, "{AT}{}", Scope::CONTROL)?;
        }
        if let Some(lane) = self.lane {
            write!(f, "{AT}{}", lane.as_str())?;
        }
        if self.against {
            write!(f, "{AT}{}", Scope::AGAINST)?;
        }
        Ok(())
    }
}

/// Where a suffix stands in the grammar; each comes after the one before
/// it, at most once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    Control,
    Lane,
    Against,
}

impl FromStr for Scope {
    type Err = Error;

    /// The scope `text` spells, refused where a suffix is not one the
    /// grammar has, or stands out of its order or twice.
    fn from_str(text: &str) -> Result<Scope, Error> {
        let mut parts = text.split(AT);
        let subset = parts.next().unwrap_or_default();
        if subset.is_empty() {
            bail!("the scope {text:?} names no subset before its first `{AT}`");
        }
        let mut scope = Scope::of(subset);
        let mut last = None;
        for suffix in parts {
            let lane = Lane::value_variants()
                .iter()
                .copied()
                .find(|lane| lane.as_str() == suffix);
            let place = match (suffix, lane) {
                (Scope::CONTROL, _) => Place::Control,
                (_, Some(_)) => Place::Lane,
                (Scope::AGAINST, _) => Place::Against,
                _ => bail!(
                    "the scope {text:?} carries `{AT}{suffix}`, which is no control, lane or comparison"
                ),
            };
            if last.is_some_and(|last| last >= place) {
                bail!(
                    "the scope {text:?} carries `{AT}{suffix}` out of the order `<subset>[{AT}control][{AT}<lane>][{AT}against]`"
                );
            }
            last = Some(place);
            match place {
                Place::Control => scope.control = true,
                Place::Lane => scope.lane = lane,
                Place::Against => scope.against = true,
            }
        }
        Ok(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::{Lane, Scope};

    /// Every scope the writers spell reads back as the parts that spelled
    /// it, and renders as the same bytes (#1744).
    #[test]
    fn a_scope_round_trips_through_its_parts() {
        let cases = [
            ("ext/json", "ext/json", false, None, false),
            ("Zend/tests@control", "Zend/tests", true, None, false),
            ("sapi/cli@against", "sapi/cli", false, None, true),
            ("X@control@against", "X", true, None, true),
            (
                "ext/pcre@force",
                "ext/pcre",
                false,
                Some(Lane::Force),
                false,
            ),
            (
                "ext/pcre@exit-fuzz",
                "ext/pcre",
                false,
                Some(Lane::ExitFuzz),
                false,
            ),
            (
                "ext/pcre@force@against",
                "ext/pcre",
                false,
                Some(Lane::Force),
                true,
            ),
            (
                "ext/pcre@control@exit-fuzz",
                "ext/pcre",
                true,
                Some(Lane::ExitFuzz),
                false,
            ),
        ];
        for (text, subset, control, lane, against) in cases {
            let scope: Scope = text.parse().unwrap();
            assert_eq!(
                scope,
                Scope {
                    subset: subset.to_owned(),
                    control,
                    lane,
                    against,
                },
                "{text}"
            );
            assert_eq!(scope.to_string(), text);
        }
    }

    /// A suffix the grammar has not, one out of its order and one twice
    /// are each refused by name, rather than read as part of a subset.
    #[test]
    fn a_scope_outside_the_grammar_is_refused() {
        for text in [
            "ext/pcre@jit",
            "X@against@control",
            "X@force@control",
            "X@control@control",
            "@control",
        ] {
            let refused = text.parse::<Scope>().unwrap_err().to_string();
            assert!(refused.contains(text), "{text}: {refused}");
        }
    }

    /// The comparison of a run is the run's scope with `@against` last.
    #[test]
    fn a_comparison_is_its_run_held_against() {
        let run: Scope = "ext/pcre@force".parse().unwrap();
        assert_eq!(run.held_against().to_string(), "ext/pcre@force@against");
    }
}
