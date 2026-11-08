//! `xtask ticket brief <n>`: the brief a spawned builder starts with
//! (#4263; CONTEXT.md, Brief; Round 1, Q8; Round 3, Q15 (a)). Its fixed
//! clauses are a skill's file, [`TEMPLATE`], so a clause edit stays a
//! skill edit; what differs per ticket is filled here from the ticket,
//! the branch that records it ([`crate::track`]) and the flags of the
//! one who dispatches: the scratch folder, what the builder owns and the
//! spec line. The template's grammar is [`render`]'s alone.
//!
//! The brief is rendered by the built binary, which a hook may run
//! straight from `target/` without cargo's rebuild, so a build apart from
//! the tree's `xtask/src` or `seams.toml` is refused the way `permit` and
//! `done --hook` refuse one ([`crate::built::judges`]), saying how to
//! rebuild: a brief from a stale build carries clauses a fixed binary has
//! since changed.
//!
//! The brief writes no reading and asks the tracker for nothing but the
//! ticket.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Gh;
use crate::track::{self, MAIN, Ticket, Track};
use crate::track_next::{TRACK_LABEL, tracks_of};

/// The template, under the workspace: a skill's file, an agent's to edit.
pub const TEMPLATE: &str = ".claude/skills/build/brief.md";
/// The heading the brief opens with, and the template's first rendered
/// line; the builder's role looks for it and stops without it.
pub const HEADING: &str = "## Brief";

/// What the one who dispatches says that the tracker and the branch do
/// not: the scratchpad the builder's files go under, in a folder named
/// for the ticket, what it owns, and the spec line. Each may be absent,
/// as from a hook that has the number alone (#4270), and its line is then
/// left out for the dispatcher's own text to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub scratch: Option<PathBuf>,
    pub owns: Vec<String>,
    pub spec: Option<String>,
}

/// `xtask ticket brief`: [`TEMPLATE`] under `root` rendered for `ticket`,
/// refused first where this build is not the one `built_root`'s sources
/// and `root`'s manifest make, and where no branch records the ticket or
/// no worktree stands on it.
pub fn brief(root: &Path, built_root: &Path, gh: &Gh, ticket: Ticket, ask: &Ask) -> Result<String> {
    crate::built::judges(root, root, built_root)?;
    let template = root.join(TEMPLATE);
    let template = std::fs::read_to_string(&template)
        .with_context(|| format!("reading the brief's template {}", template.display()))?;
    let issue = gh.view(root, ticket)?;
    let Some(branch) = track::branch_of_ticket(root, ticket) else {
        bail!(
            "no branch records {ticket}; lay its worktree down first, `cargo xtask worktree add t{0} --ticket {0}`",
            ticket.number()
        );
    };
    let worktree = track::worktrees(root)?
        .into_iter()
        .find(|(_, on)| on.as_deref() == Some(branch.as_str()))
        .map(|(dir, _)| dir)
        .with_context(|| format!("{branch} records {ticket} and no worktree stands on it"))?;
    let base = Track::of(root, Some(&branch))?.base;
    let unmerged = laid_on(root, &branch);
    let track_label = tracks_of(issue.labels.iter().map(|l| l.name.as_str()))
        .into_iter()
        .find(|t| *t != MAIN)
        .map(|t| format!("{TRACK_LABEL}{t}"));
    let mut fields = BTreeMap::new();
    let mut set = |name: &'static str, value: String| {
        fields.insert(name, value);
    };
    set("ticket", ticket.number().to_string());
    set("title", issue.title.clone());
    set("worktree", worktree.display().to_string());
    set("branch", branch.clone());
    set("base", base);
    set(
        "laid_on",
        unmerged.as_ref().map_or_else(
            || MAIN.to_owned(),
            |(t, b)| format!("{t}'s finished branch {b}, not yet merged"),
        ),
    );
    set(
        "unmerged_base",
        unmerged.map(|(_, b)| b).unwrap_or_default(),
    );
    set(
        "checkout",
        track::main_worktree(root)?.display().to_string(),
    );
    let folder = format!("t{}", ticket.number());
    let scratch = ask.scratch.as_ref().map(|dir| dir.join(folder));
    set(
        "scratch",
        scratch
            .map(|dir| format!("{}/", dir.display()))
            .unwrap_or_default(),
    );
    set("owns", ask.owns.join(", "));
    set("track_label", track_label.unwrap_or_default());
    set("spec", ask.spec.clone().unwrap_or_default());
    set(
        "stamp",
        format!(
            "xtask sources {}",
            &crate::built::SOURCES[..12.min(crate::built::SOURCES.len())]
        ),
    );
    render(&template, &fields)
}

/// The branch `branch` was laid on where that is not `main`: the commit
/// it was made at, its reflog's first entry, is one `main` does not
/// contain, and another branch recording a ticket contains it; the first
/// such in git's order of the records, with its ticket. Read from where
/// the branch was made, not from its tip, since a brief is rendered
/// before the branch has a commit of its own, its tip still the other's.
/// A branch with no reflog was laid on `main`, as `worktree add` lays
/// every branch.
fn laid_on(root: &Path, branch: &str) -> Option<(Ticket, String)> {
    let contains =
        |of: &str, b: &str| crate::git::git(root, &["merge-base", "--is-ancestor", of, b]).is_ok();
    let reflog = format!("refs/heads/{branch}");
    let made =
        crate::git::git(root, &["reflog", "show", "--format=%H", &reflog]).unwrap_or_default();
    let made = made.lines().last().filter(|made| !contains(made, MAIN))?;
    track::branch_records(root)
        .into_iter()
        .find_map(|(other, ticket)| {
            (other != branch
                && crate::git::has_ref(root, &format!("refs/heads/{other}"))
                && contains(made, &other))
            .then_some((ticket, other))
        })
}

/// `template` rendered: what stands below its [`HEADING`] line, the line
/// included, each `{field}` replaced by its value in `fields`, and every
/// line naming a field whose value is empty left out, with the blank line
/// that would then stand twice. A template with no
/// heading, and a field `fields` does not name, are refused, so a brief
/// never reaches a builder with a hole in it.
pub fn render(template: &str, fields: &BTreeMap<&str, String>) -> Result<String> {
    let Some(at) = template.lines().position(|line| line.trim_end() == HEADING) else {
        bail!("the brief's template has no `{HEADING}` line, which a builder looks for");
    };
    let mut brief = String::new();
    'line: for line in template.lines().skip(at) {
        let mut out = String::new();
        let mut rest = line;
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            let name = after.find('}').map(|close| &after[..close]).filter(|name| {
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            });
            let Some(name) = name else {
                out.push_str(&rest[..=open]);
                rest = after;
                continue;
            };
            let value = fields.get(name).with_context(|| {
                format!("the brief's template names `{{{name}}}`, a field the brief does not fill")
            })?;
            if value.is_empty() {
                continue 'line;
            }
            out.push_str(&rest[..open]);
            out.push_str(value);
            rest = &after[name.len() + 1..];
        }
        out.push_str(rest);
        // A line left out leaves no second blank line behind it.
        if out.trim().is_empty() && (brief.is_empty() || brief.ends_with("\n\n")) {
            continue;
        }
        brief.push_str(&out);
        brief.push('\n');
    }
    Ok(brief)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::render;

    /// The grammar: the note above the heading left out, a field filled, a
    /// line whose field is empty left out, a brace that opens no field
    /// kept, and a field the brief does not fill refused, as is a template
    /// with no heading.
    #[test]
    fn a_template_renders_below_its_heading_and_refuses_a_field_it_cannot_fill() {
        let fields: BTreeMap<&str, String> =
            [("ticket", "7".to_owned()), ("spec", String::new())].into();
        let template = "the editor's note {nothing}\n## Brief\n#{ticket} at {ticket}, {not a field} {}\n\nSpec: {spec}\n\nend\n";
        assert_eq!(
            render(template, &fields).unwrap(),
            "## Brief\n#7 at 7, {not a field} {}\n\nend\n"
        );
        let unknown = render("## Brief\n{owner}\n", &fields).unwrap_err();
        assert!(unknown.to_string().contains("`{owner}`"), "{unknown}");
        assert!(render("# Brief\n", &fields).is_err());
    }
}
