//! A stand-in for `crun`, for the tests of the OCI backend. It runs nothing.
//!
//! A test copies this binary beside a script file and gives the copy to the backend as
//! its runtime. The script is the copy's path with the extension `script`: one directive
//! per line, `<key> <text>`:
//!
//! - `run-exit <n>`: `crun run` of a job's container exits `n` (default 0);
//! - `run-log <json>`: it first appends this line to its `--log` file;
//! - `run-out <text>` and `run-err <text>`: it writes the text and a newline to its
//!   standard output or error before it exits;
//! - `run-flood <n>`: it writes `n` bytes (`x`, then a final newline) to its standard
//!   output, in 4096-byte writes;
//! - `helper-fail <text>`: `crun run` of a helper bundle (a bundle directory whose name
//!   starts with `helper-`) prints the text on stderr and exits 1;
//! - `kill-fail <text>` and `delete-fail <text>`: `kill` and `delete` do the same;
//! - `state-pid <n>`: `state` prints a state whose `pid` is `n` (default 1); `state-raw
//!   <text>` prints the text instead; `state-fail <text>` fails like `kill-fail`.
//!
//! Every call appends its arguments, joined by spaces, as a line to the copy's path with
//! the extension `calls`.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

fn directive(script: &str, key: &str) -> Option<String> {
    script
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
        .map(str::to_owned)
}

fn fail(text: &str) -> ExitCode {
    eprintln!("{text}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let me: PathBuf = std::env::current_exe().expect("own path");
    let script = std::fs::read_to_string(me.with_extension("script")).unwrap_or_default();
    let mut calls = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(me.with_extension("calls"))
        .expect("calls file");
    writeln!(calls, "{}", args.join(" ")).expect("record");

    let mut log = None;
    let mut bundle = None;
    let mut command = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--log" => log = rest.next().cloned(),
            "--root" | "--log-format" | "--signal" => {
                rest.next();
            }
            "--bundle" | "-b" => bundle = rest.next().cloned(),
            word if !word.starts_with('-') && command.is_none() => command = Some(word.to_owned()),
            _ => {}
        }
    }
    match command.as_deref() {
        Some("run") => {
            let helper = bundle
                .as_deref()
                .and_then(|b| std::path::Path::new(b).file_name())
                .is_some_and(|name| name.to_string_lossy().starts_with("helper-"));
            if helper {
                return directive(&script, "helper-fail").map_or(ExitCode::SUCCESS, |t| fail(&t));
            }
            if let (Some(line), Some(path)) = (directive(&script, "run-log"), log) {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .expect("crun log");
                writeln!(file, "{line}").expect("log");
            }
            if let Some(text) = directive(&script, "run-out") {
                println!("{text}");
            }
            if let Some(text) = directive(&script, "run-err") {
                eprintln!("{text}");
            }
            if let Some(n) = directive(&script, "run-flood") {
                let mut left: usize = n.parse().expect("flood size");
                let mut out = std::io::stdout().lock();
                while left > 0 {
                    let step = left.min(4096);
                    let mut block = vec![b'x'; step];
                    if left == step {
                        block[step - 1] = b'\n';
                    }
                    out.write_all(&block).expect("flood");
                    left -= step;
                }
            }
            let code = directive(&script, "run-exit").map_or(0, |n| n.parse().expect("exit code"));
            ExitCode::from(code)
        }
        Some("kill") => directive(&script, "kill-fail").map_or(ExitCode::SUCCESS, |t| fail(&t)),
        Some("delete") => directive(&script, "delete-fail").map_or(ExitCode::SUCCESS, |t| fail(&t)),
        Some("state") => {
            if let Some(text) = directive(&script, "state-fail") {
                return fail(&text);
            }
            let pid = directive(&script, "state-pid").unwrap_or_else(|| "1".to_owned());
            let raw = directive(&script, "state-raw");
            println!(
                "{}",
                raw.unwrap_or_else(|| format!(
                    "{{\"id\":\"x\",\"pid\":{pid},\"status\":\"running\"}}"
                ))
            );
            ExitCode::SUCCESS
        }
        _ => fail("fake crun: unknown command"),
    }
}
