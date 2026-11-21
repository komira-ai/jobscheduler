//! The checks of `ci.yml` and of the merge queue section of `docs/ci.md`, each returning what is
//! wrong as one line per defect.

use std::collections::{BTreeMap, BTreeSet};

use crate::yaml::{self, Node, Value};

/// The job the ruleset of `main` requires.
pub const AGGREGATE: &str = "all-passed";
/// The concurrency key: a pull request's number, else the run's own id.
pub const GROUP: &str =
    "${{ github.workflow }}-${{ github.event.pull_request.number || github.run_id }}";
/// Only a pull request's run is cancelled by a newer one.
pub const CANCEL: &str = "${{ github.event_name == 'pull_request' }}";
/// The aggregate runs whatever the jobs it needs did.
pub const ALWAYS: &str = "${{ always() }}";
/// The one job condition that keeps no job off a merge group.
pub const NOT_CANCELLED: &str = "${{ !cancelled() }}";
/// The condition of the aggregate's step: every result but success.
pub const FAIL_IF: &str = "${{ contains(needs.*.result, 'failure') || contains(needs.*.result, 'cancelled') || contains(needs.*.result, 'skipped') }}";
/// The group base the site step passes.
pub const GROUP_BASE: &str = "${{ github.event.merge_group.base_sha }}";
/// The base the coverage comparison reads: the group's base commit, or the base branch.
pub const COMPARE_BASE: &str = "${{ github.event_name == 'merge_group' && github.event.merge_group.base_sha || format('origin/{0}', github.base_ref) }}";
/// The step of `coverage` that compares with the base.
pub const COMPARE_STEP: &str = "Compare with the base branch";

/// The value of `key` in the mapping `node`.
pub fn get<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    entries(node)
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The entries of a mapping; none for any other node.
fn entries(node: &Node) -> &[(Node, Node)] {
    match &node.value {
        Value::Map(entries) => entries,
        _ => &[],
    }
}

/// The items of a sequence; none for any other node.
fn items(node: &Node) -> &[Node] {
    match &node.value {
        Value::Seq(items) => items,
        _ => &[],
    }
}

/// The text of a scalar, or the texts of a sequence's scalars; nothing for anything else.
pub fn strs(node: Option<&Node>) -> Vec<String> {
    let Some(node) = node else {
        return Vec::new();
    };
    match &node.value {
        Value::Seq(items) => items
            .iter()
            .filter_map(|n| n.as_str().map(str::to_owned))
            .collect(),
        _ => node.as_str().map(str::to_owned).into_iter().collect(),
    }
}

/// The text of the scalar at `key` of `node`, or `missing`.
fn text(node: &Node, key: &str) -> String {
    get(node, key)
        .and_then(Node::as_str)
        .unwrap_or("missing")
        .to_owned()
}

/// Every scalar under `node`, keys included.
fn scalars(node: &Node, out: &mut Vec<String>) {
    match &node.value {
        Value::Scalar { .. } => out.extend(node.as_str().map(str::to_owned)),
        Value::Seq(items) => items.iter().for_each(|n| scalars(n, out)),
        Value::Map(entries) => entries.iter().for_each(|(k, v)| {
            scalars(k, out);
            scalars(v, out);
        }),
    }
}

/// All the text under `node`, one scalar per line.
fn all_text(node: &Node) -> String {
    let mut out = Vec::new();
    scalars(node, &mut out);
    out.join("\n")
}

/// What is wrong with `ci` (the text of `ci.yml`) and `doc` (the text of `docs/ci.md`).
pub fn problems(ci: &str, doc: &str) -> Vec<String> {
    let root = match yaml::load(ci) {
        Ok(root) => root,
        Err(e) => return vec![format!("ci.yml: {e}")],
    };
    let mut p = Vec::new();
    trigger(&root, &mut p);
    concurrency(&root, &mut p);
    let jobs: Vec<(String, &Node)> = get(&root, "jobs")
        .map(entries)
        .unwrap_or_default()
        .iter()
        .filter_map(|(k, v)| k.as_str().map(|k| (k.to_owned(), v)))
        .collect();
    aggregate(&jobs, &mut p);
    conditions(&jobs, &mut p);
    reads(&jobs, &mut p);
    bases(&jobs, &mut p);
    timeouts(&jobs, doc, &mut p);
    p
}

/// `on`: a merge group of `main`'s queue starts the workflow, beside a pull request and a
/// push to main.
fn trigger(root: &Node, p: &mut Vec<String>) {
    let Some(on) = get(root, "on") else {
        p.push("on: missing".to_owned());
        return;
    };
    let mut want = |event: &str, key: &str, value: &[&str], why: &str| {
        let Some(e) = get(on, event) else {
            p.push(format!("on: no {event}{why}"));
            return;
        };
        let got = strs(get(e, key));
        if got != value {
            p.push(format!("on.{event}.{key} is {got:?}, not {value:?}{why}"));
        }
    };
    let queue = ", so a merge group of main's queue starts no run and all-passed never reports";
    want("merge_group", "types", &["checks_requested"], queue);
    want("merge_group", "branches", &["main"], queue);
    want(
        "pull_request",
        "types",
        &["opened", "synchronize", "reopened"],
        "",
    );
    want("push", "branches", &["main"], "");
}

/// `concurrency`: a merge group gets a group of its own and is never cancelled in progress.
fn concurrency(root: &Node, p: &mut Vec<String>) {
    let Some(c) = get(root, "concurrency") else {
        p.push("concurrency: missing".to_owned());
        return;
    };
    let group = text(c, "group");
    if group != GROUP {
        p.push(format!(
            "concurrency.group is `{group}`, not `{GROUP}`: a merge group has no pull request number and needs a group of its own"
        ));
    }
    let cancel = text(c, "cancel-in-progress");
    if cancel != CANCEL {
        p.push(format!(
            "concurrency.cancel-in-progress is `{cancel}`, not `{CANCEL}`: only a pull request's run may be cancelled by a newer one"
        ));
    }
}

/// The aggregate: needs every other job, runs always, and fails on any result but success.
fn aggregate(jobs: &[(String, &Node)], p: &mut Vec<String>) {
    let Some((_, job)) = jobs.iter().find(|(id, _)| id == AGGREGATE) else {
        p.push(format!(
            "jobs: no job {AGGREGATE}, the check main's ruleset requires"
        ));
        return;
    };
    let needs: BTreeSet<String> = strs(get(job, "needs")).into_iter().collect();
    for (id, _) in jobs.iter().filter(|(id, _)| id != AGGREGATE) {
        if !needs.contains(id) {
            p.push(format!(
                "{AGGREGATE}: does not need {id}, so the queue would merge whatever {id} did"
            ));
        }
    }
    let cond = text(job, "if");
    if cond != ALWAYS {
        p.push(format!(
            "{AGGREGATE}: its if is `{cond}`, not `{ALWAYS}`: skipped after a failed job, and a skipped required check counts as passed"
        ));
    }
    let steps = get(job, "steps").map(items).unwrap_or_default();
    let fails = match steps {
        [step] => {
            entries(step)
                .iter()
                .all(|(k, _)| matches!(k.as_str(), Some("name" | "if" | "run")))
                && text(step, "if") == FAIL_IF
                && text(step, "run") == "exit 1"
        }
        _ => false,
    };
    if !fails {
        p.push(format!(
            "{AGGREGATE}: its steps are not the one step `if: {FAIL_IF}` `run: exit 1`, so it may pass when a job it needs did not"
        ));
    }
}

/// A short name of the step at `index` of a job: its name, id, action or first line of run.
fn step_label(step: &Node, index: usize) -> String {
    ["name", "id", "uses", "run"]
        .iter()
        .find_map(|k| get(step, k).and_then(Node::as_str))
        .map_or_else(
            || format!("step {}", index + 1),
            |s| s.lines().next().unwrap_or_default().to_owned(),
        )
}

/// The conditions of jobs and steps: none keeps a job, or a step that decides, off a merge
/// group.
fn conditions(jobs: &[(String, &Node)], p: &mut Vec<String>) {
    for (id, job) in jobs.iter().filter(|(id, _)| id != AGGREGATE) {
        if let Some(cond) = get(job, "if") {
            let cond = cond.as_str().unwrap_or("missing");
            if cond != NOT_CANCELLED {
                p.push(format!(
                    "{id}: its if is `{cond}`; only `{NOT_CANCELLED}` keeps no job off a merge group"
                ));
            }
        }
        if get(job, "continue-on-error").is_some() {
            p.push(format!(
                "{id}: continue-on-error lets it fail without failing {AGGREGATE}"
            ));
        }
        for (i, step) in get(job, "steps")
            .map(items)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            let Some(cond) = get(step, "if").and_then(Node::as_str) else {
                continue;
            };
            let label = step_label(step, i);
            if cond.contains("github.event") && !cond.contains("github.event_name == 'merge_group'")
            {
                p.push(format!(
                    "{id}, step {label}: its if `{cond}` reads the event and does not run on a merge group"
                ));
            }
            let saves_cache = text(step, "uses").starts_with("actions/cache/save@");
            if cond.contains("github.ref") && !saves_cache {
                p.push(format!(
                    "{id}, step {label}: its if `{cond}` reads the ref, which a merge group's is not main's; only a cache save may"
                ));
            }
        }
    }
}

/// The reads of the pull request's event: none outside `concurrency.group`, and a base branch
/// is read only beside the merge group's base.
fn reads(jobs: &[(String, &Node)], p: &mut Vec<String>) {
    for (id, job) in jobs {
        let all = all_text(job);
        for (what, why) in [
            (
                "github.event.pull_request",
                "which a merge group does not have",
            ),
            ("github.head_ref", "which is empty on a merge group"),
            (
                "merge_group.head_sha",
                "where its base_sha is meant (HEAD is the group's head)",
            ),
        ] {
            if all.contains(what) {
                p.push(format!("{id}: reads {what}, {why}"));
            }
        }
        // A base branch read per step: the job's own keys, then each step.
        let own: Vec<String> = entries(job)
            .iter()
            .filter(|(k, _)| k.as_str() != Some("steps"))
            .map(|(_, v)| all_text(v))
            .collect();
        let mut units = vec![(id.clone(), own.join("\n"))];
        for (i, step) in get(job, "steps")
            .map(items)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            units.push((
                format!("{id}, step {}", step_label(step, i)),
                all_text(step),
            ));
        }
        for (unit, text) in units {
            if text.contains("github.base_ref")
                && !text.contains("github.event.merge_group.base_sha")
            {
                p.push(format!(
                    "{unit}: reads github.base_ref, empty on a merge group, and not github.event.merge_group.base_sha"
                ));
            }
        }
    }
}

/// The step of `job` whose `key` is `value`.
fn step<'a>(job: &'a Node, key: &str, value: &str) -> Option<&'a Node> {
    get(job, "steps")
        .map(items)
        .unwrap_or_default()
        .iter()
        .find(|s| get(s, key).and_then(Node::as_str) == Some(value))
}

/// The two places that read a base: the site decision and the coverage comparison.
fn bases(jobs: &[(String, &Node)], p: &mut Vec<String>) {
    let job = |name: &str| jobs.iter().find(|(id, _)| id == name).map(|(_, j)| *j);
    match job("lint").and_then(|j| step(j, "id", "site")) {
        None => p.push("lint: no step `site`".to_owned()),
        Some(site) => {
            let env = get(site, "env");
            let passed = env.map(|e| text(e, "GROUP_BASE")).unwrap_or_default();
            if passed != GROUP_BASE || !text(site, "run").contains("--group-base \"$GROUP_BASE\"") {
                p.push(format!(
                    "lint, step site: does not pass `{GROUP_BASE}` as --group-base, so a merge group's change is not read"
                ));
            }
        }
    }
    match job("coverage").and_then(|j| step(j, "name", COMPARE_STEP)) {
        None => p.push(format!("coverage: no step `{COMPARE_STEP}`")),
        Some(compare) => {
            let base = get(compare, "env")
                .map(|e| text(e, "BASE"))
                .unwrap_or_default();
            let run = text(compare, "run");
            let reads_base = ["\"$BASE^{commit}\"", "git show \"$BASE:coverage-baseline\""]
                .iter()
                .all(|r| run.contains(r));
            if base != COMPARE_BASE || !reads_base {
                p.push(format!(
                    "coverage, step {COMPARE_STEP}: does not read the baseline of `{COMPARE_BASE}`"
                ));
            }
        }
    }
}

/// The ruleset in the section "The merge queue" of `docs/ci.md`: its first JSON block.
pub fn ruleset(doc: &str) -> Option<serde_json::Value> {
    let section = doc.split("\n## The merge queue\n").nth(1)?;
    let section = section.split("\n## ").next()?;
    let block = section.split("```json\n").nth(1)?.split("\n```").next()?;
    serde_json::from_str(block).ok()
}

/// The parameters of the ruleset's rule `kind`.
fn rule<'a>(ruleset: &'a serde_json::Value, kind: &str) -> Option<&'a serde_json::Value> {
    ruleset
        .get("rules")?
        .as_array()?
        .iter()
        .find(|r| r.get("type").and_then(serde_json::Value::as_str) == Some(kind))?
        .get("parameters")
}

/// The longest chain of `timeout-minutes` through `needs` ending at `id`, and its jobs.
fn chain(
    id: &str,
    jobs: &BTreeMap<String, (u64, Vec<String>)>,
    seen: &mut BTreeMap<String, (u64, Vec<String>)>,
) -> (u64, Vec<String>) {
    if let Some(known) = seen.get(id) {
        return known.clone();
    }
    let (minutes, needs) = jobs.get(id).cloned().unwrap_or_default();
    // A cycle is GitHub's to refuse; it is not walked twice here.
    seen.insert(id.to_owned(), (0, Vec::new()));
    let (before, mut path) = needs
        .iter()
        .map(|n| chain(n, jobs, seen))
        .max_by_key(|(m, _)| *m)
        .unwrap_or_default();
    path.push(id.to_owned());
    let out = (before + minutes, path);
    seen.insert(id.to_owned(), out.clone());
    out
}

/// Every job has a timeout, and the longest chain of them ends before the queue's check
/// timeout; the ruleset requires the aggregate alone.
fn timeouts(jobs: &[(String, &Node)], doc: &str, p: &mut Vec<String>) {
    let mut table = BTreeMap::new();
    for (id, job) in jobs {
        let minutes = get(job, "timeout-minutes")
            .and_then(Node::as_str)
            .and_then(|m| m.parse::<u64>().ok());
        if minutes.is_none() {
            p.push(format!(
                "{id}: no timeout-minutes, so it may run as long as the queue waits"
            ));
        }
        table.insert(id.clone(), (minutes.unwrap_or(0), strs(get(job, "needs"))));
    }
    let mut seen = BTreeMap::new();
    let (longest, path) = table
        .keys()
        .map(|id| chain(id, &table, &mut seen))
        .max_by_key(|(m, _)| *m)
        .unwrap_or_default();
    let Some(ruleset) = ruleset(doc) else {
        p.push("docs/ci.md: no JSON ruleset in \"The merge queue\"".to_owned());
        return;
    };
    match rule(&ruleset, "merge_queue")
        .and_then(|q| q.get("check_response_timeout_minutes"))
        .and_then(serde_json::Value::as_u64)
    {
        None => p.push("docs/ci.md: the ruleset has no check_response_timeout_minutes".to_owned()),
        Some(queue) if longest >= queue => p.push(format!(
            "the longest chain of jobs ({}) may run {longest} minutes, not under the queue's check timeout of {queue} (docs/ci.md)",
            path.join(", ")
        )),
        Some(_) => {}
    }
    let required: Vec<&str> = rule(&ruleset, "required_status_checks")
        .and_then(|r| r.get("required_status_checks"))
        .and_then(serde_json::Value::as_array)
        .map(|checks| {
            checks
                .iter()
                .filter_map(|c| c.get("context").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if required != [AGGREGATE] {
        p.push(format!(
            "docs/ci.md: the ruleset requires {required:?}, not [\"{AGGREGATE}\"]"
        ));
    }
}
