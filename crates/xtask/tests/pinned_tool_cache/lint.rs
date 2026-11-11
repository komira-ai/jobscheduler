//! The steps of a workflow's jobs, read as text, and the rule for the steps around each
//! `xtask fetch` (see `main.rs`).

use std::collections::{BTreeMap, BTreeSet};

/// One step of a job: the line of its `-` (1-based), its keys and the keys under its `with:`.
#[derive(Debug, Default)]
pub(crate) struct Step {
    pub(crate) line: usize,
    pub(crate) keys: BTreeMap<String, String>,
    pub(crate) with: BTreeMap<String, String>,
}

impl Step {
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(String::as_str)
    }

    /// The action this step uses, without a trailing comment.
    pub(crate) fn uses(&self) -> Option<&str> {
        self.get("uses")
            .map(|u| u.split(" #").next().unwrap_or(u).trim())
    }
}

/// A job: its name and its steps.
#[derive(Debug)]
pub(crate) struct Job {
    pub(crate) name: String,
    pub(crate) steps: Vec<Step>,
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn skipped(line: &str) -> bool {
    line.trim().is_empty() || line.trim_start().starts_with('#')
}

/// `key: value` split at the first `: ` (or a trailing `:`).
fn key_value(text: &str) -> Option<(String, String)> {
    if let Some(key) = text.strip_suffix(':') {
        return Some((key.to_owned(), String::new()));
    }
    let (key, value) = text.split_once(": ")?;
    Some((key.trim().to_owned(), value.trim().to_owned()))
}

/// The jobs of `text`, each with its steps.
pub(crate) fn jobs(text: &str) -> Vec<Job> {
    let lines: Vec<&str> = text.lines().collect();
    let mut jobs = Vec::new();
    let mut in_jobs = false;
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        if skipped(line) {
            at += 1;
            continue;
        }
        if indent(line) == 0 {
            in_jobs = line == "jobs:";
        } else if in_jobs && indent(line) == 2 {
            jobs.push(Job {
                name: line.trim().trim_end_matches(':').to_owned(),
                steps: Vec::new(),
            });
        } else if in_jobs && line.trim() == "steps:" {
            let (steps, next) = steps(&lines, at);
            if let Some(job) = jobs.last_mut() {
                job.steps = steps;
            }
            at = next;
            continue;
        }
        at += 1;
    }
    jobs
}

/// The steps of the `steps:` key at line `at`, and the index of the first line after them.
fn steps(lines: &[&str], at: usize) -> (Vec<Step>, usize) {
    let base = indent(lines[at]);
    let mut steps: Vec<Step> = Vec::new();
    let mut dash = None;
    // The key of the current step whose value continues on the lines under it.
    let mut open: Option<String> = None;
    let mut i = at + 1;
    while i < lines.len() {
        let line = lines[i];
        if skipped(line) {
            i += 1;
            continue;
        }
        let n = indent(line);
        let body = line.trim_start();
        if n < base || (n == base && !body.starts_with("- ")) {
            break;
        }
        let d = *dash.get_or_insert(n);
        let key_at = d + 2;
        let text = if n == d && body.starts_with("- ") {
            steps.push(Step {
                line: i + 1,
                ..Step::default()
            });
            open = None;
            &body[2..]
        } else if n == key_at {
            body
        } else {
            // A line under a key: a `with:` entry or a line of a block value.
            if let (Some(key), Some(step)) = (&open, steps.last_mut()) {
                if key == "with" {
                    if let Some((k, v)) = key_value(body) {
                        step.with.insert(k, v);
                    }
                } else if let Some(value) = step.keys.get_mut(key) {
                    if !value.is_empty() {
                        value.push('\n');
                    }
                    value.push_str(body);
                }
            }
            i += 1;
            continue;
        };
        if let (Some((key, value)), Some(step)) = (key_value(text), steps.last_mut()) {
            let value = if value == "|" { String::new() } else { value };
            open = Some(key.clone());
            step.keys.insert(key, value);
        }
        i += 1;
    }
    (steps, i)
}

/// The words of a one-line shell command, with double quotes removed.
pub(crate) fn words(run: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in run.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' if !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            _ => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

/// The value of `--flag` in `words`.
fn flag<'a>(words: &'a [String], flag: &str) -> Option<&'a str> {
    let at = words.iter().position(|w| w == flag)?;
    words.get(at + 1).map(String::as_str)
}

/// The variable names `$NAME` and `${NAME}` in `text`.
fn variables(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        rest = &rest[at + 1..];
        let name: String = rest
            .trim_start_matches('{')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        // `${{ ... }}` is an expression, not a variable: its name starts with a space.
        if !name.is_empty() {
            found.insert(name);
        }
    }
    found
}

/// The insides of the `${{ ... }}` expressions in `text`, trimmed.
fn expressions(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("${{") {
        rest = &rest[at + 3..];
        let end = rest.find("}}").unwrap_or(rest.len());
        found.push(rest[..end].trim().to_owned());
        rest = &rest[end.min(rest.len())..];
    }
    found
}

/// `NAME` when `text` is `$NAME` or `${NAME}`.
fn one_variable(text: &str) -> Option<String> {
    let name = text.strip_prefix('$')?;
    let name = name
        .strip_prefix('{')
        .and_then(|n| n.strip_suffix('}'))
        .unwrap_or(name);
    let word = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    word.then(|| name.to_owned())
}

/// An action pinned by commit: `<name>@<40 hex digits>`.
pub(crate) fn pinned(uses: Option<&str>, name: &str) -> bool {
    uses.and_then(|u| u.strip_prefix(name))
        .and_then(|u| u.strip_prefix('@'))
        .is_some_and(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A `--cache` path the rule accepts: `$RUNNER_TEMP/<rest>`, with no other variable.
fn cache_rest(cache: &str) -> Option<&str> {
    let rest = cache.strip_prefix("$RUNNER_TEMP/")?;
    (!rest.is_empty() && !rest.contains('$')).then_some(rest)
}

/// One `xtask fetch` step that passed: its job and its `--cache` path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Site {
    pub(crate) job: String,
    pub(crate) cache: String,
}

/// The fetch sites of the workflow `file` with text `text`, or the reasons it breaks the
/// rule, each as `<file>:<line>: <job>: <why>`.
pub(crate) fn lint(file: &str, text: &str) -> Result<Vec<Site>, Vec<String>> {
    let mut sites = Vec::new();
    let mut errors = Vec::new();
    for job in jobs(text) {
        let mut paths = BTreeSet::new();
        for (i, step) in job.steps.iter().enumerate() {
            let Some(run) = step.get("run") else {
                continue;
            };
            let mut fail =
                |why: String| errors.push(format!("{file}:{}: {}: {why}", step.line, job.name));
            if !run.contains("xtask fetch") {
                if run.contains("releases/download/") {
                    fail("a release download outside `cargo xtask fetch`".to_owned());
                }
                continue;
            }
            match fetch_site(&job.steps, i) {
                Ok(cache) => {
                    if !paths.insert(cache.clone()) {
                        fail(format!("a second fetch with the cache path {cache}"));
                    }
                    sites.push(Site {
                        job: job.name.clone(),
                        cache,
                    });
                }
                Err(why) => fail(why),
            }
        }
    }
    if errors.is_empty() {
        Ok(sites)
    } else {
        Err(errors)
    }
}

/// The `--cache` path of the fetch at `steps[i]`, once it and the steps around it keep the
/// rule.
fn fetch_site(steps: &[Step], i: usize) -> Result<String, String> {
    let fetch = &steps[i];
    let run = fetch.get("run").unwrap_or_default();
    if fetch.get("if").is_some() {
        return Err("the fetch has an `if:`: it must run on a hit and a miss alike".to_owned());
    }
    for chain in ["\n", "&&", "||", ";", "|", "`", "$(", ">", "${{"] {
        if run.contains(chain) {
            return Err(format!("the fetch is not one command ({chain:?} in it)"));
        }
    }
    let words = words(run);
    let url = flag(&words, "--url").ok_or("the fetch has no --url")?;
    let sha = flag(&words, "--sha256").ok_or("the fetch has no --sha256")?;
    let sha_var = one_variable(sha)
        .ok_or_else(|| format!("the fetch's --sha256 {sha:?} is not one variable"))?;
    let versions = variables(url);
    if versions.is_empty() {
        return Err(format!(
            "the fetch's --url {url:?} names no version variable"
        ));
    }
    let cache = flag(&words, "--cache").ok_or("the fetch has no --cache")?;
    let rest = cache_rest(cache)
        .ok_or_else(|| format!("the fetch's --cache {cache:?} is not \"$RUNNER_TEMP/<file>\""))?;
    let path = format!("${{{{ runner.temp }}}}/{rest}");

    let restore = i
        .checked_sub(1)
        .map(|r| &steps[r])
        .ok_or("no actions/cache/restore step right before the fetch")?;
    if !pinned(restore.uses(), "actions/cache/restore") {
        return Err(format!(
            "the step before the fetch (line {}) is not actions/cache/restore pinned by commit",
            restore.line
        ));
    }
    if restore.get("if").is_some() {
        return Err("the restore has an `if:`".to_owned());
    }
    let id = restore.get("id").ok_or("the restore has no `id:`")?;
    expect_with(restore, "restore", &path)?;
    let key = &restore.with["key"];
    let want: BTreeSet<String> = versions
        .iter()
        .chain([&sha_var])
        .map(|v| format!("env.{v}"))
        .collect();
    let got: BTreeSet<String> = expressions(key).into_iter().collect();
    if got != want || key.matches('$').count() != expressions(key).len() {
        return Err(format!(
            "the restore's key {key:?} is not of the URL's version and the pinned SHA-256 \
             alone: its expressions must be exactly {want:?}"
        ));
    }

    let save = steps
        .get(i + 1)
        .ok_or("no actions/cache/save step right after the fetch")?;
    if !pinned(save.uses(), "actions/cache/save") {
        return Err(format!(
            "the step after the fetch (line {}) is not actions/cache/save pinned by commit",
            save.line
        ));
    }
    let hit = format!("steps.{id}.outputs.cache-hit != 'true'");
    let cond = save.get("if").unwrap_or_default();
    let cond = cond
        .strip_prefix("${{")
        .and_then(|c| c.strip_suffix("}}"))
        .unwrap_or(cond)
        .trim();
    if cond != hit {
        return Err(format!("the save's `if:` is {cond:?}, not {hit:?}"));
    }
    expect_with(save, "save", &path)?;
    let primary = format!("${{{{ steps.{id}.outputs.cache-primary-key }}}}");
    if save.with["key"] != primary {
        return Err(format!(
            "the save's key is {:?}, not {primary:?}",
            save.with["key"]
        ));
    }
    Ok(cache.to_owned())
}

/// `step`'s `with:` holds exactly `path` (equal to `path`) and `key`.
fn expect_with(step: &Step, what: &str, path: &str) -> Result<(), String> {
    let keys: Vec<&str> = step.with.keys().map(String::as_str).collect();
    if keys != ["key", "path"] {
        return Err(format!(
            "the {what}'s `with:` has {keys:?}, not exactly path and key"
        ));
    }
    if step.with["path"] != path {
        return Err(format!(
            "the {what}'s path is {:?}, not the fetch's --cache {path:?}",
            step.with["path"]
        ));
    }
    Ok(())
}
