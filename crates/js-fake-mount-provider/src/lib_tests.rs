use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use js_testkit::dir::TestDir;

use super::*;

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// Asserts every flag is read, and the defaults of the optional ones.
#[test]
fn the_flags_are_read() {
    let all = parse(args(&[
        "--socket",
        "/run/p.sock",
        "--control",
        "/run/c.sock",
        "--root",
        "/run/trees",
        "--max-placements",
        "2",
        "--refuse-at",
        "prepare",
        "--mount-wait-ms",
        "300",
        "--restart-pause-ms",
        "40",
    ]));
    assert_eq!(
        all,
        Ok(Config {
            socket: PathBuf::from("/run/p.sock"),
            control: PathBuf::from("/run/c.sock"),
            root: PathBuf::from("/run/trees"),
            limit: 2,
            refuse_at: RefuseAt::Prepare,
            wait: Duration::from_millis(300),
            pause: Duration::from_millis(40),
        })
    );
    let defaults = parse(args(&[
        "--socket",
        "/p",
        "--control",
        "/c",
        "--root",
        "/t",
        "--refuse-at",
        "mount",
    ]))
    .expect("the required flags");
    assert_eq!(
        (
            defaults.limit,
            defaults.refuse_at,
            defaults.wait,
            defaults.pause
        ),
        (
            1024,
            RefuseAt::Mount,
            Duration::from_millis(25_000),
            Duration::from_millis(1_000)
        )
    );
}

/// Asserts each usage error, by its full text.
#[test]
fn a_usage_error_names_the_flag() {
    let required = ["--socket", "/p", "--control", "/c", "--root", "/t"];
    let with = |extra: &[&str]| {
        let mut words = required.to_vec();
        words.extend_from_slice(extra);
        parse(args(&words))
    };
    assert_eq!(
        with(&["--socket"]),
        Err("--socket needs a value".to_owned())
    );
    assert_eq!(
        with(&["--max-placements", "two"]),
        Err("--max-placements: \"two\" is not a count".to_owned())
    );
    assert_eq!(
        with(&["--refuse-at", "release"]),
        Err("--refuse-at: \"release\" is not mount or prepare".to_owned())
    );
    assert_eq!(
        with(&["--verbose", "yes"]),
        Err("--verbose: not a flag of js-fake-mount-provider".to_owned())
    );
    assert_eq!(
        parse(args(&["--socket", "/p", "--control", "/c"])),
        Err("--root is required".to_owned())
    );
    assert_eq!(
        parse(args(&["--socket", "/p", "--control", "/c", "--root", "t"])),
        Err("--root: t is not an absolute path".to_owned())
    );
    assert_eq!(
        parse(args(&["--control", "/c", "--root", "/t"])),
        Err("--socket is required".to_owned())
    );
    assert_eq!(
        parse(args(&["--socket", "/p", "--root", "/t"])),
        Err("--control is required".to_owned())
    );
}

/// Asserts the exit statuses: 2 for a usage error, 1 when the provider socket cannot be
/// bound, and 1 when the control socket cannot be.
#[test]
fn a_usage_error_and_a_socket_not_bound_end_the_program() {
    assert_eq!(main(args(&["--root"])), ExitCode::from(2));
    let dir = TestDir::under(std::env::temp_dir(), "jsfmp-exit");
    let missing = dir.join("absent");
    let root = dir.path().display().to_string();
    let control = dir.join("c.sock").display().to_string();
    let socket = missing.join("p.sock").display().to_string();
    let words = ["--socket", &socket, "--control", &control, "--root", &root];
    assert_eq!(main(args(&words)), ExitCode::from(1));
    let socket = dir.join("p.sock").display().to_string();
    let control = missing.join("c.sock").display().to_string();
    let words = ["--socket", &socket, "--control", &control, "--root", &root];
    assert_eq!(main(args(&words)), ExitCode::from(1));
}
