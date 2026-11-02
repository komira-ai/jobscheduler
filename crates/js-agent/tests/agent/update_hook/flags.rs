//! The flags before the step (`--release-link=...` and the five connect values): read from
//! the command line, put into the params, and refused when unknown, repeated, valueless,
//! relative, after the step, or given in the params as well.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use js_agent::update_hook::{
    ArgsError, CA_CERT_FLAG, CA_CERT_KEY, ConnectParams, Done, Flags, HOST_ID_FLAG, HOST_ID_KEY,
    Host, Invocation, Outcome, Params, ParamsError, RELEASE_LINK_FLAG, RELEASE_LINK_KEY, Report,
    SERVER_FLAG, SERVER_KEY, STATE_ROOT_FLAG, STATE_ROOT_KEY, Step, TOKEN_FILE_FLAG,
    TOKEN_FILE_KEY, VERB, run,
};
use js_testkit::dir::TestDir;

const PARAMS: &str = concat!(
    "cordon_lease_secs = 300\ncordon_max_secs = 17355\n",
    "drain_wait_secs = 3600\ndefer_retry_secs = 300\n",
    "\"updater.artifact_digest\" = \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
    "\"updater.staged_release\" = \"/staged\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
);

/// Each flag, its params key, and a value the flag takes.
const EACH: [(&str, &str, &str); 6] = [
    (
        RELEASE_LINK_FLAG,
        RELEASE_LINK_KEY,
        "/opt/js-agent/link/current",
    ),
    (SERVER_FLAG, SERVER_KEY, "https://lease.example:8443"),
    (STATE_ROOT_FLAG, STATE_ROOT_KEY, "/var/lib/js-agent/state"),
    (TOKEN_FILE_FLAG, TOKEN_FILE_KEY, "/etc/js-agent/token"),
    (CA_CERT_FLAG, CA_CERT_KEY, "/etc/js-agent/ca.pem"),
    (HOST_ID_FLAG, HOST_ID_KEY, "burst-1"),
];

/// The flags that name a path.
const PATH_FLAGS: [&str; 4] = [
    RELEASE_LINK_FLAG,
    STATE_ROOT_FLAG,
    TOKEN_FILE_FLAG,
    CA_CERT_FLAG,
];

fn parse(words: &[&str]) -> Result<Invocation, ArgsError> {
    Invocation::parse(words.iter().map(OsString::from))
}

fn every_flag() -> Vec<String> {
    EACH.iter()
        .map(|(flag, _, value)| format!("{flag}={value}"))
        .collect()
}

fn all_set() -> Flags {
    Flags {
        release_link: Some(PathBuf::from("/opt/js-agent/link/current")),
        server: Some("https://lease.example:8443".to_owned()),
        state_root: Some(PathBuf::from("/var/lib/js-agent/state")),
        token_file: Some(PathBuf::from("/etc/js-agent/token")),
        ca_cert: Some(PathBuf::from("/etc/js-agent/ca.pem")),
        host_id: Some("burst-1".to_owned()),
    }
}

#[test]
fn every_flag_before_the_step_is_read_with_its_value() {
    let mut words = every_flag();
    words.extend(["cutover", "--params", "/p"].map(str::to_owned));
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    assert_eq!(
        parse(&words),
        Ok(Invocation {
            flags: all_set(),
            step: Step::Cutover,
            artifact: None,
            params: PathBuf::from("/p"),
        })
    );
    // No flag: every value is absent.
    assert_eq!(
        parse(&["cutover", "--params", "/p"]).unwrap().flags,
        Flags::default()
    );
    // `prepare` keeps its artifact after the flags.
    let got = parse(&["--server=s", "prepare", "/a", "--params", "/p"]).unwrap();
    assert_eq!(got.artifact, Some(PathBuf::from("/a")));
    assert_eq!(got.flags.server.as_deref(), Some("s"));
}

#[test]
fn an_unknown_word_before_the_step_is_refused_by_name() {
    for word in [
        "--params=/p",
        "--params",
        "--verbose",
        "--release_link=/x",
        "--",
    ] {
        assert_eq!(
            parse(&[word, "verify", "--params", "/p"]),
            Err(ArgsError::UnknownFlag(word.to_owned())),
            "{word}"
        );
    }
}

#[test]
fn a_flag_with_no_value_or_an_empty_one_is_refused() {
    for (flag, _, _) in EACH {
        for word in [flag.to_owned(), format!("{flag}=")] {
            assert_eq!(
                parse(&[&word, "verify", "--params", "/p"]),
                Err(ArgsError::FlagValue(flag.to_owned())),
                "{word}"
            );
        }
    }
}

#[test]
fn a_flag_given_twice_is_refused_even_with_the_same_value() {
    for (flag, _, value) in EACH {
        let word = format!("{flag}={value}");
        assert_eq!(
            parse(&[&word, &word, "verify", "--params", "/p"]),
            Err(ArgsError::DuplicateFlag(flag.to_owned())),
            "{flag}"
        );
    }
}

#[test]
fn a_path_flag_with_a_relative_path_is_refused() {
    for flag in PATH_FLAGS {
        for value in ["current", "./x", "opt/js-agent"] {
            let word = format!("{flag}={value}");
            assert_eq!(
                parse(&[&word, "verify", "--params", "/p"]),
                Err(ArgsError::RelativeFlag(flag.to_owned())),
                "{word}"
            );
        }
    }
}

#[test]
fn flags_come_before_the_step_and_nowhere_else() {
    for word in every_flag() {
        assert_eq!(
            parse(&["verify", &word, "--params", "/p"]),
            Err(ArgsError::Unexpected(OsString::from(&word)))
        );
    }
    // Flags and no step.
    assert_eq!(
        parse(&["--server=s", "--host-id=h"]),
        Err(ArgsError::MissingStep)
    );
}

#[test]
fn the_flag_refusals_say_what_was_wrong() {
    let texts = [
        (
            ArgsError::UnknownFlag("--x=1".to_owned()),
            "unknown flag \"--x=1\" before the step",
        ),
        (
            ArgsError::FlagValue("--server".to_owned()),
            "--server needs a value, written --server=<value>",
        ),
        (
            ArgsError::DuplicateFlag("--host-id".to_owned()),
            "--host-id given twice",
        ),
        (
            ArgsError::RelativeFlag("--release-link".to_owned()),
            "--release-link must be an absolute path",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
    assert_eq!(
        ParamsError::Twice(RELEASE_LINK_KEY).to_string(),
        "params key \"release_link\" is also given as a flag before the step; give it once"
    );
}

#[test]
fn the_flags_fill_the_keys_the_params_leave_out() {
    let params = Params::parse(PARAMS).unwrap();
    assert_eq!(
        params.clone().with_flags(&Flags::default()).unwrap(),
        params
    );
    let filled = params.with_flags(&all_set()).unwrap();
    assert_eq!(
        filled.release_link,
        Some(PathBuf::from("/opt/js-agent/link/current"))
    );
    assert_eq!(
        filled.connect,
        ConnectParams {
            server: Some("https://lease.example:8443".to_owned()),
            state_root: Some(PathBuf::from("/var/lib/js-agent/state")),
            token_file: Some(PathBuf::from("/etc/js-agent/token")),
            ca_cert: Some(PathBuf::from("/etc/js-agent/ca.pem")),
            host_id: Some("burst-1".to_owned()),
        }
    );
}

#[test]
fn a_value_given_as_a_flag_and_in_the_params_is_refused_by_its_key() {
    for (index, (flag, key, value)) in EACH.iter().enumerate() {
        let params = Params::parse(&format!("{PARAMS}{key} = \"{value}\"\n")).unwrap();
        // The other five flags, which the file leaves out, do not conflict.
        let mut only_others = all_set();
        let mut cleared = Flags::default();
        match index {
            0 => only_others.release_link = None,
            1 => only_others.server = None,
            2 => only_others.state_root = None,
            3 => only_others.token_file = None,
            4 => only_others.ca_cert = None,
            _ => only_others.host_id = None,
        }
        assert!(params.clone().with_flags(&only_others).is_ok(), "{flag}");
        cleared.take(&format!("{flag}={value}")).unwrap();
        assert_eq!(
            params.with_flags(&cleared).unwrap_err().to_string(),
            ParamsError::Twice(key).to_string(),
            "{flag}"
        );
    }
}

/// A params file in a directory of its own.
fn params_file(name: &str, text: &str) -> (TestDir, String) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let file = dir.path().join("params.toml");
    std::fs::write(&file, text).unwrap();
    let path = file.to_str().unwrap().to_owned();
    (dir, path)
}

fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

fn args(words: &[&str]) -> Vec<OsString> {
    ["js-agent", VERB]
        .into_iter()
        .chain(words.iter().copied())
        .map(OsString::from)
        .collect()
}

#[test]
fn the_step_is_given_the_params_with_the_flags_put_in() {
    let (_dir, params) = params_file("hook-flags-given", PARAMS);
    let seen = RefCell::new(Vec::new());
    let step = |_: &Invocation, params: &Params| {
        seen.borrow_mut().push(params.clone());
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        &args(&[
            "--release-link=/opt/js-agent/link/current",
            "--state-root=/var/lib/js-agent/state",
            "cutover",
            "--params",
            &params,
        ]),
        &mut out,
        &mut err,
        &Host { step: &step },
    );
    assert_eq!(format!("{code:?}"), status(0));
    assert_eq!(String::from_utf8(err).unwrap(), "");
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].release_link,
        Some(PathBuf::from("/opt/js-agent/link/current"))
    );
    assert_eq!(
        seen[0].connect.state_root,
        Some(PathBuf::from("/var/lib/js-agent/state"))
    );
}

#[test]
fn a_value_given_both_ways_fails_the_step_as_bad_params_before_it_runs() {
    let text = format!("{PARAMS}release_link = \"/opt/js-agent/link/current\"\n");
    let (_dir, params) = params_file("hook-flags-twice", &text);
    let ran = RefCell::new(0);
    let step = |_: &Invocation, _: &Params| {
        *ran.borrow_mut() += 1;
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        &args(&[
            "--release-link=/opt/js-agent/link/current",
            "cutover",
            "--params",
            &params,
        ]),
        &mut out,
        &mut err,
        &Host { step: &step },
    );
    assert_eq!(format!("{code:?}"), status(1));
    assert_eq!(*ran.borrow(), 0);
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "js-agent update-hook: params key \"release_link\" is also given as a flag before the step; give it once\n"
    );
    let line: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim_end()).unwrap();
    assert_eq!(line["reason"], "bad_params");
}
