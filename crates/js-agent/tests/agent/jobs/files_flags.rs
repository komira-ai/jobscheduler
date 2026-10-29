//! `--run-files-root` at start-up: it needs containers, is not, holds or lies in another
//! root of the agent, is not watched, and the default's failure is a warning, not silence.

use std::path::Path;

use clap::Parser;
use js_agent::Cli;
use js_agent::lifecycle::Local;

use super::drain::container_flags;
use super::private;
use crate::support::{Scratch, code, linux_host, run_scripted, welcome};

/// Sets the value of `key` in `owned`, or appends the pair.
fn set(owned: &mut Vec<String>, key: &str, value: &Path) {
    let value = value.display().to_string();
    match owned.iter().position(|arg| arg == key) {
        Some(at) => owned[at + 1] = value,
        None => owned.extend([key.to_owned(), value]),
    }
}

/// Runs the agent with `owned` and returns (status, stderr, hellos sent).
fn start(owned: &[String]) -> (String, String, usize) {
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (ran, sent, _) = run_scripted(&all, &linux_host(), vec![welcome(1)], 1);
    let sent = sent.borrow().len();
    (ran.status(), ran.err, sent)
}

/// Asserts that a files root equal to, inside or holding the state root, the scratch root
/// or a mount root is refused with status 2, naming `--run-files-root` and the root, and
/// that nothing is sent. It catches a sweep of the files root that would delete the taint
/// marker and the host credential (state root), collide with a run's scratch directory
/// (scratch root), or a read-write mount that shows every run's secrets to a job.
#[test]
fn a_files_root_that_overlaps_another_root_is_refused() {
    let dir = Scratch::new("files-flags-overlap");
    let outer = private(&dir, "outer");
    let (state, inner, data) = (
        private(&dir, "outer/state"),
        private(&dir, "outer/scratch"),
        private(&dir, "outer/data"),
    );
    let apart = private(&dir, "apart");
    // (flag named, its root, the files root, the scratch root given)
    let cases: [(&str, &Path, &Path, &Path); 7] = [
        ("--state-root", &state, &state, &apart),
        ("--state-root", &state, &state.join("files"), &apart),
        ("--state-root", &state, &outer, &apart),
        ("--scratch-root", &inner, &inner, &inner),
        ("--scratch-root", &inner, &outer, &inner),
        ("--mount-root", &data, &data.join("files"), &apart),
        ("--mount-root", &data, &outer, &apart),
    ];
    for (flag, root, files, scratch) in cases {
        let mut owned = container_flags(&dir);
        if flag == "--mount-root" {
            owned.extend([
                "--mount-root".to_owned(),
                format!("data={}", data.display()),
            ]);
        }
        set(&mut owned, "--state-root", &state);
        set(&mut owned, "--scratch-root", scratch);
        set(&mut owned, "--run-files-root", files);
        let (status, err, sent) = start(&owned);
        assert_eq!(status, code(2), "{flag} {files:?}: {err}");
        let said = format!(
            "--run-files-root: is, holds or lies in the {flag} {}",
            root.display()
        );
        assert!(err.contains(&said), "{flag} {files:?}: {err}");
        assert_eq!(sent, 0);
    }
}

/// Asserts that a `--watch-dir` over the files root is refused, since the root changes with
/// every job. It catches a post-job check that would flag every run's files directory.
#[test]
fn a_watch_dir_over_the_files_root_is_refused() {
    let dir = Scratch::new("files-flags-watch");
    let mut owned = container_flags(&dir);
    let files = private(&dir, "watched-files");
    set(&mut owned, "--run-files-root", &files);
    owned.extend(["--watch-dir".to_owned(), files.display().to_string()]);
    let (status, err, sent) = start(&owned);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("--watch-dir") && err.contains("lies in or holds"),
        "{err}"
    );
    assert_eq!(sent, 0);
}

/// Asserts that `--run-files-root` without `--container-bin` is refused like the other
/// job flags. It catches a misspelt unit file that reads as configured while delivering no
/// files.
#[test]
fn a_files_root_needs_containers() {
    let dir = Scratch::new("files-flags-needs");
    let root = private(&dir, "files");
    let owned = vec!["--run-files-root".to_owned(), root.display().to_string()];
    let (status, err, sent) = start(&owned);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("--run-files-root: needs --container-bin"),
        "{err}"
    );
    assert_eq!(sent, 0);
}

/// Asserts that a default files root that cannot be made (its parent is a file) leaves the
/// agent without run files and carries the reason as a warning text, that a usable default
/// is used and warns of nothing, and that the whole agent prints the warning as
/// `js-agent: warning: run files unavailable: ...`. It catches a default that fails
/// silently, which an operator would meet only as a 409 from the first start.
#[test]
fn an_unusable_default_files_root_is_a_warning() {
    let dir = Scratch::new("files-flags-default");
    let owned = container_flags(&dir);
    let without: Vec<String> = {
        let mut args = vec![
            "js-agent".to_owned(),
            "--server".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "--token-file".to_owned(),
            "/nonexistent/token".to_owned(),
            "--pool".to_owned(),
            "burst".to_owned(),
        ];
        let mut skip = false;
        for arg in &owned {
            match (skip, arg.as_str()) {
                (true, _) => skip = false,
                (false, "--run-files-root") => skip = true,
                (false, _) => args.push(arg.clone()),
            }
        }
        args
    };
    let cli = Cli::try_parse_from(&without).unwrap();
    let blocker = dir.write("blocker", "a file", 0o600);
    let bad = Local::from_cli_with_default_files_root(&cli, &blocker.join("files")).unwrap();
    assert!(bad.files.is_none());
    let said = bad.files_warning.unwrap();
    assert!(
        said.starts_with("run files unavailable: --run-files-root: "),
        "{said}"
    );
    // A default that is the state root is never swept over it: run files go off, with a
    // warning, and the agent still starts (a default must not break an unrelated setup).
    let clash = Local::from_cli_with_default_files_root(&cli, &dir.path("state")).unwrap();
    assert!(clash.files.is_none());
    let said = clash.files_warning.unwrap();
    assert!(
        said.contains("is, holds or lies in the --state-root"),
        "{said}"
    );
    let good = Local::from_cli_with_default_files_root(&cli, &dir.path("default-files")).unwrap();
    assert!(good.files.is_some() && good.files_warning.is_none());
}
