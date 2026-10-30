//! `xtask-fake`: what this crate's tests run in place of `cargo` and of `sudo`, so
//! `crun-tests` is tested without building anything, without root and without systemd.
//! Never run by CI itself.
//!
//! - Run under any name but `sudo`, it is cargo: it appends its arguments as one line to
//!   `target/debug/cargo-args` in its working directory and prints the file
//!   `cargo-messages` in its working directory, the JSON messages a test wrote for it
//!   (every build it is asked for is `--no-run`).
//! - Run as `sudo`, it appends `<working directory>: <arguments>` as one line to
//!   `sudo-args` next to it, and exits 0 (the tests it was asked to run passed).
//!
//! `XTASK_FAKE_DEFECT` asks for one failure of the real programs: `build-fails` (every build
//! exits 101), `tests-fail` (sudo exits 101, as a failed test binary does) or
//! `fails:<a>,<b>,...` (sudo exits 101 when one of its arguments is one of `<a>`, `<b>`,
//! ..., so some units of several fail).

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

/// The failure asked for, if any.
fn defect() -> String {
    std::env::var("XTASK_FAKE_DEFECT").unwrap_or_default()
}

/// Appends `line` to the file `path`, making it if needed.
fn append(path: &Path, line: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("directory");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open the log");
    writeln!(file, "{line}").expect("write the log");
}

/// The fake cargo: records the call and prints the test's messages.
fn cargo(args: &[String]) -> ExitCode {
    append(Path::new("target/debug/cargo-args"), &args.join(" "));
    if defect() == "build-fails" {
        return ExitCode::from(101);
    }
    let messages = std::fs::read_to_string("cargo-messages").expect("cargo-messages");
    print!("{messages}");
    ExitCode::SUCCESS
}

/// The fake sudo: records the call and passes or fails as asked.
fn sudo(me: &Path, args: &[String]) -> ExitCode {
    let cwd = std::env::current_dir().expect("working directory");
    let log = me.with_file_name("sudo-args");
    append(&log, &format!("{}: {}", cwd.display(), args.join(" ")));
    let defect = defect();
    let failing = match defect.strip_prefix("fails:") {
        Some(list) => list.split(',').any(|name| args.iter().any(|a| a == name)),
        None => defect == "tests-fail",
    };
    if failing {
        ExitCode::from(101)
    } else {
        ExitCode::SUCCESS
    }
}

fn main() -> ExitCode {
    let mut argv = std::env::args();
    let me = std::path::PathBuf::from(argv.next().expect("argv0"));
    let args: Vec<String> = argv.collect();
    if me.file_name().is_some_and(|name| name == "sudo") {
        sudo(&me, &args)
    } else {
        cargo(&args)
    }
}
