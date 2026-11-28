//! `xtask-fake`: what this crate's tests run in place of `cargo` and of `sudo`, so
//! `crun-tests` is tested without building anything, without root and without systemd.
//! Never run by CI itself.
//!
//! - Run under any name but `sudo`, it is cargo: it appends its arguments as one line to
//!   `target/debug/cargo-args` in its working directory and prints the file
//!   `cargo-messages` in its working directory, the JSON messages a test wrote for it
//!   (every test build it is asked for is `--no-run`; a `cargo build` prints the same file).
//! - Run as `sudo`, it appends `<working directory>: <arguments>` as one line to
//!   `sudo-args` next to it, whatever the command (`mac-root-tests` runs it too), and exits
//!   0 (the tests it was asked to run passed). Asked for
//!   `systemctl show <unit> ...`, it prints the properties of a unit whose main process
//!   exited 0; asked for `systemctl reset-failed`, it does nothing.
//!
//!   Asked for `systemd-run ... count-run --count-file <path> -- ...`, it writes the count file
//!   the real wrapper would have, as the defect asks: `3` (no defect), or `0` (`zero-tests`),
//!   nothing (`no-count`), `many` (`garbled-count`).
//! - Run as `testbin`, it is a test binary: it prints the environment's `XTASK_FAKE_TEST_OUTPUT`
//!   and exits with `XTASK_FAKE_TEST_STATUS` (default 0).
//!
//! - Run as `js-lease-server`, it is the server of `site-demo`: it exits 5 unless its working
//!   directory holds `pool.toml` and the `--token-file` it is given with mode 0600 and it is given
//!   `--job-listen`, then listens on `--listen` (default 127.0.0.1:0, a port of the system's choosing)
//!   and on `--job-listen`, prints `listening for the job service addr=<address>` with the address
//!   the job service bound and `listening on <address>` with the address it bound, answers every request
//!   `200 ok` (`503` under `server-unhealthy`), and exits 0 after a `GET /quit`.
//! - Run as `js`, it is the client of `site-demo`: `job submit` prints the refusal the real client
//!   prints when no host could serve the job and exits 3, and `job list` prints nothing.
//!
//! - Run as `buck2`, it is the buck2 of `farm-workers`, whose one call is `audit config`: it
//!   appends its arguments, joined by spaces, as one line to `buck2.calls` of its working
//!   directory, prints the file `buck2-audit.stdout` of the working directory (when there is
//!   none, `{}`, buck2's answer for a key that is not set), prints `buck2-audit.stderr` on stderr,
//!   and exits with the number in `buck2-audit.status` (0 when there is none). It answers from
//!   files, not from the environment. (`farm-table` passes its buck2 to `buildrules sync` alone,
//!   which the stand-in `buildrules` does not run.)
//! - Run as `buildrules`, it is the command line `farm-table` is given, over a table of its own: it
//!   appends its arguments as one line to `buildrules.calls` of its working directory. `sync` writes
//!   what a sync of the working directory writes: `buildrules.lock.bzl`, one line `<file> <sha256>`
//!   for each of `Cargo.toml` and `Cargo.lock`, and `third_party/buildrules/git_archives.tsv` when
//!   `Cargo.toml` has a `git = ` dependency (removed when it has none). `check` exits 1, with no
//!   `STALE:` line, when there is no `buildrules.lock.bzl`; else with one `STALE:` line, in the real
//!   command line's form, for each of the two files the table records another sha256 of, or when they
//!   agree, for each generated file that is not what `sync` writes; else 0. Defects:
//!   `sync-fails` (sync exits 1 with a line), `sync-writes-nothing` (sync exits 0 and writes
//!   nothing), `check-usage` (check exits 2 with a usage line).
//!
//! `XTASK_FAKE_DEFECT`, or the first line of a file named `defect` next to the program, asks for one
//! failure of the real programs: `build-fails` (every build
//! exits 101) or `fails:<a>,<b>,...` (sudo exits 101, as a failed test binary does, when one
//! of its arguments is one of `<a>`, `<b>`, ...: an argument every unit has fails them all,
//! a target's binary fails only its unit), `unit-fails` (the `systemd-run` exits 0 but the
//! unit's own result is `exit-code` with status 101: the failure `systemd-run` once hid),
//! `show-fails` (`systemctl show` exits 1) or `show-garbled` (it prints what is not
//! `Key=Value`), `server-dies` (the server exits 3 with a line on stderr), `server-unannounced` (it serves, but
//! prints `bound <address>` instead of `listening on <address>`), `server-unhealthy` (it answers
//! 503 to everything), `js-fails` (the client exits 4 with a line on stderr), `js-accepts` (submit succeeds and prints the job),
//! `js-list-fails` (list exits 4 with a line on stderr) or `server-no-job-address` (the server serves but does not say where the job service listens).

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

/// The failure asked for, if any: `XTASK_FAKE_DEFECT`, or else the first line of the file `defect` next
/// to this program (a test that runs fakes from its own process cannot set the environment for them).
fn defect() -> String {
    let asked = std::env::var("XTASK_FAKE_DEFECT").unwrap_or_default();
    if !asked.is_empty() {
        return asked;
    }
    let me = std::env::args().next().unwrap_or_default();
    std::fs::read_to_string(Path::new(&me).with_file_name("defect"))
        .unwrap_or_default()
        .trim()
        .to_owned()
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
    // Past `systemctl`, a `systemd-run` (whose unit runs `count-run`) writes its count; any
    // other command (`mac-root-tests`'s) is only recorded.
    write_count(args, &defect);
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

/// The fake server: checks its files, listens and answers `200 ok` to everything.
fn server(args: &[String]) -> ExitCode {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    let token = args
        .iter()
        .position(|a| a == "--token-file")
        .and_then(|i| args.get(i + 1));
    let mode = token
        .and_then(|t| std::fs::metadata(t).ok())
        .map(|m| m.permissions().mode() & 0o777);
    let job_listen = args
        .iter()
        .position(|a| a == "--job-listen")
        .and_then(|i| args.get(i + 1));
    let Some(job_listen) =
        job_listen.filter(|_| mode == Some(0o600) && Path::new("pool.toml").is_file())
    else {
        eprintln!("the fake server wants pool.toml, a 0600 token file and --job-listen");
        return ExitCode::from(5);
    };
    if defect() == "server-dies" {
        eprintln!("cannot start: the fake server was asked to die");
        return ExitCode::from(3);
    }
    let listen = args
        .iter()
        .position(|a| a == "--listen")
        .and_then(|i| args.get(i + 1))
        .map_or("127.0.0.1:0", String::as_str);
    let listener = std::net::TcpListener::bind(listen).expect("bind");
    let job_listener = std::net::TcpListener::bind(job_listen).expect("bind the job service");
    if defect() != "server-no-job-address" {
        eprintln!(
            "INFO js_lease_server::server: listening for the job service addr={} tls=false",
            job_listener.local_addr().expect("job address")
        );
    }
    let announce = if defect() == "server-unannounced" {
        "bound"
    } else {
        "listening on"
    };
    println!("{announce} {}", listener.local_addr().expect("address"));
    std::io::stdout().flush().expect("flush");
    let unhealthy = defect() == "server-unhealthy";
    // Until a request for `/quit` (a test that wants the exit status; the verb's own servers are killed).
    loop {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 1024];
        let read = stream.read(&mut buffer).expect("read");
        let answer = if unhealthy {
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno"
        } else {
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
        };
        let _ = stream.write_all(answer.as_bytes());
        if buffer[..read].starts_with(b"GET /quit") {
            return ExitCode::SUCCESS;
        }
    }
}

/// The fake `buck2`: records the call and answers it as the audit, from the files of the
/// working directory.
fn buck2(args: &[String]) -> ExitCode {
    append(Path::new("buck2.calls"), &args.join(" "));
    let read = |suffix: &str, default: &str| {
        std::fs::read_to_string(format!("buck2-audit.{suffix}"))
            .unwrap_or_else(|_| default.to_owned())
    };
    print!("{}", read("stdout", "{}"));
    eprint!("{}", read("stderr", ""));
    ExitCode::from(read("status", "0").trim().parse().unwrap_or(2))
}

/// The fake table.
const TABLE: &str = "buildrules.lock.bzl";
/// The fake's one generated file beside the table, there only for a git dependency.
const ARCHIVES: &str = "third_party/buildrules/git_archives.tsv";
/// The files the fake table records.
const INPUTS: [&str; 2] = ["Cargo.toml", "Cargo.lock"];
/// The fix the real command line ends each of its lines with.
const FIX: &str = "run `buildrules sync` and commit the result";

/// The file `path` of the working directory, `None` when it cannot be read.
fn file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The sha256 of the file `path` (of nothing, when it cannot be read), in hex.
fn sha256(path: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(file(path).unwrap_or_default()))
}

/// What a sync of the working directory writes: each generated file, and its text (`None`: the
/// file is removed).
fn synced() -> [(&'static str, Option<String>); 2] {
    let table = INPUTS
        .iter()
        .map(|input| format!("{input} {}\n", sha256(input)))
        .collect();
    let git = file("Cargo.toml").is_some_and(|m| m.contains("git = "));
    [
        (TABLE, Some(table)),
        (ARCHIVES, git.then(|| "github.com/o/r 0123abc\n".to_owned())),
    ]
}

/// The fake `buildrules`: records the call, then syncs or checks as asked.
fn buildrules(args: &[String]) -> ExitCode {
    append(Path::new("buildrules.calls"), &args.join(" "));
    let defect = defect();
    if args.first().is_some_and(|verb| verb == "sync") {
        if defect == "sync-fails" {
            eprintln!("buildrules: sync: CargoFailed");
            return ExitCode::from(1);
        }
        if defect != "sync-writes-nothing" {
            for (path, text) in synced() {
                match text {
                    Some(text) => {
                        let path = Path::new(path);
                        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
                        std::fs::write(path, text).expect("write");
                    }
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
        println!("buildrules: wrote {TABLE}");
        return ExitCode::SUCCESS;
    }
    if defect == "check-usage" {
        eprintln!("buildrules: check: unknown argument --nope");
        return ExitCode::from(2);
    }
    let Some(table) = file(TABLE) else {
        eprintln!("buildrules: {TABLE} is missing: {FIX}");
        return ExitCode::from(1);
    };
    let mut stale: Vec<String> = INPUTS
        .iter()
        .filter_map(|input| {
            let now = sha256(input);
            let recorded = table
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{input} ")))
                .unwrap_or("none");
            (recorded != now).then(|| {
                format!("{input} changed since `buildrules sync` (recorded sha256 {recorded}, now {now})")
            })
        })
        .collect();
    if stale.is_empty() {
        stale = synced()
            .into_iter()
            .filter(|(path, text)| file(path) != *text)
            .map(|(path, _)| format!("{path} is not what sync writes"))
            .collect();
    }
    for line in &stale {
        eprintln!("buildrules: STALE: {line}: {FIX}");
    }
    ExitCode::from(u8::from(!stale.is_empty()))
}

/// The fake client: `job submit` is refused for want of a host, `job list` is empty.
fn job_client(args: &[String]) -> ExitCode {
    if defect() == "js-fails" {
        eprintln!("js: the fake client was asked to fail");
        return ExitCode::from(4);
    }
    // After the four global arguments: `job <verb> ...`.
    let rest = args.get(4..).unwrap_or_default();
    match rest.get(1).map(String::as_str) {
        Some("submit") if defect() == "js-accepts" => println!("job.id: demo-1"),
        Some("submit") => {
            eprintln!(
                "js: refused: no-capable-host: no host and no catalog could ever serve the job"
            );
            return ExitCode::from(3);
        }
        Some("list") if defect() == "js-list-fails" => {
            eprintln!("js: the fake client was asked to fail the list");
            return ExitCode::from(4);
        }
        Some("list") => {}
        _ => return ExitCode::from(2),
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
    } else if name == "js-lease-server" {
        server(&args)
    } else if name == "js" {
        job_client(&args)
    } else if name == "buck2" {
        buck2(&args)
    } else if name == "buildrules" {
        buildrules(&args)
    } else if name == "testbin" {
        testbin()
    } else {
        cargo(&args)
    }
}
