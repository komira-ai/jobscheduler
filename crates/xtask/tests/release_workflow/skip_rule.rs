//! What GitHub does with release.yml's `needs:` graph and job `if:`s, modelled and run over
//! scenarios, so that a job that the text tests hold to its needs and its condition can still be
//! seen to run (or not) when a job up its chain was skipped or failed.
//!
//! The model follows GitHub's documented rules for job conditions:
//!
//! - A job's `if:` that calls none of the status functions `success()`, `failure()`,
//!   `cancelled()` and `always()` is read as `success() && (<if>)`, and a job with no `if:` as
//!   `success()` ("A default status check of `success()` is applied unless you include one of
//!   these functions").
//! - `success()` is true only when every job up the job's chain succeeded, not only the jobs it
//!   names in `needs:`: "If a job fails or is skipped, all jobs that need it are skipped", and "a
//!   failure or skip applies to all jobs in the dependency chain from the point of failure or skip
//!   onwards". `failure()` is true when any job up the chain failed ("If you have a chain of
//!   dependent jobs, `failure()` returns `true` if any ancestor job fails").
//! - A job whose condition is false is skipped; its `result` is `skipped` and its outputs are
//!   empty.
//!
//! A scenario is a push to main that is not cancelled: it names the jobs that fail when they run
//! (every other job that runs succeeds), the outputs of the jobs that ran, and the repository
//! variables. A matrix job or a call of another workflow is one job, with the one result GitHub
//! gives its caller. [`run`] gives each job its outcome; it refuses a condition that names a job
//! outside the job's `needs:` (GitHub reads it as empty), a context or function this model does
//! not know, a comparison of a boolean with a string, and a `needs:` that names no job or makes a
//! cycle, so the model never answers for a condition it does not read.

use std::collections::BTreeMap;

use super::fast_lane::{ifs, needs};
use super::{jobs, workflow};

/// What became of a job in a scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// It ran and passed.
    Success,
    /// It ran and failed.
    Failure,
    /// Its condition was false.
    Skipped,
}

impl Outcome {
    /// The job's `needs.<job>.result`.
    fn word(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Skipped => "skipped",
        }
    }
}

/// A push to main: the jobs that fail when they run, the outputs of the jobs that run
/// (`(job, output, value)`), and the repository variables that are set.
#[derive(Clone, Copy)]
struct Scenario<'a> {
    fails: &'a [&'a str],
    outputs: &'a [(&'a str, &'a str, &'a str)],
    vars: &'a [(&'a str, &'a str)],
}

/// A job of the graph: the jobs it needs, and its `if:` without `${{ }}`, if it has one.
struct Job {
    needs: Vec<String>,
    condition: Option<String>,
}

/// The jobs of `text`, by name.
fn graph(text: &str) -> Result<BTreeMap<String, Job>, String> {
    let mut graph = BTreeMap::new();
    for (name, lines) in jobs(text) {
        let condition = match ifs(&lines)[..] {
            [] => None,
            [line] => {
                let expr = line
                    .trim_start()
                    .strip_prefix("if: ${{")
                    .and_then(|rest| rest.strip_suffix("}}"))
                    .ok_or_else(|| format!("{name}: an `if:` that is not one ${{{{ }}}}"))?;
                Some(expr.trim().to_owned())
            }
            _ => return Err(format!("{name}: more than one `if:`")),
        };
        graph.insert(
            name,
            Job {
                needs: needs(&lines),
                condition,
            },
        );
    }
    Ok(graph)
}

/// The outcome of every job of `text` in `scenario`.
fn run(text: &str, scenario: Scenario<'_>) -> Result<BTreeMap<String, Outcome>, String> {
    let graph = graph(text)?;
    for (name, job) in &graph {
        if let Some(missing) = job.needs.iter().find(|n| !graph.contains_key(*n)) {
            return Err(format!("{name} needs {missing}, which is no job"));
        }
    }
    let mut done: BTreeMap<String, Outcome> = BTreeMap::new();
    while done.len() < graph.len() {
        let ready: Vec<&String> = graph
            .iter()
            .filter(|(name, job)| {
                !done.contains_key(*name) && job.needs.iter().all(|n| done.contains_key(n))
            })
            .map(|(name, _)| name)
            .collect();
        if ready.is_empty() {
            return Err("the needs make a cycle".to_owned());
        }
        for name in ready {
            let job = &graph[name];
            let up = ancestors(&graph, name);
            let context = Context {
                job: name,
                needs: &job.needs,
                up: &up,
                done: &done,
                scenario,
            };
            let runs = match &job.condition {
                None => context.success(),
                Some(expr) => {
                    let tokens = tokenize(expr)?;
                    let value = Parser {
                        tokens: &tokens,
                        at: 0,
                        context: &context,
                    }
                    .whole()?;
                    let explicit = tokens.windows(2).any(|w| {
                        matches!(&w[0], Token::Ident(f) if STATUS.contains(&f.as_str()))
                            && w[1] == Token::Open
                    });
                    value.truthy() && (explicit || context.success())
                }
            };
            let outcome = if !runs {
                Outcome::Skipped
            } else if scenario.fails.contains(&name.as_str()) {
                Outcome::Failure
            } else {
                Outcome::Success
            };
            done.insert(name.clone(), outcome);
        }
    }
    Ok(done)
}

/// Every job up the chain of `name`: the jobs it needs, the jobs they need, and so on.
fn ancestors(graph: &BTreeMap<String, Job>, name: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut todo: Vec<&str> = vec![name];
    while let Some(next) = todo.pop() {
        for need in &graph[next].needs {
            if !found.contains(need) {
                found.push(need.clone());
                todo.push(need);
            }
        }
    }
    found
}

/// The status functions.
const STATUS: [&str; 4] = ["success", "failure", "cancelled", "always"];

/// What a condition of one job reads.
struct Context<'a> {
    job: &'a str,
    needs: &'a [String],
    up: &'a [String],
    done: &'a BTreeMap<String, Outcome>,
    scenario: Scenario<'a>,
}

impl Context<'_> {
    /// `success()`: every job up the chain succeeded.
    fn success(&self) -> bool {
        self.up.iter().all(|n| self.done[n] == Outcome::Success)
    }

    /// The value of a context path (`github.ref`, `needs.<job>.result`, ...).
    fn lookup(&self, path: &str) -> Result<Value, String> {
        let parts: Vec<&str> = path.split('.').collect();
        let text = |s: &str| Ok(Value::Text(s.to_owned()));
        match parts[..] {
            ["github", "event_name"] => text("push"),
            ["github", "ref"] => text("refs/heads/main"),
            ["vars", name] => text(
                self.scenario
                    .vars
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map_or("", |(_, v)| *v),
            ),
            ["needs", job, ..] if !self.needs.iter().any(|n| n == job) => Err(format!(
                "{}: its `if:` reads {path}, but it does not need {job}",
                self.job
            )),
            ["needs", job, "result"] => text(self.done[job].word()),
            ["needs", job, "outputs", output] => {
                if self.done[job] == Outcome::Skipped {
                    return text("");
                }
                text(
                    self.scenario
                        .outputs
                        .iter()
                        .find(|(j, o, _)| *j == job && *o == output)
                        .map_or("", |(_, _, v)| *v),
                )
            }
            _ => Err(format!("{}: unknown context {path}", self.job)),
        }
    }

    /// The value of a call of the function `name` with no argument.
    fn call(&self, name: &str) -> Result<Value, String> {
        match name {
            "success" => Ok(Value::Bool(self.success())),
            "failure" => Ok(Value::Bool(
                self.up.iter().any(|n| self.done[n] == Outcome::Failure),
            )),
            "cancelled" => Ok(Value::Bool(false)),
            "always" => Ok(Value::Bool(true)),
            _ => Err(format!("{}: unknown function {name}()", self.job)),
        }
    }
}

/// A value of an expression.
#[derive(Debug, PartialEq)]
enum Value {
    Bool(bool),
    Text(String),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Text(s) => !s.is_empty(),
        }
    }
}

/// A token of an expression.
#[derive(Debug, PartialEq)]
enum Token {
    Ident(String),
    Text(String),
    Not,
    And,
    Or,
    Eq,
    Ne,
    Open,
    Close,
}

/// The tokens of `expr`.
fn tokenize(expr: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut rest = expr;
    while let Some(c) = rest.chars().next() {
        let two = rest.get(..2).unwrap_or(rest);
        let (token, len) = match (c, two) {
            (_, "&&") => (Token::And, 2),
            (_, "||") => (Token::Or, 2),
            (_, "==") => (Token::Eq, 2),
            (_, "!=") => (Token::Ne, 2),
            ('!', _) => (Token::Not, 1),
            ('(', _) => (Token::Open, 1),
            (')', _) => (Token::Close, 1),
            (' ', _) => {
                rest = &rest[1..];
                continue;
            }
            ('\'', _) => {
                let end = rest[1..]
                    .find('\'')
                    .ok_or_else(|| format!("an unclosed string in {expr}"))?;
                (Token::Text(rest[1..=end].to_owned()), end + 2)
            }
            _ if c.is_ascii_alphabetic() => {
                let len = rest
                    .find(|ch: char| !(ch.is_ascii_alphanumeric() || "._-".contains(ch)))
                    .unwrap_or(rest.len());
                (Token::Ident(rest[..len].to_owned()), len)
            }
            _ => return Err(format!("{c:?} in {expr}")),
        };
        tokens.push(token);
        rest = &rest[len..];
    }
    Ok(tokens)
}

/// Evaluates tokens: `or := and ('||' and)*`, `and := cmp ('&&' cmp)*`,
/// `cmp := not (('==' | '!=') not)?`, `not := '!' not | atom` (so `!` binds tighter than `==`,
/// as in GitHub's expressions),
/// `atom := '(' or ')' | text | ident | ident '(' ')'`.
struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    context: &'a Context<'a>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<&'a Token> {
        self.at += 1;
        self.tokens.get(self.at - 1)
    }

    fn whole(mut self) -> Result<Value, String> {
        let value = self.or()?;
        match self.peek() {
            None => Ok(value),
            Some(t) => Err(format!("{}: {t:?} after the expression", self.context.job)),
        }
    }

    fn or(&mut self) -> Result<Value, String> {
        let mut value = self.and()?;
        while self.peek() == Some(&Token::Or) {
            self.next();
            let right = self.and()?;
            value = Value::Bool(value.truthy() || right.truthy());
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<Value, String> {
        let mut value = self.cmp()?;
        while self.peek() == Some(&Token::And) {
            self.next();
            let right = self.cmp()?;
            value = Value::Bool(value.truthy() && right.truthy());
        }
        Ok(value)
    }

    fn not(&mut self) -> Result<Value, String> {
        if self.peek() == Some(&Token::Not) {
            self.next();
            return Ok(Value::Bool(!self.not()?.truthy()));
        }
        self.atom()
    }

    fn cmp(&mut self) -> Result<Value, String> {
        let left = self.not()?;
        let equal = match self.peek() {
            Some(Token::Eq) => true,
            Some(Token::Ne) => false,
            _ => return Ok(left),
        };
        self.next();
        let right = self.not()?;
        match (&left, &right) {
            (Value::Text(a), Value::Text(b)) => Ok(Value::Bool(a.eq_ignore_ascii_case(b) == equal)),
            _ => Err(format!(
                "{}: compares {left:?} with {right:?}",
                self.context.job
            )),
        }
    }

    fn atom(&mut self) -> Result<Value, String> {
        let job = self.context.job;
        match self.next() {
            Some(Token::Open) => {
                let value = self.or()?;
                match self.next() {
                    Some(Token::Close) => Ok(value),
                    _ => Err(format!("{job}: an unclosed (")),
                }
            }
            Some(Token::Text(s)) => Ok(Value::Text(s.clone())),
            Some(Token::Ident(name)) => {
                let name = name.clone();
                if self.peek() == Some(&Token::Open) {
                    self.next();
                    match self.next() {
                        Some(Token::Close) => self.context.call(&name),
                        _ => Err(format!("{job}: {name}( takes no argument here")),
                    }
                } else {
                    self.context.lookup(&name)
                }
            }
            t => Err(format!("{job}: {t:?} where a value was expected")),
        }
    }
}

/// The variables of a repository with the control-plane deploy and `desired` switched on.
const VARS: [(&str, &str); 3] = [
    ("CP_DEPLOY_ENDPOINT", "set"),
    ("CP_DEPLOY_TAILSCALE_TAGS", "set"),
    ("JS_AGENT_SETS_DESIRED", "true"),
];

/// The outputs of a normal release: the plan releases, the turn says go, and the first write
/// of each apt suite meets no conflict (so every retry slot is skipped).
const NORMAL: [(&str, &str, &str); 4] = [
    ("plan", "release", "true"),
    ("turn", "go", "true"),
    ("testing-apt-write", "conflict", "false"),
    ("stable-apt-write", "conflict", "false"),
];

/// The jobs that tell the farm: the gate, the control-plane deploy, and `desired`.
const FARM: [&str; 4] = ["blackbox", "cp-deploy-digest", "cp-deploy", "desired"];

/// The outcomes of `jobs` in `scenario` over release.yml.
fn outcomes(text: &str, scenario: Scenario<'_>, jobs: &[&str]) -> Vec<(String, Outcome)> {
    let all = run(text, scenario).unwrap_or_else(|e| panic!("{e}"));
    jobs.iter().map(|j| ((*j).to_owned(), all[*j])).collect()
}

/// `jobs` paired with `outcome`.
fn each(jobs: &[&str], outcome: Outcome) -> Vec<(String, Outcome)> {
    jobs.iter().map(|j| ((*j).to_owned(), outcome)).collect()
}

/// A normal release with `fails` failing.
fn normal<'a>(fails: &'a [&'a str]) -> Scenario<'a> {
    Scenario {
        fails,
        outputs: &NORMAL,
        vars: &VARS,
    }
}

/// Asserts that a normal release (no apt conflict, every retry slot skipped) runs the gate, the
/// control-plane deploy and `desired`, that the retry slots were skipped, and that `deployed`
/// and `release-green` run while `release-red` does not.
#[test]
fn a_normal_release_runs_the_gate_and_tells_the_farm() {
    let text = workflow();
    assert_eq!(
        outcomes(
            &text,
            normal(&[]),
            &[
                "testing-apt-write-2",
                "testing-apt-write-3",
                "stable-apt-write-2",
                "stable-apt-write-3",
                "release-red",
            ]
        ),
        each(
            &[
                "testing-apt-write-2",
                "testing-apt-write-3",
                "stable-apt-write-2",
                "stable-apt-write-3",
                "release-red",
            ],
            Outcome::Skipped
        )
    );
    let told = ["check-apt", "deployed", "release-green"]
        .into_iter()
        .chain(FARM)
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes(&text, normal(&[]), &told),
        each(&told, Outcome::Success)
    );
}

/// Asserts that an apt conflict that the second slot (or the third) resolves still runs the
/// gate, the control-plane deploy and `desired`, through the slot that wrote; and that a stable
/// conflict that its second slot resolves still runs `desired`.
#[test]
fn an_apt_conflict_resolved_by_a_retry_slot_still_tells_the_farm() {
    let text = workflow();
    for (outputs, wrote, skipped) in [
        (
            &[
                ("plan", "release", "true"),
                ("turn", "go", "true"),
                ("testing-apt-write", "conflict", "true"),
                ("testing-apt-write-2", "conflict", "false"),
                ("stable-apt-write", "conflict", "false"),
            ][..],
            "testing-apt-write-2",
            "testing-apt-write-3",
        ),
        (
            &[
                ("plan", "release", "true"),
                ("turn", "go", "true"),
                ("testing-apt-write", "conflict", "true"),
                ("testing-apt-write-2", "conflict", "true"),
                ("testing-apt-write-3", "conflict", "false"),
                ("stable-apt-write", "conflict", "false"),
            ][..],
            "testing-apt-write-3",
            "stable-apt-write-2",
        ),
        (
            &[
                ("plan", "release", "true"),
                ("turn", "go", "true"),
                ("testing-apt-write", "conflict", "false"),
                ("stable-apt-write", "conflict", "true"),
                ("stable-apt-write-2", "conflict", "false"),
            ][..],
            "stable-apt-write-2",
            "stable-apt-write-3",
        ),
    ] {
        let scenario = Scenario {
            fails: &[],
            outputs,
            vars: &VARS,
        };
        assert_eq!(
            outcomes(&text, scenario, &[wrote, skipped]),
            [
                (wrote.to_owned(), Outcome::Success),
                (skipped.to_owned(), Outcome::Skipped)
            ],
            "{wrote}"
        );
        assert_eq!(
            outcomes(&text, scenario, &FARM),
            each(&FARM, Outcome::Success),
            "{wrote}"
        );
    }
}

/// Asserts that a failure of a job the gate checks (`check-apt`, `publish`, the first install of
/// the macOS package, or the package job it needs) runs neither the gate nor anything that tells
/// the farm, and that `release-red` reports it.
#[test]
fn a_failed_black_box_test_or_publish_tells_the_farm_nothing() {
    let text = workflow();
    for failed in [
        "check-apt",
        "publish",
        "beta-macos-first-install",
        "beta-macos-pkg",
    ] {
        assert_eq!(
            outcomes(&text, normal(&[failed]), &FARM),
            each(&FARM, Outcome::Skipped),
            "{failed}"
        );
        assert_eq!(
            outcomes(&text, normal(&[failed]), &["release-red", "release-green"]),
            [
                ("release-red".to_owned(), Outcome::Success),
                ("release-green".to_owned(), Outcome::Skipped)
            ],
            "{failed}"
        );
    }
}

/// Asserts that a failed gate runs neither deploy job nor `desired`.
#[test]
fn a_failed_gate_tells_the_farm_nothing() {
    let text = workflow();
    assert_eq!(
        outcomes(&text, normal(&["blackbox"]), &FARM),
        [("blackbox".to_owned(), Outcome::Failure)]
            .into_iter()
            .chain(each(&FARM[1..], Outcome::Skipped))
            .collect::<Vec<_>>()
    );
}

/// Asserts that a release whose `check-apt` was skipped (the apt jobs are off, so the first write
/// reports no conflict output, or every slot met a conflict) runs no gate and tells the farm
/// nothing.
#[test]
fn a_skipped_check_apt_tells_the_farm_nothing() {
    let text = workflow();
    for outputs in [
        &[("plan", "release", "true"), ("turn", "go", "true")][..],
        &[
            ("plan", "release", "true"),
            ("turn", "go", "true"),
            ("testing-apt-write", "conflict", "true"),
            ("testing-apt-write-2", "conflict", "true"),
            ("testing-apt-write-3", "conflict", "true"),
        ][..],
    ] {
        let scenario = Scenario {
            fails: &[],
            outputs,
            vars: &VARS,
        };
        let watched = ["check-apt"].into_iter().chain(FARM).collect::<Vec<_>>();
        assert_eq!(
            outcomes(&text, scenario, &watched),
            each(&watched, Outcome::Skipped)
        );
    }
}

/// Asserts that a failed beta job outside the gate's chain (the probe and the isolation test, as
/// in the first release after the gate) holds back neither the gate nor the farm, and that the
/// run is reported red.
#[test]
fn a_failed_beta_job_outside_the_chain_holds_nothing_back() {
    let text = workflow();
    let fails = ["beta-macos-probe", "beta-macos-isolation"];
    assert_eq!(
        outcomes(&text, normal(&fails), &FARM),
        each(&FARM, Outcome::Success)
    );
    assert_eq!(
        outcomes(&text, normal(&fails), &["release-red", "release-green"]),
        [
            ("release-red".to_owned(), Outcome::Success),
            ("release-green".to_owned(), Outcome::Skipped)
        ]
    );
}

/// Asserts that a failed `cp-deploy-digest` runs no `cp-deploy`, and that the deploy's
/// variables unset skip both deploy jobs while the gate and `desired` still run.
#[test]
fn the_control_plane_deploy_runs_only_after_its_digest() {
    let text = workflow();
    assert_eq!(
        outcomes(&text, normal(&["cp-deploy-digest"]), &FARM),
        [
            ("blackbox".to_owned(), Outcome::Success),
            ("cp-deploy-digest".to_owned(), Outcome::Failure),
            ("cp-deploy".to_owned(), Outcome::Skipped),
            ("desired".to_owned(), Outcome::Success),
        ]
    );
    let off = Scenario {
        fails: &[],
        outputs: &NORMAL,
        vars: &[("JS_AGENT_SETS_DESIRED", "true")],
    };
    assert_eq!(
        outcomes(&text, off, &FARM),
        [
            ("blackbox".to_owned(), Outcome::Success),
            ("cp-deploy-digest".to_owned(), Outcome::Skipped),
            ("cp-deploy".to_owned(), Outcome::Skipped),
            ("desired".to_owned(), Outcome::Success),
        ]
    );
}

/// A small workflow for the model's own tests: `a`, then `b` (skipped unless `a` says go),
/// then `c` with the condition `c_if`.
fn tiny(c_if: &str) -> String {
    format!(
        "jobs:\n  a:\n    runs-on: x\n  b:\n    needs: [a]\n    if: ${{{{ needs.a.outputs.go == 'true' }}}}\n  c:\n    needs: [b]\n{c_if}"
    )
}

/// Asserts the model's reading of the rule on a small graph: a skipped job skips a job below it
/// whose `if:` has no status function, even when the direct need succeeded one step further
/// down; `!cancelled() && !failure()` lets it run; `failure()` and `always()` read the chain.
#[test]
fn the_model_reads_the_rule_on_a_small_graph() {
    let skip_b = Scenario {
        fails: &[],
        outputs: &[],
        vars: &[],
    };
    let go = Scenario {
        fails: &["a"],
        outputs: &[("a", "go", "true")],
        vars: &[],
    };
    let three = |text: &str, s: Scenario<'_>| -> Vec<Outcome> {
        let all = run(text, s).unwrap_or_else(|e| panic!("{e}"));
        ["a", "b", "c"].iter().map(|j| all[*j]).collect()
    };
    use Outcome::{Failure, Skipped, Success};
    // No `if:` at all, and an `if:` with no status function: implied `success()`.
    assert_eq!(three(&tiny(""), skip_b), [Success, Skipped, Skipped]);
    let c_true = "    if: ${{ github.ref == 'refs/heads/main' }}\n";
    assert_eq!(three(&tiny(c_true), skip_b), [Success, Skipped, Skipped]);
    let explicit = "    if: ${{ !cancelled() && !failure() && needs.b.result != 'success' }}\n";
    assert_eq!(three(&tiny(explicit), skip_b), [Success, Skipped, Success]);
    // A failed grandparent: `b` reads its output, but the implied `success()` skips it.
    assert_eq!(three(&tiny(explicit), go), [Failure, Skipped, Skipped]);
    let failed = "    if: ${{ failure() }}\n";
    assert_eq!(three(&tiny(failed), go), [Failure, Skipped, Success]);
    let always = "    if: ${{ always() && (vars.X == 'ON' || vars.Y != '') }}\n";
    let on = Scenario {
        fails: &[],
        outputs: &[],
        vars: &[("X", "on")],
    };
    assert_eq!(three(&tiny(always), on), [Success, Skipped, Success]);
    assert_eq!(three(&tiny(always), skip_b), [Success, Skipped, Skipped]);
    let success = "    if: ${{ success() || 'x' }}\n";
    assert_eq!(three(&tiny(success), skip_b), [Success, Skipped, Success]);
}

/// Asserts that the model refuses, by name, what it does not read.
#[test]
fn the_model_refuses_what_it_does_not_read() {
    let none = Scenario {
        fails: &[],
        outputs: &[],
        vars: &[],
    };
    for (c_if, problem) in [
        (
            "    if: ${{ needs.a.result == 'success' }}\n",
            "c: its `if:` reads needs.a.result, but it does not need a",
        ),
        ("    if: ${{ env.X == '' }}\n", "c: unknown context env.X"),
        (
            "    if: ${{ needs.b.outputs }}\n",
            "c: unknown context needs.b.outputs",
        ),
        (
            "    if: ${{ startsWith() }}\n",
            "c: unknown function startsWith()",
        ),
        (
            "    if: ${{ always(github.ref) }}\n",
            "c: always( takes no argument here",
        ),
        (
            "    if: ${{ always() == 'true' }}\n",
            "c: compares Bool(true) with Text(\"true\")",
        ),
        ("    if: ${{ (always() }}\n", "c: an unclosed ("),
        (
            "    if: ${{ always() always() }}\n",
            "c: Ident(\"always\") after the expression",
        ),
        (
            "    if: ${{ && }}\n",
            "c: Some(And) where a value was expected",
        ),
        ("    if: ${{ 'x }}\n", "an unclosed string in 'x"),
        ("    if: ${{ 1 }}\n", "'1' in 1"),
        ("    if: always()\n", "c: an `if:` that is not one ${{ }}"),
        (
            "    if: ${{ always() }}\n    if: ${{ always() }}\n",
            "c: more than one `if:`",
        ),
    ] {
        assert_eq!(
            run(&tiny(c_if), none).err().as_deref(),
            Some(problem),
            "{c_if}"
        );
    }
    let unknown = "jobs:\n  a:\n    needs: [z]\n";
    assert_eq!(
        run(unknown, none).err().as_deref(),
        Some("a needs z, which is no job")
    );
    let cycle = "jobs:\n  a:\n    needs: [b]\n  b:\n    needs: [a]\n";
    assert_eq!(
        run(cycle, none).err().as_deref(),
        Some("the needs make a cycle")
    );
}

/// `text` with the `if:` of `job` left to GitHub's implied `success()`: its leading
/// `!cancelled() && !failure() && ` removed, as the conditions of the gate and the deploy were
/// before they named their results.
fn implied(text: &str, job: &str) -> String {
    let start = text
        .find(&format!("\n  {job}:\n"))
        .unwrap_or_else(|| panic!("no job {job}"));
    let from = "if: ${{ !cancelled() && !failure() && ";
    let at = start
        + text[start..]
            .find(from)
            .unwrap_or_else(|| panic!("{job} does not start its if: with {from}"));
    format!("{}if: ${{{{ {}", &text[..at], &text[at + from.len()..])
}

/// Asserts that each of the gate, `cp-deploy-digest` and `cp-deploy`, its `if:` left to the
/// implied `success()`, is skipped on a normal release, with everything below it: the skipped
/// retry slots up its chain hold it back, so naming the results is what makes it run.
#[test]
fn the_implied_success_skips_the_gate_and_the_deploy_on_a_normal_release() {
    let text = workflow();
    use Outcome::{Skipped, Success};
    for (job, want) in [
        ("blackbox", [Skipped, Skipped, Skipped, Skipped]),
        ("cp-deploy-digest", [Success, Skipped, Skipped, Success]),
        ("cp-deploy", [Success, Success, Skipped, Success]),
    ] {
        let want: Vec<(String, Outcome)> = FARM
            .iter()
            .zip(want)
            .map(|(j, o)| ((*j).to_owned(), o))
            .collect();
        assert_eq!(
            outcomes(&implied(&text, job), normal(&[]), &FARM),
            want,
            "{job}"
        );
    }
}
