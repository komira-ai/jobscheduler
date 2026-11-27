//! The release passes secrets to a called workflow exactly when that workflow names one.
//!
//! A workflow that another calls sees none of the caller's repository or organization secrets
//! unless the calling job passes them: without `secrets: inherit` every `secrets.NAME` in it
//! reads as empty, and a job that skips itself when its secrets are missing (the macOS package's
//! `sign`) then succeeds having done nothing. So a call of a local workflow that names a secret
//! carries `secrets: inherit`; a call of one that names none passes nothing; and a call of
//! another repository's workflow passes nothing either.

use std::collections::BTreeSet;

use super::{calls, jobs, read_workflow, workflow};

/// The one form of passing secrets a call may have.
const INHERIT: &str = "    secrets: inherit";

/// The expressions of a line of workflow code: what each `${{ ... }}` holds, and the value of
/// an `if:` key, which may leave out the braces.
fn expressions(code: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = code;
    while let Some((_, after)) = rest.split_once("${{") {
        let (inside, tail) = after.split_once("}}").unwrap_or((after, ""));
        found.push(inside);
        rest = tail;
    }
    let trimmed = code.trim_start().trim_start_matches("- ");
    if let Some(value) = trimmed.strip_prefix("if:")
        && !value.contains("${{")
    {
        found.push(value);
    }
    found
}

/// The secrets the text's expressions name, outside comments, sorted: `NAME` for
/// `secrets.NAME` and `secrets['NAME']` (the context's name in any case, as GitHub reads it),
/// and `*` for the whole context (`toJSON(secrets)`).
fn named_secrets(text: &str) -> BTreeSet<String> {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut named = BTreeSet::new();
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let code = line.split_once(" #").map_or(line, |(code, _)| code);
        for expr in expressions(code) {
            let lower = expr.to_ascii_lowercase();
            for (at, _) in lower.match_indices("secrets") {
                if lower[..at].ends_with(|c: char| ident(c) || c == '.') {
                    continue;
                }
                let after = &expr[at + "secrets".len()..];
                let name: String = if let Some(field) = after.strip_prefix('.') {
                    field.chars().take_while(|c| ident(*c)).collect()
                } else if let Some(index) = after.trim_start().strip_prefix('[') {
                    index
                        .split(']')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .trim_matches(['\'', '"'])
                        .to_owned()
                } else if after.starts_with(ident) {
                    continue;
                } else {
                    "*".to_owned()
                };
                named.insert(if name.is_empty() {
                    "*".to_owned()
                } else {
                    name
                });
            }
        }
    }
    named
}

/// What is wrong with the way the release's calls pass secrets, one line per wrong call;
/// `read` gives the text of a workflow of `.github/workflows` by its file name.
fn secret_passing_errors(release: &str, read: impl Fn(&str) -> String) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, lines) in jobs(release) {
        let Some(called) = calls(&lines) else {
            continue;
        };
        let passes: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|l| l.starts_with("    secrets:"))
            .map(|l| l.split_once(" #").map_or(l, |(code, _)| code).trim_end())
            .collect();
        let Some(file) = called.strip_prefix("./.github/workflows/") else {
            if !passes.is_empty() {
                errors.push(format!(
                    "{name}: passes secrets to a workflow of another repository"
                ));
            }
            continue;
        };
        let named = named_secrets(&read(file));
        if named.is_empty() {
            if !passes.is_empty() {
                errors.push(format!(
                    "{name}: passes secrets to {file}, which names none"
                ));
            }
        } else if passes != [INHERIT] {
            let list: Vec<&str> = named.iter().map(String::as_str).collect();
            errors.push(format!(
                "{name}: calls {file}, which names {}, without `secrets: inherit`; it would read them as empty",
                list.join(", ")
            ));
        }
    }
    errors
}

/// The text with `old`, which it holds exactly once, replaced by `new`.
fn edit(text: &str, old: &str, new: &str) -> String {
    assert_eq!(text.matches(old).count(), 1, "{old}");
    text.replacen(old, new, 1)
}

/// The errors for a release text, every called workflow read from the repository.
fn errors(release: &str) -> Vec<String> {
    secret_passing_errors(release, read_workflow)
}

#[test]
fn the_release_passes_secrets_to_the_package_call_alone() {
    assert_eq!(errors(&workflow()), Vec::<String>::new());
    // The package workflow names the two application secrets (which sign the programs) and the
    // two installer secrets (which sign the package) and no other; the other local calls
    // name none, so `beta-macos-pkg` is the one call that must pass them. This is also what
    // bounds `secrets: inherit`: of all the caller's secrets (the apt signing key among them),
    // the package workflow can read only these four, in any job or step.
    let want: BTreeSet<String> = [
        "MACOS_APPLICATION_P12",
        "MACOS_APPLICATION_P12_PASSWORD",
        "MACOS_INSTALLER_P12",
        "MACOS_INSTALLER_P12_PASSWORD",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        named_secrets(&read_workflow("js-agent-macos-pkg.yml")),
        want
    );
    for file in [
        "artifacts.yml",
        "beta.yml",
        "farm.yml",
        "js-agent-macos-first-install.yml",
        "js-agent-native-isolation.yml",
        "macos-probe.yml",
    ] {
        assert_eq!(
            named_secrets(&read_workflow(file)),
            BTreeSet::new(),
            "{file}"
        );
    }
    let text = workflow();
    let all = jobs(&text);
    let pkg = &all
        .iter()
        .find(|(n, _)| n == "beta-macos-pkg")
        .expect("beta-macos-pkg")
        .1;
    assert!(pkg.iter().any(|l| l == INHERIT), "{pkg:?}");
}

#[test]
fn the_package_call_without_inherit_is_refused() {
    let text = edit(&workflow(), "    secrets: inherit\n", "");
    assert_eq!(
        errors(&text),
        [
            "beta-macos-pkg: calls js-agent-macos-pkg.yml, which names MACOS_APPLICATION_P12, MACOS_APPLICATION_P12_PASSWORD, MACOS_INSTALLER_P12, MACOS_INSTALLER_P12_PASSWORD, without `secrets: inherit`; it would read them as empty"
        ]
    );
}

#[test]
fn the_package_call_passing_a_mapping_instead_of_inherit_is_refused() {
    let text = edit(
        &workflow(),
        "    secrets: inherit\n",
        "    secrets:\n      MACOS_INSTALLER_P12: ${{ secrets.MACOS_INSTALLER_P12 }}\n",
    );
    assert_eq!(
        errors(&text),
        [
            "beta-macos-pkg: calls js-agent-macos-pkg.yml, which names MACOS_APPLICATION_P12, MACOS_APPLICATION_P12_PASSWORD, MACOS_INSTALLER_P12, MACOS_INSTALLER_P12_PASSWORD, without `secrets: inherit`; it would read them as empty"
        ]
    );
}

#[test]
fn a_call_passing_secrets_to_a_workflow_that_names_none_is_refused() {
    let text = edit(
        &workflow(),
        "    uses: ./.github/workflows/beta.yml\n",
        "    uses: ./.github/workflows/beta.yml\n    secrets: inherit\n",
    );
    assert_eq!(
        errors(&text),
        ["beta-macos: passes secrets to beta.yml, which names none"]
    );
}

/// The first-install call passing the caller's secrets is refused: the workflow it calls names
/// none, and it installs a package built from the branch's code.
#[test]
fn the_first_install_call_passing_secrets_is_refused() {
    let text = edit(
        &workflow(),
        "    uses: ./.github/workflows/js-agent-macos-first-install.yml\n",
        "    uses: ./.github/workflows/js-agent-macos-first-install.yml\n    secrets: inherit\n",
    );
    assert_eq!(
        errors(&text),
        [
            "beta-macos-first-install: passes secrets to js-agent-macos-first-install.yml, which names none"
        ]
    );
}

#[test]
fn a_called_workflow_that_starts_naming_a_secret_needs_inherit() {
    let release = workflow();
    let found = secret_passing_errors(&release, |file| {
        let text = read_workflow(file);
        if file == "macos-probe.yml" {
            format!(
                "{text}\n# a step's environment\n          TOKEN: ${{{{ secrets.PROBE_TOKEN }}}}\n"
            )
        } else {
            text
        }
    });
    assert_eq!(
        found,
        [
            "beta-macos-probe: calls macos-probe.yml, which names PROBE_TOKEN, without `secrets: inherit`; it would read them as empty"
        ]
    );
}

#[test]
fn a_secret_named_only_in_a_comment_needs_nothing() {
    let release = workflow();
    let found = secret_passing_errors(&release, |file| {
        let text = read_workflow(file);
        if file == "beta.yml" {
            format!("{text}\n# secrets.IN_A_COMMENT\n          run: true # secrets.AFTER_CODE\n")
        } else {
            text
        }
    });
    assert_eq!(found, Vec::<String>::new());
    assert_eq!(
        named_secrets("# secrets.A\n  x: y # secrets.B\n  z: ${{ secrets.C }}\n  w: secrets.\n"),
        ["C".to_owned()].into_iter().collect()
    );
}

#[test]
fn a_call_passing_secrets_to_another_repository_is_refused() {
    let text = edit(
        &workflow(),
        "    uses: komira-ai/docker-swarm-updater/",
        "    secrets: inherit\n    uses: komira-ai/docker-swarm-updater/",
    );
    assert_eq!(
        errors(&text),
        ["cp-deploy: passes secrets to a workflow of another repository"]
    );
}

#[test]
fn every_form_of_naming_a_secret_counts_and_prose_does_not() {
    let set =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(|n| (*n).to_owned()).collect() };
    // The field, the index in either quote, the context in another case, and an `if:` without
    // braces each name the secret.
    assert_eq!(
        named_secrets(
            "  a: ${{ secrets.A_1 }}\n  b: ${{ secrets['B'] }}\n  c: ${{ secrets[\"C\"] }}\n  d: ${{ SECRETS.D }}\n    if: secrets.E != ''\n"
        ),
        set(&["A_1", "B", "C", "D", "E"])
    );
    // The whole context, or an index by expression, names every secret.
    assert_eq!(
        named_secrets("  all: ${{ toJSON(secrets) }}\n"),
        set(&["*"])
    );
    assert_eq!(
        named_secrets("  x: ${{ secrets[inputs.name] }}\n"),
        set(&["inputs.name"])
    );
    assert_eq!(named_secrets("  x: ${{ secrets[''] }}\n"), set(&["*"]));
    // Prose, keys and other identifiers name none: a notice about secrets, a `secrets:` key,
    // a context of another name ending in `secrets`, and a word starting with it.
    assert_eq!(
        named_secrets(
            "  run: echo \"::notice::its secrets are not set up\"\n    secrets: inherit\n  x: ${{ my_secrets.A }}\n  y: ${{ steps.secrets.outputs.A }}\n  z: ${{ secretsfile }}\n"
        ),
        BTreeSet::new()
    );
}

#[test]
fn the_whole_context_named_in_a_called_workflow_needs_inherit() {
    let release = workflow();
    let found = secret_passing_errors(&release, |file| {
        let text = read_workflow(file);
        if file == "beta.yml" {
            format!("{text}\n          ALL: ${{{{ toJSON(secrets) }}}}\n")
        } else {
            text
        }
    });
    assert_eq!(
        found,
        [
            "beta-macos: calls beta.yml, which names *, without `secrets: inherit`; it would read them as empty"
        ]
    );
}

#[test]
fn inherit_with_a_trailing_comment_passes() {
    let text = edit(
        &workflow(),
        "    secrets: inherit\n",
        "    secrets: inherit # the installer certificate\n",
    );
    assert_eq!(errors(&text), Vec::<String>::new());
}
