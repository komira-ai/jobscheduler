//! The command line of the config hook: the step after the verb, the staged directory of
//! `prepare` only, the params file, and `uninstall`, which needs none.

use std::ffi::OsString;
use std::path::PathBuf;

use js_agent::update_hook::config::{Invocation, VERB};
use js_agent::update_hook::{ArgsError, Invocation as StepInvocation, Step, is_hook_word};

fn words(line: &[&str]) -> Vec<OsString> {
    line.iter().map(OsString::from).collect()
}

fn step(step: Step, artifact: Option<&str>, params: &str) -> Invocation {
    Invocation::Step(StepInvocation {
        step,
        artifact: artifact.map(PathBuf::from),
        params: PathBuf::from(params),
    })
}

/// Asserts that each of the five steps parses with its params file, that `prepare` takes its
/// artifact and no other step does, and that the order of the words around `--params` does not
/// matter.
///
/// Catches a parser that reads the step without its file, or hands an artifact to a step that
/// has none.
#[test]
fn the_five_steps_take_a_params_file_and_prepare_takes_the_staged_directory() {
    assert_eq!(VERB, "config-hook");
    for (word, which) in [
        ("cutover", Step::Cutover),
        ("verify", Step::Verify),
        ("rollback", Step::Rollback),
        ("cleanup", Step::Cleanup),
    ] {
        assert_eq!(
            Invocation::parse(words(&[word, "--params", "/p"])),
            Ok(step(which, None, "/p")),
            "{word}"
        );
    }
    assert_eq!(
        Invocation::parse(words(&["prepare", "/staged", "--params", "/p"])),
        Ok(step(Step::Prepare, Some("/staged"), "/p"))
    );
    assert_eq!(
        Invocation::parse(words(&["prepare", "--params", "/p", "/staged"])),
        Ok(step(Step::Prepare, Some("/staged"), "/p"))
    );
}

/// Asserts each refusal: no step, an unknown step, `prepare` without its artifact, an artifact
/// for another step, a second artifact, no `--params`, `--params` without a file, and twice.
///
/// Catches a hook that runs a step it was not asked for, or one on arguments it does not take.
#[test]
fn a_command_line_that_is_not_a_step_with_its_arguments_is_refused_by_name() {
    let refused = |line: &[&str]| Invocation::parse(words(line)).unwrap_err();
    assert_eq!(refused(&[]), ArgsError::MissingStep);
    assert_eq!(
        refused(&["restart", "--params", "/p"]),
        ArgsError::UnknownStep("restart".to_owned())
    );
    assert_eq!(
        refused(&["prepare", "--params", "/p"]),
        ArgsError::MissingArtifact
    );
    assert_eq!(
        refused(&["cutover", "/staged", "--params", "/p"]),
        ArgsError::Unexpected("/staged".into())
    );
    assert_eq!(
        refused(&["prepare", "/a", "/b", "--params", "/p"]),
        ArgsError::Unexpected("/b".into())
    );
    assert_eq!(refused(&["verify"]), ArgsError::MissingParams);
    assert_eq!(
        refused(&["verify", "--params"]),
        ArgsError::MissingParamsFile
    );
    assert_eq!(
        refused(&["verify", "--params", "/a", "--params", "/b"]),
        ArgsError::DuplicateParams
    );
}

/// Asserts that `uninstall` parses with no params, with a params file (hostkeeper may pass
/// one), and refuses any other argument.
///
/// Catches an uninstall that is refused for want of a table it does nothing with, or one that
/// accepts what it will not read.
#[test]
fn uninstall_needs_no_params_and_takes_nothing_else() {
    assert_eq!(
        Invocation::parse(words(&["uninstall"])),
        Ok(Invocation::Uninstall)
    );
    assert_eq!(
        Invocation::parse(words(&["uninstall", "--params", "/p"])),
        Ok(Invocation::Uninstall)
    );
    for line in [
        &["uninstall", "/staged"][..],
        &["uninstall", "--params"],
        &["uninstall", "--params", "/p", "/q"],
    ] {
        assert!(
            matches!(
                Invocation::parse(words(line)),
                Err(ArgsError::Unexpected(_))
            ),
            "{line:?}"
        );
    }
}

/// Asserts that the program routes the config hook's verb to the hook, and still the update
/// hook's words, and no other word.
///
/// Catches a router that sends `config-hook` to the agent's flag parser, or drops a word the
/// update hook has.
#[test]
fn the_program_answers_the_config_verb_besides_the_update_hook_words() {
    for word in ["config-hook", "update-hook", "cutover", "uninstall"] {
        assert!(is_hook_word(std::ffi::OsStr::new(word)), "{word}");
    }
    assert!(!is_hook_word(std::ffi::OsStr::new("config")));
}
