//! The region-body rule of revision 2: a computed region's body is
//! rendered by `computed run` and never edited (computed ADR 0005). Not a
//! manifest rule, so it runs before them and carries [`REGION_RULE`].

use std::fs;
use std::ops::Range;
use std::path::Path;

use super::REGION_RULE;
use super::call::Call;
use super::rules::{Finding, Kind, Placement};

const REGION_TEXT: &str = "The body of a computed region is rendered by `computed run`, not edited; change the command or its inputs, or the prose around the region (computed ADR 0005).";

/// One computed region of a Markdown file: its opener line and the byte
/// range of the body between opener and closer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Region<'a> {
    opener: &'a str,
    body: Range<usize>,
}

/// The regions of a file. Markers inside a fenced code block are prose.
fn regions(text: &str) -> Vec<Region<'_>> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    let mut open: Option<(&str, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let end = offset + line.len();
        if let Some(mark) = fence {
            if trimmed.starts_with(mark) {
                fence = None;
            }
        } else if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = Some(&trimmed[..3]);
        } else if trimmed.starts_with("<!-- computed") && trimmed.ends_with("-->") {
            open = Some((trimmed, end));
        } else if trimmed.starts_with("<!-- /computed")
            && let Some((opener, start)) = open.take()
        {
            out.push(Region {
                opener,
                body: start..offset,
            });
        }
        offset = end;
    }
    out
}

/// The region-body rule: an `Edit` whose `old_string` lies in a body, or a
/// `Write` that leaves a body other than it found, is denied. A `Write`'s
/// regions pair with the file's by opener, or by position when an opener
/// changed; a region the `Write` removes is a template change, not a body
/// edit.
pub(super) fn region_body(root: &Path, path: &Path, call: &Call) -> Option<Finding> {
    let existing = fs::read_to_string(root.join(path)).ok()?;
    let bodies = regions(&existing);
    if bodies.is_empty() {
        return None;
    }
    let edited = match call.tool_name.as_str() {
        "Edit" => {
            let old = call.tool_input.old_string.as_deref().unwrap_or_default();
            let mut sites = existing.match_indices(old).map(|(i, _)| i..i + old.len());
            let overlaps = |site: Range<usize>| {
                bodies
                    .iter()
                    .any(|r| site.start < r.body.end && site.end > r.body.start)
            };
            if call.tool_input.replace_all {
                sites.any(overlaps)
            } else {
                sites.next().is_some_and(overlaps)
            }
        }
        "Write" => {
            let content = call.tool_input.content.as_deref().unwrap_or_default();
            let after = regions(content);
            bodies.iter().enumerate().any(|(i, before)| {
                after
                    .iter()
                    .find(|r| r.opener == before.opener)
                    .or_else(|| after.get(i))
                    .is_some_and(|r| content[r.body.clone()] != existing[before.body.clone()])
            })
        }
        _ => false,
    };
    edited.then(|| Finding {
        kind: Kind::Deny,
        rule: REGION_RULE.to_owned(),
        text: REGION_TEXT.to_owned(),
        adr: Vec::new(),
        admission: None,
        placement: Some(Placement::In(root.to_path_buf())),
        remedy: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_bodies_between_markers_outside_fences() {
        let text = "a\n<!-- computed tree src=. name=x -->\nbody\n<!-- /computed -->\n```\n<!-- computed exec cmd=y -->\nprose\n<!-- /computed -->\n```\n<!-- computed exec cmd=z volatile -->\n<!-- /computed -->\n";
        let found = regions(text);
        assert_eq!(found.len(), 2);
        assert_eq!(&text[found[0].body.clone()], "body\n");
        assert_eq!(&text[found[1].body.clone()], "");
    }
}
