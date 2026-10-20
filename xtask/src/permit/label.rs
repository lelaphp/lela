//! The `exempt_label` qualifier's sensor: whether every issue the call
//! names carries a label the rule stands down for (#341).
//!
//! `ticket.close` denies an agent `gh issue close`, so that nothing closes
//! a ticket but the instrument that read its acceptance criteria. A chart
//! sheet carries none: it is a container, its continuation is the decision
//! #266 settled, and closing a filled one is the bookkeeping the frontier
//! skill spells out step by step (CONTEXT.md, Chart). So the rule's row
//! names the label it does not judge, `frontier:chart`, and this reads it.
//!
//! The label is the tracker's fact and nothing in the tree carries it, so
//! the sensor asks the tracker, through [`Gh`], the one program the ticket
//! instrument reaches it by. It asks only about a call the predicate has
//! already fired on, which is a call that is otherwise denied, and only
//! about the issues named in the simple command that fired: the numbers
//! standing in it as words of their own, so the `--comment` a sheet's
//! close carries is one quoted word and names nothing. Every answer but
//! "this issue carries the label" leaves the denial standing — a call that
//! names no issue, a number the tracker cannot read, a tracker that is not
//! there — since an exemption that cannot be established does not apply.

use std::path::Path;

use super::argv::Argv;
use crate::manifest::Rule;
use crate::ticket::Gh;
use crate::track::Ticket;

/// Whether the rule stands down for this call: it carries an
/// `exempt_label`, the call names issues, and every one of them carries
/// one of those labels on the tracker.
pub(super) fn exempted(root: &Path, rule: &Rule, commands: &[Argv]) -> bool {
    if rule.exempt_label.is_empty() {
        return false;
    }
    let named: Vec<Ticket> = commands
        .iter()
        .filter(|argv| the_act(rule, argv))
        .flat_map(|argv| argv.words().iter().filter_map(|t| issue(t)))
        .collect();
    if named.is_empty() {
        return false;
    }
    let gh = Gh::on_path();
    named.into_iter().all(|ticket| {
        gh.view(root, ticket).is_ok_and(|issue| {
            issue
                .labels
                .iter()
                .any(|carried| rule.exempt_label.contains(&carried.name))
        })
    })
}

/// Whether this simple command is the act the rule denies: the rule's
/// `with` phrase names it, and one of its `deny_argv` elements stands in
/// it where it has them, which is what [`super::rules::judge`] fired on.
fn the_act(rule: &Rule, argv: &Argv) -> bool {
    let p = &rule.predicate;
    match &p.deny_argv {
        Some(denied) => argv.denies(denied, &p.with, p.unless_argv.as_ref()),
        None => argv.invokes(&p.with),
    }
}

/// The issue a word names: a number, with an optional `#`.
fn issue(word: &str) -> Option<Ticket> {
    word.strip_prefix('#')
        .unwrap_or(word)
        .parse::<u64>()
        .ok()
        .map(Ticket::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{OneOrMany, Predicates};
    use crate::permit::argv::commands;

    fn rule() -> Rule {
        let mut rule = Rule::named("ticket.close", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Bash".to_owned());
        rule.exempt_label = vec!["frontier:chart".to_owned()];
        rule.predicate = Predicates {
            deny_argv: Some(vec!["close".to_owned(), "reopen".to_owned()]),
            with: OneOrMany::One("gh issue".to_owned()),
            ..Predicates::default()
        };
        rule
    }

    /// The issues a call names are the numbers of the command that fired,
    /// so the sheet's number is read and the comment beside it is not; a
    /// neighbour command that names another issue is not the act.
    #[test]
    fn the_issues_named_are_the_numbers_of_the_command_that_fired() {
        let rule = rule();
        let named = |line: &str| -> Vec<u64> {
            commands(line)
                .iter()
                .filter(|argv| the_act(&rule, argv))
                .flat_map(|argv| argv.words().iter().filter_map(|t| issue(t)))
                .map(Ticket::number)
                .collect()
        };
        assert_eq!(
            named("gh issue close 274 --comment 'sheet 7 continues 274'"),
            vec![274]
        );
        assert_eq!(
            named("gh issue view 28 --json labels && gh issue close 460"),
            vec![460]
        );
        assert_eq!(named("gh issue reopen '#274'"), vec![274]);
        // Unquoted, `#` starts a comment and the command names no issue.
        assert_eq!(named("gh issue reopen #274"), Vec::<u64>::new());
        assert_eq!(named("gh issue close"), Vec::<u64>::new());
        assert_eq!(named("cargo xtask ticket close 341"), Vec::<u64>::new());
    }

    /// A rule that carries no `exempt_label` asks the tracker nothing,
    /// which is every rule of the manifest but one.
    #[test]
    fn a_rule_without_the_key_stands_down_for_nothing() {
        let mut rule = rule();
        rule.exempt_label = Vec::new();
        assert!(!exempted(
            Path::new("."),
            &rule,
            &commands("gh issue close 274")
        ));
    }
}
