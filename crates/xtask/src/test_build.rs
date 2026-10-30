//! Building integration test targets as the runner's user and finding their binaries,
//! for the verbs that run a target's `#[ignore]` tests somewhere other than under
//! `cargo test` ([`crate::crun_tests`]).
//!
//! Cargo builds the targets with `cargo test --locked --no-run --message-format=json -p
//! <package> --test <name>...` in the repository root, so nothing is compiled as root and
//! `target/` keeps its owner. [`find`] takes a test binary's path from cargo's JSON
//! messages. The binary is then run in its package's directory, where `cargo test` runs
//! it too.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// Why a test build or run step failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A step failed; the reason is the whole message.
    #[error("{0}")]
    Failed(String),
    /// A program could not be started.
    #[error("{what}: {why}")]
    Spawn {
        /// The program.
        what: String,
        /// What the system said.
        why: String,
    },
    /// Several units failed, one line each.
    #[error("{}", .0.join("\n"))]
    Refused(Vec<String>),
}

impl Error {
    /// The error for a program `what` that could not be started.
    pub(crate) fn spawn(what: String) -> impl FnOnce(std::io::Error) -> Error {
        move |why| Error::Spawn {
            what,
            why: why.to_string(),
        }
    }
}

/// The test binary cargo built, and the directory `cargo test` would run it in: its
/// package's, the directory of its `Cargo.toml`.
#[derive(Debug, PartialEq, Eq)]
pub struct Built {
    /// The test binary.
    pub executable: PathBuf,
    /// The directory of the package's `Cargo.toml`.
    pub dir: PathBuf,
}

/// The binary of the integration test target `test` in `messages`, cargo's
/// `--message-format=json` output: the one `compiler-artifact` whose target is of
/// kind `test`, is named `test`, and was built with the test profile.
///
/// # Errors
/// A line is not JSON, or there is not exactly one such artifact, or it names no
/// executable or manifest.
pub fn find(messages: &str, test: &str) -> Result<Built, Error> {
    let mut found = Vec::new();
    for line in messages.lines() {
        let m: Value = serde_json::from_str(line).map_err(|e| {
            Error::Failed(format!(
                "cargo printed a line that is not JSON ({e}): {line}"
            ))
        })?;
        // `&`, not `&&`: every message is judged on all four, so each comparison is
        // tested both ways.
        let wanted = (m["reason"] == "compiler-artifact")
            & (m["target"]["kind"] == serde_json::json!(["test"]))
            & (m["target"]["name"] == test)
            & (m["profile"]["test"] == true);
        if wanted {
            found.push(m);
        }
    }
    let [one] = found.as_slice() else {
        return Err(Error::Failed(format!(
            "cargo built {} test binaries for the target `{test}`, not 1",
            found.len()
        )));
    };
    let executable = one["executable"].as_str().map(PathBuf::from);
    let dir = one["manifest_path"]
        .as_str()
        .and_then(|p| Path::new(p).parent())
        .map(Path::to_path_buf);
    match (executable, dir) {
        (Some(executable), Some(dir)) => Ok(Built { executable, dir }),
        _ => Err(Error::Failed(format!(
            "cargo's artifact for the target `{test}` names no executable or no manifest: {one}"
        ))),
    }
}

/// `-p <package> --test <name>...`: the targets of a build, as messages name them.
fn targets(package: &str, tests: &[&str]) -> String {
    let mut what = format!("-p {package}");
    for test in tests {
        what.push_str(" --test ");
        what.push_str(test);
    }
    what
}

/// Builds the integration test targets `tests` of `package` as this user, with `cargo
/// test --locked --no-run --message-format=json` in `root`: cargo's JSON messages,
/// which [`find`] reads each target's binary from. The build's progress goes to this
/// process's stderr.
///
/// # Errors
/// Cargo could not be started, or the build failed.
pub fn build(cargo: &OsStr, root: &Path, package: &str, tests: &[&str]) -> Result<String, Error> {
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args(["test", "--locked", "--no-run", "--message-format=json"])
        .args(["-p", package]);
    for test in tests {
        command.args(["--test", *test]);
    }
    let out = command
        .stderr(Stdio::inherit())
        .output()
        .map_err(Error::spawn("cargo".to_owned()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "cargo test --no-run {} failed: {}",
            targets(package, tests),
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What cargo prints for `-p js-guest --test as_root --no-run`, cut down: a
    /// dependency's library, the package's own library and bin (built for the test,
    /// but not test-profile), a test-profile artifact of another target, a
    /// non-test-kind artifact named like the target, a compiler message on the target,
    /// the target itself, and the closing message.
    const MESSAGES: &str = r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"libc"},"profile":{"test":false},"executable":null,"manifest_path":"/r/libc/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"js_guest"},"profile":{"test":false},"executable":null,"manifest_path":"/w/crates/js-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"js-guest"},"profile":{"test":false},"executable":"/w/target/debug/js-guest","manifest_path":"/w/crates/js-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["test"],"name":"binary"},"profile":{"test":true},"executable":"/w/target/debug/deps/binary-1","manifest_path":"/w/crates/js-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"as_root"},"profile":{"test":true},"executable":"/w/target/debug/deps/as_root-bin","manifest_path":"/w/crates/js-guest/Cargo.toml"}
{"reason":"compiler-message","target":{"kind":["test"],"name":"as_root"},"profile":{"test":true},"manifest_path":"/w/crates/js-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["test"],"name":"as_root"},"profile":{"test":false},"executable":"/w/target/debug/deps/as_root-notest","manifest_path":"/w/crates/js-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["test"],"name":"as_root"},"profile":{"test":true},"executable":"/w/target/debug/deps/as_root-0f1e","manifest_path":"/w/crates/js-guest/Cargo.toml"}
{"reason":"build-finished","success":true}
"#;

    /// Catches the wrong artifact taken: any of the decoys above (another target,
    /// another kind, a message rather than an artifact, a build without the test
    /// profile) instead of the target's test binary, or the binary run from somewhere
    /// other than its package's directory.
    #[test]
    fn the_target_s_test_binary_is_found_among_the_decoys() {
        assert_eq!(
            find(MESSAGES, "as_root"),
            Ok(Built {
                executable: "/w/target/debug/deps/as_root-0f1e".into(),
                dir: "/w/crates/js-guest".into(),
            })
        );
    }

    /// Catches a guess where there is no single answer: no artifact for the target
    /// (a misspelt `--test`), or two (the first one taken).
    #[test]
    fn none_or_two_test_binaries_are_refused() {
        let none = find(MESSAGES, "as_rot").expect_err("no target as_rot");
        assert_eq!(
            none.to_string(),
            "cargo built 0 test binaries for the target `as_rot`, not 1"
        );
        let last = MESSAGES.lines().nth(7).expect("the target's line");
        let twice = format!("{MESSAGES}{last}\n");
        let two = find(&twice, "as_root").expect_err("two binaries");
        assert_eq!(
            two.to_string(),
            "cargo built 2 test binaries for the target `as_root`, not 1"
        );
    }

    /// Catches cargo output that is not its JSON messages (a `--message-format` that
    /// did not reach cargo) passed over as "no binary".
    #[test]
    fn a_line_that_is_not_json_is_refused() {
        let e = find("   Compiling js-guest v0.1.0\n", "as_root").expect_err("not JSON");
        let text = e.to_string();
        assert!(
            text.starts_with("cargo printed a line that is not JSON ("),
            "{text}"
        );
        assert!(text.ends_with("):    Compiling js-guest v0.1.0"), "{text}");
    }

    /// Catches an artifact with no executable, or no manifest to take the directory
    /// from, run anyway (as an empty path, or from the wrong directory).
    #[test]
    fn an_artifact_without_executable_or_manifest_is_refused() {
        let no_exe = r#"{"reason":"compiler-artifact","target":{"kind":["test"],"name":"t"},"profile":{"test":true},"executable":null,"manifest_path":"/w/Cargo.toml"}"#;
        let no_manifest = r#"{"reason":"compiler-artifact","target":{"kind":["test"],"name":"t"},"profile":{"test":true},"executable":"/w/t"}"#;
        for line in [no_exe, no_manifest] {
            let e = find(line, "t").expect_err(line);
            assert!(
                e.to_string()
                    .starts_with("cargo's artifact for the target `t` names no executable"),
                "{e}"
            );
        }
    }

    /// Catches the three errors reading wrongly: the failure as its text, the program
    /// that could not start named before the system's words, and several failures one
    /// per line.
    #[test]
    fn each_error_says_what_it_holds() {
        let io = std::io::Error::other("no such program");
        assert_eq!(
            Error::spawn("sudo".to_owned())(io).to_string(),
            "sudo: no such program"
        );
        assert_eq!(Error::Failed("x".to_owned()).to_string(), "x");
        let two = Error::Refused(vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(two.to_string(), "a\nb");
    }
}
