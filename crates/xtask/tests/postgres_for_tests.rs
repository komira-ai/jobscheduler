//! `cargo xtask postgres-for-tests`, run as the built `xtask` program with stand-in `initdb`,
//! `pg_ctl` and `pg_isready` (small shell scripts in the test's own scratch directory) that log
//! their arguments, one per line, and end with the status the test chooses. Nothing real is
//! started.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory of its own for each test, holding the stand-ins in `bin/`.
struct Rig {
    dir: PathBuf,
}

/// What one run did.
#[derive(Debug)]
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Rig {
    /// Stand-ins that end with the statuses given for `initdb`, `pg_ctl` and `pg_isready`.
    fn new(name: &str, statuses: [i32; 3]) -> Rig {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("postgres-for-tests-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        for (program, status) in ["initdb", "pg_ctl", "pg_isready"].iter().zip(statuses) {
            let path = dir.join("bin").join(program);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\n{{ echo '== {program}'; printf '%s\\n' \"$@\"; }} >> \"{}\"\nexit {status}\n",
                    dir.join("calls").display()
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Rig { dir }
    }

    fn data(&self) -> PathBuf {
        self.dir.join("data")
    }

    fn env_file(&self) -> PathBuf {
        self.dir.join("github-env")
    }

    /// The log of the stand-ins: each call as `== <program>` and its arguments; empty when
    /// none ran.
    fn calls(&self) -> String {
        fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
    }

    fn run_with(&self, bin_dir: &Path, env_file: &Path, extra: &[&str]) -> Ran {
        let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("postgres-for-tests")
            .arg("--bin-dir")
            .arg(bin_dir)
            .arg("--data-dir")
            .arg(self.data())
            .arg("--socket-dir")
            .arg(self.dir.join("sock"))
            .arg("--github-env")
            .arg(env_file)
            .args(extra)
            .output()
            .unwrap();
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
        }
    }

    fn run(&self, extra: &[&str]) -> Ran {
        self.run_with(&self.dir.join("bin"), &self.env_file(), extra)
    }
}

/// The calls the three stand-ins log on a run with this port and a rig in `dir`.
fn expected_calls(dir: &Path, port: &str) -> String {
    let data = dir.join("data").display().to_string();
    let sock = dir.join("sock").display().to_string();
    format!(
        "== initdb\n--username\npostgres\n--auth\ntrust\n--encoding\nUTF8\n-D\n{data}\n\
         == pg_ctl\n-D\n{data}\n-l\n{data}.log\n-o\n-h 127.0.0.1 -p {port} -k {sock}\n-w\nstart\n\
         == pg_isready\n-h\n127.0.0.1\n-p\n{port}\n"
    )
}

/// Asserts that a run where all three programs succeed runs them in order with the exact
/// arguments (the `-o` value one entry), appends exactly one URL line to the environment file
/// and keeps what it held, and prints one line for each step.
#[test]
fn starts_the_server_and_appends_its_url_keeping_the_env_file() {
    let rig = Rig::new("success", [0, 0, 0]);
    fs::write(rig.env_file(), "A=1\n").unwrap();
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(rig.calls(), expected_calls(&rig.dir, "5432"));
    assert_eq!(
        fs::read_to_string(rig.env_file()).unwrap(),
        "A=1\nKBF_TEST_POSTGRES_URL=postgres://postgres@127.0.0.1:5432/postgres\n"
    );
    assert_eq!(
        ran.stdout,
        format!(
            "ran initdb\nran pg_ctl\nran pg_isready\nrecorded KBF_TEST_POSTGRES_URL=postgres://postgres@127.0.0.1:5432/postgres in {}\n",
            rig.env_file().display()
        )
    );
}

/// Asserts that an env file that does not exist yet is created holding the one line, and that
/// an existing but empty data directory is accepted.
#[test]
fn creates_the_env_file_and_accepts_an_empty_data_dir() {
    let rig = Rig::new("empty-data", [0, 0, 0]);
    fs::create_dir_all(rig.data()).unwrap();
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(
        fs::read_to_string(rig.env_file()).unwrap(),
        "KBF_TEST_POSTGRES_URL=postgres://postgres@127.0.0.1:5432/postgres\n"
    );
}

/// Asserts that `--port` is the port of the server's options, of the readiness check and of the
/// URL.
#[test]
fn the_port_flag_is_used_in_the_options_the_check_and_the_url() {
    let rig = Rig::new("port", [0, 0, 0]);
    let ran = rig.run(&["--port", "6543"]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(rig.calls(), expected_calls(&rig.dir, "6543"));
    assert_eq!(
        fs::read_to_string(rig.env_file()).unwrap(),
        "KBF_TEST_POSTGRES_URL=postgres://postgres@127.0.0.1:6543/postgres\n"
    );
}

/// Asserts that a data directory that holds a file is refused with no program run and the env
/// file untouched.
#[test]
fn a_non_empty_data_dir_is_refused_before_anything_runs() {
    let rig = Rig::new("non-empty", [0, 0, 0]);
    fs::create_dir_all(rig.data()).unwrap();
    fs::write(rig.data().join("PG_VERSION"), "17\n").unwrap();
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        format!(
            "xtask: {} already exists and is not empty\n",
            rig.data().display()
        )
    );
    assert_eq!(rig.calls(), "");
    assert!(!rig.env_file().exists());
}

/// Asserts that each program ending with a failure status fails the verb naming the program and
/// the status, stops before the next one, and leaves the env file as it was.
#[test]
fn a_failing_program_stops_the_run_and_names_itself_and_its_status() {
    for (failing, name) in [(0, "initdb"), (1, "pg_ctl"), (2, "pg_isready")] {
        let mut statuses = [0, 0, 0];
        statuses[failing] = 3;
        let rig = Rig::new(&format!("fail-{name}"), statuses);
        fs::write(rig.env_file(), "A=1\n").unwrap();
        let ran = rig.run(&[]);
        assert_eq!(ran.status, 1, "{name}: {ran:?}");
        assert_eq!(
            ran.stderr,
            format!("xtask: {name} failed: exit status: 3\n"),
            "{name}"
        );
        // The log holds the calls up to and including the failing program, and no later one.
        let calls = rig.calls();
        let ran_programs: Vec<&str> = calls
            .lines()
            .filter_map(|l| l.strip_prefix("== "))
            .collect();
        let all = ["initdb", "pg_ctl", "pg_isready"];
        assert_eq!(ran_programs, all[..=failing].to_vec(), "{name}");
        assert_eq!(
            fs::read_to_string(rig.env_file()).unwrap(),
            "A=1\n",
            "{name}"
        );
    }
}

/// Asserts that a missing program fails the verb naming its path and the reason, with the env
/// file not created.
#[test]
fn a_missing_program_fails_naming_its_path() {
    let rig = Rig::new("missing", [0, 0, 0]);
    let bin = rig.dir.join("no-bin");
    let ran = rig.run_with(&bin, &rig.env_file(), &[]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.stderr,
        format!(
            "xtask: cannot run {}: No such file or directory (os error 2)\n",
            bin.join("initdb").display()
        )
    );
    assert_eq!(rig.calls(), "");
    assert!(!rig.env_file().exists());
}

/// Asserts that an env file that cannot be appended to (a directory) fails the verb naming it,
/// after the three programs ran.
#[test]
fn an_env_file_that_cannot_be_appended_to_fails_naming_it() {
    let rig = Rig::new("env-unwritable", [0, 0, 0]);
    let env = rig.dir.join("env-dir");
    fs::create_dir_all(&env).unwrap();
    let ran = rig.run_with(&rig.dir.join("bin"), &env, &[]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.stderr,
        format!(
            "xtask: cannot write {}: Is a directory (os error 21)\n",
            env.display()
        )
    );
    assert_eq!(rig.calls(), expected_calls(&rig.dir, "5432"));
}
