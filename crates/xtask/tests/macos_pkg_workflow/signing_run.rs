//! When the package workflow signs: in the release's call (a push to main that asks, `sign:
//! true`) and in a signing run (a manual run with `sign` set, on a branch of this repository),
//! and in no other run. A signing run anywhere else (a tag, a fork's copy) is refused by name at
//! the first step of `build`, which every signing job needs, so it fails rather than going green
//! with nothing signed. The conditions are read from the committed file and evaluated
//! (expr.rs) for each kind of run.

use super::*;

/// The `if:` of `sign-code`, `package` and `sign`.
pub const SIGN_IF: &str = "if: ${{ (github.event_name == 'push' && github.ref == 'refs/heads/main' && inputs.sign) || (github.event_name == 'workflow_dispatch' && inputs.sign && github.repository == 'komira-ai/jobscheduler' && startsWith(github.ref, 'refs/heads/')) }}";

/// The name of the first step of `build`, which refuses a signing run anywhere but a branch of
/// this repository.
const REFUSAL: &str =
    "\"Refused: a signing run (sign: true) runs only on a branch of komira-ai/jobscheduler\"";

/// The jobs that hold the signing secrets or need what they sign.
const SIGNING_JOBS: [&str; 3] = ["sign-code", "package", "sign"];

/// The condition of `job`, its `if:` line.
fn job_if(text: &str, job: &str) -> String {
    jobs(text)[job]
        .iter()
        .find(|l| indent(l) == 4 && l.trim().starts_with("if:"))
        .map(|l| l.trim().to_owned())
        .unwrap_or_default()
}

/// The steps of `build` as written, each step's lines.
fn build_steps(text: &str) -> Vec<(String, Vec<&str>)> {
    steps(&jobs(text)["build"])
}

/// One kind of run: what `github.event_name`, `github.ref`, `github.repository` and
/// `inputs.sign` are (an input left out is absent), and whether it must sign and whether it
/// must be refused.
struct Run {
    what: &'static str,
    event: &'static str,
    git_ref: &'static str,
    repository: &'static str,
    sign: Option<&'static str>,
    signs: bool,
    refused: bool,
}

const THIS: &str = "komira-ai/jobscheduler";
const FORK: &str = "someone/jobscheduler";

const RUNS: [Run; 13] = [
    Run {
        what: "the release's call, a push to main that asks",
        event: "push",
        git_ref: "refs/heads/main",
        repository: THIS,
        sign: Some("true"),
        signs: true,
        refused: false,
    },
    Run {
        what: "a call on a push to main that does not ask",
        event: "push",
        git_ref: "refs/heads/main",
        repository: THIS,
        sign: Some("false"),
        signs: false,
        refused: false,
    },
    Run {
        what: "a call on a push elsewhere that asks",
        event: "push",
        git_ref: "refs/heads/topic",
        repository: THIS,
        sign: Some("true"),
        signs: false,
        refused: false,
    },
    Run {
        what: "a signing run on a branch",
        event: "workflow_dispatch",
        git_ref: "refs/heads/topic",
        repository: THIS,
        sign: Some("true"),
        signs: true,
        refused: false,
    },
    Run {
        what: "a signing run on main",
        event: "workflow_dispatch",
        git_ref: "refs/heads/main",
        repository: THIS,
        sign: Some("true"),
        signs: true,
        refused: false,
    },
    Run {
        what: "a manual run on a branch without sign",
        event: "workflow_dispatch",
        git_ref: "refs/heads/topic",
        repository: THIS,
        sign: Some("false"),
        signs: false,
        refused: false,
    },
    Run {
        what: "a manual run on main without sign",
        event: "workflow_dispatch",
        git_ref: "refs/heads/main",
        repository: THIS,
        sign: Some("false"),
        signs: false,
        refused: false,
    },
    Run {
        what: "a manual run that gives no sign at all",
        event: "workflow_dispatch",
        git_ref: "refs/heads/topic",
        repository: THIS,
        sign: None,
        signs: false,
        refused: false,
    },
    Run {
        what: "a signing run on a tag",
        event: "workflow_dispatch",
        git_ref: "refs/tags/v0.1.0",
        repository: THIS,
        sign: Some("true"),
        signs: false,
        refused: true,
    },
    Run {
        what: "a signing run on a tag whose name holds refs/heads/",
        event: "workflow_dispatch",
        git_ref: "refs/tags/refs/heads/main",
        repository: THIS,
        sign: Some("true"),
        signs: false,
        refused: true,
    },
    Run {
        what: "a signing run in a fork",
        event: "workflow_dispatch",
        git_ref: "refs/heads/main",
        repository: FORK,
        sign: Some("true"),
        signs: false,
        refused: true,
    },
    Run {
        what: "a manual run in a fork without sign",
        event: "workflow_dispatch",
        git_ref: "refs/heads/main",
        repository: FORK,
        sign: Some("false"),
        signs: false,
        refused: false,
    },
    Run {
        what: "a pull request",
        event: "pull_request",
        git_ref: "refs/pull/1/merge",
        repository: THIS,
        sign: Some("true"),
        signs: false,
        refused: false,
    },
];

impl Run {
    fn names(&self) -> BTreeMap<&'static str, &'static str> {
        let mut names = BTreeMap::from([
            ("github.event_name", self.event),
            ("github.ref", self.git_ref),
            ("github.repository", self.repository),
        ]);
        if let Some(sign) = self.sign {
            names.insert("inputs.sign", sign);
        }
        names
    }
}

/// The condition of the refusal step of `build` (its `if:`, without the key).
fn refusal_if(text: &str) -> String {
    build_steps(text)
        .first()
        .and_then(|(_, lines)| lines.iter().find(|l| l.trim().starts_with("if: ")))
        .map(|l| l.trim().trim_start_matches("if: ").to_owned())
        .unwrap_or_default()
}

#[test]
fn a_push_to_main_that_asks_and_a_signing_run_on_a_branch_sign_and_nothing_else_does() {
    // Catches: a condition that signs a pull request, a push that does not ask, a tag, a fork,
    // or a manual run without `sign`; or one that does not sign the release's call or a signing
    // run on a branch.
    let text = read(WORKFLOW);
    for job in SIGNING_JOBS {
        let line = job_if(&text, job);
        assert_eq!(line, SIGN_IF, "{job}");
        let condition = line.trim_start_matches("if: ");
        for run in &RUNS {
            assert_eq!(
                expr::holds(condition, &run.names()),
                Ok(run.signs),
                "{job}: {}",
                run.what
            );
        }
    }
}

#[test]
fn a_signing_run_outside_a_branch_of_this_repository_is_refused_and_no_other_run_is() {
    // Catches: a signing run on a tag or in a fork that goes green with nothing signed, and a
    // refusal that also stops the release's call, a manual run without `sign` or a signing run
    // on a branch.
    let text = read(WORKFLOW);
    let condition = refusal_if(&text);
    for run in &RUNS {
        assert_eq!(
            expr::holds(&condition, &run.names()),
            Ok(run.refused),
            "{}",
            run.what
        );
    }
}

#[test]
fn every_manual_run_with_sign_either_signs_or_is_refused_and_never_both() {
    // Catches: a gap between the two conditions, where a manual run asked to sign neither signs
    // nor fails, and an overlap, where a run is refused and signs.
    let text = read(WORKFLOW);
    let sign = SIGN_IF.trim_start_matches("if: ");
    let refusal = refusal_if(&text);
    for git_ref in [
        "refs/heads/main",
        "refs/heads/a/b",
        "refs/tags/v1",
        "refs/pull/2/head",
        "",
    ] {
        for repository in [THIS, FORK, "Komira-AI/JobScheduler", ""] {
            let names = BTreeMap::from([
                ("github.event_name", "workflow_dispatch"),
                ("github.ref", git_ref),
                ("github.repository", repository),
                ("inputs.sign", "true"),
            ]);
            let signs = expr::holds(sign, &names).expect("the signing condition");
            let refused = expr::holds(&refusal, &names).expect("the refusal");
            assert!(
                signs != refused,
                "{git_ref} {repository}: {signs} {refused}"
            );
        }
    }
}

/// One line per way the first step of `build` is not the refusal: named for what it refuses,
/// run under the refusal's condition, failing, and not let off by `continue-on-error`.
fn refusal_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let all = build_steps(text);
    let Some((name, lines)) = all.first() else {
        return vec!["build has no steps".to_owned()];
    };
    if *name != REFUSAL {
        problems.push(format!(
            "the first step of build is not the refusal: {name}"
        ));
    }
    let body: Vec<&str> = lines.iter().skip(1).map(|l| l.trim()).collect();
    let want = format!(
        "if: ${{{{ github.event_name == 'workflow_dispatch' && inputs.sign && !(github.repository == '{THIS}' && startsWith(github.ref, 'refs/heads/')) }}}}"
    );
    if body != [want.as_str(), "run: exit 1"] {
        problems.push(format!(
            "the refusal is not `{want}` then `run: exit 1` and nothing else: {body:?}"
        ));
    }
    problems
}

#[test]
fn the_refusal_is_the_first_step_of_build_and_fails() {
    assert_eq!(refusal_problems(&read(WORKFLOW)), Vec::<String>::new());
}

#[test]
fn a_build_without_the_refusal_first_or_with_a_refusal_that_cannot_fail_is_refused() {
    // Catches: the refusal removed, moved after another step, under a name that does not say
    // why, let off by `continue-on-error`, exiting 0, or under a looser condition.
    let text = read(WORKFLOW);
    let step = format!(
        "      - name: {REFUSAL}\n        if: ${{{{ github.event_name == 'workflow_dispatch' && inputs.sign && !(github.repository == '{THIS}' && startsWith(github.ref, 'refs/heads/')) }}}}\n        run: exit 1\n"
    );
    assert_eq!(text.matches(&step).count(), 1);
    let check = "      - name: Check the version\n        run: |\n          set -euo pipefail\n          if ! [[ \"$VERSION\" =~ ^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$ ]]; then\n            echo \"::error::the version must be MAJOR.MINOR.PATCH\"\n            exit 1\n          fi\n";
    assert!(
        text.contains(&format!("{step}{check}")),
        "the version check follows"
    );
    let cases: [(&str, String, &str); 6] = [
        ("removed", String::new(), "not the refusal"),
        ("moved", String::new(), "not the refusal"),
        (
            "renamed",
            step.replace(REFUSAL, "Check the run"),
            "not the refusal",
        ),
        (
            "let off",
            step.replace(
                "        run: exit 1\n",
                "        continue-on-error: true\n        run: exit 1\n",
            ),
            "and nothing else",
        ),
        (
            "exits 0",
            step.replace("run: exit 1", "run: exit 0"),
            "and nothing else",
        ),
        (
            "looser",
            step.replace("inputs.sign && !(", "!("),
            "and nothing else",
        ),
    ];
    for (what, to, says) in cases {
        let changed = if what == "moved" {
            text.replacen(&format!("{step}{check}"), &format!("{check}{step}"), 1)
        } else {
            text.replacen(&step, &to, 1)
        };
        assert_ne!(changed, text, "{what}");
        let problems = refusal_problems(&changed);
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{what} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn the_sign_input_is_read_only_by_the_signing_conditions_and_the_refusal() {
    // Catches: a step or an environment that reads the input (`inputs.sign` or the string form
    // `github.event.inputs.sign`) and so acts on a signing run outside the two conditions.
    let text = read(WORKFLOW);
    let readers: Vec<&str> = code(&text)
        .into_iter()
        .filter(|l| l.contains("inputs.sign"))
        .map(str::trim)
        .collect();
    let refusal = format!("if: {}", refusal_if(&text));
    assert_eq!(readers, [refusal.as_str(), SIGN_IF, SIGN_IF, SIGN_IF]);
    assert!(!text.contains("github.event.inputs"));
}

#[test]
fn the_evaluator_reads_a_boolean_input_as_a_boolean_and_starts_with_ignoring_case() {
    // Catches: an evaluator that reads the input `false` as a non-empty, so true, string (which
    // would make every manual run a signing run in these tests), or a `startsWith` that is not
    // GitHub's.
    let names = BTreeMap::from([
        ("inputs.sign", "false"),
        ("inputs.yes", "true"),
        ("github.word", "false"),
        ("github.ref", "refs/heads/topic"),
    ]);
    let holds = |text: &str| expr::holds(text, &names);
    assert_eq!(holds("${{ inputs.sign }}"), Ok(false));
    assert_eq!(holds("${{ inputs.yes }}"), Ok(true));
    assert_eq!(holds("${{ github.word }}"), Ok(true));
    assert_eq!(holds("${{ inputs.unset }}"), Ok(false));
    assert_eq!(holds("${{ !inputs.sign && !!inputs.yes }}"), Ok(true));
    assert_eq!(
        holds("${{ startsWith(github.ref, 'REFS/HEADS/') }}"),
        Ok(true)
    );
    assert_eq!(
        holds("${{ startsWith(github.ref, 'refs/tags/') }}"),
        Ok(false)
    );
    assert_eq!(holds("${{ github.ref != 'refs/heads/TOPIC' }}"), Ok(false));
    assert_eq!(
        holds("${{ (inputs.yes || inputs.sign) && 'x' == 'X' }}"),
        Ok(true)
    );
    for bad in [
        "inputs.sign",
        "${{ startsWith(github.ref) }}",
        "${{ startsWith(github.ref, 'a' }}",
        "${{ startsWith(inputs.yes, 'a') }}",
        "${{ endsWith(github.ref, 'a') }}",
        "${{ inputs.yes == 'true' }}",
        "${{ (inputs.yes }}",
        "${{ 'open }}",
        "${{ inputs.yes ) }}",
        "${{ github.ref ~ 'x' }}",
        "${{ && }}",
    ] {
        assert!(holds(bad).is_err(), "{bad}");
    }
}
