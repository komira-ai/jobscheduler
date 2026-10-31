//! `xtask-fake`: what this crate's tests run in place of `cargo` and of `sudo`, so
//! `crun-tests` is tested without building anything, without root and without systemd.
//! Never run by CI itself.
//!
//! - Run under any name but `sudo`, it is cargo: it appends its arguments as one line to
//!   `target/debug/cargo-args` in its working directory and prints the file
//!   `cargo-messages` in its working directory, the JSON messages a test wrote for it
//!   (every build it is asked for is `--no-run`).
//! - Run as `sudo`, it appends `<working directory>: <arguments>` as one line to
//!   `sudo-args` next to it, and exits 0 (the tests it was asked to run passed). Asked for
//!   `systemctl show <unit> ...`, it prints the properties of a unit whose main process
//!   exited 0; asked for `systemctl reset-failed`, it does nothing.
//!
//!   Asked for `systemd-run ... count-run --count-file <path> -- ...`, it writes the count file
//!   the real wrapper would have, as the defect asks: `3` (no defect), or `0` (`zero-tests`),
//!   nothing (`no-count`), `many` (`garbled-count`).
//! - Run as `testbin`, it is a test binary: it prints the environment's `XTASK_FAKE_TEST_OUTPUT`
//!   and exits with `XTASK_FAKE_TEST_STATUS` (default 0).
//!
//! `XTASK_FAKE_DEFECT` asks for one failure of the real programs: `build-fails` (every build
//! exits 101) or `fails:<a>,<b>,...` (sudo exits 101, as a failed test binary does, when one
//! of its arguments is one of `<a>`, `<b>`, ...: an argument every unit has fails them all,
//! a target's binary fails only its unit), `unit-fails` (the `systemd-run` exits 0 but the
//! unit's own result is `exit-code` with status 101: the failure `systemd-run` once hid),
//! `show-fails` (`systemctl show` exits 1) or `show-garbled` (it prints what is not
//! `Key=Value`).

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

/// The failure asked for, if any.
fn defect() -> String {
    std::env::var("XTASK_FAKE_DEFECT").unwrap_or_default()
}

/// Appends `line` to the file `path`, making it if needed.
fn append(path: &Path, line: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
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
    let failing = defect
        .strip_prefix("fails:")
        .is_some_and(|list| list.split(',').any(|name| args.iter().any(|a| a == name)));
    // `sudo -n <command>...`.
    if args.get(1).is_some_and(|program| program == "systemctl") {
        return systemctl(&args[2..], &defect);
    }
    if args.get(1).is_some_and(|program| program == "systemd-run") {
        write_count(args, &defect);
    }
    if failing {
        ExitCode::from(101)
    } else {
        ExitCode::SUCCESS
    }
}

/// What the wrapper in a `systemd-run` command would have written to its count file.
fn write_count(args: &[String], defect: &str) {
    let Some(at) = args.iter().position(|a| a == "--count-file") else {
        return;
    };
    let text = match defect {
        "no-count" => return,
        "zero-tests" => "0\n",
        "garbled-count" => "many\n",
        _ => "3\n",
    };
    let file = Path::new(&args[at + 1]);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("directory");
    std::fs::write(file, text).expect("write the count");
}

/// The fake test binary: prints what the test asked for and exits as it asked.
fn testbin() -> ExitCode {
    print!(
        "{}",
        std::env::var("XTASK_FAKE_TEST_OUTPUT").unwrap_or_default()
    );
    let status = std::env::var("XTASK_FAKE_TEST_STATUS").unwrap_or_default();
    ExitCode::from(status.parse().unwrap_or(0))
}

/// The fake `systemctl`, behind sudo: what `show` prints for the unit, as asked.
fn systemctl(args: &[String], defect: &str) -> ExitCode {
    if args.first().is_none_or(|a| a != "show") {
        return ExitCode::SUCCESS;
    }
    match defect {
        "show-fails" => return ExitCode::from(1),
        "show-garbled" => println!("not systemd output"),
        "unit-fails" => {
            println!("Result=exit-code\nExecMainCode=1\nExecMainStatus=101\nLoadState=loaded");
        }
        _ => println!("Result=success\nExecMainCode=1\nExecMainStatus=0\nLoadState=loaded"),
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let mut argv = std::env::args();
    let me = std::path::PathBuf::from(argv.next().expect("argv0"));
    let args: Vec<String> = argv.collect();
    let name = me.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name == "sudo" {
        sudo(&me, &args)
    } else if name == "testbin" {
        testbin()
    } else {
        cargo(&args)
    }
}
