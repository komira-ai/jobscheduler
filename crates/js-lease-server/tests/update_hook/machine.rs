//! The hook over the machine's own ports: the program runner, the `/healthz` look, the store
//! probe over SQLite and Postgres, the store file, and `run_step` wired over a declaration, a
//! state directory and a store of the test's own.

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::JoinHandle;
use std::time::Duration;

use js_lease_server::update_hook::{
    DECLARATION_PATH, Done, HEALTHZ_LIMIT, Health, Host, HttpHealth, Invocation, Machine, Outcome,
    Params, Reason, Schema, Step, StoreProbe, StoreUnreachable, StoreView, SystemRunner, Writer,
    judge, run, run_step, store_target,
};
use js_state::postgres::schema::Supported;
use js_state::postgres::{PostgresConfig, TestDatabase, test_database};
use js_state::{WriterLock, WriterTarget};
use js_testkit::dir::TestDir;
use js_update_hook::CommandRunner;

use crate::support::{Rig, as_root};

const MARKER: &str = "MARKER-SECRET-7f3a91";

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("machine-{name}"))
}

/// A port nothing listens on: bound, noted and released.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Writes `text` to `path` with the mode `mode`.
fn write_mode(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

// ---------------------------------------------------------------- SystemRunner

/// Asserts that a program that exists and exits 0 gives back what it printed, with and
/// without an environment of its own.
#[test]
fn a_program_that_exits_zero_returns_its_output() {
    let me = std::env::current_exe().unwrap();
    let me = me.to_str().unwrap();
    let out = SystemRunner
        .run(me, &["--list", "--exact", "no-such-test"])
        .unwrap();
    assert!(out.contains("0 tests"), "{out}");
    let in_env = SystemRunner
        .run_in(me, &["--list", "--exact", "no-such-test"], &[("A", "b")])
        .unwrap();
    assert_eq!(in_env, out);
}

/// Asserts that a program that exits with a failure status is an error naming the program and
/// the status, and that a program that does not exist is an error naming the program.
#[test]
fn a_program_that_fails_or_is_missing_is_an_error_naming_it() {
    let me = std::env::current_exe().unwrap();
    let me = me.to_str().unwrap();
    let failed = SystemRunner.run(me, &["--no-such-flag"]).unwrap_err();
    assert!(
        failed.starts_with(&format!("{me}: exit status: ")),
        "{failed}"
    );

    let dir = scratch("runner");
    let missing = dir.path().join("no-such-program");
    let missing = missing.to_str().unwrap();
    let err = SystemRunner.run(missing, &[]).unwrap_err();
    assert!(err.starts_with(&format!("{missing}: ")), "{err}");
}

// ---------------------------------------------------------------- HttpHealth

/// Serves one connection on a loopback port: reads the request, answers `answer`, and
/// returns the request's first line.
fn serve_once(answer: &'static [u8]) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
        }
        stream.write_all(answer).unwrap();
        String::from_utf8(request)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned()
    });
    (addr, handle)
}

fn status_of(answer: &'static [u8]) -> (Result<u16, String>, String) {
    let (addr, server) = serve_once(answer);
    let status = HttpHealth::new(addr, HEALTHZ_LIMIT).status();
    (status, server.join().unwrap())
}

/// Asserts that a 200 answer reads as 200 and a 503 as 503, and that the request line the
/// server received is `GET /healthz HTTP/1.1`.
#[test]
fn healthz_status_is_read_from_the_answer() {
    let (ok, line) = status_of(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(ok, Ok(200));
    assert_eq!(line, "GET /healthz HTTP/1.1");
    let (down, line) = status_of(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
    assert_eq!(down, Ok(503));
    assert_eq!(line, "GET /healthz HTTP/1.1");
}

/// Asserts that an answer that is not HTTP/1.x with a status (garbage, a wrong version, a
/// status that is not a number, no status, nothing at all) is an error naming the address.
#[test]
fn an_answer_that_is_not_http_is_an_error() {
    for answer in [
        &b"garbage that is not http\r\n"[..],
        b"HTTP/2 200 OK\r\n",
        b"HTTP/1.1 abc\r\n",
        b"HTTP/1.1\r\n",
        b"",
    ] {
        let (addr, server) = serve_once(Box::leak(answer.to_vec().into_boxed_slice()));
        let err = HttpHealth::new(addr, HEALTHZ_LIMIT).status().unwrap_err();
        server.join().unwrap();
        assert_eq!(
            err,
            format!("GET http://{addr}/healthz: the answer is not HTTP/1.x with a status"),
            "{answer:?}"
        );
    }
}

/// Asserts that a port nothing listens on is an error that starts with the request's URL.
#[test]
fn a_closed_port_is_an_error_naming_the_url() {
    let port = closed_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let err = HttpHealth::new(addr, Duration::from_secs(5))
        .status()
        .unwrap_err();
    assert!(
        err.starts_with(&format!("GET http://127.0.0.1:{port}/healthz: ")),
        "{err}"
    );
}

// ---------------------------------------------------------------- judge

/// Asserts that a version below the oldest is too old, and the oldest, one between and one
/// above the newest run.
#[test]
fn a_schema_below_the_oldest_is_too_old() {
    let range = Supported { min: 3, max: 5 };
    assert_eq!(judge(2, range), Schema::TooOld { found: 2, min: 3 });
    assert_eq!(judge(3, range), Schema::Runs(3));
    assert_eq!(judge(4, range), Schema::Runs(4));
    assert_eq!(judge(9, range), Schema::Runs(9));
}

// ---------------------------------------------------------------- StoreProbe, SQLite

/// Asserts that a SQLite store nobody holds is free and unchecked, that a held one reports the
/// holder's own text, and that probing never takes the lock: a second acquire is still refused
/// after the probes, and succeeds once the first holder lets go.
#[test]
fn a_sqlite_probe_reports_the_holder_and_never_takes_the_lock() {
    let dir = scratch("sqlite");
    let target = WriterTarget::Sqlite(dir.path().join("state.db"));
    let probe = StoreProbe::new(target.clone());
    assert_eq!(probe.writer(), Ok(Writer::Free));
    assert_eq!(probe.schema(), Ok(Schema::Unchecked));

    let held = WriterLock::acquire(&target).unwrap();
    let js_state::LockState::Held { holder } = WriterLock::probe(&target).unwrap() else {
        panic!("the lock is held");
    };
    assert_eq!(
        probe.writer(),
        Ok(Writer::Held {
            holder: holder.to_string()
        })
    );
    assert_eq!(probe.writer(), probe.writer());
    assert!(WriterLock::acquire(&target).is_err());
    drop(held);
    assert_eq!(probe.writer(), Ok(Writer::Free));
    assert!(WriterLock::acquire(&target).is_ok());
}

/// Asserts that a SQLite lock file that cannot be read as a token (a directory in its
/// place) makes the store unreachable.
#[test]
fn a_sqlite_lock_file_that_cannot_be_read_is_unreachable() {
    let dir = scratch("sqlite-bad");
    let db = dir.path().join("state.db");
    fs::create_dir(dir.path().join("state.db.lock")).unwrap();
    let probe = StoreProbe::new(WriterTarget::Sqlite(db));
    assert_eq!(probe.writer(), Err(StoreUnreachable));
}

// ---------------------------------------------------------------- StoreProbe, Postgres

fn postgres_target(database: &TestDatabase) -> WriterTarget {
    let config = PostgresConfig::parse(&database.url()).unwrap();
    WriterTarget::Postgres(Box::new(config))
}

/// Asserts that a fresh test database has a schema this release runs on, and that its writer
/// is free, then held while a lock is taken, with the holder the lock's own, and that the
/// probe left the lock alone.
#[test]
fn a_postgres_probe_reads_the_schema_and_the_writer() {
    let database = test_database();
    let target = postgres_target(&database);
    let probe = StoreProbe::new(target.clone());
    assert_eq!(
        probe.schema(),
        Ok(Schema::Runs(Supported::this_build().max))
    );
    assert_eq!(probe.writer(), Ok(Writer::Free));
    let held = WriterLock::acquire(&target).unwrap();
    let js_state::LockState::Held { holder } = WriterLock::probe(&target).unwrap() else {
        panic!("the lock is held");
    };
    assert_eq!(
        probe.writer(),
        Ok(Writer::Held {
            holder: holder.to_string()
        })
    );
    assert!(WriterLock::acquire(&target).is_err());
    drop(held);
}

/// Asserts that a database with no schema reads as `Missing`.
#[test]
fn a_postgres_database_without_a_schema_is_missing() {
    let database = TestDatabase::empty();
    let probe = StoreProbe::new(postgres_target(&database));
    assert_eq!(probe.schema(), Ok(Schema::Missing));
}

/// Asserts that a server that cannot be reached makes both the writer probe and the schema
/// read `StoreUnreachable`, whose text carries nothing of the URL.
#[test]
fn an_unreachable_postgres_is_store_unreachable() {
    let url = format!("postgres://u:{MARKER}@127.0.0.1:{}/db", closed_port());
    let config = PostgresConfig::parse(&url).unwrap();
    let probe = StoreProbe::new(WriterTarget::Postgres(Box::new(config)));
    assert_eq!(probe.writer(), Err(StoreUnreachable));
    assert_eq!(probe.schema(), Err(StoreUnreachable));
    assert_eq!(
        StoreUnreachable.to_string(),
        "the store could not be reached"
    );
}

// ---------------------------------------------------------------- store_target

/// Asserts that a 0600 file holding `sqlite:<absolute path>` names that SQLite store.
#[test]
fn an_owner_only_sqlite_file_names_the_store() {
    let dir = scratch("target-ok");
    let file = dir.path().join("store");
    write_mode(&file, "sqlite:/abs/path/state.db\n", 0o600);
    match store_target(&file).unwrap() {
        WriterTarget::Sqlite(path) => assert_eq!(path, PathBuf::from("/abs/path/state.db")),
        WriterTarget::Postgres(_) => panic!("a SQLite store"),
    }
}

/// Asserts that a 0600 file holding a Postgres URL names a Postgres store.
#[test]
fn an_owner_only_postgres_file_names_the_store() {
    let dir = scratch("target-pg");
    let file = dir.path().join("store");
    write_mode(&file, "postgres://js:pw@127.0.0.1:5432/js\n", 0o600);
    assert!(matches!(store_target(&file), Ok(WriterTarget::Postgres(_))));
}

/// Asserts that a file others can read is refused by the exact text naming the path, that a
/// directory, a missing file and a file that is not one store line are refused, and that
/// a Postgres URL that does not parse is refused without its secret.
#[test]
fn a_store_file_that_is_not_private_or_not_one_line_is_refused() {
    let dir = scratch("target-bad");
    let open = dir.path().join("open");
    write_mode(&open, "sqlite:/abs/x\n", 0o644);
    assert_eq!(
        store_target(&open).unwrap_err(),
        format!(
            "--store: {}: must be a regular file that only its owner can read or write",
            open.display()
        )
    );

    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o600)).unwrap();
    let refused = store_target(&sub).unwrap_err();
    assert!(
        refused.starts_with(&format!("--store: {}: ", sub.display())),
        "{refused}"
    );

    let missing = dir.path().join("missing");
    let refused = store_target(&missing).unwrap_err();
    assert!(
        refused.starts_with(&format!("--store: {}: ", missing.display())),
        "{refused}"
    );

    let junk = dir.path().join("junk");
    write_mode(&junk, "mysql://nope\n", 0o600);
    assert_eq!(
        store_target(&junk).unwrap_err(),
        format!(
            "--store: {} must hold one line: postgres://..., postgresql://... or sqlite:<absolute path>",
            junk.display()
        )
    );

    let secret = dir.path().join("secret");
    write_mode(
        &secret,
        &format!("postgres://js:{MARKER}@host:notaport/js\n"),
        0o600,
    );
    let refused = store_target(&secret).unwrap_err();
    assert!(!refused.contains(MARKER), "{refused}");
    assert!(
        refused.starts_with(&format!("--store: {}: ", secret.display())),
        "{refused}"
    );
}

// ---------------------------------------------------------------- Machine

/// Asserts that this machine's declaration is the installed path and that its stat looks at an
/// open directory and reports its real owner and mode.
#[test]
fn the_system_machine_looks_at_the_directory_it_is_given() {
    let machine = Machine::system();
    assert_eq!(machine.declaration, Path::new(DECLARATION_PATH));
    let dir = scratch("system-stat");
    let handle = fs::File::open(dir.path()).unwrap();
    let meta = fs::metadata(dir.path()).unwrap();
    let seen = (machine.stat)(dir.path(), handle.as_fd()).unwrap();
    assert!(seen.is_dir && !seen.is_symlink);
    assert_eq!(seen.uid, meta.uid());
    assert_eq!(seen.gid, meta.gid());
    assert_eq!(seen.mode, meta.mode() & 0o7777);
}

/// A runner that answers every call with an empty success and keeps the calls.
#[derive(Default)]
struct FakeRunner {
    calls: RefCell<Vec<String>>,
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.run_in(program, args, &[])
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        _env: &[(&str, &str)],
    ) -> Result<String, String> {
        self.calls
            .borrow_mut()
            .push(format!("{program} {}", args.join(" ")));
        Ok("ActiveState=active\nSubState=running\n".to_owned())
    }
}

/// A rig whose declaration is written to a file, with the release link in place.
struct Hosted {
    rig: Rig,
    declaration: PathBuf,
    runner: FakeRunner,
}

fn toml_list(paths: &[PathBuf]) -> String {
    let quoted: Vec<String> = paths
        .iter()
        .map(|p| format!("{:?}", p.display().to_string()))
        .collect();
    quoted.join(", ")
}

impl Hosted {
    fn new(name: &str) -> Hosted {
        let rig = Rig::new(&format!("machine-{name}"));
        symlink(rig.previous(), rig.path("link/current")).unwrap();
        let t = rig.declaration.timeouts;
        let text = format!(
            "state_dir = {:?}\n[hook]\nunits = [{:?}]\nwrite_paths = [{}]\n[hook.timeout_secs]\nprepare = {}\ncutover = {}\nverify = {}\nrollback = {}\ncleanup = {}\n",
            rig.declaration.state_dir.display().to_string(),
            rig.declaration.unit,
            toml_list(&rig.declaration.write_paths),
            t.prepare,
            t.cutover,
            t.verify,
            t.rollback,
            t.cleanup,
        );
        let declaration = rig.path("declaration.toml");
        fs::write(&declaration, text).unwrap();
        Hosted {
            rig,
            declaration,
            runner: FakeRunner::default(),
        }
    }

    fn machine(&self) -> Machine<'_> {
        Machine {
            declaration: &self.declaration,
            stat: &as_root,
            runner: &self.runner,
        }
    }

    fn store_file(&self, text: &str) -> PathBuf {
        let file = self.rig.path("store");
        write_mode(&file, text, 0o600);
        file
    }

    fn invocation(
        &self,
        step: Step,
        store: Option<PathBuf>,
        healthz: Option<SocketAddr>,
    ) -> Invocation {
        let flags = js_lease_server::update_hook::Flags {
            store,
            healthz,
            release_link: self.rig.params.release_link.clone(),
            ..Default::default()
        };
        Invocation {
            flags,
            step,
            artifact: None,
            params: self.rig.path("params.toml"),
        }
    }

    fn run(&self, step: Step, store: Option<PathBuf>, healthz: Option<SocketAddr>) -> Done {
        let invocation = self.invocation(step, store, healthz);
        run_step(&invocation, &self.rig.params, &self.machine())
    }
}

fn some_addr() -> Option<SocketAddr> {
    Some("127.0.0.1:9".parse().unwrap())
}

/// Asserts that a declaration that is missing fails the step with exit 1 `state_invalid`.
#[test]
fn a_missing_declaration_is_state_invalid() {
    let hosted = Hosted::new("no-declaration");
    fs::remove_file(&hosted.declaration).unwrap();
    let store = hosted.store_file("sqlite:/abs/x\n");
    let done = hosted.run(Step::Cleanup, Some(store), some_addr());
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
}

/// Asserts that a missing `--store` or `--healthz-url`, or a store file that cannot be read,
/// fails with `bad_params` and the text naming the flag.
#[test]
fn the_flags_the_hook_needs_are_required() {
    let hosted = Hosted::new("flags");
    let store = hosted.store_file("sqlite:/abs/x\n");

    let done = hosted.run(Step::Cleanup, None, some_addr());
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::BadParams));
    assert_eq!(done.report.detail.as_deref(), Some("--store is required"));

    let done = hosted.run(Step::Cleanup, Some(store), None);
    assert_eq!(done.report.reason, Some(Reason::BadParams));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("--healthz-url is required")
    );

    let gone = hosted.rig.path("no-such-store");
    let done = hosted.run(Step::Cleanup, Some(gone.clone()), some_addr());
    assert_eq!(done.report.reason, Some(Reason::BadParams));
    assert!(
        done.report
            .detail
            .unwrap()
            .starts_with(&format!("--store: {}: ", gone.display()))
    );
}

/// Asserts that cleanup with no record succeeds over the wired ports, touching the state
/// directory only (no program is run).
#[test]
fn cleanup_without_a_record_succeeds_over_the_machine() {
    let hosted = Hosted::new("cleanup");
    let store = hosted.store_file("sqlite:/abs/x\n");
    let done = hosted.run(Step::Cleanup, Some(store), some_addr());
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(hosted.runner.calls.borrow().is_empty());
}

/// Asserts that prepare over a SQLite store and a real release link succeeds and writes the
/// record with no holder (nobody holds the lock) and the link's previous target.
#[test]
fn prepare_over_a_sqlite_store_and_a_real_link_writes_the_record() {
    let hosted = Hosted::new("prepare");
    let db = hosted.rig.path("store.db");
    let store = hosted.store_file(&format!("sqlite:{}\n", db.display()));
    let done = hosted.run(Step::Prepare, Some(store), some_addr());
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let record = hosted.rig.record().expect("the record");
    assert_eq!(record.holder, None);
    assert_eq!(record.store_schema, None);
}

// ---------------------------------------------------------------- no_step_prints_the_connection

fn args_of(
    hosted: &Hosted,
    store: &Path,
    healthz: &str,
    step: &str,
    params: &Path,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec!["js-lease-server".into(), "update-hook".into()];
    args.push(
        format!(
            "--release-link={}",
            hosted.rig.path("link/current").display()
        )
        .into(),
    );
    args.push(format!("--store={}", store.display()).into());
    args.push(format!("--healthz-url={healthz}").into());
    args.push(step.into());
    if step == "prepare" {
        args.push(hosted.rig.staged().into());
    }
    args.push("--params".into());
    args.push(params.into());
    args
}

fn params_file(hosted: &Hosted) -> PathBuf {
    let path = hosted.rig.path("params.toml");
    let p = &hosted.rig.params;
    fs::write(
        &path,
        format!(
            "\"updater.staged_release\" = {:?}\n\"updater.previous_release\" = {:?}\n\"updater.target_version\" = \"2.0.0\"\n\"updater.previous_version\" = \"1.0.0\"\ndefer_retry_secs = 60\n",
            p.staged_release.display().to_string(),
            p.previous_release.as_ref().unwrap().display().to_string(),
        ),
    )
    .unwrap();
    path
}

/// Runs the verb over `hosted`'s machine; returns the exit code, stdout and stderr.
fn run_verb(hosted: &Hosted, args: &[std::ffi::OsString]) -> (ExitCode, String, String) {
    let machine = hosted.machine();
    let step =
        move |invocation: &Invocation, params: &Params| run_step(invocation, params, &machine);
    let host = Host { step: &step };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(args, &mut out, &mut err, &host);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Asserts that with a store file holding a Postgres URL with a secret and a closed port, none
/// of the five steps prints the secret on standard output, standard error or in the report;
/// and that `prepare` defers with exit 75 and `store_unreachable`.
#[test]
fn no_step_prints_the_connection() {
    let hosted = Hosted::new("no-print");
    let store = hosted.store_file(&format!(
        "postgres://js:{MARKER}@127.0.0.1:{}/js\n",
        closed_port()
    ));
    let params = params_file(&hosted);
    for step in ["prepare", "cutover", "verify", "rollback", "cleanup"] {
        let args = args_of(&hosted, &store, "http://127.0.0.1:9", step, &params);
        let (code, out, err) = run_verb(&hosted, &args);
        assert!(!out.contains(MARKER), "{step}: {out}");
        assert!(!err.contains(MARKER), "{step}: {err}");
        assert!(!format!("{code:?}").contains(MARKER));
        if step == "prepare" {
            assert_eq!(code, ExitCode::from(75), "{out}");
            assert!(out.contains("\"reason\":\"store_unreachable\""), "{out}");
        }
    }
}

/// Asserts that a store file whose URL does not parse and holds a secret fails every step
/// with `bad_params` (exit 1) and prints the secret nowhere.
#[test]
fn an_unparsable_store_url_is_refused_without_its_secret() {
    let hosted = Hosted::new("no-print-parse");
    let store = hosted.store_file(&format!("postgres://js:{MARKER}@host:notaport/js\n"));
    let params = params_file(&hosted);
    for step in ["prepare", "cutover", "verify", "rollback", "cleanup"] {
        let args = args_of(&hosted, &store, "http://127.0.0.1:9", step, &params);
        let (code, out, err) = run_verb(&hosted, &args);
        assert_eq!(code, ExitCode::from(1), "{step}: {out}");
        assert!(out.contains("\"reason\":\"bad_params\""), "{out}");
        assert!(!out.contains(MARKER), "{step}: {out}");
        assert!(!err.contains(MARKER), "{step}: {err}");
    }
}
