//! The command line of the maintenance steps: `[update-hook] <drain|resume> --params <file>`,
//! read through [`Request`], beside the update steps it leaves as they were.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use js_agent::update_hook::{
    ArgsError, Invocation, MaintainInvocation, MaintenanceStep, Request, Step, VERB, is_hook_word,
};

fn request(words: &[&str]) -> Result<Request, ArgsError> {
    Request::parse(words.iter().map(OsString::from))
}

fn maintain(step: MaintenanceStep, params: &str) -> Request {
    Request::Maintain(MaintainInvocation {
        step,
        params: PathBuf::from(params),
    })
}

fn unexpected(word: &str) -> Result<Request, ArgsError> {
    Err(ArgsError::Unexpected(OsString::from(word)))
}

/// Asserts that `drain` and `resume` parse to a maintenance invocation with their params file,
/// behind the verb and bare, and that both forms give the same request. It catches a parser that
/// reads the maintenance words only in one form, or that takes them for update steps.
#[test]
fn drain_and_resume_parse_with_the_verb_and_without_it() {
    for step in MaintenanceStep::ALL {
        let bare = [step.name(), "--params", "/run/params.toml"];
        let verb = [VERB, step.name(), "--params", "/run/params.toml"];
        let want = maintain(step, "/run/params.toml");
        assert_eq!(request(&bare), Ok(want.clone()), "{step}");
        assert_eq!(request(&verb), Ok(want), "{step}");
    }
}

/// Asserts that the params flag may stand before nothing else and still be read once: the file
/// follows the flag whatever the verb. It catches a parser that takes the word after the step for
/// the file.
#[test]
fn the_maintenance_steps_read_the_file_that_follows_the_params_flag() {
    assert_eq!(
        request(&["drain", "--params", "/a/b.toml"]),
        Ok(maintain(MaintenanceStep::Drain, "/a/b.toml"))
    );
    assert_eq!(
        request(&["update-hook", "resume", "--params", "relative.toml"]),
        Ok(maintain(MaintenanceStep::Resume, "relative.toml"))
    );
}

/// Asserts that a maintenance step without `--params` is `MissingParams`, in both forms. It
/// catches a drain that runs without the table that holds the lease server's flags.
#[test]
fn a_maintenance_step_without_params_is_refused() {
    for step in MaintenanceStep::ALL {
        assert_eq!(
            request(&[step.name()]),
            Err(ArgsError::MissingParams),
            "{step}"
        );
        assert_eq!(
            request(&[VERB, step.name()]),
            Err(ArgsError::MissingParams),
            "{step}"
        );
    }
}

/// Asserts that `--params` as the last word is `MissingParamsFile`. It catches a parser that
/// reads a missing file name as no flag at all.
#[test]
fn a_params_flag_without_its_file_is_refused_for_a_maintenance_step() {
    for step in MaintenanceStep::ALL {
        assert_eq!(
            request(&[step.name(), "--params"]),
            Err(ArgsError::MissingParamsFile),
            "{step}"
        );
    }
}

/// Asserts that `--params` given twice is `DuplicateParams`, even with the same file. It catches
/// a parser that keeps the last of two files.
#[test]
fn a_maintenance_step_given_two_params_flags_is_refused() {
    for step in MaintenanceStep::ALL {
        assert_eq!(
            request(&[step.name(), "--params", "/p", "--params", "/q"]),
            Err(ArgsError::DuplicateParams),
            "{step}"
        );
        assert_eq!(
            request(&[step.name(), "--params", "/p", "--params", "/p"]),
            Err(ArgsError::DuplicateParams),
            "{step}"
        );
    }
}

/// Asserts that an argument besides the flag, an artifact among them, is `Unexpected` with that
/// argument, whichever side of the flag it stands on. It catches a drain that takes a staged
/// release as `prepare` does.
#[test]
fn a_maintenance_step_takes_no_artifact_and_no_other_argument() {
    for step in MaintenanceStep::ALL {
        let name = step.name();
        assert_eq!(
            request(&[name, "/staged", "--params", "/p"]),
            unexpected("/staged"),
            "{step}"
        );
        assert_eq!(
            request(&[name, "--params", "/p", "/staged"]),
            unexpected("/staged"),
            "{step}"
        );
        assert_eq!(
            request(&[name, "--params", "/p", "--verbose"]),
            unexpected("--verbose"),
            "{step}"
        );
        assert_eq!(
            request(&[name, "--params=/p"]),
            unexpected("--params=/p"),
            "{step}"
        );
    }
}

/// Asserts that the update steps still parse through `Request` to the same `Invocation` that
/// `Invocation::parse` gives, with and without the verb: `prepare` with its artifact, the others
/// without. It catches a request parser that changes what an update step reads.
#[test]
fn the_update_steps_parse_through_a_request_as_they_always_did() {
    for step in Step::ALL {
        let mut words = vec![step.name()];
        if step == Step::Prepare {
            words.push("/staged/release");
        }
        words.extend(["--params", "/run/params.toml"]);
        let want = Invocation::parse(words.iter().map(OsString::from)).unwrap();
        assert_eq!(want.step, step);
        assert_eq!(request(&words), Ok(Request::Update(want.clone())), "{step}");
        let mut verb = vec![VERB];
        verb.extend(&words);
        assert_eq!(request(&verb), Ok(Request::Update(want)), "{step}");
    }
    let cleanup = request(&["cleanup", "--params", "/p"]).unwrap();
    assert_eq!(
        cleanup,
        Request::Update(Invocation {
            step: Step::Cleanup,
            artifact: None,
            params: PathBuf::from("/p"),
        })
    );
}

/// Asserts that the refusals of an update step are those of `Invocation::parse`: a bare
/// `prepare` is `MissingArtifact` before it is `MissingParams`, an unknown word is named, nothing
/// is `MissingStep`, and the verb alone is `MissingStep`. It catches a request parser that
/// answers the maintenance errors for an update's command line.
#[test]
fn the_refusals_of_an_update_step_are_unchanged_through_a_request() {
    assert_eq!(request(&["prepare"]), Err(ArgsError::MissingArtifact));
    assert_eq!(
        request(&["prepare", "--params", "/p"]),
        Err(ArgsError::MissingArtifact)
    );
    assert_eq!(request(&["prepare", "/a"]), Err(ArgsError::MissingParams));
    assert_eq!(request(&["cutover"]), Err(ArgsError::MissingParams));
    assert_eq!(request(&[]), Err(ArgsError::MissingStep));
    assert_eq!(request(&[VERB]), Err(ArgsError::MissingStep));
    assert_eq!(
        request(&["restart", "--params", "/p"]),
        Err(ArgsError::UnknownStep("restart".to_owned()))
    );
    assert_eq!(
        request(&["uninstall", "--params", "/p"]),
        Err(ArgsError::UnknownStep("uninstall".to_owned()))
    );
    assert_eq!(
        request(&["verify", "--params", "/p", "/a"]),
        unexpected("/a")
    );
    // The verb twice is an unknown step, not a maintenance step behind a second verb.
    assert_eq!(
        request(&[VERB, VERB, "drain", "--params", "/p"]),
        Err(ArgsError::UnknownStep(VERB.to_owned()))
    );
}

/// Asserts that a maintenance word that is not valid UTF-8 is no maintenance step and is refused
/// as an unknown step. It catches a parser that lossily reads such a word as `drain`.
#[test]
fn a_step_word_that_is_not_text_is_an_unknown_step() {
    use std::os::unix::ffi::OsStringExt;
    let word = OsString::from_vec(vec![b'd', 0xff]);
    let got = Request::parse([word, OsString::from("--params"), OsString::from("/p")]);
    assert_eq!(got, Err(ArgsError::UnknownStep("d\u{fffd}".to_owned())));
}

/// Asserts that the program routes `drain` and `resume` to the hook, and a near word does not. It
/// catches a router that sends the maintenance steps to the agent's flag parser.
#[test]
fn the_hook_answers_drain_and_resume_and_no_near_word() {
    for word in ["drain", "resume"] {
        assert!(is_hook_word(OsStr::new(word)), "{word}");
    }
    for word in ["drained", "Drain", "resumed", "pause"] {
        assert!(!is_hook_word(OsStr::new(word)), "{word}");
    }
}

/// Asserts that the maintenance words round-trip through their names and differ from the update
/// steps' words. It catches a maintenance step that shares a word with an update step.
#[test]
fn maintenance_step_names_round_trip_and_differ_from_the_update_steps() {
    for step in MaintenanceStep::ALL {
        assert_eq!(step.name().parse::<MaintenanceStep>(), Ok(step));
        assert_eq!(step.to_string(), step.name());
        assert!(step.name().parse::<Step>().is_err(), "{step}");
    }
    assert!("prepare".parse::<MaintenanceStep>().is_err());
}
