//! What a line budget counts (design §4.1): non-blank, non-comment lines,
//! tests excluded. Rust is counted through the `rust` module's scanner,
//! which knows every shape of comment and literal; TOML and every other
//! text are counted here.

use crate::rust;

/// The counted lines of one file, by its extension: Rust as its code
/// lines, TOML without its `#` comments, and every other text as its
/// non-blank lines.
pub fn count(path: &str, text: &str) -> usize {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("rs") => rust::code_lines(text, true).len(),
        Some("toml") => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .count(),
        _ => text.lines().filter(|l| !l.trim().is_empty()).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::count;

    #[test]
    fn other_texts_count_non_blank_lines_and_toml_drops_its_comments() {
        assert_eq!(count("a.md", "# Title\n\ntext\n"), 2);
        assert_eq!(count("a.toml", "# c\nkey = 1\n\n[t]\n"), 2);
        assert_eq!(count("a.rs", "// c\nfn a() {}\n#[cfg(test)] mod t;\n"), 1);
    }
}
