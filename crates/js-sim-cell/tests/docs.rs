//! Holds `docs/simulation.md` to the crate, so the page and the code cannot drift apart.
//!
//! The check fails when
//! - a name of `Invariant::ALL` is not a row of the table under "The invariants" (or a row
//!   there names an invariant the crate does not have);
//! - a test binary of the crate, `tests/<family>.rs` (other than `docs.rs`, `replay.rs` and
//!   `invariants.rs`), is not named on the page as `` `tests/<family>.rs` ``;
//! - a row of the table under "Which section 8 rows have a scenario" names a scenario that
//!   is not a `Scenario::new("<name>")` in the file the row names;
//! - an example on the page, the lines between `// Copied from crates/js-sim-cell/<file>`
//!   and `// End of the copy.`, is not a run of lines of that file, each line compared
//!   after trimming; or the page has no example.
//!
//! [`check`] is one function over the page's text and a [`Crate`], so the negative cases
//! run it over a copy of the page, or of the crate's file list, changed in memory; none
//! touches the real page.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use js_sim_cell::Invariant;

/// The page, from the repository root.
const PAGE: &str = "docs/simulation.md";

/// Test binaries of the crate that are not a scenario family.
const NOT_FAMILIES: [&str; 3] = ["docs", "replay", "invariants"];

/// The heading of the invariants table.
const INVARIANTS: &str = "## The invariants";

/// The heading of the table of section 8 rows.
const ROWS: &str = "## Which section 8 rows have a scenario";

/// The line that opens an example, followed by the copied file's path in the crate.
const COPY_FROM: &str = "// Copied from crates/js-sim-cell/";

/// The line that closes an example.
const COPY_END: &str = "// End of the copy.";

/// What the page is held to.
struct Crate {
    /// The name of every invariant of `Invariant::ALL`, as a failure writes it.
    invariants: Vec<String>,
    /// The scenario families: the `<family>` of each `tests/<family>.rs`, but those of
    /// [`NOT_FAMILIES`].
    families: Vec<String>,
    /// The text of every `.rs` file under `tests/`, by its path from the crate's
    /// directory (`tests/smoke.rs`, `tests/cell/lossy_wire.rs`).
    files: BTreeMap<String, String>,
}

/// The crate's directory.
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The page as it is in the repository.
fn page() -> String {
    let path = crate_dir().join("../..").join(PAGE);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// The crate as it is: its invariants, its families and its test files.
fn this_crate() -> Crate {
    let mut files = BTreeMap::new();
    read_sources(&crate_dir(), Path::new("tests"), &mut files);
    let families = files.keys().filter_map(|path| family(path)).collect();
    let invariants = Invariant::ALL.iter().map(|i| i.name().to_owned()).collect();
    Crate {
        invariants,
        families,
        files,
    }
}

/// Reads every `.rs` file under `root/dir` into `files`, by its path from `root`.
fn read_sources(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) {
    let entries = fs::read_dir(root.join(dir)).expect("the test directory is readable");
    for entry in entries {
        let path = dir.join(entry.expect("a directory entry").file_name());
        let full = root.join(&path);
        if full.is_dir() {
            read_sources(root, &path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(&full).expect("a source file is readable");
            files.insert(path.to_string_lossy().into_owned(), text);
        }
    }
}

/// The family a test file is, if it is one: `smoke` for `tests/smoke.rs`; none for a
/// file in a subdirectory or one of [`NOT_FAMILIES`].
fn family(path: &str) -> Option<String> {
    let name = path.strip_prefix("tests/")?.strip_suffix(".rs")?;
    let binary = !name.contains('/') && !NOT_FAMILIES.contains(&name);
    binary.then(|| name.to_owned())
}

/// Every way the page and the crate differ, each in one line; empty when they agree.
fn check(page: &str, krate: &Crate) -> Vec<String> {
    let mut problems = unnamed_invariants(page, &krate.invariants);
    problems.extend(unnamed_families(page, &krate.families));
    problems.extend(unknown_scenarios(page, &krate.files));
    problems.extend(changed_examples(page, &krate.files));
    problems
}

/// The text under `heading`, up to the next heading of its level.
fn section<'a>(page: &'a str, heading: &str) -> Result<&'a str, String> {
    let missing = || format!("the page has no section \"{heading}\"");
    let start = page.find(&format!("\n{heading}\n")).ok_or_else(missing)?;
    let body = &page[start + heading.len() + 2..];
    Ok(body.find("\n## ").map_or(body, |end| &body[..end]))
}

/// The rows of the tables in `section`, after each table's heading and separator lines,
/// each cell trimmed of spaces and backticks.
fn table(section: &str) -> Vec<Vec<&str>> {
    let mut rows = Vec::new();
    let mut in_table = 0;
    for line in section.lines() {
        if !line.starts_with('|') {
            in_table = 0;
            continue;
        }
        in_table += 1;
        if in_table > 2 {
            let cells = line.trim().trim_matches('|').split('|');
            rows.push(cells.map(|cell| cell.trim().trim_matches('`')).collect());
        }
    }
    rows
}

/// Invariants missing from the table under [`INVARIANTS`], and rows of it naming an
/// invariant that is not one of `names`.
fn unnamed_invariants(page: &str, names: &[String]) -> Vec<String> {
    let rows = match section(page, INVARIANTS) {
        Ok(text) => table(text),
        Err(problem) => return vec![problem],
    };
    let listed: BTreeSet<&str> = rows.iter().filter_map(|row| row.first().copied()).collect();
    let missing = names.iter().filter(|name| !listed.contains(name.as_str()));
    let mut problems: Vec<String> = missing
        .map(|name| format!("invariant {name} is not in the table under \"{INVARIANTS}\""))
        .collect();
    let stale = listed
        .iter()
        .filter(|row| !names.iter().any(|name| name == *row));
    problems.extend(stale.map(|row| {
        format!("the table under \"{INVARIANTS}\" lists {row}, which is not in Invariant::ALL")
    }));
    problems
}

/// Families the page does not name as `` `tests/<family>.rs` ``.
fn unnamed_families(page: &str, families: &[String]) -> Vec<String> {
    let unnamed = families
        .iter()
        .filter(|family| !page.contains(&format!("`tests/{family}.rs`")));
    unnamed
        .map(|family| format!("family {family} (tests/{family}.rs) is not named on the page"))
        .collect()
}

/// Rows under [`ROWS`] whose scenario is not a `Scenario::new` in the file they name.
fn unknown_scenarios(page: &str, files: &BTreeMap<String, String>) -> Vec<String> {
    let rows = match section(page, ROWS) {
        Ok(text) => table(text),
        Err(problem) => return vec![problem],
    };
    let mut problems = Vec::new();
    for row in rows {
        let [number, scenario, file, ..] = row.as_slice() else {
            let row = row.join(" | ");
            problems.push(format!(
                "a row under \"{ROWS}\" has fewer than three cells: {row}"
            ));
            continue;
        };
        let made = format!("Scenario::new(\"{scenario}\")");
        match files.get(*file) {
            None => problems.push(format!(
                "row {number} names {file}, which is not a test file of the crate"
            )),
            Some(text) if !text.contains(&made) => problems.push(format!(
                "row {number} names scenario {scenario}, which is not a {made} in {file}"
            )),
            Some(_) => {}
        }
    }
    problems
}

/// Examples on the page that are not the text of the file they say they copy, and a page
/// without an example.
fn changed_examples(page: &str, files: &BTreeMap<String, String>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut examples = 0;
    let mut lines = page.lines();
    while let Some(line) = lines.next() {
        let Some(file) = line.trim().strip_prefix(COPY_FROM) else {
            continue;
        };
        examples += 1;
        let mut copy = Vec::new();
        let mut ended = false;
        for line in lines.by_ref() {
            if line.trim() == COPY_END {
                ended = true;
                break;
            }
            copy.push(line.trim());
        }
        let problem = match files.get(file) {
            _ if !ended => Some(format!("the copy of {file} has no line \"{COPY_END}\"")),
            None => Some(format!(
                "the page copies {file}, which is not a test file of the crate"
            )),
            Some(text) => differs(&copy, text).map(|how| format!("the copy of {file} {how}")),
        };
        problems.extend(problem);
    }
    if examples == 0 {
        problems.push(format!(
            "the page has no example: no line \"{COPY_FROM}<file>\""
        ));
    }
    problems
}

/// How `copy` differs from a run of lines of `text`, each line trimmed; `None` when it is
/// one.
fn differs(copy: &[&str], text: &str) -> Option<String> {
    let source: Vec<&str> = text.lines().map(str::trim).collect();
    if copy.is_empty() {
        return Some("is empty".to_owned());
    }
    if source.windows(copy.len()).any(|run| run == copy) {
        return None;
    }
    let stray = copy.iter().find(|line| !source.contains(*line));
    Some(match stray {
        Some(line) => format!("has the line {line:?}, which that file does not have"),
        None => "has the lines of that file in another order".to_owned(),
    })
}

/// `page` with its one line equal to `line` removed.
fn without_line(page: &str, line: &str) -> String {
    assert_eq!(page.lines().filter(|l| *l == line).count(), 1, "{line}");
    let kept: Vec<&str> = page.lines().filter(|l| *l != line).collect();
    kept.join("\n") + "\n"
}

/// `page` with its one occurrence of `from` replaced by `to`.
fn replaced(page: &str, from: &str, to: &str) -> String {
    assert_eq!(page.matches(from).count(), 1, "{from}");
    page.replacen(from, to, 1)
}

/// Asserts that every name of `Invariant::ALL` is a row of the page's invariants table
/// and that the table names no other.
#[test]
fn the_page_names_every_invariant() {
    let krate = this_crate();
    assert_eq!(krate.invariants.len(), Invariant::ALL.len());
    assert_eq!(
        unnamed_invariants(&page(), &krate.invariants),
        Vec::<String>::new()
    );
}

/// Asserts that every scenario family of the crate is named on the page, and that the
/// crate's files are read (the families include `smoke`, and the three test binaries that
/// are not families are left out).
#[test]
fn the_page_names_every_scenario_family() {
    let krate = this_crate();
    assert!(
        krate.families.iter().any(|f| f == "smoke"),
        "{:?}",
        krate.families
    );
    for not in NOT_FAMILIES {
        assert!(!krate.families.iter().any(|f| f == not), "{not}");
    }
    assert_eq!(
        unnamed_families(&page(), &krate.families),
        Vec::<String>::new()
    );
}

/// Asserts that each scenario the page's section 8 table lists is a `Scenario::new` of
/// that name in the file the row names, and that the table has rows to check.
#[test]
fn every_scenario_the_page_lists_exists_in_its_file() {
    let page = page();
    let rows = table(section(&page, ROWS).expect("the section exists"));
    assert!(!rows.is_empty(), "the section 8 table has rows");
    assert_eq!(
        unknown_scenarios(&page, &this_crate().files),
        Vec::<String>::new()
    );
}

/// Asserts that each example on the page is the text of the file it was copied from, and
/// that the whole check passes on the page as written.
#[test]
fn the_example_is_the_text_of_a_real_scenario() {
    let (page, krate) = (page(), this_crate());
    assert!(page.contains(&format!("{COPY_FROM}tests/cell/lossy_wire.rs\n")));
    assert_eq!(changed_examples(&page, &krate.files), Vec::<String>::new());
    assert_eq!(check(&page, &krate), Vec::<String>::new());
}

/// Asserts that the check, over the page without the row of `settles-when-quiet`, fails
/// naming that invariant and nothing else. Catches a check that does not read the table.
#[test]
fn the_check_fails_on_a_page_missing_an_invariant() {
    let page = page();
    let row = page
        .lines()
        .find(|line| line.starts_with("| `settles-when-quiet` |"))
        .expect("the row of settles-when-quiet");
    let without = without_line(&page, row);
    assert_eq!(
        check(&without, &this_crate()),
        ["invariant settles-when-quiet is not in the table under \"## The invariants\""]
    );
}

/// Asserts that the check, over a crate with one more family (`tests/new_family.rs`) than
/// the page names, fails naming that family and nothing else. Catches a family merged
/// without its line on the page.
#[test]
fn the_check_fails_on_a_page_missing_a_family() {
    let mut krate = this_crate();
    let file = "#[test]\nfn f() {}\n".to_owned();
    krate.files.insert("tests/new_family.rs".to_owned(), file);
    krate.families = krate.files.keys().filter_map(|p| family(p)).collect();
    assert!(krate.families.iter().any(|f| f == "new_family"));
    assert_eq!(
        check(&page(), &krate),
        ["family new_family (tests/new_family.rs) is not named on the page"]
    );
}

/// Asserts that the check, over the page with row 31 naming a scenario no file makes,
/// fails naming that scenario and its file, and nothing else. Catches a row left behind
/// when its scenario is renamed or removed.
#[test]
fn the_check_fails_on_a_listed_scenario_that_does_not_exist() {
    let changed = replaced(
        &page(),
        "| 31 | `an_agent_that_relaunches_a_repeated_start` |",
        "| 31 | `an_agent_that_restarts_and_runs_a_stale_start` |",
    );
    assert_eq!(
        check(&changed, &this_crate()),
        [
            "row 31 names scenario an_agent_that_restarts_and_runs_a_stale_start, which is \
             not a Scenario::new(\"an_agent_that_restarts_and_runs_a_stale_start\") in \
             tests/broken_agent.rs"
        ]
    );
}

/// Asserts that the check, over the page with one line of the lossy-wire example changed,
/// fails naming the copied file and the changed line, and nothing else. Catches an example
/// edited on the page, or a scenario edited in the crate, without the other.
#[test]
fn the_check_fails_when_the_example_differs() {
    let changed = replaced(
        &page(),
        "        .hosts(\"burst\", HOSTS)\n",
        "        .hosts(\"burst\", 10)\n",
    );
    assert_eq!(
        check(&changed, &this_crate()),
        [
            "the copy of tests/cell/lossy_wire.rs has the line \".hosts(\\\"burst\\\", 10)\", \
             which that file does not have"
        ]
    );
}

/// Asserts that a row of the invariants table naming an invariant the crate does not have
/// fails the check. Catches a page that keeps a removed or renamed invariant.
#[test]
fn the_check_fails_on_an_invariant_the_crate_does_not_have() {
    let page = page();
    let row = page
        .lines()
        .find(|line| line.starts_with("| `one-terminal-answer` |"))
        .expect("the row of one-terminal-answer");
    let more = replaced(
        &page,
        row,
        &format!("{row}\n| `no-lost-run` | a | b | at the end |"),
    );
    assert_eq!(
        check(&more, &this_crate()),
        [
            "the table under \"## The invariants\" lists no-lost-run, which is not in \
             Invariant::ALL"
        ]
    );
}

/// Asserts that a section 8 row naming a file the crate does not have, and a row of fewer
/// than three cells, each fail the check by name.
#[test]
fn the_check_fails_on_a_row_naming_a_missing_file_or_a_short_row() {
    let page = page();
    let moved = replaced(
        &page,
        "`tests/broken_agent.rs` | The server sends",
        "`tests/no_such_family.rs` | The server sends",
    );
    assert_eq!(
        check(&moved, &this_crate()),
        ["row 31 names tests/no_such_family.rs, which is not a test file of the crate"]
    );
    let short = format!("x\n{ROWS}\n\n| row | scenario |\n|---|---|\n| 12 | `x` |\n");
    assert_eq!(
        unknown_scenarios(&short, &this_crate().files),
        [
            "a row under \"## Which section 8 rows have a scenario\" has fewer than three cells: 12 | x"
        ]
    );
}

/// Asserts the failures of an example: lines in another order, a copy of a file the crate
/// does not have, a copy without its end line, an empty copy, and a page with no example.
#[test]
fn the_check_fails_on_each_kind_of_broken_example() {
    let files = this_crate().files;
    let page = page();
    let swapped = replaced(
        &page,
        "#[test]\nfn a_lossy_agent_wire_settles_every_run() {\n",
        "fn a_lossy_agent_wire_settles_every_run() {\n#[test]\n",
    );
    assert_eq!(
        changed_examples(&swapped, &files),
        ["the copy of tests/smoke.rs has the lines of that file in another order"]
    );
    let block = |file: &str, body: &str, end: &str| format!("{COPY_FROM}{file}\n{body}{end}");
    let unknown = block("tests/none.rs", "x\n", COPY_END);
    assert_eq!(
        changed_examples(&unknown, &files),
        ["the page copies tests/none.rs, which is not a test file of the crate"]
    );
    let open = block("tests/smoke.rs", "#[test]\n", "");
    assert_eq!(
        changed_examples(&open, &files),
        ["the copy of tests/smoke.rs has no line \"// End of the copy.\""]
    );
    let empty = block("tests/smoke.rs", "", COPY_END);
    assert_eq!(
        changed_examples(&empty, &files),
        ["the copy of tests/smoke.rs is empty"]
    );
    assert_eq!(
        changed_examples("no example\n", &files),
        ["the page has no example: no line \"// Copied from crates/js-sim-cell/<file>\""]
    );
}

/// Asserts that a page without the invariants section or without the section 8 section
/// fails by naming the section, and that a section ends at the next heading of its level.
#[test]
fn the_check_fails_on_a_page_missing_a_section() {
    let names = this_crate().invariants;
    assert_eq!(
        unnamed_invariants("no tables\n", &names),
        ["the page has no section \"## The invariants\""]
    );
    assert_eq!(
        unknown_scenarios("no tables\n", &this_crate().files),
        ["the page has no section \"## Which section 8 rows have a scenario\""]
    );
    let page = "a\n## One\n| h |\n|---|\n| x |\n### Sub\n| h |\n|---|\n| y |\n## Two\n| z |\n";
    let one = section(page, "## One").expect("the section exists");
    assert_eq!(table(one), [["x"], ["y"]]);
}

/// Asserts which test files are families: a top-level file but `docs`, `replay` and
/// `invariants`; never a shared file in a subdirectory or a file that is not Rust.
#[test]
fn only_top_level_test_binaries_but_three_are_families() {
    assert_eq!(family("tests/smoke.rs"), Some("smoke".to_owned()));
    assert_eq!(
        family("tests/broken_agent.rs"),
        Some("broken_agent".to_owned())
    );
    for not in [
        "tests/docs.rs",
        "tests/replay.rs",
        "tests/invariants.rs",
        "tests/cell/lossy_wire.rs",
        "tests/golden/trace-hashes.txt",
        "src/lib.rs",
    ] {
        assert_eq!(family(not), None, "{not}");
    }
}
