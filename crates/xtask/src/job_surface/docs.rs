//! The generated blocks of [`super::DOC_FILE`], the gateway's page: its route table and the
//! paragraph that says which calls the server serves. Each block stands between two
//! marker lines, `<!-- gen-job-surface <name> begin -->` and `<!-- gen-job-surface <name>
//! end -->`, and the generator replaces what is between them and nothing else, so the prose
//! around them stays hand-written.
//!
//! - `routes`: one row per route of `js_api_gateway::ROUTES` (its build reads them from the
//!   `google.api.http` options of the job service), in the order of the service definition:
//!   the verb and template, the method, the status of a success.
//! - `served`: the calls of `js_lease_server::SERVED_CALLS`, which a test of the server holds
//!   to what the service it builds serves, and the calls that answer `UNIMPLEMENTED`, both in
//!   the order of the route table.

use js_api_gateway::{Route, Rpc};

/// The name of the route table's block.
pub const ROUTES_BLOCK: &str = "routes";

/// The name of the served paragraph's block.
pub const SERVED_BLOCK: &str = "served";

/// The page `text` with both blocks rendered from the gateway's routes and the server's
/// served calls.
///
/// # Errors
///
/// What is wrong with the markers of a block, as [`splice`] says it.
pub fn page(text: &str) -> Result<String, String> {
    page_with(text, js_api_gateway::ROUTES, &js_lease_server::SERVED_CALLS)
}

/// The page `text` with both blocks rendered from `routes` and `served`.
///
/// # Errors
///
/// What is wrong with the markers of a block, as [`splice`] says it.
pub fn page_with(text: &str, routes: &[Route], served: &[Rpc]) -> Result<String, String> {
    let text = splice(text, ROUTES_BLOCK, &route_table(routes))?;
    splice(&text, SERVED_BLOCK, &served_paragraph(routes, served))
}

/// The route table: a header and one row per route.
#[must_use]
pub fn route_table(routes: &[Route]) -> String {
    let mut table = String::from("| route | method of `JobService` | success |\n|---|---|---|\n");
    for route in routes {
        table.push_str(&format!(
            "| `{} {}` | `{}` | {} |\n",
            route.verb.as_str(),
            route.template,
            route.rpc.name(),
            route.status
        ));
    }
    table
}

/// The paragraph that says which calls of `routes` the server serves (those in `served`) and
/// which answer `UNIMPLEMENTED`.
#[must_use]
pub fn served_paragraph(routes: &[Route], served: &[Rpc]) -> String {
    let (yes, no): (Vec<&str>, Vec<&str>) = routes
        .iter()
        .map(|route| route.rpc.name())
        .partition(|name| served.iter().any(|rpc| rpc.name() == *name));
    let source = "(`js_lease_server::SERVED_CALLS`)";
    let unimplemented = "answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`)";
    let text = match (yes.is_empty(), no.is_empty()) {
        (true, _) => format!("The server serves no call yet {source}: every call {unimplemented}."),
        (false, true) => format!("The server serves every call of the service {source}."),
        (false, false) => format!(
            "The server serves {} {source}. Every other call {unimplemented} until the server \
             serves it: {}.",
            listed(&yes),
            listed(&no)
        ),
    };
    text + "\n"
}

/// `names` in backticks, joined by commas and a last `and`.
fn listed(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => quoted.concat(),
    }
}

/// `text` with the lines between the block `name`'s begin and end markers replaced by
/// `block`.
///
/// # Errors
///
/// A marker line that is missing or there more than once, or an end before its begin.
pub fn splice(text: &str, name: &str, block: &str) -> Result<String, String> {
    let begin = format!("<!-- gen-job-surface {name} begin -->");
    let end = format!("<!-- gen-job-surface {name} end -->");
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let first = marker(&lines, &begin)?;
    let last = marker(&lines, &end)?;
    if last < first {
        return Err(format!("the line {end} comes before {begin}"));
    }
    let mut spliced: String = lines[..=first].concat();
    spliced.push_str(block);
    spliced.push_str(&lines[last..].concat());
    Ok(spliced)
}

/// The index of the one line of `lines` that is `marker`.
fn marker(lines: &[&str], marker: &str) -> Result<usize, String> {
    let at: Vec<usize> = (0..lines.len())
        .filter(|index| lines[*index].trim_end() == marker)
        .collect();
    match at.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!("has no line {marker}")),
        _ => Err(format!("has the line {marker} more than once")),
    }
}
