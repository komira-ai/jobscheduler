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

/// `text` with its comments and the insides of its string and character literals blanked,
/// line for line: what is left is code.
fn code_of(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut block = 0usize;
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        match (block > 0, in_string, c, next) {
            (true, _, '*', Some('/')) => {
                chars.next();
                block -= 1;
            }
            (_, false, '/', Some('*')) => {
                chars.next();
                block += 1;
            }
            (true, _, '\n', _) => out.push('\n'),
            (true, _, _, _) => {}
            (false, true, '\\', _) => {
                chars.next();
            }
            (false, true, '"', _) => in_string = false,
            (false, true, '\n', _) => out.push('\n'),
            (false, true, _, _) => {}
            (false, false, '"', _) => in_string = true,
            (false, false, '/', Some('/')) => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            (false, false, _, _) => out.push(c),
        }
    }
    out
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
