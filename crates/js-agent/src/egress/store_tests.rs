//! The egress store's records on disk, over real files in a scratch directory of each test.

use std::os::unix::fs::symlink;

use js_testkit::dir::TestDir;

use super::*;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("js-egress-store-{name}"))
}

fn port(host: u16, container: u16) -> PortMap {
    PortMap { host, container }
}

fn record(index: u32, level: JobNetwork, ports: &[PortMap]) -> Record {
    Record {
        index,
        level,
        ports: ports.to_vec(),
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// Asserts that opening a store makes the directory `egress` of the state root with mode 0700
/// and names it by [`Store::dir`], that opening it again finds it as it is (its records stay),
/// and that the stores of one root are equal. Catches a store whose records are readable by
/// other users or are lost when the agent starts again.
#[test]
fn opening_makes_the_directory_private_and_reopening_keeps_it() {
    let root = scratch("open");
    let store = Store::open(&root).expect("opened");
    assert_eq!(store.dir(), root.join("egress"));
    assert_eq!(mode_of(store.dir()), 0o700);
    store
        .write("run-a", &record(1, JobNetwork::AllowList, &[]))
        .expect("written");
    let again = Store::open(&root).expect("opened again");
    assert_eq!(again, store);
    assert_eq!(
        again.records().expect("listed"),
        [(
            "run-a".to_owned(),
            Ok(record(1, JobNetwork::AllowList, &[]))
        )]
    );
}

/// Asserts that a state root that is not there is an error of its own (the store does not make
/// the root), and that a file or a link under the name `egress` is refused with the path in
/// the text, the link not followed even when it leads to a directory. Catches a store that
/// writes records through a link an attacker placed, or into a place that is not a directory.
#[test]
fn opening_refuses_a_missing_root_a_file_and_a_link() {
    let root = scratch("refused");
    let missing = Store::open(&root.join("no-such-root")).expect_err("no root");
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);

    let file_root = root.join("file");
    fs::create_dir(&file_root).expect("root");
    fs::write(file_root.join("egress"), b"not a directory").expect("file");
    let refused = Store::open(&file_root).expect_err("a file");
    assert_eq!(
        refused.to_string(),
        format!("{} is not a directory", file_root.join("egress").display())
    );

    let link_root = root.join("link");
    fs::create_dir(&link_root).expect("root");
    let elsewhere = root.join("elsewhere");
    fs::create_dir(&elsewhere).expect("directory");
    symlink(&elsewhere, link_root.join("egress")).expect("link");
    let refused = Store::open(&link_root).expect_err("a link");
    assert_eq!(
        refused.to_string(),
        format!("{} is not a directory", link_root.join("egress").display())
    );
}

/// Asserts the record on disk, byte for byte: the version, the index, the level's name and the
/// ports in the order given, in a file of mode 0600 named after the run, with no temporary
/// file left beside it; writing again replaces the record whole (fewer ports, another level).
/// Catches a record this agent cannot read back, one other users can read, or a leftover
/// temporary file that the listing would have to tell from a record.
#[test]
fn a_written_record_is_the_exact_json_in_a_private_file_and_replacing_leaves_no_temp_file() {
    let root = scratch("write");
    let store = Store::open(&root).expect("opened");
    let ports = [port(9001, 90), port(8080, 80)];
    store
        .write("js-x.r1", &record(7, JobNetwork::Open, &ports))
        .expect("written");
    let path = store.dir().join("js-x.r1.json");
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        r#"{"version":1,"index":7,"level":"open","ports":[{"host":9001,"container":90},{"host":8080,"container":80}]}"#
    );
    assert_eq!(mode_of(&path), 0o600);
    store
        .write("js-x.r1", &record(2, JobNetwork::AllowList, &[]))
        .expect("replaced");
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        r#"{"version":1,"index":2,"level":"allow-list","ports":[]}"#
    );
    let names: Vec<_> = fs::read_dir(store.dir())
        .expect("listing")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["js-x.r1.json"]);
}

/// Asserts that a name a record cannot have is refused by every call with the name quoted
/// (empty, a dot first, a slash, a space, a name that climbs), and that nothing is made in the
/// store or outside it. Catches a run name that writes or removes a file outside the store.
#[test]
fn a_name_a_record_cannot_have_is_refused_by_write_and_remove() {
    let root = scratch("names");
    let store = Store::open(&root).expect("opened");
    let any = record(0, JobNetwork::AllowList, &[]);
    for name in ["", ".hidden", "a/b", "a b", "../x", "x\n"] {
        let text = format!("{name:?} is not a name a record can have");
        let written = store.write(name, &any).expect_err("refused");
        assert_eq!(written.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        assert_eq!(written.to_string(), text);
        assert_eq!(store.remove(name).expect_err("refused").to_string(), text);
    }
    assert_eq!(fs::read_dir(store.dir()).expect("listing").count(), 0);
    assert_eq!(fs::read_dir(&*root).expect("listing").count(), 1);
}

/// Asserts that a write that fails says so and leaves the old record whole: a directory in the
/// place of the temporary file fails the open, and a non-empty directory in the place of the
/// record fails the rename; the temporary file of the rename's failure is then still there but
/// is not a record. Catches a write that reports success after a step failed.
#[test]
fn a_write_that_fails_reports_it_and_the_record_stays_what_it_was() {
    let root = scratch("write-fails");
    let store = Store::open(&root).expect("opened");
    let any = record(0, JobNetwork::AllowList, &[]);
    fs::create_dir(store.dir().join(".run.json.tmp")).expect("a directory");
    let open = store
        .write("run", &any)
        .expect_err("the temp file is a directory");
    assert_eq!(open.to_string(), "Is a directory (os error 21)");
    fs::remove_dir(store.dir().join(".run.json.tmp")).expect("removed");

    fs::create_dir_all(store.dir().join("run.json").join("inside")).expect("a directory");
    let rename = store
        .write("run", &any)
        .expect_err("the record is a directory");
    assert_eq!(rename.to_string(), "Is a directory (os error 21)");
    assert_eq!(
        store.records().expect("listed").len(),
        1,
        "the directory is listed, as one that cannot be read, and the temp file is not"
    );
}

/// Asserts that removing a record deletes its file, that a record that is not there is not an
/// error, and that a record that cannot be removed is (a non-empty directory under its name).
/// Catches a release that fails because its record was removed already, or hides a record it
/// could not remove.
#[test]
fn removing_a_record_forgives_an_absent_one_and_reports_one_that_stays() {
    let root = scratch("remove");
    let store = Store::open(&root).expect("opened");
    store
        .write("run", &record(0, JobNetwork::AllowList, &[]))
        .expect("written");
    store.remove("run").expect("removed");
    assert_eq!(store.records().expect("listed"), []);
    store.remove("run").expect("absent is fine");
    fs::create_dir_all(store.dir().join("stuck.json").join("inside")).expect("a directory");
    let stuck = store.remove("stuck").expect_err("a directory");
    assert_eq!(stuck.to_string(), "Is a directory (os error 21)");
}

/// Asserts the listing: records sorted by run whatever order they were written in, with their
/// ports; names that do not end in `.json`, a temporary file, a dot file, a name with a
/// character a record cannot have and a directory named `x` are not records; and the listing
/// of a store whose directory is gone is an error. Catches a start that takes a stray file of
/// the directory for a run's record, or reads the records in an order that changes what it
/// keeps.
#[test]
fn the_listing_is_sorted_and_skips_what_is_not_a_record() {
    let root = scratch("list");
    let store = Store::open(&root).expect("opened");
    let ports = [port(8080, 80)];
    store
        .write("b", &record(2, JobNetwork::Open, &ports))
        .expect("written");
    store
        .write("a", &record(1, JobNetwork::AllowList, &[]))
        .expect("written");
    for name in [
        "notes.txt",
        "c.json.bak",
        ".a.json.tmp",
        ".hidden.json",
        "a b.json",
        ".json",
    ] {
        fs::write(store.dir().join(name), b"{}").expect("file");
    }
    assert_eq!(
        store.records().expect("listed"),
        [
            ("a".to_owned(), Ok(record(1, JobNetwork::AllowList, &[]))),
            ("b".to_owned(), Ok(record(2, JobNetwork::Open, &ports))),
        ]
    );
    fs::remove_dir_all(store.dir()).expect("removed");
    let gone = store.records().expect_err("no directory");
    assert_eq!(gone.kind(), io::ErrorKind::NotFound);
}

/// Asserts the text of each way a record cannot be read, in the listing and not as an error of
/// it: text that is not JSON, JSON of another shape, a version other than 1, a level that is
/// not one, a record with no `ports` (which reads as none), a file that cannot be read (a
/// directory under the name). Catches a start that takes a record it cannot read for a run's.
#[test]
fn a_record_that_cannot_be_read_is_listed_with_why() {
    let root = scratch("unreadable");
    let store = Store::open(&root).expect("opened");
    for (name, text) in [
        ("junk", "nope"),
        ("shape", r#"{"version":1}"#),
        (
            "version",
            r#"{"version":2,"index":0,"level":"open","ports":[]}"#,
        ),
        (
            "level",
            r#"{"version":1,"index":0,"level":"closed","ports":[]}"#,
        ),
        ("portless", r#"{"version":1,"index":4,"level":"none"}"#),
    ] {
        fs::write(store.dir().join(format!("{name}.json")), text).expect("file");
    }
    fs::create_dir(store.dir().join("dir.json")).expect("a directory");
    let read: Vec<(String, Result<Record, String>)> = store.records().expect("listed");
    assert_eq!(
        read.iter().map(|(run, _)| run.as_str()).collect::<Vec<_>>(),
        ["dir", "junk", "level", "portless", "shape", "version"]
    );
    let by = |run: &str| {
        read.iter()
            .find(|(name, _)| name == run)
            .expect(run)
            .1
            .clone()
    };
    assert_eq!(by("dir"), Err("Is a directory (os error 21)".to_owned()));
    assert_eq!(
        by("junk"),
        Err("not a record: expected ident at line 1 column 2".to_owned())
    );
    assert_eq!(
        by("shape"),
        Err("not a record: missing field `index` at line 1 column 13".to_owned())
    );
    assert_eq!(by("version"), Err("a record of version 2".to_owned()));
    assert_eq!(
        by("level"),
        Err("the level \"closed\" is not one".to_owned())
    );
    assert_eq!(by("portless"), Ok(record(4, JobNetwork::None, &[])));
}
