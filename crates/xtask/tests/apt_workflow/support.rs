//! What the lints share: the workflow files as text, reads of a parsed workflow
//! ([`super::yaml::Node`]), and the two helpers the defect tables use to change one thing in one job.

use std::path::PathBuf;

use super::yaml::{Node, Value};

/// The directory of the workflows of this repository.
fn workflows_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows")
        .canonicalize()
        .expect("the workflows directory")
}

/// `(file name, text)` of every workflow, sorted by name.
pub fn workflows() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(workflows_dir()).expect("list the workflows") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if name.ends_with(".yml") || name.ends_with(".yaml") {
            found.push((name, std::fs::read_to_string(&path).expect("read")));
        }
    }
    found.sort();
    found
}

/// The text of the workflow `file`.
pub fn text_of(file: &str) -> String {
    workflows()
        .into_iter()
        .find(|(name, _)| name == file)
        .unwrap_or_else(|| panic!("{file} is not in .github/workflows"))
        .1
}

/// The value of `key` in the mapping `map`, if it is one and has the key.
pub fn entry<'a>(map: &'a Node, key: &str) -> Option<&'a Node> {
    match &map.value {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

/// The entries of the mapping `map`, none for any other node.
pub fn entries(map: &Node) -> &[(Node, Node)] {
    match &map.value {
        Value::Map(entries) => entries,
        _ => &[],
    }
}

/// The names `node` lists, sorted: a scalar, the items of a sequence, or the keys of a mapping
/// (the three shapes GitHub reads for `on` and for `needs`).
pub fn names(node: Option<&Node>) -> Vec<String> {
    let mut found: Vec<String> = match node.map(|n| &n.value) {
        Some(Value::Scalar { .. }) => node
            .and_then(Node::as_str)
            .map(str::to_owned)
            .into_iter()
            .collect(),
        Some(Value::Seq(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        Some(Value::Map(entries)) => entries
            .iter()
            .filter_map(|(k, _)| k.as_str().map(str::to_owned))
            .collect(),
        None => Vec::new(),
    };
    found.sort();
    found
}

/// The steps of the job `job`.
pub fn steps(job: &Node) -> &[Node] {
    match entry(job, "steps").map(|s| &s.value) {
        Some(Value::Seq(items)) => items,
        _ => &[],
    }
}

/// The steps of `job`, none when there is no job.
pub fn steps_of(job: Option<&Node>) -> &[Node] {
    job.map_or(&[][..], steps)
}

/// Every scalar of `node`, keys and values.
fn scalars<'a>(node: &'a Node, out: &mut Vec<&'a str>) {
    match &node.value {
        Value::Scalar { .. } => out.extend(node.as_str()),
        Value::Seq(items) => items.iter().for_each(|i| scalars(i, out)),
        Value::Map(entries) => {
            for (k, v) in entries {
                scalars(k, out);
                scalars(v, out);
            }
        }
    }
}

/// Does any scalar of `node` (a key or a value, a comment is not one) contain `word`?
pub fn mentions(node: &Node, word: &str) -> bool {
    let mut all = Vec::new();
    scalars(node, &mut all);
    all.iter().any(|s| s.contains(word))
}

/// The text of `key` of `step`, empty when it has none.
pub fn text<'a>(step: &'a Node, key: &str) -> &'a str {
    entry(step, key).and_then(Node::as_str).unwrap_or_default()
}

/// The text at `path` (keys, one level each) under `node`.
pub fn at<'a>(node: &'a Node, path: &[&str]) -> Option<&'a str> {
    let mut here = node;
    for key in path {
        here = entry(here, key)?;
    }
    here.as_str()
}

/// The permissions of `job`, as `scope: level`, sorted.
pub fn permissions(job: Option<&Node>) -> Vec<String> {
    let mut held: Vec<String> = job
        .and_then(|j| entry(j, "permissions"))
        .map_or(&[][..], entries)
        .iter()
        .map(|(k, v)| {
            format!(
                "{}: {}",
                k.as_str().unwrap_or_default(),
                v.as_str().unwrap_or_default()
            )
        })
        .collect();
    held.sort();
    held
}

/// The job `name` of the `jobs` mapping `jobs`.
pub fn job<'a>(jobs: &'a [(Node, Node)], name: &str) -> Option<&'a Node> {
    jobs.iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

/// `text` with `replace` replaced by `with` inside the job `job` alone (from its key to the key
/// of the next job), which must hold it.
pub fn in_job(text: &str, job: &str, replace: &str, with: &str) -> String {
    let key = format!("\n  {job}:\n");
    let start = text.find(&key).unwrap_or_else(|| panic!("no job {job}")) + 1;
    let body = &text[start + key.len() - 1..];
    let end = body
        .match_indices("\n  ")
        .map(|(at, _)| at)
        .find(|&at| {
            let line = &body[at + 3..];
            line.starts_with(|c: char| c.is_ascii_alphanumeric())
                && line.split('\n').next().is_some_and(|l| l.ends_with(':'))
        })
        .map_or(text.len(), |at| start + key.len() - 1 + at);
    let slice = &text[start..end];
    assert!(
        slice.contains(replace),
        "{replace:?} is not in the job {job}"
    );
    format!(
        "{}{}{}",
        &text[..start],
        slice.replacen(replace, with, 1),
        &text[end..]
    )
}

/// For each `(job, replace, with, says)`: `file` with that one change in that job must give a
/// problem from `check` that contains `says`.
pub fn each_named(
    file: &str,
    check: fn(&str) -> Vec<String>,
    defects: &[(&str, &str, &str, &str)],
) {
    let text = text_of(file);
    for (job, replace, with, says) in defects {
        let problems = check(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{file} {job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}
