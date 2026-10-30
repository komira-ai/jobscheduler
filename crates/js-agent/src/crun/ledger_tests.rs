//! The ledger of leases and its sweep, over a directory and a stand-in for the removal of
//! a lease (the real one needs `crun`; `tests/crun_runtime` runs it).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::ledger::{DIR, Entry, Ledger, Left, Read, Removed};
use crate::cli_runtime::container_name;
use crate::lifecycle::label;

fn ledger(scratch: &Path, agent: &str) -> Ledger {
    Ledger::open(scratch, agent).expect("open the ledger")
}

/// The sidecar of `agent`'s run `run`.
fn entry(agent: &str, run: &str) -> Entry {
    let name = container_name(agent, run).expect("a lease name");
    let labels = BTreeMap::from([(label::RUN.to_owned(), run.to_owned())]);
    Entry::new(agent, run, &name, &labels)
}

/// The file the sidecar of `entry` is in.
fn file_of(ledger: &Ledger, entry: &Entry) -> PathBuf {
    ledger.dir().join(format!("{}.json", entry.name))
}

/// Everything in the ledger's directory, by name.
fn files(ledger: &Ledger) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(ledger.dir())
        .expect("list the ledger")
        .map(|e| e.expect("entry").file_name().into_string().expect("text"))
        .collect();
    names.sort();
    names
}

/// A stand-in for the removal of leases: a lease is a directory `<root>/<name>`, which may
/// not exist (a crash before it was made); what it is asked to remove is recorded.
struct Leases {
    root: PathBuf,
    asked: RefCell<Vec<String>>,
}

impl Leases {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            asked: RefCell::new(Vec::new()),
        }
    }

    fn make(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(dir.join("root")).expect("make a lease");
        fs::write(dir.join("root").join("file"), "x").expect("write in it");
        dir
    }

    fn tear_down(&self, entry: &Entry) -> Result<(), String> {
        self.asked.borrow_mut().push(entry.name.clone());
        match fs::remove_dir_all(self.root.join(&entry.name)) {
            Err(why) if why.kind() != io::ErrorKind::NotFound => Err(why.to_string()),
            _ => Ok(()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

/// Catches a sweep that leaves this agent's leases behind or its sidecars in the ledger
/// (the leftovers of a dead process would pile up), and one that reports a lease it did not
/// remove: both leases and both sidecars go, and each removal is reported with the run, the
/// name and the labels the sidecar held, the agent's own label among them.
#[test]
fn a_sweep_removes_this_agents_leases_and_sidecars() {
    let dir = TestDir::under(std::env::temp_dir(), "sweep-own");
    let ledger = ledger(&dir, "agent-a");
    let leases = Leases::new(&dir.path().join("leases"));
    let (one, two) = (entry("agent-a", "r1"), entry("agent-a", "r2"));
    for e in [&one, &two] {
        ledger.record(e).expect("record");
        leases.make(&e.name);
    }
    let report = ledger.sweep(|e| leases.tear_down(e)).expect("sweep");
    let removed = |e: &Entry| Removed {
        run_id: e.run_id.clone(),
        name: e.name.clone(),
        labels: e.labels.clone(),
    };
    assert_eq!(report.removed, [removed(&one), removed(&two)]);
    assert_eq!(report.removed[0].labels[label::AGENT], "agent-a");
    assert!(report.failed.is_empty() && report.left.is_empty());
    assert!(files(&ledger).is_empty(), "a sidecar stayed");
    assert!(!dir.path().join("leases").join(&one.name).exists());
    assert!(!dir.path().join("leases").join(&two.name).exists());
}

/// Catches the defect that makes two agents on one host remove each other's leases: the
/// sweep of agent a removes a's lease only, asks for no other, leaves b's sidecar and
/// lease in place and reports b's sidecar as another agent's; b's own sweep then removes
/// b's lease. Neither agent reads the other's sidecar as its own.
#[test]
fn two_agents_on_one_store_never_sweep_each_others_leases() {
    let dir = TestDir::under(std::env::temp_dir(), "sweep-two");
    let (a, b) = (ledger(&dir, "agent-a"), ledger(&dir, "agent-b"));
    let leases = Leases::new(&dir.path().join("leases"));
    let (mine, theirs) = (entry("agent-a", "r1"), entry("agent-b", "r1"));
    assert_ne!(mine.name, theirs.name, "one run id, two leases");
    for e in [&mine, &theirs] {
        let owner = if e.agent == "agent-a" { &a } else { &b };
        owner.record(e).expect("record");
        leases.make(&e.name);
    }
    assert_eq!(a.read(&theirs.name), Read::Foreign(theirs.clone()));
    assert_eq!(b.read(&mine.name), Read::Foreign(mine.clone()));

    let report = a.sweep(|e| leases.tear_down(e)).expect("sweep a");
    assert_eq!(leases.asked(), std::slice::from_ref(&mine.name));
    assert_eq!(report.removed.len(), 1);
    assert_eq!(
        report.left,
        [Left::Foreign {
            path: file_of(&b, &theirs),
            agent: "agent-b".to_owned()
        }]
    );
    assert!(leases.root.join(&theirs.name).is_dir(), "b's lease went");
    assert_eq!(files(&a), [format!("{}.json", theirs.name)]);

    let report = b.sweep(|e| leases.tear_down(e)).expect("sweep b");
    assert_eq!(report.removed.len(), 1);
    assert!(report.left.is_empty());
    assert!(files(&b).is_empty());
}

/// Catches a sweep that acts on a file that is not a well-formed sidecar of its own name:
/// each of these is left where it is, not handed to the removal, and reported as malformed
/// with a reason, and none stops the good sidecar beside them from being swept. The bad
/// ones: garbage, a sidecar of another form, an unknown field, a sidecar copied under
/// another lease's name (its agent and run id do not give that name), a lease name that is
/// not a path component, a link to a good sidecar (a link is never followed), a directory,
/// one that is too long, and a leftover temporary file.
#[test]
fn a_sidecar_that_is_malformed_is_left_alone_and_reported() {
    let dir = TestDir::under(std::env::temp_dir(), "sweep-bad");
    let ledger = ledger(&dir, "agent-a");
    let leases = Leases::new(&dir.path().join("leases"));
    let good = entry("agent-a", "good");
    ledger.record(&good).expect("record");
    leases.make(&good.name);

    let other = entry("agent-a", "other");
    let text = serde_json::to_string(&other).expect("json");
    let at = |name: &str| ledger.dir().join(name);
    let named = |e: &Entry| format!("{}.json", e.name);
    fs::write(at("js-0000000000000001-garbage.json"), "not json").expect("write");
    fs::write(
        at("js-0000000000000002-old.json"),
        text.replace("\"version\":1", "\"version\":9"),
    )
    .expect("write");
    fs::write(
        at("js-0000000000000003-extra.json"),
        text.replacen('{', "{\"extra\":1,", 1),
    )
    .expect("write");
    // The sidecar of run `other`, under the name of run `moved`.
    fs::write(at(&named(&entry("agent-a", "moved"))), &text).expect("write");
    fs::write(at("..json"), &text).expect("write");
    symlink(at(&named(&good)), at("js-0000000000000004-link.json")).expect("link");
    fs::create_dir(at("js-0000000000000005-dir.json")).expect("dir");
    fs::write(at("js-0000000000000006-big.json"), vec![b' '; 70 * 1024]).expect("write");
    fs::write(at(".js-0000000000000007-tmp.tmp"), &text).expect("write");
    let before = files(&ledger);

    let report = ledger.sweep(|e| leases.tear_down(e)).expect("sweep");
    assert_eq!(
        leases.asked(),
        std::slice::from_ref(&good.name),
        "only the good one"
    );
    assert_eq!(report.removed.len(), 1);
    assert!(report.failed.is_empty());
    let mut kept = before.clone();
    kept.retain(|name| *name != named(&good));
    assert_eq!(files(&ledger), kept, "a malformed file was removed");
    assert_eq!(report.left.len(), kept.len(), "{:?}", report.left);
    for left in &report.left {
        let Left::Malformed { path, why } = left else {
            panic!("expected a malformed sidecar: {left:?}");
        };
        assert!(path.starts_with(ledger.dir()) && !why.is_empty());
    }
    assert!(at("js-0000000000000004-link.json").is_symlink());
}

/// Catches a process that dies after it recorded a lease and before, or while, it made it
/// (the sidecar comes first): a sidecar of a lease that was never made, and one of a lease
/// made in part, are both swept, the removal being asked for each, and neither leaves a
/// sidecar to be swept again.
#[test]
fn a_crash_between_the_sidecar_and_the_lease_is_swept() {
    let dir = TestDir::under(std::env::temp_dir(), "sweep-crash");
    let ledger = ledger(&dir, "agent-a");
    let leases = Leases::new(&dir.path().join("leases"));
    let (never, partly) = (entry("agent-a", "never"), entry("agent-a", "partly"));
    ledger.record(&never).expect("record");
    ledger.record(&partly).expect("record");
    fs::create_dir_all(leases.root.join(&partly.name)).expect("a lease with a directory only");

    let report = ledger.sweep(|e| leases.tear_down(e)).expect("sweep");
    let mut asked = leases.asked();
    asked.sort();
    let mut wanted = vec![never.name.clone(), partly.name.clone()];
    wanted.sort();
    assert_eq!(asked, wanted);
    assert_eq!(report.removed.len(), 2);
    assert!(files(&ledger).is_empty());
    assert!(!leases.root.join(&partly.name).exists());
    let again = ledger.sweep(|e| leases.tear_down(e)).expect("sweep again");
    assert_eq!(again, super::ledger::SweepReport::default());
}

/// Catches a sweep that drops the sidecar of a lease it could not remove (the leftover
/// would never be removed, and nothing would say it exists): the failure is reported with
/// the run and the reason, the sidecar stays, and the next sweep removes the lease.
#[test]
fn a_lease_that_cannot_be_removed_keeps_its_sidecar_for_the_next_sweep() {
    let dir = TestDir::under(std::env::temp_dir(), "sweep-stuck");
    let ledger = ledger(&dir, "agent-a");
    let leases = Leases::new(&dir.path().join("leases"));
    let stuck = entry("agent-a", "stuck");
    ledger.record(&stuck).expect("record");
    leases.make(&stuck.name);

    let report = ledger.sweep(|_| Err("busy".to_owned())).expect("sweep");
    assert!(report.removed.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].run_id, "stuck");
    assert_eq!(report.failed[0].why, "busy");
    assert_eq!(files(&ledger), [format!("{}.json", stuck.name)]);

    let report = ledger.sweep(|e| leases.tear_down(e)).expect("sweep again");
    assert_eq!(report.removed.len(), 1);
    assert!(files(&ledger).is_empty());
    assert!(!leases.root.join(&stuck.name).exists());
}

/// Catches the removal of one orphan by run id reaching the wrong lease: `read` of a lease
/// says this agent's (with the labels it was recorded with), another agent's, or absent,
/// and `forget` removes the one sidecar and is quiet for one that is gone.
#[test]
fn a_sidecar_is_read_back_with_its_labels_and_forgotten_by_name() {
    let dir = TestDir::under(std::env::temp_dir(), "ledger-read");
    let ledger = ledger(&dir, "agent-a");
    let (one, two) = (entry("agent-a", "r1"), entry("agent-a", "r2"));
    ledger.record(&one).expect("record");
    ledger.record(&two).expect("record");
    assert_eq!(ledger.read(&one.name), Read::Mine(one.clone()));
    let Read::Mine(read) = ledger.read(&two.name) else {
        panic!("not this agent's");
    };
    assert_eq!(read.labels[label::RUN], "r2");
    assert_eq!(read.labels[label::AGENT], "agent-a");

    ledger.forget(&one.name).expect("forget");
    ledger.forget(&one.name).expect("forget again is quiet");
    assert_eq!(ledger.read(&one.name), Read::Absent);
    assert_eq!(ledger.read(&two.name), Read::Mine(two.clone()), "the other");
    assert_eq!(
        files(&ledger),
        [format!("{}.json", two.name)],
        "no tmp file"
    );
}

/// Catches a sidecar written over another (a run id used twice would hide the first run's
/// lease from the sweep) and a name that reaches outside the ledger: the second record of
/// a name fails and the first stays as it was; a name with a separator, a dot start or no
/// characters is refused by every call and nothing is made.
#[test]
fn a_sidecar_is_never_written_over_and_a_bad_name_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "ledger-names");
    let ledger = ledger(&dir, "agent-a");
    let first = entry("agent-a", "r1");
    ledger.record(&first).expect("record");
    let mut second = first.clone();
    second.labels.insert("k".to_owned(), "v".to_owned());
    let why = ledger.record(&second).expect_err("a second record");
    assert_eq!(why.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(ledger.read(&first.name), Read::Mine(first.clone()));

    for bad in ["", ".", "..", ".hidden", "a/b", "../x"] {
        let mut named = first.clone();
        named.name = bad.to_owned();
        assert!(ledger.record(&named).is_err(), "{bad:?} recorded");
        assert!(ledger.forget(bad).is_err(), "{bad:?} forgotten");
        assert!(matches!(ledger.read(bad), Read::Malformed(_)), "{bad:?}");
    }
    assert_eq!(files(&ledger), [format!("{}.json", first.name)]);
    assert!(dir.path().join(DIR).is_dir());
}
