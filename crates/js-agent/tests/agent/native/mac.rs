//! The runtime's command line applied for real, on a Mac, as the user running the tests: the
//! fake helper starts the `run` it is asked for (`real_runs`), so `sandbox-exec` reads the
//! profile the runtime built. What this cannot show is the change of user, which needs root
//! (`tests/native_privileged.rs`).

use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

use js_agent::lifecycle::{Exit, Runtime, Status};

use super::Rig;

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Asserts that a program runs under the profile the runtime builds, and that what it prints
/// reaches the log. It catches a profile `sandbox-exec` refuses to read, a parameter
/// spelled wrong, and a program that cannot start under it.
#[test]
fn a_program_runs_under_the_profile_and_its_output_reaches_the_log() {
    let mut rig = Rig::new("mac-echo");
    rig.fake.real_runs();
    let id = rig.start("r1", "/bin/echo", &["hello", "native"]);
    assert_eq!(rig.settle(&id), exited(0));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "hello native\n");
    assert_eq!(tail.stderr, "");
}

/// Asserts that the profile grants writes in the run directory and nowhere else: a shell
/// writes a file in its working directory, and the same shell is refused a file in a
/// directory every user may write. It catches a profile that does not confine the program.
#[test]
fn the_program_writes_in_its_run_directory_and_nowhere_else() {
    static UNIQUE: AtomicU32 = AtomicU32::new(0);
    let mut rig = Rig::new("mac-write");
    rig.fake.real_runs();
    let inside = rig.start("r1", "/bin/sh", &["-c", "echo data > made.txt"]);
    assert_eq!(rig.settle(&inside), exited(0));
    let made = rig.fake.home_of("7.0").join("exec").join("made.txt");
    assert_eq!(fs::read_to_string(made).unwrap(), "data\n");

    let outside = std::path::PathBuf::from(format!(
        "/private/tmp/js-native-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    ));
    let script = format!("echo data > {}", outside.display());
    let denied = rig.start("r2", "/bin/sh", &["-c", &script]);
    let status = rig.settle(&denied);
    let written = outside.exists();
    // Removed before anything is asserted, so a profile that fails leaves nothing behind.
    let _ = fs::remove_file(&outside);
    assert!(
        !written,
        "the profile let the program write {}",
        outside.display()
    );
    assert_eq!(status, exited(1));
}
