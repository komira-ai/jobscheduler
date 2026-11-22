//! The start of a native Mac job (`js-agent __mac-job-start`) from a process that has other
//! threads: the agent with an account pool, a tree client or a session always has some, so the
//! start must not depend on its starter having one. The test process starts threads that stay
//! alive, then starts the mode with a start file of its own user (no drop, no new audit session:
//! both need root) and the confined-writes profile on a scratch job folder. Catches a start that
//! allocates between fork and exec in its starter, a profile that is not applied, and a start
//! that does not remove its file.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;

use js_agent::mac_job::profile::{self, Kind};
use js_agent::mac_job::spec::Network;
use js_agent::mac_job::start::{START_FAILED, StartFile, VERB};
use js_testkit::dir::TestDir;

/// Starts the mode with `start` written to its file, while three other threads of this process
/// are alive; the program's output, and whether the file was removed.
fn start_with_threads(dir: &Path, start: &StartFile) -> (Output, bool) {
    let file = dir.join("start.json");
    fs::write(&file, serde_json::to_string(start).unwrap()).unwrap();
    let (stop, stopped) = mpsc::channel::<()>();
    let stopped = std::sync::Arc::new(std::sync::Mutex::new(stopped));
    let workers: Vec<_> = (0..3)
        .map(|_| {
            let stopped = std::sync::Arc::clone(&stopped);
            thread::spawn(move || {
                let _ = stopped.lock().unwrap().recv();
            })
        })
        .collect();
    let output = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args([VERB, "--start"])
        .arg(&file)
        .output()
        .unwrap();
    drop(stop);
    for worker in workers {
        worker.join().unwrap();
    }
    (output, !file.exists())
}

/// A job folder and a folder beside it, resolved (the profile names resolved paths).
fn folders(name: &str) -> (TestDir, std::path::PathBuf, std::path::PathBuf) {
    let scratch = TestDir::under(std::env::temp_dir(), name);
    let root = fs::canonicalize(scratch.path()).unwrap();
    let (job, beside) = (root.join("job"), root.join("beside"));
    fs::create_dir(&job).unwrap();
    fs::create_dir(&beside).unwrap();
    (scratch, job, beside)
}

fn start_file(job: &Path, profile: String, args: Vec<String>) -> StartFile {
    StartFile {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        new_session: false,
        asid_file: job.join("asid").display().to_string(),
        profile,
        params: vec![("JOB".to_owned(), job.display().to_string())],
        program: "/bin/sh".to_owned(),
        args,
        disclaim: false,
    }
}

#[test]
fn a_job_started_from_a_process_with_other_threads_runs_under_its_profile() {
    let (_scratch, job, beside) = folders("mac-job-start");
    let text = profile::render(Kind::ConfinedWrites, &Network::default(), "", "");
    let script = r#"echo in > "$1/in" && echo in-written; echo out > "$2/out" 2>/dev/null || echo out-refused"#;
    let start = start_file(
        &job,
        text,
        vec![
            "-c".to_owned(),
            script.to_owned(),
            "sh".to_owned(),
            job.display().to_string(),
            beside.display().to_string(),
        ],
    );
    let (output, removed) = start_with_threads(&job.join(".."), &start);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "in-written\nout-refused\n"
    );
    assert_eq!(fs::read_to_string(job.join("in")).unwrap(), "in\n");
    assert!(!beside.join("out").exists());
    assert!(removed, "the start file is removed");
}

#[test]
fn a_refused_profile_ends_the_start_before_the_program_runs() {
    let (_scratch, job, _beside) = folders("mac-job-start-refused");
    let start = start_file(
        &job,
        "(version 1)\n(no-such-operation)\n".to_owned(),
        vec!["-c".to_owned(), format!("echo ran > {}/ran", job.display())],
    );
    let (output, removed) = start_with_threads(&job.join(".."), &start);
    assert_eq!(output.status.code(), Some(i32::from(START_FAILED)));
    let err = String::from_utf8_lossy(&output.stderr);
    // The library prints its own message first; the start's line names what failed.
    let prefix = "js-agent: the start of the job failed: the Seatbelt profile was refused: ";
    assert!(err.lines().any(|line| line.starts_with(prefix)), "{err}");
    assert!(!job.join("ran").exists());
    assert!(removed);
}
