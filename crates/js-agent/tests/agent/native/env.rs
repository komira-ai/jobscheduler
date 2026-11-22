//! A job start's environment on the native runtime: laid over the environment the agent
//! builds, carried to the supervisor as its environment (the whole environment of the job)
//! and never on the command line, and checked before an account is lent.

use js_agent::lifecycle::Runtime;

use super::{Rig, native};

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

/// Asserts that a job start's environment reaches the supervisor: `A` is there, the job's
/// `PATH` replaces the one the body set (the agent's base), `TMPDIR` stays the agent's, the
/// account's names follow last, and no value of the job's environment is anywhere in the
/// program's arguments. Admission passes a non-empty environment.
///
/// Catches: the job's environment dropped, the agent's value kept over the job's, and a value
/// put on the command line, which every user of the Mac can read.
#[test]
fn a_job_env_reaches_the_supervisor_over_the_agents_and_stays_off_the_command_line() {
    let mut rig = Rig::new("env-over");
    let env = vec![pair("A", "value-a-17"), pair("PATH", "/job/path")];
    assert_eq!(rig.rt.check_env(&env), Ok(()));
    let mut body = native("/bin/echo", &["plain"]);
    body.env.insert("PATH".to_owned(), "/agent/path".to_owned());
    body.env.insert("B".to_owned(), "kept".to_owned());
    let mut spec = rig.spec("r1", body);
    spec.env = env;
    let id = rig.rt.create_and_start(&spec).unwrap();
    let launched = rig.launcher.last();
    let name = launched.var("USER").unwrap().to_owned();
    let exec = rig.home_of(&name).join("exec");
    let tmp = exec.join(".tmp");
    let job_home = exec.join(".home").display().to_string();
    assert_eq!(
        launched.env,
        [
            pair("B", "kept"),
            pair("TMPDIR", &tmp.display().to_string()),
            pair("A", "value-a-17"),
            pair("PATH", "/job/path"),
            pair("HOME", &job_home),
            pair("CFFIXED_USER_HOME", &job_home),
            pair("USER", &name),
            pair("LOGNAME", &name),
            pair("xcrun_db", &tmp.join("xcrun_db").display().to_string()),
        ]
    );
    let leaked: Vec<&String> = launched
        .args
        .iter()
        .chain([&launched.program])
        .filter(|arg| arg.contains("value-a-17") || arg.contains("/job/path"))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a job start's environment the wire would refuse fails the start before an
/// account is lent: a reserved name (`TMPDIR`, which the agent sets, `JS_FILES_DIR`, the
/// agent's prefix, and `DEVELOPER_DIR`, which only a start's Xcode sets) and a name given
/// twice. No supervisor is started, no lease is written and the pool's ready account is
/// still ready.
///
/// Catches: a job that replaces the run's temporary directory or the path of its files, and
/// an environment with two values for one name, of which the program would see either.
#[test]
fn a_job_env_the_wire_refuses_lends_no_account() {
    let mut rig = Rig::new("env-refused");
    let ready = rig.accounts.records();
    let cases = [
        (vec![pair("TMPDIR", "/elsewhere")], "env uses the reserved"),
        (
            vec![pair("JS_FILES_DIR", "/elsewhere")],
            "env uses the reserved",
        ),
        (
            vec![pair("DEVELOPER_DIR", "/elsewhere")],
            "env uses the reserved",
        ),
        (
            vec![pair("A", "1"), pair("A", "2")],
            "env names a variable twice",
        ),
    ];
    for (env, said) in cases {
        let mut spec = rig.spec("r1", native("/bin/true", &[]));
        spec.env = env.clone();
        let error = rig.rt.create_and_start(&spec).unwrap_err();
        assert!(error.0.starts_with(said), "{env:?}: {}", error.0);
    }
    assert!(rig.launcher.launched().is_empty());
    assert!(rig.pool.held().is_empty());
    assert_eq!(rig.accounts.records(), ready);
    assert!(
        !rig.dir.path("native-leases").exists(),
        "the journal was touched"
    );
}
