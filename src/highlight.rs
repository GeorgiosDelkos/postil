//! Syntax colors: a file's text becomes lines of styled tokens.
//!
//! # Invariant
//!
//! Tokens never drop or reorder text. Joining the tokens of each line, and the lines with
//! `\n`, gives back the input with line endings normalized to `\n` and one trailing newline
//! removed. The page relies on this: line `n` of the result is line `n` of the file.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent};

use crate::error::{Error, Result};

/// A run of text and the CSS class the page gives it. The class is empty for plain text.
pub(crate) type Token<'a> = (&'static str, &'a str);

/// Capture names from the grammars' highlight queries, each with the CSS class of
/// `ui/app.css` that colors it. A capture such as `function.method` matches its longest
/// listed prefix, here `function`. Captures not listed stay plain.
const STYLES: &[(&str, &str)] = &[
    ("attribute", "f"),
    ("comment", "m"),
    ("constant", "o"),
    ("constructor", "t"),
    ("escape", "o"),
    ("function", "f"),
    ("keyword", "k"),
    ("label", "o"),
    ("number", "o"),
    ("punctuation", "p"),
    ("string", "s"),
    ("tag", "k"),
    ("type", "t"),
    ("variable.builtin", "k"),
];

/// One language: the file extensions it claims and how to build its highlighter.
struct Grammar {
    /// Without the dot. Matched ignoring ASCII case.
    extensions: &'static [&'static str],
    build: fn() -> HighlightConfiguration,
}

/// The languages the app colors. Adding one is a grammar crate in `Cargo.toml` and a row
/// here. The TypeScript queries only add to the JavaScript ones, so those rows list both.
/// No row passes an injections query: `Highlighter::lines` does not run injections.
const GRAMMARS: &[Grammar] = &[
    Grammar {
        extensions: &["rs"],
        build: || {
            configured(HighlightConfiguration::new(
                tree_sitter_rust::LANGUAGE.into(),
                "rust",
                tree_sitter_rust::HIGHLIGHTS_QUERY,
                "",
                "",
            ))
        },
    },
    Grammar {
        extensions: &["js", "mjs", "cjs", "jsx"],
        build: || {
            let highlights = [
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ]
            .concat();
            configured(HighlightConfiguration::new(
                tree_sitter_javascript::LANGUAGE.into(),
                "javascript",
                &highlights,
                "",
                tree_sitter_javascript::LOCALS_QUERY,
            ))
        },
    },
    Grammar {
        extensions: &["ts", "mts", "cts"],
        build: || {
            let highlights = [
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ]
            .concat();
            let locals = [
                tree_sitter_javascript::LOCALS_QUERY,
                tree_sitter_typescript::LOCALS_QUERY,
            ]
            .concat();
            configured(HighlightConfiguration::new(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "typescript",
                &highlights,
                "",
                &locals,
            ))
        },
    },
    Grammar {
        extensions: &["tsx"],
        build: || {
            let highlights = [
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ]
            .concat();
            let locals = [
                tree_sitter_javascript::LOCALS_QUERY,
                tree_sitter_typescript::LOCALS_QUERY,
            ]
            .concat();
            configured(HighlightConfiguration::new(
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                "tsx",
                &highlights,
                "",
                &locals,
            ))
        },
    },
    Grammar {
        extensions: &["py"],
        build: || {
            configured(HighlightConfiguration::new(
                tree_sitter_python::LANGUAGE.into(),
                "python",
                tree_sitter_python::HIGHLIGHTS_QUERY,
                "",
                "",
            ))
        },
    },
    Grammar {
        extensions: &["go"],
        build: || {
            configured(HighlightConfiguration::new(
                tree_sitter_go::LANGUAGE.into(),
                "go",
                tree_sitter_go::HIGHLIGHTS_QUERY,
                "",
                "",
            ))
        },
    },
    Grammar {
        extensions: &["json"],
        build: || {
            configured(HighlightConfiguration::new(
                tree_sitter_json::LANGUAGE.into(),
                "json",
                tree_sitter_json::HIGHLIGHTS_QUERY,
                "",
                "",
            ))
        },
    },
    Grammar {
        extensions: &["toml"],
        build: || {
            configured(HighlightConfiguration::new(
                tree_sitter_toml_ng::LANGUAGE.into(),
                "toml",
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                "",
                "",
            ))
        },
    },
    Grammar {
        extensions: &["sh", "bash", "zsh"],
        build: || {
            configured(HighlightConfiguration::new(
                tree_sitter_bash::LANGUAGE.into(),
                "bash",
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "",
                "",
            ))
        },
    },
];

/// Finishes a grammar's configuration: its captures are mapped onto [`STYLES`].
///
/// Generic over the error only so this crate need not depend on `tree-sitter` directly to
/// name its `QueryError`; there is one error type.
fn configured<E: std::fmt::Debug>(
    configuration: std::result::Result<HighlightConfiguration, E>,
) -> HighlightConfiguration {
    let mut configuration = configuration.expect(
        "every row of GRAMMARS builds: the test \
             `every_grammar_colors_its_language_and_keeps_the_text` opens a file of each",
    );
    let names: Vec<&str> = STYLES.iter().map(|(name, _)| *name).collect();
    configuration.configure(&names);
    configuration
}

/// Highlights source files.
///
/// Compiling a grammar's queries takes milliseconds, so each one is built the first time a
/// file of its language is opened, and kept. Starting the app pays for none of them.
pub(crate) struct Highlighter {
    /// One slot per row of [`GRAMMARS`], in the same order.
    configurations: Vec<OnceLock<HighlightConfiguration>>,
}

impl Highlighter {
    pub(crate) fn new() -> Self {
        Self {
            configurations: GRAMMARS.iter().map(|_| OnceLock::new()).collect(),
        }
    }

    /// The configuration for `path`'s extension, or `None` for a language without a grammar.
    fn configuration(&self, path: &str) -> Option<&HighlightConfiguration> {
        let extension = Path::new(path).extension()?.to_str()?;
        let index = GRAMMARS.iter().position(|grammar| {
            grammar
                .extensions
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })?;
        Some(self.configurations[index].get_or_init(GRAMMARS[index].build))
    }

    /// Splits `text` into lines of tokens. `path` picks the grammar by extension; a file with
    /// no known grammar comes back as plain lines.
    pub(crate) fn lines<'a>(&self, path: &str, text: &'a str) -> Result<Vec<Vec<Token<'a>>>> {
        let mut lines = Lines::new(text);

        if let Some(configuration) = self.configuration(path) {
            let mut highlighter = tree_sitter_highlight::Highlighter::new();
            let events = highlighter
                .highlight(configuration, text.as_bytes(), None, None, |_| None)
                .map_err(Error::Highlight)?;

            // Captures nest (a string inside a macro call); the innermost one colors the text.
            let mut open: Vec<&'static str> = Vec::new();
            for event in events {
                match event.map_err(Error::Highlight)? {
                    HighlightEvent::HighlightStart(highlight) => open.push(STYLES[highlight.0].1),
                    HighlightEvent::HighlightEnd => {
                        open.pop();
                    }
                    HighlightEvent::Source { start, end } => {
                        lines.push(open.last().copied().unwrap_or(""), start..end);
                    }
                }
            }
        } else {
            lines.push("", 0..text.len());
        }

        Ok(lines.finish())
    }
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

/// Collects styled byte ranges of one text into lines.
struct Lines<'a> {
    text: &'a str,
    /// Finished and current lines, as byte ranges into `text`. Never empty: the last entry
    /// is the line being filled.
    lines: Vec<Vec<(&'static str, Range<usize>)>>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            lines: vec![Vec::new()],
        }
    }

    /// Appends `range` of the text with `class`, starting a new line at every `\n`.
    fn push(&mut self, class: &'static str, range: Range<usize>) {
        let mut start = range.start;
        for piece in self.text[range].split_inclusive('\n') {
            let end = start + piece.len();
            let content = piece.strip_suffix('\n').unwrap_or(piece);
            self.push_on_line(class, start..start + content.len());
            if piece.ends_with('\n') {
                self.end_line();
            }
            start = end;
        }
    }

    /// Ends the current line at a `\n`, dropping the `\r` of a `\r\n` ending.
    ///
    /// The `\r` is removed here and not in `push`, because the highlighter may end one range
    /// between the `\r` and the `\n`.
    fn end_line(&mut self) {
        let text = self.text;
        let line = self
            .lines
            .last_mut()
            .expect("`lines` starts with one line and only grows");
        if let Some((_, last)) = line.last_mut()
            && text[last.clone()].ends_with('\r')
        {
            last.end -= 1;
            if last.start == last.end {
                line.pop();
            }
        }
        self.lines.push(Vec::new());
    }

    fn push_on_line(&mut self, class: &'static str, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let line = self
            .lines
            .last_mut()
            .expect("`lines` starts with one line and only grows");

        // Adjacent ranges of one class become one token: fewer elements for the page to build.
        match line.last_mut() {
            Some((last_class, last)) if *last_class == class && last.end == range.start => {
                last.end = range.end;
            }
            _ => line.push((class, range)),
        }
    }

    fn finish(mut self) -> Vec<Vec<Token<'a>>> {
        // A file that ends with a newline has no line after it, and an empty file has no
        // lines at all: the count then agrees with `str::lines`, which `notes` uses.
        if self.text.is_empty() || self.text.ends_with('\n') {
            self.lines.pop();
        }
        let text = self.text;
        self.lines
            .into_iter()
            .map(|line| {
                line.into_iter()
                    .map(|(class, range)| (class, &text[range]))
                    .collect()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module invariant, as a function: what the tokens say the file contains.
    fn rejoin(lines: &[Vec<Token<'_>>]) -> String {
        let lines: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|(_, text)| *text).collect())
            .collect();
        lines.join("\n")
    }

    fn normalized(text: &str) -> String {
        let text = text.replace("\r\n", "\n");
        text.strip_suffix('\n').unwrap_or(&text).to_owned()
    }

    #[test]
    fn tokens_rejoin_to_the_input() {
        let highlighter = Highlighter::new();
        let cases = [
            ("empty.rs", ""),
            ("one.rs", "fn main() {}"),
            ("newline.rs", "fn main() {}\n"),
            ("blank_lines.rs", "\n\nfn a() {}\n\n\nfn b() {}\n"),
            ("crlf.rs", "fn a() {}\r\n// note\r\n\r\nfn b() {}\r\n"),
            // Tokens that span lines: a block comment and a string literal.
            ("spans.rs", "/* one\n   two */\nconst S: &str = \"a\nb\";\n"),
            ("unicode.rs", "// héllo wörld ✓\nlet s = \"日本語\";\n"),
            ("broken.rs", "fn ( { \"unterminated\n"),
            ("notes.txt", "plain\ntext\r\nwith both endings\n"),
            ("no_extension", "x\n"),
        ];

        for (path, text) in cases {
            let lines = highlighter.lines(path, text).unwrap();
            assert_eq!(rejoin(&lines), normalized(text), "{path}");
        }
    }

    #[test]
    fn every_short_text_keeps_its_text_and_its_line_count() {
        // Every string of up to five characters from an alphabet that opens and closes Rust
        // comments and strings and mixes both line endings. The fixed cases above miss what
        // this found once: an empty file came back as one line.
        let alphabet = ['a', '\n', '\r', '"', '/', '*'];
        let highlighter = Highlighter::new();
        let mut texts = vec![String::new()];
        let mut longest = vec![String::new()];
        for _ in 0..5 {
            longest = longest
                .iter()
                .flat_map(|text| alphabet.iter().map(move |c| format!("{text}{c}")))
                .collect();
            texts.extend(longest.iter().cloned());
        }

        for text in &texts {
            for path in ["a.rs", "a.txt"] {
                let lines = highlighter.lines(path, text).unwrap();
                assert_eq!(rejoin(&lines), normalized(text), "{path} {text:?}");
                assert_eq!(lines.len(), text.lines().count(), "{path} {text:?}");
            }
        }
    }

    #[test]
    fn line_count_matches_the_file() {
        let highlighter = Highlighter::new();
        let text = "fn a() {}\n\n/* x\ny */\nfn b() {}\n";

        let lines = highlighter.lines("a.rs", text).unwrap();

        assert_eq!(lines.len(), text.lines().count());
        assert!(lines[1].is_empty(), "a blank line has no tokens");
    }

    #[test]
    fn rust_gets_classes_and_other_files_stay_plain() {
        let highlighter = Highlighter::new();
        let text = "// note\nfn main() { let s = \"hi\"; }\n";

        let rust = highlighter.lines("main.rs", text).unwrap();
        assert_eq!(rust[0], [("m", "// note")]);
        assert!(rust[1].contains(&("k", "fn")), "{:?}", rust[1]);
        assert!(rust[1].contains(&("f", "main")), "{:?}", rust[1]);
        assert!(rust[1].contains(&("s", "\"hi\"")), "{:?}", rust[1]);

        let plain = highlighter.lines("main.txt", text).unwrap();
        assert_eq!(plain[1], [("", "fn main() { let s = \"hi\"; }")]);
    }

    #[test]
    fn every_grammar_colors_its_language_and_keeps_the_text() {
        let highlighter = Highlighter::new();
        // A file for every row of GRAMMARS, with tokens only that row's own queries color:
        // a JSX tag needs the JSX query, a type annotation needs the TypeScript one.
        let cases: [(&str, &str, &[Token<'_>]); 10] = [
            ("a.rs", "fn main() {}\n", &[("k", "fn")]),
            ("a.js", "function f() { return 1; }\n", &[("k", "function")]),
            ("a.jsx", "const a = <div>hi</div>;\n", &[("k", "div")]),
            ("a.ts", "const x: number = 1;\n", &[("t", "number")]),
            (
                "a.tsx",
                "const a: number = <div/>;\n",
                &[("t", "number"), ("k", "div")],
            ),
            ("a.py", "def f():\n    return 'x'\n", &[("k", "def")]),
            ("a.go", "func main() {}\n", &[("k", "func")]),
            ("a.json", "{\"a\": 12}\n", &[("o", "12")]),
            ("a.toml", "name = \"x\"\n", &[("s", "\"x\"")]),
            ("a.sh", "if true; then echo hi; fi\n", &[("k", "if")]),
        ];
        for grammar in GRAMMARS {
            let covered = cases.iter().any(|(path, _, _)| {
                grammar
                    .extensions
                    .iter()
                    .any(|extension| path.ends_with(&format!(".{extension}")))
            });
            assert!(covered, "no case for {:?}", grammar.extensions);
        }

        for (path, text, expected) in cases {
            let lines = highlighter.lines(path, text).unwrap();
            for token in expected {
                assert!(
                    lines[0].contains(token),
                    "{path}: {token:?} in {:?}",
                    lines[0]
                );
            }
            assert_eq!(rejoin(&lines), normalized(text), "{path}");
        }
    }

    #[test]
    fn no_extension_belongs_to_two_grammars() {
        let mut extensions: Vec<&str> = GRAMMARS
            .iter()
            .flat_map(|grammar| grammar.extensions.iter().copied())
            .collect();
        let count = extensions.len();
        extensions.sort_unstable();
        extensions.dedup();
        assert_eq!(extensions.len(), count);
    }

    #[test]
    fn extension_match_ignores_case() {
        let highlighter = Highlighter::new();

        let lines = highlighter.lines("MAIN.RS", "fn main() {}\n").unwrap();

        assert!(lines[0].contains(&("k", "fn")));
    }
}
