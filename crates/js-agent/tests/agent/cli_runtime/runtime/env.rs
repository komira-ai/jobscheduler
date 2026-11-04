//! A job's environment on the `create` call: one `--env` pair for each variable, after the
//! labels and before `--` and the image, each value one argument whatever it holds; none
//! for a job without one; and an environment the wire's check refuses never reaches the
//! engine.

use std::collections::BTreeMap;

use js_agent::cli_runtime::{ArgsError, CliRuntime, env_args};
use js_agent::lifecycle::{RunSpec, Runtime};

use super::super::scripted::Scripted;
use super::super::{created, docker, open, open_with, settings, spec};
use super::subs;

/// A Podman runtime over a new world.
fn podman() -> (Scripted, CliRuntime) {
    let world = Scripted::new("podman version 5.2.1");
    let rt = open_with(&world, "linux", settings());
    (world, rt)
}

/// `spec("r1")` with the environment `env`, given by name in order.
fn with_env(env: &[(&str, &str)]) -> RunSpec {
    let mut job = spec("r1");
    job.env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    job
}

/// The value after each `--env` of `args`, in order.
fn env_values(args: &[String]) -> Vec<&str> {
    args.windows(2)
        .filter(|pair| pair[0] == "--env")
        .map(|pair| pair[1].as_str())
        .collect()
}

/// Asserts that a job's environment reaches the `create` of a Podman job (and of a Docker
/// one) as one `--env` and one `NAME=VALUE` argument for each variable, in name order,
/// right after the last label and before `--` and the image; that a value holding `=`, a
/// leading `-`, a newline and a space arrives as one unchanged argument; and that nothing
/// is joined, quoted or put after the image (where it would be the job's command line).
#[test]
fn a_job_start_env_reaches_a_podman_job() {
    let tricky = "-x=y\nz w";
    let job = with_env(&[("A", "1"), ("B", "two words"), ("C", tricky)]);
    let (world, mut rt) = podman();
    rt.create_and_start(&job).unwrap();
    let docker_world = docker();
    open(&docker_world).create_and_start(&job).unwrap();
    for args in [created(&world, "r1"), created(&docker_world, "r1")] {
        let values = env_values(&args);
        let tricky_pair = format!("C={tricky}");
        assert_eq!(values, ["A=1", "B=two words", tricky_pair.as_str()]);
        let first = args.iter().position(|arg| arg == "--env").unwrap();
        let last_label = args.iter().rposition(|arg| arg == "--label").unwrap();
        assert_eq!(first, last_label + 2, "{args:?}");
        let dashes = args.iter().position(|arg| arg == "--").unwrap();
        assert!(first + 6 <= dashes, "{args:?}");
        assert_eq!(args[dashes + 1], "registry.example/job:1");
        assert_eq!(&args[dashes + 2..], ["--go", "x y"]);
        let joined = args
            .iter()
            .filter(|arg| arg.starts_with("--env") && arg.as_str() != "--env");
        assert_eq!(joined.count(), 0, "{args:?}");
    }
}

/// Asserts that a job with no environment gets no `--env` on its `create`, and that the
/// builder adds nothing for an empty environment. It catches an empty environment that
/// adds arguments.
#[test]
fn a_job_with_no_env_is_unchanged() {
    let (world, mut rt) = podman();
    rt.create_and_start(&spec("r1")).unwrap();
    let args = created(&world, "r1");
    assert!(!args.iter().any(|arg| arg.contains("--env")), "{args:?}");
    assert_eq!(env_args(&[]), Ok(Vec::new()));
}

/// Asserts that a run whose environment names a reserved variable, handed to the runtime
/// without the wire's validation, is refused before the engine is asked to create
/// anything: no `create` call, no container. The same holds for a name the grammar
/// refuses, a NUL in a value and a name given twice. It catches a runtime that trusts the
/// wire to have checked an environment it may never have seen.
#[test]
fn a_runtime_refuses_a_reserved_env_name_it_is_handed_directly() {
    for env in [
        vec![("LD_PRELOAD", "/x.so")],
        vec![("JS_AGENT_TOKEN", "t")],
        vec![("A-B", "x")],
        vec![("A", "x\0y")],
        vec![("A", "1"), ("A", "2")],
    ] {
        let (world, mut rt) = podman();
        let job = with_env(&env);
        let refused = rt.create_and_start(&job).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("the job's environment is refused"),
            "{env:?}: {refused}"
        );
        assert_eq!(subs(&world), ["--version"], "{env:?}");
        assert!(world.ids().is_empty(), "{env:?}");
    }
    // The text is the wire's own refusal of that environment: its field and rule alone.
    let reserved = BTreeMap::from([("LD_PRELOAD".to_owned(), "/x.so".to_owned())]);
    let wire = js_agent_wire::check_env("env", &reserved).unwrap_err();
    assert_eq!(
        env_args(&[("LD_PRELOAD".to_owned(), "/x.so".to_owned())]),
        Err(ArgsError::Env(wire.to_string()))
    );
}
