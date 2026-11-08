//! The corpus, `data/hostile.toml`, decoded once: each program's id, its
//! form and its code, ids unique and each spelt as a divergence key's tail
//! may be, and a form one of [`Form`]'s, so nothing past this module reads
//! the file's spelling.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The corpus's path, relative to the workspace root.
pub(crate) const FILE: &str = "data/hostile.toml";

/// What a program's error handler does to the step that raised the
/// diagnostic, as the file's header lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Form {
    /// It adds entries to a table the step reads or writes.
    Grows,
    /// It unsets the container or an operand the step holds.
    Unsets,
    /// It assigns over the container or an operand, freeing what stood.
    Replaces,
    /// It throws, so the step ends partway.
    Throws,
}

impl Form {
    /// The name the file spells it by, and a reading's group is keyed by.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Form::Grows => "grows",
            Form::Unsets => "unsets",
            Form::Replaces => "replaces",
            Form::Throws => "throws",
        }
    }
}

/// One program of the corpus.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Program {
    /// What names it for good: its verdict in a kept run, and its
    /// divergence key after [`crate::divergence::HOSTILE_PREFIX`].
    pub id: String,
    pub form: Form,
    /// Where it was taken from, where it was; read by people alone.
    #[serde(default, rename = "from")]
    _from: Option<String>,
    /// What runs, as a witness's snippet does.
    pub code: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    program: Vec<Program>,
}

/// The corpus: its programs in the file's order, and the blob id of the
/// file's bytes, which a kept run names so a reader can say which corpus
/// it was taken over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Corpus {
    pub programs: Vec<Program>,
    pub digest: String,
}

impl Corpus {
    /// The corpus of the workspace at `root`.
    pub(crate) fn load(root: &Path) -> Result<Corpus> {
        let path = root.join(FILE);
        let text = crate::read(&path)?;
        Corpus::parse(&text).with_context(|| format!("{FILE} is no corpus"))
    }

    /// `text` as a corpus: refused where it holds no program, names an id
    /// twice, or spells one with other than lowercase letters, digits and
    /// `-`, or writes a program with no code.
    pub(crate) fn parse(text: &str) -> Result<Corpus> {
        let File { program } = toml::from_str(text)?;
        if program.is_empty() {
            bail!("it holds no [[program]]");
        }
        let mut ids = BTreeSet::new();
        for p in &program {
            let spelt = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
            if p.id.is_empty() || !p.id.chars().all(spelt) {
                bail!(
                    "the id {:?} is spelt with other than lowercase letters, digits and `-`",
                    p.id
                );
            }
            if !ids.insert(p.id.as_str()) {
                bail!("the id {:?} names two programs", p.id);
            }
            if p.code.trim().is_empty() {
                bail!("the program {:?} has no code", p.id);
            }
        }
        Ok(Corpus {
            programs: program,
            digest: crate::git::blob_id(text.as_bytes()),
        })
    }
}
