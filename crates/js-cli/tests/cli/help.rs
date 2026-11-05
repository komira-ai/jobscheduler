//! The help text of the commands, held to golden files under `goldens/`: the lease commands'
//! as they were before the job commands were added beside them, and the job commands', which
//! are built from the generated table.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use crate::support::{Never, run, services};

/// The directory of the help goldens.
const GOLDENS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cli/goldens");

/// The lease commands whose help is held to `goldens/lease_help/`.
const LEASE: [&[&str]; 7] = [
    &["lease"],
    &["lease", "acquire"],
    &["lease", "renew"],
    &["lease", "release"],
    &["lease", "list"],
    &["lease", "run"],
    &["lease", "native"],
];

/// The program and the job commands, whose help is held to `goldens/job_help/`.
const JOB: [&[&str]; 17] = [
    &[],
    &["job"],
    &["job", "submit"],
    &["job", "get"],
    &["job", "list"],
    &["job", "cancel"],
    &["job", "result"],
    &["job", "logs"],
    &["host"],
    &["host", "list"],
    &["host", "cordon"],
    &["host", "uncordon"],
    &["pool"],
    &["pool", "list"],
    &["provider"],
    &["provider", "list"],
    &["usage"],
];

/// What `js <words> --help` prints on stdout; it must exit 0 and print nothing on stderr.
fn help(words: &[&str]) -> String {
    let mut args: Vec<OsString> = vec!["js".into()];
    args.extend(words.iter().map(OsString::from));
    args.push("--help".into());
    let got = run(args, services(Arc::new(Never)));
    assert_eq!((got.code, got.err.as_str()), (0, ""), "{words:?}");
    got.out
}

/// The golden file of a command: `js.txt` for the program, `js-lease-run.txt` for
/// `js lease run`.
fn golden_name(words: &[&str]) -> String {
    std::iter::once("js")
        .chain(words.iter().copied())
        .collect::<Vec<_>>()
        .join("-")
        + ".txt"
}

/// The commands of `commands` whose help differs from its golden in `goldens/<dir>/`, each
/// as the golden's path and the text it should hold as one JSON string (so the text can be
/// copied from a test log exactly).
pub fn differing(dir: &str, commands: &[&[&str]]) -> Vec<String> {
    let differs = |words: &&[&str]| {
        let path = Path::new(GOLDENS).join(dir).join(golden_name(words));
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        let got = help(words);
        let shown = serde_json::to_string(&got).unwrap_or_default();
        (got != want).then(|| format!("{}: {shown}", path.display()))
    };
    commands.iter().filter_map(differs).collect()
}

/// Asserts that `js lease --help` and the help of each lease subcommand are the text they
/// had before the job commands were added: the job commands add no flag to them and change
/// none of their words. It catches a job flag that leaks into the lease commands and a
/// change to a global flag's text.
#[test]
fn lease_help_is_unchanged() {
    let differ = differing("lease_help", &LEASE);
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// Asserts that `js --help` and the help of every job group and command equal their
/// goldens: the commands' words, each flag with its field's comment, the required flags,
/// and `--id` in place of `--spec.job-id`. A change to the definition or to the generator's
/// rules moves these goldens.
#[test]
fn job_help_text_matches_goldens() {
    let differ = differing("job_help", &JOB);
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// Asserts the names of the golden files: the program's own and a subcommand's.
#[test]
fn golden_files_are_named_by_the_command() {
    assert_eq!(golden_name(&[]), "js.txt");
    assert_eq!(golden_name(&["lease", "run"]), "js-lease-run.txt");
}
