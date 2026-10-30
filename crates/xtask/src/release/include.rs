//! The files a source compiles in with `include_str!`, `include_bytes!` or `include!`.
//!
//! Most of a shipped crate outside `src/` is either compiled in or not, and the blanket
//! rule ([`super::is_shipped`]) lets markdown, `examples/` and `tests/` through as not
//! shipped. A source that includes one of them compiles it into the binary all the same,
//! so the plan reads the includes of every shipped source and ships what they name.
//!
//! An include is read as `name!(` and a plain string literal, with the path relative to
//! the source file. Anything else after the parenthesis (`concat!`, `env!`, a raw string,
//! an escape) cannot be resolved here and is an error, not a guess. The one exception is
//! `concat!(env!("OUT_DIR"), ...)`, a file the build script writes: it is not in the
//! repository, and the build script is shipped with its crate.

use super::Error;

/// The macros that compile a file in.
const MACROS: [&str; 3] = ["include_str!", "include_bytes!", "include!"];

/// How an include of a build script's output starts.
const OUT_DIR_PREFIX: &str = "concat!(env!(\"OUT_DIR\"),";

/// The files the source `text` of `file` includes, as paths relative to the repository
/// root (`file` is one too).
pub fn targets(file: &str, text: &str) -> Result<Vec<String>, Error> {
    let mut found = Vec::new();
    for name in MACROS {
        for (at, _) in text.match_indices(name) {
            let after = text[at + name.len()..].trim_start();
            // The name in prose or a comment, not a call.
            let Some(args) = after.strip_prefix('(') else {
                continue;
            };
            // A file the build script writes into `OUT_DIR` is not in the repository: the
            // script that writes it is a shipped file already.
            if args.trim_start().starts_with(OUT_DIR_PREFIX) {
                continue;
            }
            let path = args
                .trim_start()
                .strip_prefix('"')
                .and_then(|rest| rest.split_once('"'))
                .map(|(path, _)| path)
                .filter(|path| !path.contains('\\'));
            let Some(path) = path else {
                return Err(Error::Include {
                    file: file.to_owned(),
                    line: 1 + text[..at].matches('\n').count(),
                });
            };
            found.push(resolve(file, path));
        }
    }
    Ok(found)
}

/// `path`, which is relative to the directory of `from`, as a path from the root.
fn resolve(from: &str, path: &str) -> String {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    parts.join("/")
}
