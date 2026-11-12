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

/// Whether a runner label is a GitHub-hosted Linux x86_64 image: `ubuntu-latest` or
/// `ubuntu-<YY>.<MM>`. Everything else (`macos-*`, `windows-*`, `*-arm`, a larger runner,
/// `self-hosted`, a custom label) is not.
pub(crate) fn is_linux_x86_64(label: &str) -> bool {
    let Some(version) = label.strip_prefix("ubuntu-") else {
        return false;
    };
    version == "latest"
        || version.split_once('.').is_some_and(|(year, month)| {
            [year, month]
                .iter()
                .all(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// What a job runs on, as far as its text says.
#[derive(Debug, PartialEq)]
pub(crate) enum Runner {
    /// Every runner label the job names is Linux x86_64.
    LinuxX86,
    /// A label that is not: the labels named.
    Other(Vec<String>),
    /// An expression of inputs, variables, the event or another job's outputs, or a matrix
    /// key with no value in the job, which the text does not settle.
    Unknown,
}

/// The values of the matrix key `key` in a job's lines: `key: value`, `key: [a, b]` and
/// `- key: value`.
fn matrix_values(job: &[String], key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in job {
        let line = line.trim().trim_start_matches("- ");
        let Some(value) = line.strip_prefix(&format!("{key}:")) else {
            continue;
        };
        let value = plain(value);
        let value = value.trim_start_matches('[').trim_end_matches(']');
        out.extend(
            value
                .split(',')
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
        );
    }
    out
}

pub(crate) fn runner(job: &[String]) -> Runner {
    let Some(runs_on) = block(job, 4, "runs-on") else {
        // A job with no runner of its own calls a workflow, which the caller handles.
        return Runner::LinuxX86;
    };
    let mut labels = Vec::new();
    for value in runs_on {
        let value = plain(&value);
        // `labels: x` and `group: x` under a mapping; a list item; a flow list; a plain label.
        let value = value
            .strip_prefix("labels:")
            .or_else(|| value.strip_prefix("- "))
            .unwrap_or(&value)
            .trim()
            .to_owned();
        let value = value
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        for label in value.split(',').map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(expr) = label.strip_prefix("${{").and_then(|l| l.strip_suffix("}}")) {
                let expr = expr.trim();
                let key = expr.strip_prefix("matrix.").filter(|k| {
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                });
                let Some(values) = key.map(|k| matrix_values(job, k)).filter(|v| !v.is_empty())
                else {
                    return Runner::Unknown;
                };
                labels.extend(values);
            } else {
                labels.push(label.to_owned());
            }
        }
    }
    let others: Vec<String> = labels.into_iter().filter(|l| !is_linux_x86_64(l)).collect();
    if others.is_empty() {
        Runner::LinuxX86
    } else {
        Runner::Other(others)
    }
}

/// What is wrong with one job that a pull request starts, by its id.
fn runner_problems(id: &str, lines: &[String]) -> Vec<String> {
    match runner(lines) {
        Runner::LinuxX86 => Vec::new(),
        Runner::Other(labels) => vec![format!(
            "{id}: runs on {}, not Linux x86_64 (ubuntu-*)",
            labels.join(", ")
        )],
        Runner::Unknown => vec![format!("{id}: runs on a runner the lint cannot read")],
    }
}

/// The jobs a pull request starts in `workflows` (file name and text), as `<file>/<job>` and,
/// for a called workflow's job, `<file>/<calling job> -> <called file>/<job>`, and what is
/// wrong with them: a runner that is not Linux x86_64, a call the lint cannot read.
pub(crate) fn lint(workflows: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let parsed: Vec<Workflow> = workflows
        .iter()
        .map(|(file, text)| Workflow::parse(file, text))
        .collect();
    let by_file = |file: &str| parsed.iter().find(|w| w.file == file);
    let mut jobs = Vec::new();
    let mut problems = Vec::new();
    for w in parsed.iter().filter(|w| w.on_pull_request()) {
        for (name, lines) in &w.jobs {
            if pins_another_event(lines) {
                continue;
            }
            let id = format!("{}/{name}", w.file);
            // A job that calls a workflow: that workflow's jobs run for this pull request.
            let Some(uses) = block(lines, 4, "uses") else {
                problems.extend(runner_problems(&id, lines));
                jobs.push(id);
                continue;
            };
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
                problems.extend(runner_problems(&inner_id, inner_lines));
                jobs.push(inner_id);
            }
        }
    }
    jobs.sort();
    (jobs, problems)
}
