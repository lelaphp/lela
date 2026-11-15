//! A corpus reading's scope,
//! `<subset>[@control][@<lane>][@regime=<name>][@against]`: the subset,
//! and what the run was beside it. The writers render it here and every
//! reader parses it here, so `status`, `done`'s stamp and the comparisons
//! all read one subset off `X@control@against`.
//! `compile-identity` writes its control's scope through it as well, the
//! same grammar over a corpus of its own, and the regime it dumped under
//! beside `dump` where it was asked for one.

use std::fmt;
use std::str::FromStr;

use anyhow::{Error, bail};
use clap::ValueEnum;

use super::Lane;

/// What separates the subset from each suffix after it.
const AT: char = '@';
/// What a regime's suffix opens with, ahead of the pin's name for it.
const REGIME: &str = "regime=";

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
    /// The `[regime.<name>]` of the pin the run stood under beside the
    /// lane's own, by name.
    pub regime: Option<String>,
    /// The comparison of the run against a kept commit's.
    pub against: bool,
}

/// What a scope is beside the run it compares: the comparison against a
/// kept commit, or the runner's control under the oracle. `status` folds
/// each into the run's row as the column its word names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comparison {
    Against,
    Control,
}

impl Comparison {
    pub(crate) const ALL: [Comparison; 2] = [Comparison::Against, Comparison::Control];

    /// The word of the suffix, and of the column.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Comparison::Against => "against",
            Comparison::Control => "control",
        }
    }
}

impl Scope {
    /// The engine's own run over `subset`, beside nothing.
    pub(crate) fn of(subset: &str) -> Scope {
        Scope {
            subset: subset.to_owned(),
            control: false,
            lane: None,
            regime: None,
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

    /// The run this scope compares, and how: `@against` compares the scope
    /// without it, and `@control`, where nothing is held against, the
    /// engine's run of the same subset and lane. None for a run of its own.
    pub(crate) fn compares(&self) -> Option<(Scope, Comparison)> {
        if self.against {
            let run = Scope {
                against: false,
                ..self.clone()
            };
            Some((run, Comparison::Against))
        } else if self.control {
            let run = Scope {
                control: false,
                ..self.clone()
            };
            Some((run, Comparison::Control))
        } else {
            None
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.subset)?;
        if self.control {
            write!(f, "{AT}{}", Comparison::Control.as_str())?;
        }
        if let Some(lane) = self.lane {
            write!(f, "{AT}{}", lane.as_str())?;
        }
        if let Some(regime) = &self.regime {
            write!(f, "{AT}{REGIME}{regime}")?;
        }
        if self.against {
            write!(f, "{AT}{}", Comparison::Against.as_str())?;
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
    Regime,
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
            let regime = suffix.strip_prefix(REGIME).filter(|name| !name.is_empty());
            let place = match (lane, regime) {
                _ if suffix == Comparison::Control.as_str() => Place::Control,
                (Some(_), _) => Place::Lane,
                (None, Some(_)) => Place::Regime,
                (None, None) if suffix == Comparison::Against.as_str() => Place::Against,
                _ => bail!(
                    "the scope {text:?} carries `{AT}{suffix}`, which is no control, lane, regime or comparison"
                ),
            };
            if last.is_some_and(|last| last >= place) {
                bail!(
                    "the scope {text:?} carries `{AT}{suffix}` out of the order `<subset>[{AT}control][{AT}<lane>][{AT}{REGIME}<name>][{AT}against]`"
                );
            }
            last = Some(place);
            match place {
                Place::Control => scope.control = true,
                Place::Lane => scope.lane = lane,
                Place::Regime => scope.regime = regime.map(str::to_owned),
                Place::Against => scope.against = true,
            }
        }
        Ok(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::{Comparison, Lane, Scope};

    /// Every scope the writers spell reads back as the parts that spelled
    /// it, and renders as the same bytes.
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
                    regime: None,
                    against,
                },
                "{text}"
            );
            assert_eq!(scope.to_string(), text);
        }
    }

    /// `compile-identity` under a regime beside `dump` keeps a reading of
    /// its own, its control's too, and reads back as the parts that
    /// spelled it.
    #[test]
    fn a_regime_is_a_suffix_of_its_own_after_the_lane() {
        for (text, control) in [
            ("php-src@regime=extended-info", false),
            ("php-src@control@regime=extended-info", true),
        ] {
            let scope: Scope = text.parse().unwrap();
            assert_eq!(scope.subset, "php-src");
            assert_eq!(scope.control, control);
            assert_eq!(scope.regime.as_deref(), Some("extended-info"));
            assert_eq!(scope.to_string(), text);
        }
        for text in ["X@regime=", "X@regime=a@force", "X@regime=a@regime=b"] {
            let refused = text.parse::<Scope>().unwrap_err().to_string();
            assert!(refused.contains(text), "{text}: {refused}");
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

    /// The comparison of a run is the run's scope with `@against` last,
    /// and what a scope compares is read back off the same suffixes: the
    /// comparison first, then the control, a lane kept either way.
    #[test]
    fn a_comparison_is_its_run_held_against() {
        let run: Scope = "ext/pcre@force".parse().unwrap();
        assert_eq!(run.held_against().to_string(), "ext/pcre@force@against");
        let compares = |text: &str| {
            let scope: Scope = text.parse().unwrap();
            scope.compares().map(|(run, how)| (run.to_string(), how))
        };
        assert_eq!(compares("ext/pcre"), None);
        assert_eq!(compares("ext/pcre@force"), None);
        assert_eq!(
            compares("ext/pcre@force@against"),
            Some(("ext/pcre@force".to_owned(), Comparison::Against))
        );
        assert_eq!(
            compares("ext/pcre@control@force"),
            Some(("ext/pcre@force".to_owned(), Comparison::Control))
        );
        assert_eq!(
            compares("X@control@against"),
            Some(("X@control".to_owned(), Comparison::Against))
        );
    }
}
