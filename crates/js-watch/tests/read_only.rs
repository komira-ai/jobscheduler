//! The watcher holds no spend capability, shown two ways: by type (what it is built from
//! has no call that spends, and a type that offers only reads satisfies it) and by source
//! (the modules the watch path runs through name no call that spends).

use std::fs;
use std::path::Path;
use std::sync::Arc;

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
/// spend call to the trait and this type stops satisfying it. It catches a trait that grew
/// a terminate the watcher could then reach.
#[test]
fn a_watcher_runs_on_a_cloud_that_can_only_read() {
    let status = Arc::new(FakeStatus::new(Status::default()));
    let settings = Settings {
        stale_after: std::time::Duration::from_secs(60),
        confirm_ticks: 1,
        sweep_every: std::time::Duration::from_secs(60),
    };
    let mut watcher = Watcher::new(
        (Arc::new(ReadsOnly), status),
        Arc::new(FakeSink::new()),
        settings,
    );
    let report = watcher.tick(std::time::Duration::ZERO);
    assert_eq!((report.instances, report.alerts), (0, 0));
}

/// The words that mean a call that makes, changes or ends something at a cloud, and (with
/// `types`) the names of the types that have such calls.
fn spends(text: &str, types: bool) -> Vec<&'static str> {
    let calls = ["terminate", "release", "launch", "create_", "delete"];
    let holders = ["LambdaApi", "LambdaProvider", "HttpApi", "ElasticProvider"];
    let words = calls
        .into_iter()
        .chain(holders.into_iter().filter(|_| types));
    words.filter(|word| text.contains(word)).collect()
}

/// Asserts the scan sees what it should: a text that names a spend call is caught, a
/// type that holds one is caught only when asked for, and a text that names neither is
/// not. It catches a scan that passes everything.
#[test]
fn the_scan_finds_a_spend_word_and_only_then() {
    assert_eq!(spends("api.terminate(id)", false), ["terminate"]);
    assert_eq!(spends("Arc<dyn LambdaApi>", true), ["LambdaApi"]);
    assert!(spends("Arc<dyn LambdaApi>", false).is_empty());
    assert!(spends("self.reader.tagged()", true).is_empty());
}

/// Asserts that the modules the watch path runs through (the cloud view, the status read,
/// the watcher, the configuration) and the read-only side of the Lambda client name no call
/// that spends and no type that has one. The spend side lives in `abort.rs` and the
/// command line, which build it only for the `abort` command. It catches a spend call
/// added to a module the watcher uses.
#[test]
fn the_watch_path_names_no_spend_call() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The read side of the Lambda client implements the reads for every client, so it
    // names the trait of the client; it names no call that spends.
    let lambda = (here.join("../js-provider-lambdalabs/src/read.rs"), false);
    let mut paths = vec![lambda];
    for name in ["view.rs", "status.rs", "watcher.rs", "config.rs"] {
        paths.push((here.join("src").join(name), true));
    }
    for (path, types) in paths {
        let text = fs::read_to_string(&path).unwrap();
        // The tests of a module may name what it must not use; the code may not.
        let code = text.split("#[cfg(test)]").next().unwrap();
        // Comments say what a module does not do.
        let code: String = code
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let found = spends(&code, types);
        assert!(found.is_empty(), "{}: {found:?}", path.display());
    }
}

/// Asserts that the read-only trait of the Lambda client declares exactly the two reads it
/// declares today. It catches a third method (a spend) added to the trait that the
/// watcher's view is built over.
#[test]
fn the_lambda_read_trait_declares_two_reads() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(here.join("../js-provider-lambdalabs/src/read.rs")).unwrap();
    let start = text.find("pub trait LambdaRead").unwrap();
    let end = text[start..].find("\n}\n").unwrap();
    let declared: Vec<&str> = text[start..start + end]
        .lines()
        .filter(|line| line.trim_start().starts_with("fn "))
        .collect();
    assert_eq!(declared.len(), 2, "{declared:?}");
    assert!(declared[0].contains("read_instances"));
    assert!(declared[1].contains("read_instance_types"));
}
