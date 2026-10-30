//! The image config: what reaches the container spec, and what is refused.

use std::path::PathBuf;

use js_agent::image::{ConfigError, DEFAULT_PATH, ImageConfig};
use js_contain::container::{ContainerLimits, ContainerSpec};

/// A config whose `config` object is `process` and whose `rootfs` lists `layers` layers.
pub fn config_json(process: &str, layers: usize) -> String {
    let ids: Vec<String> = (0..layers)
        .map(|n| format!(r#""sha256:{n:064x}""#))
        .collect();
    format!(
        r#"{{"architecture":"amd64","os":"linux","config":{process},"rootfs":{{"type":"layers","diff_ids":[{}]}}}}"#,
        ids.join(",")
    )
}

fn parse(process: &str) -> ImageConfig {
    ImageConfig::parse(config_json(process, 1).as_bytes()).expect("a config")
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// A spec whose program, environment and directory are placeholders a test sees replaced.
fn spec() -> ContainerSpec {
    ContainerSpec {
        name: "lease-1-2".to_owned(),
        image: format!("ghcr.io/komira-ai/task@sha256:{}", "a".repeat(64)),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        dir: PathBuf::from("/scratch/lease-1-2"),
        cgroup_parent: "/js.slice/actions/lease-1-2".to_owned(),
        exec_root: "/job/root".to_owned(),
        input_root: PathBuf::from("/scratch/lease-1-2/root"),
        upper: PathBuf::from("/scratch/lease-1-2/upper"),
        overlay_work: PathBuf::from("/scratch/lease-1-2/work"),
        working_directory: "placeholder".to_owned(),
        env: vec![("PLACEHOLDER".to_owned(), "1".to_owned())],
        argv: strings(&["placeholder"]),
        limits: ContainerLimits::DEFAULT,
        shared: Vec::new(),
        mounts: Vec::new(),
    }
}

fn applied(process: &str, args: &[&str]) -> Result<ContainerSpec, ConfigError> {
    let mut made = spec();
    parse(process).apply(&strings(args), &mut made)?;
    Ok(made)
}

/// The entrypoint and environment (the image's `PATH` among it) of the image reach the
/// container spec, and its `WORKDIR` does not: the spec's working directory is read
/// relative to the overlay at `exec_root`, so it stays what the caller set, and the image's
/// directory is read with `working_dir`. and a run that names only flags runs the image's
/// entrypoint with them: the image `CMD` is replaced, not appended to. Every other field of
/// the spec is as it was. It catches a runtime that execs the flag string, one that drops
/// the image's `PATH`, and an apply that rewrites the lease's own fields.
#[test]
fn the_entrypoint_and_environment_reach_the_container_spec_and_the_workdir_does_not() {
    let process = r#"{"Entrypoint":["/usr/bin/kbf-task"],"Cmd":["--default"],
        "Env":["LANG=C","PATH=/opt/bin:/usr/bin","OPTS=a=b","EMPTY="],
        "WorkingDir":"/scratch/work"}"#;
    let made = applied(process, &["--mode", "build"]).expect("applies");
    let mut expected = spec();
    expected.argv = strings(&["/usr/bin/kbf-task", "--mode", "build"]);
    expected.env = vec![
        ("LANG".to_owned(), "C".to_owned()),
        ("PATH".to_owned(), "/opt/bin:/usr/bin".to_owned()),
        ("OPTS".to_owned(), "a=b".to_owned()),
        ("EMPTY".to_owned(), String::new()),
    ];
    assert_eq!(made, expected);
    assert_eq!(parse(process).working_dir(), "/scratch/work");
}

/// A run with no arguments runs the image as it was built: the entrypoint, then the `CMD`.
#[test]
fn a_run_with_no_arguments_runs_the_entrypoint_and_the_image_cmd() {
    let made = applied(
        r#"{"Entrypoint":["/usr/bin/kbf-task"],"Cmd":["--default","x"]}"#,
        &[],
    )
    .expect("applies");
    assert_eq!(made.argv, strings(&["/usr/bin/kbf-task", "--default", "x"]));
}

/// Without an entrypoint the run's arguments are the command; without arguments the `CMD`
/// is. `null` is how older builders write an entrypoint that is not there.
#[test]
fn without_an_entrypoint_the_arguments_or_the_cmd_are_the_command() {
    let with_args = applied(
        r#"{"Entrypoint":null,"Cmd":["/bin/sh"]}"#,
        &["/bin/echo", "hi"],
    )
    .expect("applies");
    assert_eq!(with_args.argv, strings(&["/bin/echo", "hi"]));
    let cmd = applied(r#"{"Cmd":["/bin/sh","-c","true"]}"#, &[]).expect("applies");
    assert_eq!(cmd.argv, strings(&["/bin/sh", "-c", "true"]));
}

/// An image with no entrypoint and no `CMD`, run with no arguments, has nothing to run:
/// refused, and the spec keeps its placeholders.
#[test]
fn nothing_to_run_is_refused() {
    let mut made = spec();
    let refused = parse(r#"{"Entrypoint":[],"Cmd":null}"#)
        .apply(&[], &mut made)
        .expect_err("no command");
    assert_eq!(refused, ConfigError::NoCommand);
    assert_eq!(made, spec());
}

/// An image that sets no `PATH` gets the engines' default one, after the image's own
/// variables; one that sets it keeps its own value, once, where it put it. It catches a
/// container with no `PATH` at all (the entrypoint found by name only by luck).
#[test]
fn the_default_path_is_added_only_when_the_image_sets_none() {
    let without = applied(r#"{"Env":["LANG=C"]}"#, &["x"]).expect("applies");
    assert_eq!(
        without.env,
        [
            ("LANG".to_owned(), "C".to_owned()),
            ("PATH".to_owned(), DEFAULT_PATH.to_owned())
        ]
    );
    let none_at_all = applied(r#"{"Env":null}"#, &["x"]).expect("applies");
    assert_eq!(
        none_at_all.env,
        [("PATH".to_owned(), DEFAULT_PATH.to_owned())]
    );
    let own = applied(r#"{"Env":["PATH=/mine","X=1"]}"#, &["x"]).expect("applies");
    assert_eq!(
        own.env,
        [
            ("PATH".to_owned(), "/mine".to_owned()),
            ("X".to_owned(), "1".to_owned())
        ]
    );
}

/// `ENV` entries must be `NAME=value`.
#[test]
fn an_env_entry_without_an_equals_sign_is_refused() {
    let refused = ImageConfig::parse(config_json(r#"{"Env":["PATH"]}"#, 1).as_bytes())
        .expect_err("not NAME=value");
    assert_eq!(refused, ConfigError::Env);
}

/// The working directory is an absolute path of the image: none or `/` is `/`, `.` and empty
/// parts are dropped, and a `..` is refused rather than resolved. Applying the config never
/// changes the spec's working directory.
#[test]
fn the_workdir_is_absolute_and_never_leaves_the_image_root() {
    for (workdir, expected) in [
        ("", "/"),
        ("/", "/"),
        ("/work", "/work"),
        ("/a/./b//c/", "/a/b/c"),
    ] {
        let process = format!(r#"{{"WorkingDir":"{workdir}"}}"#);
        assert_eq!(parse(&process).working_dir(), expected, "{workdir:?}");
        let made = applied(&process, &["x"]).expect("applies");
        assert_eq!(made.working_directory, "placeholder", "{workdir:?}");
    }
    assert_eq!(parse("{}").working_dir(), "/");
    let refused = ImageConfig::parse(config_json(r#"{"WorkingDir":"/a/../b"}"#, 1).as_bytes())
        .expect_err("leaves the root");
    assert_eq!(refused, ConfigError::WorkingDir("/a/../b".to_owned()));
}

/// A NUL byte in an `Env` entry, an entrypoint or `CMD` word or the working directory is
/// refused: none of them can be passed to a program or opened as a path.
#[test]
fn a_nul_byte_in_the_process_settings_is_refused() {
    for process in [
        r#"{"Env":["A=b\u0000c"]}"#,
        r#"{"Entrypoint":["/bin/a\u0000b"]}"#,
        r#"{"Cmd":["x","\u0000"]}"#,
        r#"{"WorkingDir":"/a\u0000b"}"#,
    ] {
        let refused = ImageConfig::parse(config_json(process, 1).as_bytes()).expect_err(process);
        assert_eq!(refused, ConfigError::Nul, "{process}");
    }
}

/// The image's `USER` is not read: a config that names root, an id or a user that does not
/// exist parses to the same thing as one that names none, so nothing of it can reach the
/// container (a job runs as the id its runtime gives it, the slot's).
#[test]
fn the_image_user_is_ignored() {
    let base = r#""Entrypoint":["/t"],"WorkingDir":"/w""#;
    let none = parse(&format!("{{{base}}}"));
    for user in ["root", "0:0", "nobody:nogroup", "4242"] {
        let named = parse(&format!(r#"{{{base},"User":"{user}"}}"#));
        assert_eq!(named, none, "{user}");
        let mut made = spec();
        named.apply(&[], &mut made).expect("applies");
        let mut plain = spec();
        none.apply(&[], &mut plain).expect("applies");
        assert_eq!(made, plain, "{user}");
    }
}

/// A config that lists another number of layers than its manifest is refused; one that
/// agrees is not. It catches a config of one image served with the manifest of another.
#[test]
fn the_layer_count_of_the_config_must_be_the_manifests() {
    let config = ImageConfig::parse(config_json("{}", 2).as_bytes()).expect("a config");
    config.check_layers(2).expect("agrees");
    let refused = config.check_layers(3).expect_err("differs");
    assert_eq!(
        refused,
        ConfigError::Layers {
            config: 2,
            manifest: 3
        }
    );
    assert_eq!(
        refused.to_string(),
        "the image config lists 2 layers, the manifest 3"
    );
}

/// Bytes that are not a config are refused, with the reason: not JSON, no `rootfs`, a
/// `diff_ids` that is not a list. A config with no `config` object, or a null one, is an
/// image with no process settings, which is fine.
#[test]
fn bytes_that_are_not_a_config_are_refused() {
    for bad in [
        "not json",
        "{}",
        r#"{"rootfs":{"diff_ids":"sha256:0"}}"#,
        r#"{"config":{"Env":"PATH=/bin"},"rootfs":{"diff_ids":[]}}"#,
    ] {
        let refused = ImageConfig::parse(bad.as_bytes()).expect_err(bad);
        assert!(
            matches!(refused, ConfigError::Malformed(_)),
            "{bad}: {refused}"
        );
    }
    for fine in [
        r#"{"rootfs":{"diff_ids":[]}}"#,
        r#"{"config":null,"rootfs":{"diff_ids":[]}}"#,
    ] {
        let config = ImageConfig::parse(fine.as_bytes()).expect(fine);
        config.check_layers(0).expect("no layers");
    }
}
