//! The technical report's check (`xtask::report`): the report on this tree holds to it, and
//! each rule of the check, and of the generator of its section 8 table, shown on a small
//! repository made here, failing on the defect it is for.

use std::path::Path;
use std::process::Command as Process;

use js_testkit::dir::TestDir;
use xtask::report::{
    Args, BEGIN, DESIGN, END, REPORT, ReportError, SIMULATION, Stale, check, generated, rows, run,
    splice,
};

/// The repository root.
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// The simulation page of the small repository: two scenario rows.
const SIM: &str = "# The cell\n\n## Which section 8 rows have a scenario\n\nRows covered.\n\n\
| row | scenario | family file | what the scenario shows |\n|---|---|---|---|\n\
| 1 | `a_b` | `tests/jobs.rs` | It's `x` & <y> |\n\
| 2 | `c_d` | `tests/cell/more.rs` | Shows |\n\nAfter the table.\n\n## A well-known foo_bar\n";

/// The design of the small repository: three rows in section 8.
const DESIGN_TEXT: &str = "# The design\n\n## 8. Races and failures the current code loses\n\n\
The test list.\n\n| # | Failure | State | What happens | Test to write |\n|---|---|---|---|---|\n\
| 1 | Submit lost | Open | w | t |\n| 2 | Other's | Documented | w | t |\n\
| 3 | Third | Open | w | t |\n\n## 9. Assumptions\n";

/// The block [`rows`] renders from [`SIM`] and [`DESIGN_TEXT`].
const BLOCK: &str = "<div class=\"tblw wide\"><table class=\"tbl\">\n\
<thead><tr><th>Row</th><th>Failure</th><th>State in the design</th><th>Scenario</th><th>What the scenario shows</th></tr></thead>\n\
<tbody>\n\
<tr><td>1</td><td>Submit lost</td><td>Open</td><td><code data-test=\"crates/js-sim-cell/tests/jobs.rs\">a_b</code></td><td>It&rsquo;s <code>x</code> &amp; &lt;y&gt;</td></tr>\n\
<tr><td>2</td><td>Other&rsquo;s</td><td>Documented</td><td><code data-test=\"crates/js-sim-cell/tests/cell/more.rs\">c_d</code></td><td>Shows</td></tr>\n\
</tbody></table></div>\n\
<p>Rows of section 8 with no scenario (1 of 3): 3.</p>\n";

/// A page of the small repository that holds to it: one element of each kind the check
/// reads and passes, around the generated block (empty until [`repo`] renders it).
const GOOD: &str = "<html><body id=\"top\">\n<!-- gen-report rows begin -->\n<!-- gen-report rows end -->\n\
<p><code data-test=\"crates/js-sim-cell/tests/jobs.rs\">a_test_with_five_words_name</code>\n\
<code>cargo test -p js-demo2 --test flat</code> <code>cargo test -p js-demo2 --test in_dir</code>\n\
<code>cargo test -p js-demo2</code> <code>crates/js-demo2/src/lib.rs</code>\n\
<code class=\"a\" title>crates/js-demo2 is here</code> <code>js-demo2</code> <code>xtask</code>\n\
<code>js-Demo</code> <code>plain</code> <code>short_name</code> <code>a_b_c_d_E</code>\n\
<code>p99_of_the_queue_wait</code> <codex>not code</codex>\n\
<a href=\"https://github.com/komira-ai/jobscheduler/blob/main/crates/js-demo2/src/lib.rs#L1\">a</a>\n\
<a href=\"https://github.com/komira-ai/jobscheduler/tree/main/crates/js-demo2\">b</a>\n\
<a href=\"https://example.org/\">c</a> <a>d</a> <abbr>e</abbr> <a href=\"#top\">f</a>\n\
<a href=\"https://github.com/komira-ai/jobscheduler/blob/main/docs/simulation.md#a-well-known-foo_bar\">g</a>\n\
<a href=\"https://github.com/komira-ai/jobscheduler/blob/main/docs/design/jobscheduler-design.md#8-races-and-failures-the-current-code-loses\">h</a>\n\
<a href=\"https://github.com/komira-ai/jobscheduler/blob/main/docs/simulation.md\">i</a>\n\
<span class=\"x\">plain</span>\n\
<span data-cell=\"c-limit\" data-source=\"crates/js-demo2/src/lib.rs\" data-item=\"LIMIT\">4,096 ids</span>\n\
<span data-cell=\"c-wait\" data-source=\"crates/js-demo2/src/lib.rs\" data-item=\"WAIT\">30.</span>\n\
<span data-cell=\"m-open\" data-open=\"an experiment\">NOT YET; <code>jobs</code>, <code>reported_steady_ms</code></span>\n\
<span data-cell=\"m-none\" data-open=\"another\">NOT YET; no metric yet</span>\n\
</p></body></html>\n";

/// Writes `text` to `path` under `dir`, making its directory.
fn put(dir: &TestDir, path: &str, text: &str) {
    let path = dir.path().join(path);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(path, text).expect("the file");
}

/// A small repository whose report is [`GOOD`] with its block rendered, and every file the
/// elements of [`GOOD`] name.
fn repo(name: &str) -> TestDir {
    let dir = TestDir::under(Path::new(env!("CARGO_TARGET_TMPDIR")).join("report"), name);
    put(&dir, SIMULATION, SIM);
    put(&dir, DESIGN, DESIGN_TEXT);
    put(
        &dir,
        "crates/js-sim-cell/tests/jobs.rs",
        "Scenario::new(\n    \"a_b\",\n)\n#[test]\nfn a_test_with_five_words_name() {}\n",
    );
    put(
        &dir,
        "crates/js-sim-cell/tests/cell/more.rs",
        "Scenario::new(\"c_d\")",
    );
    put(
        &dir,
        "crates/js-measure/src/metric.rs",
        "pub mod name {\n    pub const JOBS: &str = \"jobs\";\n    pub const P99: &str = \"p99_of_the_queue_wait\";\n    \
         pub const COUNT: usize = 3;\n    pub const OPEN: &str = \"unterminated\n}\n",
    );
    put(
        &dir,
        "crates/js-measure/src/lib.rs",
        "//! records `reported_steady_ms`, and reporting_things, and reported_ alone.\n",
    );
    put(&dir, "crates/js-demo2/Cargo.toml", "");
    put(
        &dir,
        "crates/js-demo2/src/lib.rs",
        "pub const LIMIT: usize = 4_096;\npub static WAIT: u64 = 30;\n",
    );
    put(&dir, "crates/js-demo2/tests/flat.rs", "");
    put(&dir, "crates/js-demo2/tests/in_dir/main.rs", "");
    put(&dir, "crates/xtask/Cargo.toml", "");
    put(&dir, REPORT, GOOD);
    let page = generated(dir.path()).expect("the small report renders");
    put(&dir, REPORT, &page);
    dir
}

/// The small repository with `extra` added at the end of its report.
fn with(name: &str, extra: &str) -> TestDir {
    let dir = repo(name);
    let page = std::fs::read_to_string(dir.path().join(REPORT)).expect("the report");
    put(&dir, REPORT, &format!("{page}{extra}"));
    dir
}

/// What the check finds in the small repository with `extra` added to its report.
fn found(name: &str, extra: &str) -> Vec<Stale> {
    check(with(name, extra).path()).expect("the check runs")
}

/// Runs the xtask program with `args`: its exit status, stdout and stderr.
fn xtask(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Process::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("xtask runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Asserts that the report on this tree holds to it: its generated block is current, and
/// every crate, path, test, constant and metric it quotes exists; and that the verb's
/// `--check` says so and exits 0.
#[test]
fn the_report_holds_to_the_tree() {
    assert_eq!(
        check(Path::new(ROOT)).expect("the check runs"),
        Vec::<Stale>::new()
    );
    let (code, out, err) = xtask(&["gen-report", "--check", "--root", ROOT]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            Some(0),
            "docs/report/jobscheduler.html holds to the tree\n",
            ""
        )
    );
}

/// Asserts that the block is rendered from both tables exactly: one row per scenario with
/// the design's failure and state, markdown escaped and code spans as `code`, then the rows
/// no scenario covers; and that the small repository's page holds to it.
#[test]
fn the_block_is_rendered_from_the_simulation_page_and_the_design() {
    assert_eq!(rows(SIM, DESIGN_TEXT).expect("it renders"), BLOCK);
    let dir = repo("renders");
    let page = std::fs::read_to_string(dir.path().join(REPORT)).expect("the report");
    assert!(page.contains(&format!("{BEGIN}\n{BLOCK}{END}")));
    assert_eq!(
        check(dir.path()).expect("the check runs"),
        Vec::<Stale>::new()
    );
}

/// Asserts that each defect of a source table is refused naming the file and the defect: no
/// heading, no table under it, a short row, a row with no number, and a scenario row that
/// section 8 does not have.
#[test]
fn a_source_table_that_is_wrong_is_refused() {
    let refused = |simulation: &str, design: &str| match rows(simulation, design) {
        Err(error) => error.to_string(),
        Ok(block) => panic!("rendered {block}"),
    };
    assert_eq!(
        refused("# nothing\n", DESIGN_TEXT),
        "docs/simulation.md: no heading \"## Which section 8 rows have a scenario\""
    );
    assert_eq!(
        refused(SIM, "# nothing\n"),
        "docs/design/jobscheduler-design.md: no heading \"## 8. Races and failures the current code loses\""
    );
    assert_eq!(
        refused(
            "x\n## Which section 8 rows have a scenario\n\nNo table.\n",
            DESIGN_TEXT
        ),
        "docs/simulation.md: no table rows under \"## Which section 8 rows have a scenario\""
    );
    let short = SIM.replace(
        "| 2 | `c_d` | `tests/cell/more.rs` | Shows |",
        "| 2 | `c_d` |",
    );
    assert_eq!(
        refused(&short, DESIGN_TEXT),
        "docs/simulation.md: a row under \"## Which section 8 rows have a scenario\" has fewer than 4 cells or no row number: 2 | `c_d`"
    );
    let unnumbered = DESIGN_TEXT.replace("| 3 | Third |", "| three | Third |");
    assert_eq!(
        refused(SIM, &unnumbered),
        "docs/design/jobscheduler-design.md: a row under \"## 8. Races and failures the current code loses\" has fewer than 3 cells or no row number: three | Third | Open | w | t"
    );
    let unknown = SIM.replace("| 2 | `c_d`", "| 9 | `c_d`");
    assert_eq!(
        refused(&unknown, DESIGN_TEXT),
        "docs/simulation.md: row 9 is not a row of section 8 of docs/design/jobscheduler-design.md"
    );
}

/// Asserts that the block goes between the markers and nowhere else, and that a page whose
/// markers are missing, doubled or out of order is refused.
#[test]
fn the_block_goes_between_the_markers_once() {
    let page = format!("a\n{BEGIN}\nold\n{END}\nb\n");
    assert_eq!(
        splice(&page, "new\n").expect("it splices"),
        format!("a\n{BEGIN}\nnew\n{END}\nb\n")
    );
    assert_eq!(
        splice("a\n", "new\n"),
        Err(format!("the marker {BEGIN} is there 0 times, not once"))
    );
    assert_eq!(
        splice(&format!("{BEGIN}\n{END}\n{END}\n"), "new\n"),
        Err(format!("the marker {END} is there 2 times, not once"))
    );
    assert_eq!(
        splice(&format!("{END}\n{BEGIN}\n"), "new\n"),
        Err(format!("{END} comes before {BEGIN}"))
    );
}

/// Asserts that a block that is not what the generator renders is found, and that the verb
/// writes the current block back into the report.
#[test]
fn a_stale_block_is_found_and_the_verb_writes_it_again() {
    let dir = repo("stale-block");
    let path = dir.path().join(REPORT);
    let page = std::fs::read_to_string(&path).expect("the report");
    put(&dir, REPORT, &page.replace("Shows", "Showed"));
    assert_eq!(
        check(dir.path()).expect("the check runs"),
        vec![Stale::Rows]
    );
    let root = dir.path().to_str().expect("a UTF-8 path");
    let (code, out, err) = xtask(&["gen-report", "--root", root]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            Some(0),
            "wrote the generated block of docs/report/jobscheduler.html\n",
            ""
        )
    );
    assert_eq!(std::fs::read_to_string(&path).expect("the report"), page);
}

/// Asserts that a quoted path that does not exist is found, in a `code` element and in a
/// link into the repository's main (without its fragment); an element with no end tag runs to
/// the end of the page, and a tag cut off there ends the reading.
#[test]
fn a_missing_path_is_found() {
    assert_eq!(
        found("path", "<code>docs/missing.md</code> <code>open <code"),
        vec![Stale::Path("docs/missing.md".to_owned())]
    );
    assert_eq!(
        found(
            "link",
            "<a href=\"https://github.com/komira-ai/jobscheduler/blob/main/docs/gone.md#part\">x</a>"
        ),
        vec![Stale::Path("docs/gone.md".to_owned())]
    );
}

/// Asserts that a `js-` name that is not a crate of the workspace is found.
#[test]
fn a_crate_that_is_not_there_is_found() {
    assert_eq!(
        found("crate", "<code>js-nothing</code>"),
        vec![Stale::Crate("js-nothing".to_owned())]
    );
}

/// Asserts that a test its file does not have, a test in a file that does not exist, and a
/// test name with no file are each found.
#[test]
fn a_test_that_is_not_there_or_names_no_file_is_found() {
    let jobs = "crates/js-sim-cell/tests/jobs.rs";
    assert_eq!(
        found(
            "test",
            &format!("<code data-test=\"{jobs}\">no_such_test</code>")
        ),
        vec![Stale::Test {
            file: jobs.to_owned(),
            name: "no_such_test".to_owned()
        }]
    );
    assert_eq!(
        found(
            "test-file",
            "<code data-test=\"crates/js-sim-cell/tests/gone.rs\">a_b</code>"
        ),
        vec![Stale::Test {
            file: "crates/js-sim-cell/tests/gone.rs".to_owned(),
            name: "a_b".to_owned()
        }]
    );
    assert_eq!(
        found("unfiled", "<code>a_test_name_with_no_file</code>"),
        vec![Stale::Unfiled("a_test_name_with_no_file".to_owned())]
    );
}

/// Asserts that a quoted `cargo test` whose target or crate does not exist is found.
#[test]
fn a_test_command_with_no_target_is_found() {
    assert_eq!(
        found("target", "<code>cargo test -p js-demo2 --test nope</code>"),
        vec![Stale::Target(
            "cargo test -p js-demo2 --test nope".to_owned()
        )]
    );
    assert_eq!(
        found("target-crate", "<code>cargo test -p js-gone</code>"),
        vec![Stale::Target("cargo test -p js-gone".to_owned())]
    );
}

/// Asserts that a cell of the code is held to its constant: an item the file does not have,
/// a number the item does not hold, a number that is only in the item's type (`u64`) or
/// only part of its value (409 of 4096), and a cell with no number are each found.
#[test]
fn a_cell_of_the_code_is_held_to_its_constant() {
    let source = "data-source=\"crates/js-demo2/src/lib.rs\"";
    let cell = |id: &str, item: &str, text: &str| {
        format!("<span data-cell=\"{id}\" {source} data-item=\"{item}\">{text}</span>")
    };
    assert_eq!(
        found("item", &cell("c-gone", "GONE", "1")),
        vec![Stale::Item {
            cell: "c-gone".to_owned(),
            why: "crates/js-demo2/src/lib.rs has no const or static GONE".to_owned()
        }]
    );
    assert_eq!(
        found("value", &cell("c-value", "LIMIT", "4,095 ids")),
        vec![Stale::Item {
            cell: "c-value".to_owned(),
            why: "LIMIT in crates/js-demo2/src/lib.rs does not hold the number \"4095\"".to_owned()
        }]
    );
    assert_eq!(
        found("type", &cell("c-type", "WAIT", "64 s")),
        vec![Stale::Item {
            cell: "c-type".to_owned(),
            why: "WAIT in crates/js-demo2/src/lib.rs does not hold the number \"64\"".to_owned()
        }]
    );
    assert_eq!(
        found("part", &cell("c-part", "LIMIT", "409 ids")),
        vec![Stale::Item {
            cell: "c-part".to_owned(),
            why: "LIMIT in crates/js-demo2/src/lib.rs does not hold the number \"409\"".to_owned()
        }]
    );
    assert_eq!(
        found("no-number", &cell("c-none", "LIMIT", "many ids")),
        vec![Stale::Item {
            cell: "c-none".to_owned(),
            why: "LIMIT in crates/js-demo2/src/lib.rs does not hold the number \"\"".to_owned()
        }]
    );
}

/// Asserts that an open cell that prints a digit, names no metric and does not say so, or
/// names a metric js-measure does not have (beside one it has) is found.
#[test]
fn an_open_cell_prints_no_number_and_names_known_metrics() {
    let open =
        |id: &str, text: &str| format!("<span data-cell=\"{id}\" data-open=\"x\">{text}</span>");
    assert_eq!(
        found("number", &open("m-n", "NOT YET, 3 runs; <code>jobs</code>")),
        vec![Stale::Number("m-n".to_owned())]
    );
    assert_eq!(
        found("no-metrics", &open("m-a", "NOT YET")),
        vec![Stale::Cell(
            "the open cell m-a names no metric and does not say no metric yet".to_owned()
        )]
    );
    assert_eq!(
        found(
            "metric",
            &open("m-u", "NOT YET; <code>jobs</code>, <code>nope</code>")
        ),
        vec![Stale::Metric {
            cell: "m-u".to_owned(),
            metric: "nope".to_owned()
        }]
    );
}

/// Asserts that a link whose fragment names no heading of the markdown file it links to,
/// and a link within the page to an id the page does not have, are each found.
#[test]
fn a_link_to_a_missing_heading_or_id_is_found() {
    assert_eq!(
        found(
            "heading",
            "<a href=\"https://github.com/komira-ai/jobscheduler/blob/main/docs/simulation.md#gone\">x</a>"
        ),
        vec![Stale::Anchor("docs/simulation.md#gone".to_owned())]
    );
    assert_eq!(
        found("id", "<a href=\"#nowhere\">x</a>"),
        vec![Stale::Anchor("#nowhere".to_owned())]
    );
}

/// Asserts that a cell of neither kind (no source, an empty experiment, a source with no
/// item) and a cell id used twice are each found.
#[test]
fn a_cell_of_neither_kind_or_a_taken_id_is_found() {
    let neither = |id: &str| {
        Stale::Cell(format!(
            "the cell {id} has neither data-source and data-item nor data-open"
        ))
    };
    assert_eq!(
        found("bare", "<span data-cell=\"c-bare\">1</span>"),
        vec![neither("c-bare")]
    );
    assert_eq!(
        found(
            "empty-open",
            "<span data-cell=\"m-x\" data-open=\"\">x</span>"
        ),
        vec![neither("m-x")]
    );
    assert_eq!(
        found(
            "no-item",
            "<span data-cell=\"c-x\" data-source=\"crates/js-demo2/src/lib.rs\">1</span>"
        ),
        vec![neither("c-x")]
    );
    assert_eq!(
        found(
            "twice",
            "<span data-cell=\"m-open\" data-open=\"y\">NOT YET; no metric yet</span>"
        ),
        vec![Stale::Cell("the cell id m-open is used twice".to_owned())]
    );
}

/// Asserts the text of each kind of finding, and that the verb's `--check` prints them
/// joined, exits 1, and writes nothing on stdout.
#[test]
fn each_finding_reads_as_one_line() {
    let lines: Vec<String> = [
        Stale::Rows,
        Stale::Path("a/b".to_owned()),
        Stale::Crate("js-x".to_owned()),
        Stale::Test {
            file: "f.rs".to_owned(),
            name: "t".to_owned(),
        },
        Stale::Unfiled("a_b_c_d_e".to_owned()),
        Stale::Target("cargo test".to_owned()),
        Stale::Item {
            cell: "c".to_owned(),
            why: "why".to_owned(),
        },
        Stale::Number("m".to_owned()),
        Stale::Metric {
            cell: "m".to_owned(),
            metric: "x".to_owned(),
        },
        Stale::Cell("a cell".to_owned()),
        Stale::Anchor("a.md#b".to_owned()),
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        lines,
        [
            "the generated block is not what `cargo xtask gen-report` renders",
            "a/b does not exist",
            "js-x is not a crate of the workspace",
            "f.rs has no test t",
            "a_b_c_d_e reads as a test name and has no data-test",
            "`cargo test` names no test target",
            "cell c: why",
            "open cell m prints a number",
            "cell m: js-measure has no metric x",
            "a cell",
            "a.md#b names no heading or id",
        ]
    );
    let dir = with("verb", "<code>docs/a.md</code><code>js-nothing</code>");
    let root = dir.path().to_str().expect("a UTF-8 path");
    let (code, out, err) = xtask(&["gen-report", "--check", "--root", root]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            Some(1),
            "",
            "xtask: docs/report/jobscheduler.html does not hold to the tree: docs/a.md does not exist; js-nothing is not a crate of the workspace\n"
        )
    );
}

/// Asserts that a report, or a source of it, that cannot be read is an error naming the
/// file, and that a report whose markers are wrong is refused naming the report, by the
/// check and by the verb.
#[test]
fn a_missing_file_or_wrong_markers_is_an_error() {
    let read_error = |dir: &TestDir| match check(dir.path()) {
        Err(ReportError::File { path, .. }) => path
            .strip_prefix(dir.path())
            .expect("under the root")
            .to_string_lossy()
            .into_owned(),
        other => panic!("not a read error: {other:?}"),
    };
    for file in [
        REPORT,
        SIMULATION,
        DESIGN,
        "crates/js-measure/src/metric.rs",
        "crates/js-measure/src/lib.rs",
    ] {
        let dir = repo(&format!("missing-{}", file.replace('/', "-")));
        std::fs::remove_file(dir.path().join(file)).expect("removed");
        assert_eq!(read_error(&dir), file);
    }
    let dir = repo("markers");
    put(&dir, REPORT, "no markers\n");
    let refused =
        format!("docs/report/jobscheduler.html: the marker {BEGIN} is there 0 times, not once");
    match check(dir.path()) {
        Err(error @ ReportError::Source { .. }) => assert_eq!(error.to_string(), refused),
        other => panic!("not refused: {other:?}"),
    }
    let args = Args {
        root: dir.path().to_path_buf(),
        check: false,
    };
    match run(&args) {
        Err(error) => assert_eq!(error.to_string(), refused),
        Ok(line) => panic!("written: {line}"),
    }
    let empty = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("report"),
        "empty",
    );
    let root = empty.path().to_str().expect("a UTF-8 path");
    let (code, out, err) = xtask(&["gen-report", "--check", "--root", root]);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(
        err.starts_with(&format!("xtask: {root}/docs/report/jobscheduler.html: ")),
        "{err}"
    );
}
