//! The command line: `<step> [<artifact>] --params <file>`.

use std::ffi::OsString;
use std::path::PathBuf;

use js_agent::update_hook::{ArgsError, Invocation, Step};

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
