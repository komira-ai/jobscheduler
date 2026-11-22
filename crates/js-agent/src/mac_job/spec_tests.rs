//! The job file: what decodes, and each rule that refuses one.

use js_types::{Arch, Os};

use super::{ID_MAX, JobFile, Network, SpecError, WALL_LIMIT_MAX_SECS};

/// A job file with `extra` fields, the program and wall limit given.
fn file(extra: &str) -> String {
    format!(r#"{{"id":"job-1","os":"macos","program":"/bin/sh","wall_limit_secs":60{extra}}}"#)
}

fn parse(extra: &str) -> Result<JobFile, SpecError> {
    JobFile::parse(&file(extra), Arch::Arm64)
}

#[test]
fn the_smallest_job_takes_the_defaults() {
    let job = parse("").expect("decodes");
    assert_eq!(
        job,
        JobFile {
            id: "job-1".to_owned(),
            os: Os::Macos,
            arch: None,
            program: "/bin/sh".to_owned(),
            args: Vec::new(),
            env: Default::default(),
            inputs: None,
            outputs: Vec::new(),
            network: Network::default(),
            wall_limit_secs: 60,
        }
    );
    assert!(!job.network.loopback);
    assert!(job.network.outbound_ports.is_empty());
}

#[test]
fn every_field_decodes() {
    let job = parse(
        r#","arch":"aarch64","args":["a","b"],"env":{"LANG":"C","_X1":"v"},
           "inputs":"/in","outputs":["out","logs/x.txt"],
           "network":{"loopback":true,"outbound_ports":[8980,443]}"#,
    )
    .expect("decodes");
    assert_eq!(job.arch, Some(Arch::Arm64));
    assert_eq!(job.args, ["a", "b"]);
    assert_eq!(job.env.get("LANG").map(String::as_str), Some("C"));
    assert_eq!(job.inputs.as_deref(), Some("/in"));
    assert_eq!(job.outputs, ["out", "logs/x.txt"]);
    assert_eq!(
        job.network,
        Network {
            loopback: true,
            outbound_ports: vec![8980, 443]
        }
    );
}

#[test]
fn an_unknown_field_or_bad_json_is_refused() {
    assert!(matches!(
        parse(r#","image":"x""#),
        Err(SpecError::Decode(_))
    ));
    assert!(matches!(
        parse(r#","network":{"ports":[1]}"#),
        Err(SpecError::Decode(_))
    ));
    assert!(matches!(
        JobFile::parse("not json", Arch::Arm64),
        Err(SpecError::Decode(_))
    ));
    assert!(matches!(
        JobFile::parse(
            r#"{"id":"a","os":"windows","program":"/x","wall_limit_secs":1}"#,
            Arch::Arm64
        ),
        Err(SpecError::Decode(_))
    ));
}

#[test]
fn a_job_for_linux_is_refused_by_its_os() {
    let text = file("").replace("macos", "linux");
    assert_eq!(
        JobFile::parse(&text, Arch::Arm64),
        Err(SpecError::Os(Os::Linux))
    );
    assert_eq!(
        SpecError::Os(Os::Linux).to_string(),
        "the job is for linux; this host runs macos jobs"
    );
}

#[test]
fn a_job_for_another_architecture_is_refused_and_the_same_one_runs() {
    assert_eq!(
        parse(r#","arch":"x86_64""#),
        Err(SpecError::Arch {
            wanted: Arch::X86_64,
            here: Arch::Arm64
        })
    );
    assert!(JobFile::parse(&file(r#","arch":"x86_64""#), Arch::X86_64).is_ok());
}

#[test]
fn ids_follow_their_rule() {
    let with_id = |id: &str| file("").replace("job-1", id);
    let longest = "a".repeat(ID_MAX);
    for good in ["a", "0", "a-b-9", longest.as_str()] {
        assert!(
            JobFile::parse(&with_id(good), Arch::Arm64).is_ok(),
            "{good}"
        );
    }
    let too_long = "a".repeat(ID_MAX + 1);
    for bad in ["", "-a", "A", "a_b", "a.b", "a/b", too_long.as_str()] {
        assert_eq!(
            JobFile::parse(&with_id(bad), Arch::Arm64),
            Err(SpecError::Id(bad.to_owned())),
            "{bad}"
        );
    }
}

#[test]
fn the_program_must_be_an_absolute_path_without_a_nul() {
    let with_program = |p: &str| file("").replace("/bin/sh", p);
    assert_eq!(
        JobFile::parse(&with_program("sh"), Arch::Arm64),
        Err(SpecError::Program("sh".to_owned()))
    );
    assert_eq!(
        JobFile::parse(&with_program(r"/bin/s\u0000h"), Arch::Arm64),
        Err(SpecError::Nul("program"))
    );
}

#[test]
fn nuls_are_refused_in_args_env_and_inputs() {
    assert_eq!(parse(r#","args":["a\u0000"]"#), Err(SpecError::Nul("args")));
    assert_eq!(
        parse(r#","env":{"A":"b\u0000"}"#),
        Err(SpecError::Nul("env"))
    );
    assert_eq!(
        parse(r#","inputs":"/in\u0000""#),
        Err(SpecError::Nul("inputs"))
    );
}

#[test]
fn env_names_follow_their_rule_and_the_agents_own_are_refused() {
    for bad in [
        "",
        "1A",
        "A-B",
        "HOME",
        "TMPDIR",
        "PATH",
        "CLANG_MODULE_CACHE_PATH",
    ] {
        let extra = format!(r#","env":{{"{bad}":"v"}}"#);
        assert_eq!(parse(&extra), Err(SpecError::Env(bad.to_owned())), "{bad}");
    }
    assert!(parse(r#","env":{"_a9":"v","Home":"v"}"#).is_ok());
}

#[test]
fn inputs_must_be_absolute() {
    assert_eq!(
        parse(r#","inputs":"in""#),
        Err(SpecError::Inputs("in".to_owned()))
    );
}

#[test]
fn outputs_are_relative_paths_of_plain_parts() {
    for bad in [
        "", "/abs", "a//b", "a/", "./a", "a/./b", "..", "a/../b", "a\u{0}b",
    ] {
        let extra = format!(
            r#","outputs":[{}]"#,
            serde_json::to_string(bad).expect("json")
        );
        assert_eq!(
            parse(&extra),
            Err(SpecError::Output(bad.to_owned())),
            "{bad:?}"
        );
    }
    assert!(parse(r#","outputs":["a",".hidden/b..c"]"#).is_ok());
}

#[test]
fn port_zero_is_refused() {
    assert_eq!(
        parse(r#","network":{"outbound_ports":[80,0]}"#),
        Err(SpecError::Port)
    );
}

#[test]
fn the_wall_limit_is_one_second_to_a_day() {
    let with_limit = |n: u64| file("").replace(":60", &format!(":{n}"));
    for bad in [0, WALL_LIMIT_MAX_SECS + 1] {
        assert_eq!(
            JobFile::parse(&with_limit(bad), Arch::Arm64),
            Err(SpecError::WallLimit(bad))
        );
    }
    for good in [1, WALL_LIMIT_MAX_SECS] {
        assert!(JobFile::parse(&with_limit(good), Arch::Arm64).is_ok());
    }
}

#[test]
fn every_refusal_reads_as_a_sentence() {
    let cases = [
        (
            SpecError::Decode("x".to_owned()),
            "the job file does not decode: x",
        ),
        (
            SpecError::Id("A".to_owned()),
            r#"id "A": 1 to 63 of [a-z0-9-], the first a letter or digit"#,
        ),
        (
            SpecError::Arch {
                wanted: Arch::X86_64,
                here: Arch::Arm64,
            },
            "the job is for x86_64; this host is arm64",
        ),
        (
            SpecError::Program("p".to_owned()),
            r#"program "p" is not an absolute path"#,
        ),
        (SpecError::Nul("args"), "args holds a NUL"),
        (
            SpecError::Env("HOME".to_owned()),
            r#"env "HOME": not [A-Za-z_][A-Za-z0-9_]*, or set by the agent"#,
        ),
        (
            SpecError::Inputs("i".to_owned()),
            r#"inputs "i" is not an absolute path"#,
        ),
        (
            SpecError::Output("..".to_owned()),
            r#"output ".." is not a relative path of plain parts"#,
        ),
        (SpecError::Port, "outbound port 0"),
        (SpecError::WallLimit(0), "wall_limit_secs 0: 1 to 86400"),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}
