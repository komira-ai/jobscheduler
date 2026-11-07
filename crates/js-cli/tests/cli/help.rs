//! The help text of the commands, held to golden files under `goldens/`: the join-token
//! commands' and the job commands', which are built from the generated table; and the
//! commands that no longer exist.

use std::ffi::OsString;
use std::path::Path;

use crate::support::{Scratch, against, http, recording, run, services};

/// The directory of the help goldens.
const GOLDENS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cli/goldens");

/// The join-token commands, whose help is held to `goldens/join_token_help/`.
const JOIN_TOKEN: [&[&str]; 3] = [
    &["join-token"],
    &["join-token", "mint"],
    &["join-token", "revoke"],
];

/// The program and the job commands, whose help is held to `goldens/job_help/`: the
/// generated ones, and the hand-written `job wait`, `job follow` and `job run`.
const JOB: [&[&str]; 20] = [
    &[],
    &["job"],
    &["job", "submit"],
    &["job", "get"],
    &["job", "list"],
    &["job", "cancel"],
    &["job", "result"],
    &["job", "logs"],
    &["job", "wait"],
    &["job", "follow"],
    &["job", "run"],
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
    let got = run(args, services());
    assert_eq!((got.code, got.err.as_str()), (0, ""), "{words:?}");
    got.out
}

/// The golden file of a command: `js.txt` for the program, `js-join-token-mint.txt` for
/// `js join-token mint`.
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

/// Asserts that the help of the join-token group and its commands equals the goldens: the
/// words, the flags with their comments, and the required ones. It catches a flag that
/// changes its meaning or loses its comment.
#[test]
fn join_token_help_text_matches_goldens() {
    let differ = differing("join_token_help", &JOIN_TOKEN);
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
    assert_eq!(
        golden_name(&["join-token", "mint"]),
        "js-join-token-mint.txt"
    );
}

/// Asserts that `js_cli::command()`, the tree a documentation check walks, is the one the
/// program parses with: it is named `js`, holds every join-token and job command this file holds
/// to goldens and nothing beside them at the top, and carries the four global flags. It
/// catches a public tree that leaves out the generated job commands or the join-token group.
#[test]
fn the_public_command_tree_is_the_parsed_one() {
    let tree = js_cli::command();
    assert_eq!(tree.get_name(), "js");
    for words in JOIN_TOKEN.iter().chain(JOB.iter()) {
        let found = words
            .iter()
            .try_fold(&tree, |at, word| at.find_subcommand(word));
        assert!(found.is_some(), "{words:?}");
    }
    let top: Vec<&str> = tree
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect();
    assert_eq!(
        top,
        ["join-token", "job", "host", "pool", "provider", "usage"]
    );
    let globals: Vec<&str> = tree
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .filter_map(clap::Arg::get_long)
        .collect();
    assert_eq!(globals, ["server", "token-file", "ca-cert", "json"]);
}

/// Asserts that every spelling of the removed `lease` command group is a usage error: exit
/// 2, nothing on stdout, a message that names the unrecognized subcommand `lease`, and no
/// request sent to the server it was pointed at (the server is one that records what it
/// reads). It also asserts that `js --help` lists the join-token command and the generated
/// job, host, pool, provider and usage commands, and no `lease` command. It catches a
/// lease command that parses again or that reaches the server before it is refused.
#[test]
fn the_lease_commands_are_gone_from_the_cli() {
    let scratch = Scratch::new("help_lease_gone");
    let (url, seen) = recording(http("200 OK", "{}"));
    let js = against(&url, &scratch);
    let removed: [&[&str]; 7] = [
        &["lease"],
        &["lease", "list"],
        &["lease", "acquire", "--class", "c"],
        &["lease", "release", "1-1"],
        &["lease", "native", "1-1", "--program", "/bin/true"],
        &["lease", "run", "x"],
        &["lease", "renew", "1-1"],
    ];
    for words in removed {
        let got = js.go(words);
        assert_eq!((got.code, got.out.as_str()), (2, ""), "{words:?}");
        assert!(
            got.err
                .starts_with("error: unrecognized subcommand 'lease'\n"),
            "{words:?}: {}",
            got.err
        );
    }
    assert!(seen.lock().unwrap().is_empty());

    let listed = help(&[]);
    let commands: Vec<&str> = listed
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        commands,
        [
            "join-token",
            "job",
            "host",
            "pool",
            "provider",
            "usage",
            "help"
        ]
    );
    assert!(js_cli::command().find_subcommand("lease").is_none());
}
