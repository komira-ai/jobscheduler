//! `host-shim`: what the `crun_host_setup.sh` tests run in place of the host's programs
//! (`systemctl`, `mount`, `findmnt`, `useradd`, ...). Never run by CI itself, and never useful
//! outside those tests.
//!
//! The test copies this program into a directory under each name it stands for and puts that
//! directory first on the script's `PATH`. `HOST_SHIM_DIR` names the directory with the two files
//! the shim shares with the test:
//!
//! - `calls`: one line per call, `<name> <arguments joined by one space>`, appended before the
//!   shim answers. A call nothing answers is logged as `UNCONFIGURED <name> <arguments>`.
//! - `answers`: one rule per line, tab separated: `<name>`, `<arguments>`, `<exit code>`,
//!   `<stdout>`, then any number of effects. The arguments match exactly, or by prefix when
//!   they end in `*`. The first matching rule answers. `<stdout>` is text (`\n`, `\t` and `\\`
//!   are escapes), or `@<path>`: the contents of that file, nothing when the file is missing,
//!   or `!<path>`: the real program at that path is run with the call's arguments and its
//!   exit status is the answer (used for the file tools whose answer is a real hash). The
//!   effects are done before the answer. An effect is `append=<path>=<text>`, `write=<path>=<text>` (the file is made, with its
//!   directories; any kind but `append` writes) or `copy-self=<path>` (a copy of the shim, executable, so a program the script
//!   runs by an absolute path is a shim as well).
//!
//! A call with no `HOST_SHIM_DIR`, no answers file or no matching rule exits 97 and does nothing
//! else: a shim nobody configured fails closed.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The exit code of a call nothing answered.
const UNCONFIGURED: u8 = 97;

/// `text` with `\n`, `\t` and `\\` replaced by what they stand for.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Appends `text` to the file `path`, making it and its directories if needed.
fn append(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open");
    file.write_all(text.as_bytes()).expect("write");
}

/// Does one effect.
fn effect(me: &Path, spec: &str) {
    if let Some(path) = spec.strip_prefix("copy-self=") {
        let path = Path::new(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::copy(me, path).expect("copy the shim");
        return;
    }
    let (kind, rest) = spec.split_once('=').expect("an effect is kind=path=text");
    let (path, text) = rest.split_once('=').expect("an effect is kind=path=text");
    let path = Path::new(path);
    if kind == "append" {
        append(path, &unescape(text));
    } else {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, unescape(text)).expect("write");
    }
}

/// Whether the rule's `pattern` matches the call's `argv`.
fn matches(pattern: &str, argv: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => argv.starts_with(prefix),
        None => pattern == argv,
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    let me = args.next().expect("argv0");
    let name = Path::new(&me)
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned();
    let words: Vec<String> = args.collect();
    let argv = words.join(" ");
    // A test may limit the size of the files the script writes (to make its writes fail); the
    // stand-ins' own logs are not the script's.
    let _ = rustix::process::setrlimit(
        rustix::process::Resource::Fsize,
        rustix::process::Rlimit {
            current: None,
            maximum: None,
        },
    );
    let Ok(dir) = std::env::var("HOST_SHIM_DIR") else {
        eprintln!("host-shim: {name} {argv}: HOST_SHIM_DIR is not set");
        return ExitCode::from(UNCONFIGURED);
    };
    let dir = PathBuf::from(dir);
    let answers = std::fs::read_to_string(dir.join("answers")).unwrap_or_default();
    let rule = answers.lines().find_map(|line| {
        let fields: Vec<&str> = line.split('\t').collect();
        (fields[0] == name && matches(fields[1], &argv)).then_some(fields)
    });
    let Some(rule) = rule else {
        append(&dir.join("calls"), &format!("UNCONFIGURED {name} {argv}\n"));
        eprintln!("host-shim: {name} {argv}: not configured");
        return ExitCode::from(UNCONFIGURED);
    };
    append(&dir.join("calls"), &format!("{name} {argv}\n"));
    let me = std::env::current_exe().expect("the shim's own path");
    for spec in &rule[4..] {
        effect(&me, spec);
    }
    if let Some(program) = rule[3].strip_prefix('!') {
        let status = std::process::Command::new(program)
            .args(&words)
            .status()
            .expect("run the real program");
        return ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1));
    }
    let stdout = match rule[3].strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
        None => unescape(rule[3]),
    };
    print!("{stdout}");
    ExitCode::from(rule[2].parse::<u8>().expect("an exit code"))
}
