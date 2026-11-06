//! A job start's environment on the native runtime: laid over the environment the agent
//! builds, carried to the helper in the `run` request's environment and never on the command
//! line, and checked before a user is made.

use js_agent::lifecycle::Runtime;

use super::{Rig, native};

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

/// Asserts that a job start's environment reaches the helper's `run`: `A` is there, the
/// job's `PATH` replaces the one the body set (the agent's base), `TMPDIR` stays the agent's,
/// and no value of the job's environment is anywhere in the command line. Admission passes
/// a non-empty environment. It catches the job's environment dropped, the agent's value
/// kept over the job's, and a value put on the command line, which every user of the Mac
/// can read.
#[test]
fn a_job_env_reaches_the_helper_over_the_agents_and_stays_off_the_command_line() {
    let mut rig = Rig::new("env-over");
    let env = vec![pair("A", "value-a-17"), pair("PATH", "/job/path")];
    assert_eq!(rig.rt.check_env(&env), Ok(()));
    let mut body = native("/bin/true", &[]);
    body.env.insert("PATH".to_owned(), "/agent/path".to_owned());
    body.env.insert("B".to_owned(), "kept".to_owned());
    let mut spec = rig.spec("r1", body);
    spec.env = env;
    rig.rt.create_and_start(&spec).unwrap();
    let run = rig.fake.run_of("7.0");
    let tmp = rig.fake.home_of("7.0").join("tmp").display().to_string();
    assert_eq!(
        run.env,
        [
            pair("B", "kept"),
            pair("TMPDIR", &tmp),
            pair("A", "value-a-17"),
            pair("PATH", "/job/path"),
        ]
    );
    let leaked: Vec<&String> = run
        .argv
        .iter()
        .filter(|arg| arg.contains("value-a-17") || arg.contains("/job/path"))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
}

/// Asserts that a job start's environment the wire would refuse fails the start before the
/// helper is asked for a user: a reserved name (`TMPDIR`, which the agent sets, and
/// `JS_FILES_DIR`, the agent's prefix) and a name given twice. It catches a job that
/// replaces the run's temporary directory or the path of its files, and an environment with
/// two values for one name, of which the helper would set either.
#[test]
fn a_job_env_the_wire_refuses_makes_no_user() {
    let mut rig = Rig::new("env-refused");
    let cases = [
        (vec![pair("TMPDIR", "/elsewhere")], "env uses the reserved"),
        (
            vec![pair("JS_FILES_DIR", "/elsewhere")],
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
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
}
