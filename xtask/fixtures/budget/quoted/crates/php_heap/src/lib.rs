//! Six counted lines under a ceiling of five: the fixture is one line over
//! once a character literal holding a quote and a raw string holding the
//! two comment markers are read as what they are. A counter that takes the
//! quote in `'"'` for a string's start reads the block comment after it as
//! code, and one that takes the `/*` inside the raw string for a comment
//! counts nothing after it: four lines, under the ceiling.
#![forbid(unsafe_code)]
pub const QUOTE: char = '"'; /* a block comment after the character
   literal, which the quote inside it does not keep open */
pub const SLASHES: &str = r#"
/* a marker inside a raw string opens no block comment, and // no line comment
"#;
pub fn one() {}
