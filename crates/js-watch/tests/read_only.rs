//! Tripwires for the watcher's read-only design. The guarantee itself is structural: a
//! [`Watcher`] is built from, and holds, only an `Arc<dyn CloudView>`, an
//! `Arc<dyn StatusSource>` and an `Arc<dyn AlertSink>`, and none of those traits has a call
//! that makes, changes or ends anything at a cloud. These tests do not prove that; they
//! fail loudly when someone changes the shape that makes it true (a method added to one of
//! the traits, a spend call named in a module the watch path runs through).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_alert::FakeSink;
use js_watch::fake::FakeStatus;
use js_watch::{CloudView, Seen, Settings, Status, Stray, ViewError, Watcher};

/// A cloud that can only read: it has the three calls of [`CloudView`] and nothing else,
/// no client and no terminate.
struct ReadsOnly;

impl CloudView for ReadsOnly {
    fn cloud(&self) -> &str {
        "reads-only"
    }

    fn instances(&self) -> Result<Vec<Seen>, ViewError> {
        Ok(Vec::new())
    }

    fn strays(&self) -> Result<Vec<Stray>, ViewError> {
        Ok(Vec::new())
    }
}

/// Asserts, by the compiler, that a watcher can be built and ticked from a cloud that has
/// only reads. The test compiles only while [`CloudView`] needs no more than that: add a
/// required spend call to the trait and this type stops satisfying it.
#[test]
fn a_watcher_runs_on_a_cloud_that_can_only_read() {
    let status = Arc::new(FakeStatus::new(Status::default()));
    let settings = Settings {
        stale_after: Duration::from_secs(60),
        confirm_ticks: 1,
        sweep_every: Duration::from_secs(60),
        repeat_after: Duration::from_secs(60),
    };
    let mut watcher = Watcher::new(
        (Arc::new(ReadsOnly), status),
        Arc::new(FakeSink::new()),
        settings,
    );
    let report = watcher.tick(Duration::ZERO);
    assert_eq!((report.instances, report.alerts), (0, 0));
}

/// The words that mean a call that makes, changes or ends something at a cloud.
const CALLS: [&str; 13] = [
    "terminate",
    "release",
    "launch",
    "create_",
    "delete",
    "stop",
    "kill",
    "destroy",
    "cancel",
    "shutdown",
    ".post(",
    ".delete(",
    ".put(",
];

/// The names of the types that have such calls, and of the handles that hold them. Built
/// from parts, so that this file does not name the provider's raw API either (a scan of the
/// whole workspace refuses that name outside the provider's crate).
fn holders() -> Vec<String> {
    vec![
        ["Lambda", "Api"].concat(),
        "LambdaProvider".to_owned(),
        ["Http", "Api"].concat(),
        "ElasticProvider".to_owned(),
        "Aborter".to_owned(),
        "LambdaTarget".to_owned(),
        "client::aborter".to_owned(),
    ]
}

/// The files of the crate that hold the spend side, by name: the abort and the handle it
/// uses, the client builder, and the fake abort target. Every other file under `src` is the
/// watch path and is scanned.
const SPEND_SIDE: [&str; 3] = ["abort.rs", "client.rs", "fake.rs"];

/// The spend words in `text` (code only, comments gone); with `types`, the holders too.
fn spends(text: &str, types: bool) -> Vec<String> {
    let calls = CALLS.iter().map(|word| (*word).to_owned());
    let held = holders().into_iter().filter(|_| types);
    calls
        .chain(held)
        .filter(|word| text.contains(word.as_str()))
        .collect()
}

/// `text` without `/* */` comments and without everything from `//` to the end of a line
/// (doc comments included). A `//` inside a string cuts the line there too, which can only
/// hide less of a line than it shows.
fn code_of(text: &str) -> String {
    let mut whole = text.to_owned();
    while let Some(start) = whole.find("/*") {
        let end = whole[start..]
            .find("*/")
            .map_or(whole.len(), |at| start + at + 2);
        whole.replace_range(start..end, "");
    }
    let lines: Vec<&str> = whole
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect();
    lines.join("\n")
}

/// Whether the code of the file `name` of the watch path reaches the abort module: only
/// `cli.rs`, which dispatches the `abort` command, may.
fn routes_to_abort(name: &str, code: &str) -> bool {
    name != "cli.rs" && code.contains("abort::")
}

/// Asserts the scan sees what it should: a text that names a spend call is caught in each
/// spelling the list holds, a holder is caught only when asked for, comments of both kinds
/// hide nothing and show nothing, and clean code is not flagged.
#[test]
fn the_scan_finds_a_spend_word_and_only_then() {
    assert_eq!(spends("api.terminate(id)", false), ["terminate"]);
    assert_eq!(spends("agent.post(url)", false), [".post("]);
    assert_eq!(spends("client.cancel()", false), ["cancel"]);
    let raw = ["Lambda", "Api"].concat();
    let typed = format!("Arc<dyn {raw}>");
    assert_eq!(spends(&typed, true), [raw.as_str()]);
    assert_eq!(spends("client::aborter(t)", true), ["client::aborter"]);
    assert!(spends(&typed, false).is_empty());
    assert!(spends("self.reader.tagged()", true).is_empty());
    let hidden = "keep // terminate\n/// delete\nx /* kill\n stop */ y /* z */ w";
    assert_eq!(code_of(hidden), "keep \n\nx  y  w");
    assert!(spends(&code_of(hidden), true).is_empty());
    // The way onto the spend side: the abort module and its target.
    assert!(routes_to_abort(
        "watcher.rs",
        "crate::abort::command(a, b, true, out)"
    ));
    assert!(routes_to_abort("lib.rs", "pub use abort::{abort}"));
    assert!(!routes_to_abort("cli.rs", "abort::command(a)"));
    assert!(!routes_to_abort("lib.rs", "pub mod abort;"));
    assert_eq!(
        spends("abort::LambdaTarget::new(a)", true),
        ["LambdaTarget"]
    );
}

/// Every file under `dir`, found by walking it. Symlinks are not followed and `target` and
/// `.git` are not entered.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let name = entry.file_name();
        if kind.is_dir() && name != "target" && name != ".git" {
            found.extend(rust_files(&entry.path()));
        } else if kind.is_file() {
            found.push(entry.path());
        }
    }
    found
}

/// Asserts that every file under `js-watch/src` that is not on the spend side, and the
/// read side of the Lambda client, name no spend call and no type that has one. It is a
/// tripwire for a spend call added to a module the watcher uses.
#[test]
fn the_watch_path_names_no_spend_call() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The read side of the Lambda client implements the reads for every client, so it
    // names the trait of the client; it names no call that spends.
    let mut paths = vec![(here.join("../js-provider-lambdalabs/src/read.rs"), false)];
    let all = rust_files(&here.join("src"));
    assert!(all.len() >= 9, "the scan found {} files", all.len());
    let on_spend_side = |path: &PathBuf| {
        let name = path.file_name().and_then(|name| name.to_str()).unwrap();
        SPEND_SIDE.contains(&name)
    };
    let (spend, watch): (Vec<PathBuf>, Vec<PathBuf>) = all.into_iter().partition(on_spend_side);
    assert_eq!(spend.len(), SPEND_SIDE.len(), "{spend:?}");
    paths.extend(watch.into_iter().map(|path| (path, true)));
    for (path, types) in paths {
        let text = fs::read_to_string(&path).unwrap();
        // The tests of a module may name what it must not use; the code may not.
        let code = code_of(text.split("#[cfg(test)]").next().unwrap());
        let found = spends(&code, types);
        assert!(found.is_empty(), "{}: {found:?}", path.display());
        let name = path.file_name().and_then(|name| name.to_str()).unwrap();
        assert!(
            !routes_to_abort(name, &code),
            "{} reaches abort::",
            path.display()
        );
    }
}

/// The names of the methods the trait `header` declares in the file `path`, found by
/// reading the block from its opening line to the first line that is a lone `}`, in any
/// spelling of the declaration (`pub`, `async`, `unsafe`, `const`, `extern`).
fn methods(path: &Path, header: &str) -> BTreeSet<String> {
    let text = fs::read_to_string(path).unwrap();
    let start = text
        .find(header)
        .unwrap_or_else(|| panic!("{header} not found"));
    let block = text[start..].split("\n}\n").next().unwrap();
    let mut names = BTreeSet::new();
    for line in block.lines() {
        let mut rest = line.trim_start();
        for qualifier in ["pub ", "async ", "unsafe ", "const ", "extern \"C\" "].repeat(2) {
            rest = rest.strip_prefix(qualifier).unwrap_or(rest);
        }
        if let Some(declaration) = rest.strip_prefix("fn ") {
            let name = declaration.split(['(', '<']).next().unwrap();
            names.insert(name.trim().to_owned());
        }
    }
    names
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Asserts the method set of each trait the watcher is built from, exactly: a rename or an
/// added method (a spend call, say) fails here and has to be looked at. It also asserts
/// that the reader sees every qualifier of a declaration.
#[test]
fn the_traits_the_watcher_is_built_from_have_exactly_their_methods() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lambda = here.join("../js-provider-lambdalabs/src/read.rs");
    assert_eq!(
        methods(&lambda, "pub trait LambdaRead"),
        set(&["read_instances", "read_instance_types"])
    );
    let view = here.join("src/view.rs");
    assert_eq!(
        methods(&view, "pub trait CloudView"),
        set(&["cloud", "instances", "strays"])
    );
    assert_eq!(
        methods(&here.join("src/status.rs"), "pub trait StatusSource"),
        set(&["read"])
    );
    assert_eq!(
        methods(&here.join("../js-alert/src/sink.rs"), "pub trait AlertSink"),
        set(&["send"])
    );

    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-watch-methods.rs");
    fs::write(
        &scratch,
        "pub trait T {\n    async fn a();\n    unsafe fn b();\n    pub const fn c<X>();\n    fn d(\n        x: u8,\n    );\n}\n",
    )
    .unwrap();
    assert_eq!(methods(&scratch, "pub trait T"), set(&["a", "b", "c", "d"]));
    fs::remove_file(&scratch).unwrap();
}

/// The files of `files` (path from `crates/`, text) that name `Aborter` where it may not be
/// named: it is for operator tooling, so only the provider crate that defines it and the
/// abort side of `js-watch` (and the tests of both) may.
fn aborter_leaks(files: &[(String, String)]) -> Vec<String> {
    let allowed = [
        "js-provider-lambdalabs/",
        "js-watch/src/abort.rs",
        "js-watch/src/client.rs",
        "js-watch/tests/abort.rs",
        "js-watch/tests/read_only.rs",
    ];
    files
        .iter()
        .filter(|(_, text)| text.contains("Aborter"))
        .filter(|(path, _)| !allowed.iter().any(|prefix| path.starts_with(prefix)))
        .map(|(path, _)| path.clone())
        .collect()
}

/// Asserts the rule on planted text (a server module naming the type is caught, the places
/// that may are not), and then on the workspace: no other crate or module names `Aborter`.
/// It catches the stop-everything handle being passed to code that has no business with it.
#[test]
fn the_aborter_is_named_only_where_operator_tooling_lives() {
    let planted = [
        (
            "js-lease-server/src/x.rs".to_owned(),
            "use Aborter;".to_owned(),
        ),
        ("js-watch/src/watcher.rs".to_owned(), "Aborter".to_owned()),
        ("js-watch/src/abort.rs".to_owned(), "Aborter".to_owned()),
        (
            "js-provider-lambdalabs/src/y.rs".to_owned(),
            "Aborter".to_owned(),
        ),
        ("js-watch/src/view.rs".to_owned(), "nothing".to_owned()),
    ];
    assert_eq!(
        aborter_leaks(&planted),
        ["js-lease-server/src/x.rs", "js-watch/src/watcher.rs"]
    );
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let files: Vec<(String, String)> = rust_files(&crates)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .map(|path| {
            let relative = path.strip_prefix(&crates).unwrap();
            let relative = relative.to_string_lossy().replace('\\', "/");
            (
                relative,
                String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned(),
            )
        })
        .collect();
    assert!(files.len() > 100, "the scan found {} files", files.len());
    assert_eq!(aborter_leaks(&files), Vec::<String>::new());
}
