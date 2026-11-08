/// The workflow whose pull request run is not cancelled by a newer push: the release dry run,
/// which a pull request starts only when it changes release.yml itself
/// (`the_release_dry_run_starts_only_on_a_change_to_release_yml` pins that), and which this
/// change leaves as it is.
pub(crate) const RELEASE_DRY_RUN: &str = "release.yml";

pub(crate) fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A workflow cut into its top-level lines (before `jobs:`) and its jobs, comments dropped.
pub(crate) struct Workflow {
    pub(crate) file: String,
    pub(crate) top: Vec<String>,
    pub(crate) jobs: Vec<(String, Vec<String>)>,
}

impl Workflow {
    pub(crate) fn parse(file: &str, text: &str) -> Workflow {
        let mut top = Vec::new();
        let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
        let mut in_jobs = false;
        for line in text.lines() {
            if line.trim_start().starts_with('#') || line.trim().is_empty() {
                continue;
            }
            if line == "jobs:" {
                in_jobs = true;
            } else if in_jobs && indent(line) == 0 {
                in_jobs = false;
                top.push(line.to_owned());
            } else if in_jobs && indent(line) == 2 {
                let name = line.trim().trim_end_matches(':').to_owned();
                jobs.push((name, Vec::new()));
            } else if in_jobs {
                jobs.last_mut()
                    .expect("a job line before any job")
                    .1
                    .push(line.to_owned());
            } else {
                top.push(line.to_owned());
            }
        }
        Workflow {
            file: file.to_owned(),
            top,
            jobs,
        }
    }

    /// Whether the workflow starts on a pull request: `on: pull_request`,
    /// `on: [..., pull_request]` or an `on:` block with a `pull_request` key.
    pub(crate) fn on_pull_request(&self) -> bool {
        let mut in_on = false;
        for line in &self.top {
            if indent(line) == 0 {
                in_on = line == "on:";
                if line
                    .strip_prefix("on:")
                    .is_some_and(|value| value.contains("pull_request"))
                {
                    return true;
                }
            } else if in_on && indent(line) == 2 && line.trim().starts_with("pull_request") {
                return true;
            }
        }
        false
    }

    /// The value of the top-level key `key` and the lines under it.
    pub(crate) fn top_block(&self, key: &str) -> Option<Vec<String>> {
        block(&self.top, 0, key)
    }
}

/// The key `key` at `at` spaces in `lines`: its own value (if any) and the trimmed lines
/// indented under it.
pub(crate) fn block(lines: &[String], at: usize, key: &str) -> Option<Vec<String>> {
    let head = format!("{key}:");
    let start = lines.iter().position(|l| {
        indent(l) == at && (l.trim() == head || l.trim().starts_with(&format!("{head} ")))
    })?;
    let mut out = Vec::new();
    let value = lines[start].trim()[head.len()..].trim();
    if !value.is_empty() {
        out.push(value.to_owned());
    }
    out.extend(
        lines[start + 1..]
            .iter()
            .take_while(|l| indent(l) > at)
            .map(|l| l.trim().to_owned()),
    );
    Some(out)
}

/// The `&&`-joined parts of an `if:` expression, outside parentheses.
pub(crate) fn conjuncts(cond: &str) -> Vec<String> {
    let e = cond.trim();
    let e = e
        .strip_prefix("${{")
        .and_then(|c| c.strip_suffix("}}"))
        .unwrap_or(e);
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut chars = e.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '&' if depth == 0 && chars.peek() == Some(&'&') => {
                chars.next();
                parts.push(current.trim().to_owned());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current.trim().to_owned());
    parts
}

/// Whether a job's `if:` keeps it off pull requests.
pub(crate) fn pins_another_event(job: &[String]) -> bool {
    let Some(cond) = block(job, 4, "if") else {
        return false;
    };
    conjuncts(&cond.join(" ")).iter().any(|c| {
        let c = c.replace('"', "'");
        // The whole part is one comparison of the event name with one quoted word.
        let event = c
            .strip_prefix("github.event_name == '")
            .and_then(|rest| rest.strip_suffix('\''))
            .filter(|word| !word.contains(['\'', ' ']));
        c == "github.event_name != 'pull_request'"
            || event.is_some_and(|e| e != "pull_request" && e != "pull_request_target")
    })
}

/// A value without a trailing comment and without quotes.
pub(crate) fn plain(value: &str) -> String {
    let value = value.split(" #").next().unwrap_or("");
    value.replace(['"', '\''], "").trim().to_owned()
}

/// What a job runs on, as far as its text says.
#[derive(Debug, PartialEq)]
pub(crate) enum Runner {
    Mac,
    Other,
    /// An expression of inputs, variables or the event, which the text does not settle.
    Unknown,
}

pub(crate) fn runner(job: &[String]) -> Runner {
    let Some(runs_on) = block(job, 4, "runs-on") else {
        return Runner::Other;
    };
    let value = runs_on.join(" ").to_lowercase();
    if value.contains("macos") {
        return Runner::Mac;
    }
    if !value.contains("${{") {
        return Runner::Other;
    }
    // An expression: a matrix whose values are in the job's text, or something else.
    if job
        .iter()
        .any(|l| plain(l).to_lowercase().contains("macos"))
    {
        Runner::Mac
    } else if ["inputs.", "vars.", "github.", "fromjson", "needs."]
        .iter()
        .any(|w| value.contains(w))
    {
        Runner::Unknown
    } else {
        Runner::Other
    }
}

/// Whether `word` stands in `line` as a name of its own (not inside a longer name).
pub(crate) fn has_word(line: &str, word: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    line.match_indices(word).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + word.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// What a job's lines say about secrets and the job token.
pub(crate) fn secret_problems(id: &str, whose: &str, lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if lines.iter().any(|l| has_word(l, "secrets")) {
        out.push(format!("{id}: {whose} names a secret"));
    }
    if lines.iter().any(|l| {
        let l = l.replace([' ', '"'], "").replace('\'', "").to_lowercase();
        [
            "github.token",
            "github[token]",
            "tojson(github)",
            "github_token",
        ]
        .iter()
        .any(|w| l.contains(w))
    }) {
        out.push(format!("{id}: {whose} names the job token"));
    }
    out
}

/// What a permissions block grants that a pull request macOS job may not hold.
pub(crate) fn permission_problems(id: &str, perms: Option<Vec<String>>) -> Vec<String> {
    let Some(perms) = perms else {
        return vec![format!(
            "{id}: no permissions block, so the job gets the repository's default token"
        )];
    };
    let mut out = Vec::new();
    for p in perms.iter().map(|p| plain(p)) {
        if p.contains('{') && p != "{}" {
            out.push(format!(
                "{id}: permissions written as {p}, a mapping the lint does not read"
            ));
        } else if p == "id-token: write" || p == "id-token:write" {
            out.push(format!("{id}: holds id-token: write (OIDC)"));
        } else if p == "write-all" || p.ends_with(": write") {
            out.push(format!("{id}: holds {p}"));
        }
    }
    out
}

/// Whether a workflow's concurrency cancels the older run of a pull request.
pub(crate) fn cancels_older_pull_request_runs(w: &Workflow) -> bool {
    let Some(c) = w.top_block("concurrency") else {
        return false;
    };
    // The group is the pull request's, with nothing in it that differs from run to run (but
    // the fallback after `||`, which a pull request never takes).
    let group = c.iter().any(|l| {
        let pr = "github.event.pull_request.number";
        let mut rest = l.as_str();
        let mut kept = String::new();
        while let Some(at) = rest.find(pr) {
            kept.push_str(&rest[..at + pr.len()]);
            rest = &rest[at + pr.len()..];
            if let Some(after) = rest.trim_start().strip_prefix("||") {
                let after = after.trim_start();
                let end = after.find([' ', '}']).unwrap_or(after.len());
                rest = &after[end..];
            }
        }
        kept.push_str(rest);
        l.starts_with("group:")
            && l.contains(pr)
            && !["run_id", "run_number", "run_attempt", "github.sha"]
                .iter()
                .any(|w| kept.contains(w))
    });
    let cancel = c.iter().any(|l| {
        l == "cancel-in-progress: true"
            || l == "cancel-in-progress: ${{ github.event_name == 'pull_request' }}"
    });
    group && cancel
}

/// The pull request macOS jobs of `workflows` (file name and text) and what is wrong with them.
pub(crate) fn lint(workflows: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let parsed: Vec<Workflow> = workflows
        .iter()
        .map(|(file, text)| Workflow::parse(file, text))
        .collect();
    let by_file = |file: &str| parsed.iter().find(|w| w.file == file);
    let mut jobs = Vec::new();
    let mut problems = Vec::new();
    for w in parsed.iter().filter(|w| w.on_pull_request()) {
        let cancels = w.file == RELEASE_DRY_RUN || cancels_older_pull_request_runs(w);
        for (name, lines) in &w.jobs {
            if pins_another_event(lines) {
                continue;
            }
            let id = format!("{}/{name}", w.file);
            // A job that calls a workflow: that workflow's jobs run for this pull request.
            if let Some(uses) = block(lines, 4, "uses") {
                let target = uses.first().cloned().unwrap_or_default();
                let Some(called) = target
                    .strip_prefix("./.github/workflows/")
                    .and_then(by_file)
                else {
                    problems.push(format!(
                        "{id}: calls {target}, a workflow the lint cannot read"
                    ));
                    continue;
                };
                for (inner, inner_lines) in &called.jobs {
                    if pins_another_event(inner_lines) {
                        continue;
                    }
                    let inner_id = format!("{id} -> {}/{inner}", called.file);
                    if block(inner_lines, 4, "uses").is_some() {
                        problems.push(format!(
                            "{inner_id}: calls a workflow again, which the lint does not follow"
                        ));
                        continue;
                    }
                    match runner(inner_lines) {
                        Runner::Other => continue,
                        Runner::Unknown => {
                            problems
                                .push(format!("{inner_id}: runs on a runner the lint cannot read"));
                            continue;
                        }
                        Runner::Mac => {}
                    }
                    problems.extend(secret_problems(&inner_id, "the calling job", lines));
                    problems.extend(secret_problems(&inner_id, "the job", inner_lines));
                    problems.extend(secret_problems(
                        &inner_id,
                        "the called workflow",
                        &called.top,
                    ));
                    problems.extend(secret_problems(&inner_id, "the workflow", &w.top));
                    if block(inner_lines, 4, "environment").is_some() {
                        problems.push(format!("{inner_id}: runs in a deployment environment"));
                    }
                    let perms = block(inner_lines, 4, "permissions")
                        .or_else(|| called.top_block("permissions"))
                        .or_else(|| block(lines, 4, "permissions"));
                    problems.extend(permission_problems(&inner_id, perms));
                    if !cancels {
                        problems.push(format!(
                            "{inner_id}: a newer push to the pull request does not cancel it"
                        ));
                    }
                    jobs.push(inner_id);
                }
                continue;
            }
            match runner(lines) {
                Runner::Other => continue,
                Runner::Unknown => {
                    problems.push(format!("{id}: runs on a runner the lint cannot read"));
                    continue;
                }
                Runner::Mac => {}
            }
            problems.extend(secret_problems(&id, "the job", lines));
            problems.extend(secret_problems(&id, "the workflow", &w.top));
            if block(lines, 4, "environment").is_some() {
                problems.push(format!("{id}: runs in a deployment environment"));
            }
            let perms = block(lines, 4, "permissions").or_else(|| w.top_block("permissions"));
            problems.extend(permission_problems(&id, perms));
            if !cancels {
                problems.push(format!(
                    "{id}: a newer push to the pull request does not cancel it"
                ));
            }
            jobs.push(id);
        }
    }
    jobs.sort();
    (jobs, problems)
}
