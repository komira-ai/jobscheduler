//! The command line: `[update-hook] <step> [<artifact>] --params <file>`, with the verb or
//! without it.

use std::ffi::OsString;
use std::path::PathBuf;

use js_agent::update_hook::{ArgsError, Invocation, Step, VERB, is_hook_word};

fn parse(words: &[&str]) -> Result<Invocation, ArgsError> {
    Invocation::parse(words.iter().map(OsString::from))
}

#[test]
fn every_step_parses_with_its_arguments() {
    for step in Step::ALL {
        let artifact = (step == Step::Prepare).then(|| PathBuf::from("/staged/release"));
        let mut words = vec![step.name()];
        words.extend(artifact.iter().map(|_| "/staged/release"));
        words.extend(["--params", "/run/params.toml"]);
        let got = parse(&words).unwrap();
        assert_eq!(
            got,
            Invocation {
                step,
                artifact,
                params: PathBuf::from("/run/params.toml"),
            },
            "{step}"
        );
    }
}

#[test]
fn the_params_flag_may_come_before_the_artifact() {
    let got = parse(&["prepare", "--params", "/p", "/a"]).unwrap();
    assert_eq!(got.artifact, Some(PathBuf::from("/a")));
    assert_eq!(got.params, PathBuf::from("/p"));
}

#[test]
fn a_missing_params_flag_is_refused_for_every_step() {
    for step in Step::ALL {
        let words: Vec<&str> = if step == Step::Prepare {
            vec![step.name(), "/a"]
        } else {
            vec![step.name()]
        };
        assert_eq!(parse(&words), Err(ArgsError::MissingParams), "{step}");
    }
}

#[test]
fn prepare_without_the_staged_directory_is_refused() {
    assert_eq!(
        parse(&["prepare", "--params", "/p"]),
        Err(ArgsError::MissingArtifact)
    );
}

#[test]
fn an_unknown_step_is_refused_by_name() {
    assert_eq!(
        parse(&["restart", "--params", "/p"]),
        Err(ArgsError::UnknownStep("restart".to_owned()))
    );
}

#[test]
fn no_step_is_refused() {
    assert_eq!(parse(&[]), Err(ArgsError::MissingStep));
}

#[test]
fn an_argument_the_step_does_not_take_is_refused() {
    let unexpected = |word: &str| Err(ArgsError::Unexpected(OsString::from(word)));
    // An artifact is `prepare`'s alone.
    assert_eq!(
        parse(&["cutover", "/a", "--params", "/p"]),
        unexpected("/a")
    );
    // One artifact, not two.
    assert_eq!(
        parse(&["prepare", "/a", "/b", "--params", "/p"]),
        unexpected("/b")
    );
    // Flags only, and only this one: no `--params=<file>` form, no other flag.
    assert_eq!(parse(&["verify", "--params=/p"]), unexpected("--params=/p"));
    assert_eq!(
        parse(&["verify", "--params", "/p", "--verbose"]),
        unexpected("--verbose")
    );
}

#[test]
fn the_params_flag_needs_one_file_and_is_given_once() {
    assert_eq!(
        parse(&["verify", "--params"]),
        Err(ArgsError::MissingParamsFile)
    );
    assert_eq!(
        parse(&["verify", "--params", "/p", "--params", "/q"]),
        Err(ArgsError::DuplicateParams)
    );
}

#[test]
fn step_names_round_trip_and_differ() {
    let mut seen = std::collections::BTreeSet::new();
    for step in Step::ALL {
        assert_eq!(step.name().parse::<Step>(), Ok(step));
        assert_eq!(step.to_string(), step.name());
        assert!(seen.insert(step.name()), "{step} repeated");
    }
    assert_eq!(seen.len(), 5);
}

#[test]
fn every_refusal_says_what_was_wrong() {
    let texts = [
        (ArgsError::MissingStep, "no step given"),
        (ArgsError::UnknownStep("x".to_owned()), "unknown step \"x\""),
        (
            ArgsError::MissingArtifact,
            "prepare needs the staged release directory",
        ),
        (
            ArgsError::Unexpected(OsString::from("y")),
            "unexpected argument \"y\"",
        ),
        (ArgsError::MissingParamsFile, "--params needs a file"),
        (ArgsError::DuplicateParams, "--params given twice"),
        (ArgsError::MissingParams, "--params <file> is required"),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
}

/// hostkeeper runs the update steps as the bare word and the steps it adds behind the verb: both
/// forms give the same invocation, and the verb is optional only in the first place.
///
/// Catches a parser that takes the verb for the step, or that refuses the bare word.
#[test]
fn the_verb_before_the_step_is_taken_and_the_bare_step_gives_the_same_invocation() {
    for step in Step::ALL {
        let mut bare = vec![step.name()];
        if step == Step::Prepare {
            bare.push("/staged/release");
        }
        bare.extend(["--params", "/run/params.toml"]);
        let mut verb = vec![VERB];
        verb.extend(&bare);
        assert_eq!(parse(&verb), parse(&bare), "{step}");
        assert!(parse(&verb).is_ok(), "{step}");
    }
    // The verb twice is an unknown step, not a second verb.
    assert_eq!(
        parse(&["update-hook", "update-hook", "verify", "--params", "/p"]),
        Err(ArgsError::UnknownStep("update-hook".to_owned()))
    );
}

/// The verb alone names no step.
///
/// Catches a parser that reads past the verb.
#[test]
fn the_verb_alone_is_no_step() {
    assert_eq!(parse(&["update-hook"]), Err(ArgsError::MissingStep));
}

/// `uninstall` is hostkeeper's step for a service that declares an uninstall bound; the package
/// declares none, and the hook refuses the word in both forms like any word it does not know.
///
/// Catches an `uninstall` that is taken for a step and run.
#[test]
fn uninstall_is_refused_with_the_verb_and_without() {
    let unknown = Err(ArgsError::UnknownStep("uninstall".to_owned()));
    assert_eq!(parse(&["uninstall", "--params", "/p"]), unknown);
    assert_eq!(
        parse(&["update-hook", "uninstall", "--params", "/p"]),
        unknown
    );
}

/// Flags before the step were the kbf declaration's `hook_args`; hostkeeper passes none, and
/// the params file carries the values. A flag in front of the step is not a step.
///
/// Catches a parser that still accepts `--name=value` words before the step.
#[test]
fn a_flag_before_the_step_is_refused() {
    assert_eq!(
        parse(&["--release-link=/opt/x", "cutover", "--params", "/p"]),
        Err(ArgsError::UnknownStep("--release-link=/opt/x".to_owned()))
    );
}

/// The program routes the verb and the five update steps and `uninstall` to the hook, and nothing
/// else: the agent's other words go to the agent.
///
/// Catches a router that sends an agent flag or a verb of another program to the hook, or lets
/// an update step through to the agent's flag parser.
#[test]
fn the_hook_answers_the_verb_the_five_steps_and_uninstall_and_no_other_word() {
    for word in [
        "update-hook",
        "prepare",
        "cutover",
        "verify",
        "rollback",
        "cleanup",
        "uninstall",
    ] {
        assert!(is_hook_word(std::ffi::OsStr::new(word)), "{word}");
    }
    for word in ["--server", "cordon", "image", "update", "prepare2", ""] {
        assert!(!is_hook_word(std::ffi::OsStr::new(word)), "{word}");
    }
}
