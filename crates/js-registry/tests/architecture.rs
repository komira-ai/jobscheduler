//! The registry places nothing itself: its placer picks the host of every start and keeps
//! the whole-node queue. This reads the crate's own source and fails when the code (not a
//! comment or a string) names one of the ways the registry used to place by itself:
//!
//! - the first-free picker: the book's search for the first host by id that qualifies
//!   (`HostBook::start_where`) and the gate that refused a class that shares a host for
//!   want of a placer (`HostBook::class_gate`);
//! - the deadline give-up: dropping a reservation whose host is still busy at its deadline
//!   and barring the host (`keep_or_move`, `gives_up`, `give_up`), with its cap
//!   (`max_reserve_attempts`, `js_whole::cost::reserve_deadline`).

use std::fs;
use std::path::{Path, PathBuf};

/// The names the registry's code must not use, each with what it stood for.
const BANNED: &[(&str, &str)] = &[
    ("start_where", "the first-free picker"),
    ("class_gate", "the no-placer class gate"),
    ("keep_or_move", "the deadline give-up"),
    ("gives_up", "the deadline give-up"),
    ("give_up", "the deadline give-up"),
    ("max_reserve_attempts", "the give-up cap"),
    ("reserve_deadline", "the give-up cap"),
];

/// `text` with its comments, its string literals (raw ones included) and its character
/// literals blanked, line for line: what is left is code. A lifetime (`'a`) is code.
fn code_of(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let next = chars.get(at + 1).copied();
        let ident_before = at > 0 && is_ident(chars[at - 1]);
        let end = match (c, next) {
            ('/', Some('/')) => Some(line_end(&chars, at)),
            ('/', Some('*')) => Some(block_end(&chars, at)),
            ('"', _) => Some(string_end(&chars, at + 1)),
            ('r', Some('"' | '#')) if !ident_before => raw_end(&chars, at + 1),
            ('\'', _) => char_end(&chars, at),
            _ => None,
        };
        match end {
            Some(end) => {
                let blanked = chars[at..end].iter().filter(|c| **c == '\n');
                out.extend(blanked);
                at = end;
            }
            None => {
                out.push(c);
                at += 1;
            }
        }
    }
    out
}

/// Where the line comment at `from` ends: at the line break, which stays code.
fn line_end(chars: &[char], from: usize) -> usize {
    let rest = chars[from..].iter().position(|c| *c == '\n');
    rest.map_or(chars.len(), |offset| from + offset)
}

/// Just past the block comment opened at `from`, nested comments included.
fn block_end(chars: &[char], from: usize) -> usize {
    let mut depth = 0usize;
    let mut at = from;
    while at + 1 < chars.len() {
        match (chars[at], chars[at + 1]) {
            ('/', '*') => {
                depth += 1;
                at += 2;
            }
            ('*', '/') => {
                depth -= 1;
                at += 2;
                if depth == 0 {
                    return at;
                }
            }
            _ => at += 1,
        }
    }
    chars.len()
}

/// Just past the string whose contents start at `from`.
fn string_end(chars: &[char], from: usize) -> usize {
    let mut at = from;
    while at < chars.len() {
        match chars[at] {
            '\\' => at += 2,
            '"' => return at + 1,
            _ => at += 1,
        }
    }
    chars.len()
}

/// Just past the raw string whose hashes start at `from` (after the `r`), or `None` when
/// what follows is no raw string (a raw identifier, `r#name`).
fn raw_end(chars: &[char], from: usize) -> Option<usize> {
    let hashes = chars[from..].iter().take_while(|c| **c == '#').count();
    let open = from + hashes;
    if chars.get(open) != Some(&'"') {
        return None;
    }
    let mut at = open + 1;
    while at < chars.len() {
        let closes =
            chars[at] == '"' && (1..=hashes).all(|offset| chars.get(at + offset) == Some(&'#'));
        if closes {
            return Some(at + 1 + hashes);
        }
        at += 1;
    }
    Some(chars.len())
}

/// Just past the character literal opened at `from`, or `None` for a lifetime.
fn char_end(chars: &[char], from: usize) -> Option<usize> {
    match (chars.get(from + 1), chars.get(from + 2)) {
        (Some('\\'), _) => {
            let rest = chars[from + 2..].iter().skip(1).position(|c| *c == '\'');
            Some(rest.map_or(chars.len(), |offset| from + 3 + offset + 1))
        }
        (Some(_), Some('\'')) => Some(from + 3),
        _ => None,
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every banned name the code of `text` uses as a whole word: the line (from 1) and the
/// name.
fn violations(text: &str) -> Vec<(usize, &'static str)> {
    let code = code_of(text);
    let mut found = Vec::new();
    for (at, line) in code.lines().enumerate() {
        for (name, _) in BANNED {
            let mut from = 0;
            while let Some(offset) = line[from..].find(name) {
                let start = from + offset;
                let end = start + name.len();
                let before = line[..start].chars().next_back().is_some_and(is_ident);
                let after = line[end..].chars().next().is_some_and(is_ident);
                if !before && !after {
                    found.push((at + 1, *name));
                }
                from = end;
            }
        }
    }
    found
}

/// Every `.rs` file under `dir`, in path order.
fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files.extend(sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

/// Asserts that no source file of the crate's library uses a banned name in its code, and
/// that the check read the files it guards (the start path and the whole-node modules).
/// Catches a first-free start path or a deadline give-up brought back into the registry.
#[test]
fn the_registry_holds_no_first_free_picker_and_no_deadline_give_up() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = sources(&root);
    let read: Vec<String> = files
        .iter()
        .map(|path| path.strip_prefix(&root).unwrap().display().to_string())
        .collect();
    for guarded in [
        "registry/start.rs",
        "whole/reserve.rs",
        "registry/waiting.rs",
    ] {
        assert!(
            read.iter().any(|file| file == guarded),
            "{guarded} not read"
        );
    }
    let mut found = Vec::new();
    for (path, name) in files.iter().zip(&read) {
        let text = fs::read_to_string(path).unwrap();
        for (line, banned) in violations(&text) {
            let why = BANNED.iter().find(|(b, _)| *b == banned).unwrap().1;
            found.push(format!("{name}:{line}: {banned} ({why})"));
        }
    }
    assert_eq!(found, Vec::<String>::new());
}

/// Asserts that the check finds a call of the first-free picker and of each give-up name
/// in code, with its line, and finds them once each. Catches a check that passes whatever
/// the code says.
#[test]
fn a_first_free_call_or_a_give_up_in_code_is_found() {
    let text = "fn start(book: &mut HostBook) {\n    \
                let gate = HostBook::class_gate(classes, spec);\n    \
                book.start_where(spec, gate, &eligible)\n}\n\
                fn pass() { ctx.keep_or_move(id); if mark.gives_up() { give_up(id) } }\n\
                let cap = settings.max_reserve_attempts + reserve_deadline(wall);\n";
    assert_eq!(
        violations(text),
        vec![
            (2, "class_gate"),
            (3, "start_where"),
            (5, "keep_or_move"),
            (5, "gives_up"),
            (5, "give_up"),
            (6, "max_reserve_attempts"),
            (6, "reserve_deadline"),
        ]
    );
}

/// Asserts that a banned name in a comment, a doc comment, a block comment or a string, or
/// as part of a longer name, is not code and is not found, while code after a comment on
/// the same file still is. Catches a check that fails on prose that only names the old
/// path, or on the deprecated `give_ups` count the ledger still writes as zero.
#[test]
fn a_banned_name_in_prose_or_inside_a_longer_name_is_not_found() {
    let text = "// the old start_where picker\n\
                /// gives_up was removed\n\
                /* keep_or_move\n   and give_up */ let a = 1;\n\
                let s = \"class_gate \\\" start_where\";\n\
                let n = record.give_ups + start_where_else;\n\
                book.start_over(spec, gate, &eligible, &choose); // not start_where\n\
                start_where();\n";
    assert_eq!(violations(text), vec![(8, "start_where")]);
}

/// Asserts that a character literal holding a quote or an escaped quote, and a raw string,
/// are skipped as literals and blank nothing after them, while a lifetime is code: a banned
/// name after each is still found, and one inside the raw string is not. Catches a quote in
/// a character literal read as the start of a string, which would hide the rest of a file.
#[test]
fn a_quote_in_a_character_literal_hides_no_code_after_it() {
    let text = "let q = '\"'; start_where();\n\
                let e = '\\''; class_gate();\n\
                let r = r#\"keep_or_move \" give_up\"#; gives_up();\n\
                fn f<'a>(x: &'a str) { reserve_deadline(x) }\n";
    assert_eq!(
        violations(text),
        vec![
            (1, "start_where"),
            (2, "class_gate"),
            (3, "gives_up"),
            (4, "reserve_deadline"),
        ]
    );
}
