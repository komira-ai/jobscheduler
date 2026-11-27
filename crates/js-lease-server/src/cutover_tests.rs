//! The tests of `js-lease-server job-store`: the copy of a job store's file into a store, its
//! dry run and its refusals, the export back to files, and the round trip through SQLite and
//! through Postgres.

use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobId, JobState, NewJob, Retention};
use js_state::postgres::test_database;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Cond, Store, SystemClock, Txn};
use js_testkit::dir::TestDir;

use super::rows::{self, JOB_ROWS, Tally, USAGE_ROWS};
use crate::agent_api::job_rig::{spec, tables};
use crate::config::FeedRetention;
use crate::job_store::{JobSources, JobStoreSetup, open as open_jobs};

/// A scratch directory of its own for the test `name`.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/cutover");
    TestDir::under(root, name)
}

/// The SQLite state file at `path`, made when it is missing.
fn sqlite(path: &Path) -> SqliteStore {
    SqliteStore::open(path, Arc::new(SystemClock::default()), Options::default()).unwrap()
}

/// Writes `rows` into the SQLite state file at `path` (made when it is missing); a row given
/// a time to live is written with it.
fn file_with(path: &Path, rows: &[(&str, &[u8], Option<Duration>)]) {
    let store = sqlite(path);
    let txn = rows
        .iter()
        .fold(Txn::new(), |txn, (key, value, ttl)| match ttl {
            Some(ttl) => txn.put_ttl(*key, value.to_vec(), *ttl, Cond::Any),
            None => txn.put(*key, value.to_vec(), Cond::Any),
        });
    store.txn(txn).unwrap().unwrap();
}

/// Every live row of `store` under `prefix`: its key, its value and whether it has an end.
fn rows_of(store: &dyn Store, prefix: &str) -> Vec<(String, Vec<u8>, bool)> {
    let rows = store.scan_prefix(prefix).unwrap();
    rows.into_iter()
        .map(|(key, row)| (key, row.value, row.expires_at.is_some()))
        .collect()
}

/// [`rows_of`] the SQLite state file at `path`, every row.
fn file_rows(path: &Path) -> Vec<(String, Vec<u8>, bool)> {
    rows_of(&sqlite(path), "")
}

/// Writes the store file `store` in `dir` holding `line`, owner-only.
fn store_file(dir: &Path, line: &str) -> PathBuf {
    let path = dir.join("store");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    path
}

/// A store file in `dir` naming the SQLite store `state.db` there, and that file.
fn sqlite_store(dir: &Path) -> (PathBuf, PathBuf) {
    let db = dir.join("state.db");
    (store_file(dir, &format!("sqlite:{}", db.display())), db)
}

/// What the verb did with the words `words` after `js-lease-server job-store`: its exit status
/// (as its `Debug` text, since an `ExitCode` has no equality), its standard output and its
/// standard error.
fn verb(words: &[&dyn AsRef<std::ffi::OsStr>]) -> (String, String, String) {
    let mut args: Vec<OsString> = vec!["js-lease-server".into(), "job-store".into()];
    args.extend(words.iter().map(|word| {
        let word: &std::ffi::OsStr = (*word).as_ref();
        word.to_os_string()
    }));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::run(args, &mut out, &mut err);
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// The `Debug` text of the exit status `code`.
fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

/// A row for every prefix of the job store's file, one of them with a time to live (a
/// cordon), and `bulk` more job rows, so a copy reads more than one page.
fn job_file_rows(bulk: usize) -> Vec<(String, Vec<u8>, Option<Duration>)> {
    let mut rows: Vec<(String, Vec<u8>, Option<Duration>)> = JOB_ROWS
        .iter()
        .map(|prefix| (format!("{prefix}one"), prefix.as_bytes().to_vec(), None))
        .collect();
    rows.push((
        "cordon/host-c".into(),
        b"lease".to_vec(),
        Some(Duration::from_secs(3600)),
    ));
    rows.extend((0..bulk).map(|n| {
        (
            format!("job/bulk-{n:04}"),
            vec![u8::try_from(n % 251).unwrap()],
            None,
        )
    }));
    rows
}

/// [`file_with`] for owned rows.
fn file_with_owned(path: &Path, rows: &[(String, Vec<u8>, Option<Duration>)]) {
    let borrowed: Vec<(&str, &[u8], Option<Duration>)> = rows
        .iter()
        .map(|(key, value, ttl)| (key.as_str(), value.as_slice(), *ttl))
        .collect();
    file_with(path, &borrowed);
}

/// Asserts that a copy without `--write` prints, for the file, one line per key prefix with
/// its row count and its SHA-256 (exactly as given here, computed apart from the code), and how
/// many rows the store lacks, exits 0 and writes nothing into the store. It catches a dry run
/// that writes, and a change of the checksum's bytes that would make an earlier report
/// disagree with a later one.
#[test]
fn a_dry_run_copy_reports_each_prefix_and_writes_nothing() {
    let dir = scratch("dry");
    let from = dir.join("leases.toml.jobs");
    file_with(&from, &[("job/a", b"one", None), ("alloc/a", b"two", None)]);
    let (store, db) = sqlite_store(&dir);
    let (code, out, err) = verb(&[&"copy", &"--from", &from, &"--store", &store]);
    assert_eq!((code, err.as_str()), (status(0), ""));
    let wanted = format!(
        "{}:\n  alloc/ rows=1 \
         sha256=5c24cc75f5e29f7a27de93065af027b1a381f9902595cca5ac17da7f740077bb\n  job/ \
         rows=1 sha256=977e039b6135ef7aaeb7b1ea2a338d36b10d7de31353cefc7ad9fc61bcf310fe\nthe \
         store: 2 rows to write, 0 already there with the same value, 0 with another\ndry run: \
         nothing was written (--write writes them)\n",
        from.display()
    );
    assert_eq!(out, wanted);
    assert_eq!(file_rows(&db), Vec::new());
}

/// Asserts that a copy with `--write` puts every row of a file of more than one page into the
/// store, with the same values, the cordon still with an end, and that a dry run after it
/// finds every row in the store with the same value and none to write, and a second write
/// writes nothing. It catches a copy that stops at the first page, one that drops a row's
/// time to live, and one that cannot be run again.
#[test]
fn a_written_copy_puts_every_row_and_a_dry_run_after_it_finds_them_all() {
    let dir = scratch("write");
    let from = dir.join("leases.toml.jobs");
    let given = job_file_rows(2 * rows::PAGE.get());
    file_with_owned(&from, &given);
    let (store, db) = sqlite_store(&dir);
    let (code, out, err) = verb(&[&"copy", &"--from", &from, &"--store", &store, &"--write"]);
    assert_eq!((code, err.as_str()), (status(0), ""));
    let total = given.len();
    assert!(
        out.ends_with(&format!("wrote {total} rows into the store\n")),
        "{out}"
    );
    assert_eq!(file_rows(&db), file_rows(&from));
    assert!(file_rows(&db).contains(&("cordon/host-c".into(), b"lease".to_vec(), true)));
    let (code, again, _) = verb(&[&"copy", &"--from", &from, &"--store", &store]);
    assert_eq!(code, status(0));
    let said = format!(
        "the store: 0 rows to write, {total} already there with the same value, 0 with another\n"
    );
    assert!(again.contains(&said), "{again}");
    let (code, twice, _) = verb(&[&"copy", &"--from", &from, &"--store", &store, &"--write"]);
    assert_eq!(code, status(0));
    assert!(twice.ends_with("wrote 0 rows into the store\n"), "{twice}");
}

/// Asserts that a row the store holds with another value refuses the copy, with `--write`:
/// it names the key, says why on standard error, exits 1, and writes none of the file's rows,
/// not even those the store lacks. It catches a copy that replaces a row it did not write and
/// one that writes part of a file it refuses.
#[test]
fn a_row_the_store_holds_with_another_value_refuses_the_copy_and_writes_nothing() {
    let dir = scratch("different");
    let from = dir.join("leases.toml.jobs");
    file_with(&from, &[("job/a", b"one", None), ("alloc/a", b"two", None)]);
    let (store, db) = sqlite_store(&dir);
    file_with(&db, &[("job/a", b"else", None)]);
    let (code, out, err) = verb(&[&"copy", &"--from", &from, &"--store", &store, &"--write"]);
    assert_eq!(code, status(1));
    assert!(out.ends_with(
        "the store: 1 rows to write, 0 already there with the same value, 1 with another\n\
         different: job/a\n"
    ));
    assert_eq!(
        err,
        "js-lease-server job-store: the store holds 1 of the file's rows with other values \
         (named above): nothing was written\n"
    );
    assert_eq!(
        file_rows(&db),
        vec![("job/a".into(), b"else".to_vec(), false)]
    );
}

/// Asserts that a row another process writes into the store between the copy's read of it as
/// missing and its write stops the copy with the key named and the advice to run the dry run
/// again, and that the other process's value is kept. It catches a copy whose puts are not
/// conditional on the key being absent.
#[test]
fn a_row_another_process_writes_during_the_copy_stops_it_naming_the_key() {
    let dir = scratch("raced");
    let from = dir.join("leases.toml.jobs");
    file_with(&from, &[("job/raced", b"file", None)]);
    let source = sqlite(&from);
    let (target, _jobs) = tables();
    let other = target.inner();
    target.before_next(move || {
        let txn = Txn::new().put("job/raced", b"other".to_vec(), Cond::Any);
        other.txn(txn).unwrap().unwrap();
    });
    let stopped = rows::write(&source, &*target).err().unwrap();
    assert_eq!(
        stopped.to_string(),
        "\"job/raced\" was written by another process after it was read as missing: the rows \
         before it are written; run the dry run again"
    );
    assert_eq!(target.get("job/raced").unwrap().unwrap().value, b"other");
}

/// Asserts that a copy from a file that is not there is refused naming it, and makes no file
/// there (opening a state file that is missing would make it). It catches a typo in `--from`
/// that copies an empty job store and leaves a new file behind.
#[test]
fn a_copy_from_a_missing_file_is_refused_and_makes_no_file() {
    let dir = scratch("missing");
    let from = dir.join("leases.toml.jobs");
    let (store, _) = sqlite_store(&dir);
    let (code, out, err) = verb(&[&"copy", &"--from", &from, &"--store", &store]);
    assert_eq!((code, out.as_str()), (status(1), ""));
    let wanted = format!(
        "js-lease-server job-store: {}: there is no such file\n",
        from.display()
    );
    assert_eq!(err, wanted);
    assert!(!from.exists());
}

/// Asserts that a copy from a file that is not a state store is refused naming the file, and
/// leaves it as it was.
#[test]
fn a_copy_from_a_file_that_is_not_a_state_store_is_refused() {
    let dir = scratch("garbage");
    let from = dir.join("leases.toml.jobs");
    let text = b"this is not a SQLite database, and it is long enough";
    std::fs::write(&from, text).unwrap();
    let (store, _) = sqlite_store(&dir);
    let (code, _, err) = verb(&[&"copy", &"--from", &from, &"--store", &store]);
    assert_eq!(code, status(1));
    let named = format!("js-lease-server job-store: {}: ", from.display());
    assert!(err.starts_with(&named), "{err}");
    assert_eq!(std::fs::read(&from).unwrap(), text);
}

/// Asserts that a store file others may read is refused naming `--store`, as the server's own
/// start refuses it, for both commands, before any SQLite file is opened or made. It catches a
/// verb that reads a connection URL from a file the server would refuse.
#[test]
fn a_store_file_others_may_read_is_refused_naming_the_flag() {
    let dir = scratch("open_file");
    let from = dir.join("leases.toml.jobs");
    file_with(&from, &[("job/a", b"one", None)]);
    let (store, db) = sqlite_store(&dir);
    std::fs::set_permissions(&store, Permissions::from_mode(0o644)).unwrap();
    let (code, _, err) = verb(&[&"copy", &"--from", &from, &"--store", &store]);
    assert_eq!(code, status(1));
    assert!(
        err.starts_with("js-lease-server job-store: --store: "),
        "{err}"
    );
    let jobs_to = dir.join("back.jobs");
    let usage_to = dir.join("back-usage.db");
    let words: [&dyn AsRef<std::ffi::OsStr>; 7] = [
        &"export",
        &"--store",
        &store,
        &"--jobs-to",
        &jobs_to,
        &"--usage-to",
        &usage_to,
    ];
    let (code, _, err) = verb(&words);
    assert_eq!(code, status(1));
    assert!(
        err.starts_with("js-lease-server job-store: --store: "),
        "{err}"
    );
    assert!(!db.exists() && !jobs_to.exists() && !usage_to.exists());
}

/// Rows of every kind a store over Postgres holds: the job store's (one with a time to live),
/// the usage rows, and the host bindings, join tokens, scaler records and import marker that
/// the files the first start imported keep for a server without the setting.
fn store_rows() -> Vec<(String, Vec<u8>, Option<Duration>)> {
    let mut rows = job_file_rows(0);
    for prefix in USAGE_ROWS {
        rows.push((format!("{prefix}one"), prefix.as_bytes().to_vec(), None));
    }
    for key in [
        "hosts/binding/host-a",
        "hosts/join/digest",
        "acquires/scaler/records",
        "meta/server-state-imported",
    ] {
        rows.push((key.into(), b"not exported".to_vec(), None));
    }
    rows
}

/// Asserts that the export writes the store's job store rows, the `usage/ended` rows with
/// them, to the jobs file and its node, profile and tail rows to the usage file, each row
/// once and with its end kept, leaves out the host bindings, join tokens, scaler records and
/// import marker, makes both files owner-only, and reports each file's tally. It catches an
/// export that drops a prefix, one that puts the ended rows where a server without the
/// setting does not read them, and one that copies the server's other state into the files.
#[test]
fn the_export_writes_the_job_rows_and_the_usage_rows_to_two_new_files() {
    let dir = scratch("export");
    let (store, db) = sqlite_store(&dir);
    let given = store_rows();
    file_with_owned(&db, &given);
    let jobs_to = dir.join("back.jobs");
    let usage_to = dir.join("back-usage.db");
    let words: [&dyn AsRef<std::ffi::OsStr>; 7] = [
        &"export",
        &"--store",
        &store,
        &"--jobs-to",
        &jobs_to,
        &"--usage-to",
        &usage_to,
    ];
    let (code, out, err) = verb(&words);
    assert_eq!((code, err.as_str()), (status(0), ""));
    let under = |prefixes: &[&str]| -> Vec<(String, Vec<u8>, bool)> {
        let mut rows: Vec<(String, Vec<u8>, bool)> = given
            .iter()
            .filter(|(key, _, _)| prefixes.iter().any(|prefix| key.starts_with(prefix)))
            .map(|(key, value, ttl)| (key.clone(), value.clone(), ttl.is_some()))
            .collect();
        rows.sort();
        rows
    };
    let jobs = under(JOB_ROWS);
    assert_eq!(file_rows(&jobs_to), jobs);
    assert_eq!(file_rows(&usage_to), under(USAGE_ROWS));
    for file in [&jobs_to, &usage_to] {
        let mode = std::fs::metadata(file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", file.display());
    }
    let mut tally = Tally::default();
    jobs.iter()
        .for_each(|(key, value, _)| tally.add(key, value));
    let report = format!(
        "{}:\n{}\nwrote {} rows to {}\n",
        jobs_to.display(),
        tally.lines().join("\n"),
        jobs.len(),
        jobs_to.display()
    );
    assert!(out.starts_with(&report), "{out}");
    let usage_end = format!(
        "wrote {} rows to {}\n",
        USAGE_ROWS.len(),
        usage_to.display()
    );
    assert!(out.ends_with(&usage_end), "{out}");
}

/// Asserts that an export to a file that is there is refused naming it, leaves that file as it
/// was, and makes neither file, whichever of the two is there. It catches an export that
/// writes over a job store, and one that leaves a half made pair behind.
#[test]
fn an_export_to_a_file_that_is_there_is_refused_and_makes_neither() {
    let dir = scratch("there");
    let (store, db) = sqlite_store(&dir);
    file_with(&db, &[("job/a", b"one", None)]);
    let jobs_to = dir.join("back.jobs");
    let usage_to = dir.join("back-usage.db");
    for (there, other) in [(&jobs_to, &usage_to), (&usage_to, &jobs_to)] {
        std::fs::write(there, b"keep").unwrap();
        let words: [&dyn AsRef<std::ffi::OsStr>; 7] = [
            &"export",
            &"--store",
            &store,
            &"--jobs-to",
            &jobs_to,
            &"--usage-to",
            &usage_to,
        ];
        let (code, out, err) = verb(&words);
        assert_eq!((code, out.as_str()), (status(1), ""));
        let wanted = format!(
            "js-lease-server job-store: {}: the file is there: an export makes new files only\n",
            there.display()
        );
        assert_eq!(err, wanted);
        assert_eq!(std::fs::read(there).unwrap(), b"keep");
        assert!(!other.exists());
        std::fs::remove_file(there).unwrap();
    }
}

/// Asserts that an export naming one file for both is refused before the store is opened or
/// the file made.
#[test]
fn an_export_naming_one_file_twice_is_refused() {
    let dir = scratch("twice");
    let (store, db) = sqlite_store(&dir);
    let to = dir.join("back.jobs");
    let words: [&dyn AsRef<std::ffi::OsStr>; 7] = [
        &"export",
        &"--store",
        &store,
        &"--jobs-to",
        &to,
        &"--usage-to",
        &to,
    ];
    let (code, _, err) = verb(&words);
    assert_eq!(code, status(1));
    let wanted = format!(
        "js-lease-server job-store: {}: --jobs-to and --usage-to name one file\n",
        to.display()
    );
    assert_eq!(err, wanted);
    assert!(!to.exists() && !db.exists());
}

/// The job store's open of the file beside the state file `state`, as a server without a
/// store setting opens it.
fn jobs_beside(state: &Path) -> crate::job_store::Opened {
    let setup = JobStoreSetup {
        flag: "--job-listen",
        retention: Retention::new(Duration::from_secs(600)).unwrap(),
        feed_retention: FeedRetention::DEFAULT,
        allocation_retention: Retention::new(Duration::from_secs(1200)).unwrap(),
    };
    open_jobs((state, None), setup, &JobSources::default()).unwrap()
}

/// Copies the job store's file `from` into the store `store` names, exports the store back to
/// `jobs_to` and `usage_to`, and returns the copy's tally lines and the export's report.
fn round_trip(from: &Path, store: &Path, (jobs_to, usage_to): (&Path, &Path)) -> (String, String) {
    let (code, copied, err) = verb(&[&"copy", &"--from", &from, &"--store", &store, &"--write"]);
    assert_eq!((code, err.as_str()), (status(0), ""), "{copied}");
    let words: [&dyn AsRef<std::ffi::OsStr>; 7] = [
        &"export",
        &"--store",
        &store,
        &"--jobs-to",
        &jobs_to,
        &"--usage-to",
        &usage_to,
    ];
    let (code, exported, err) = verb(&words);
    assert_eq!((code, err.as_str()), (status(0), ""), "{exported}");
    (copied, exported)
}

/// The tally lines of a report: the lines between its first, the file's name, and the next
/// line that is not a tally's.
fn tally_lines(report: &str) -> Vec<&str> {
    report
        .lines()
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .collect()
}

/// Asserts that a job store written by the server's own job store (a queued job, its indexes,
/// its schema row), copied into a store and exported back,
/// gives a file that holds the same rows as the first, whose tally the export reports as the
/// copy did, and on which a server without a store setting opens its job store and reads the
/// job as it was. It catches a prefix the job store writes that the export leaves out, and an
/// exported file a server without the setting cannot run on.
#[test]
fn a_job_store_copied_into_the_store_and_exported_back_serves_its_jobs() {
    let dir = scratch("round_trip");
    let state = dir.join("leases.toml");
    let from = dir.join("leases.toml.jobs");
    let id = JobId::parse("job-kept").unwrap();
    {
        let opened = jobs_beside(&state);
        let new = NewJob {
            id: Some(id.clone()),
            spec: spec(),
            priority: 0,
            client: None,
        };
        opened.jobs.insert_queued(new).unwrap();
    }
    let (store, _) = sqlite_store(&dir);
    let jobs_to = dir.join("back.jobs");
    let (copied, exported) = round_trip(&from, &store, (&jobs_to, &dir.join("back-usage.db")));
    assert_eq!(file_rows(&jobs_to), file_rows(&from));
    assert_eq!(tally_lines(&exported), tally_lines(&copied));
    let back = dir.join("back.toml");
    std::fs::rename(&jobs_to, back.with_file_name("back.toml.jobs")).unwrap();
    let job = jobs_beside(&back).jobs.get_job(&id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
}

/// Asserts that a job store's file copied into a Postgres store and exported back holds the
/// same rows, the cordon with its end, and that the export's tally is the copy's. It catches
/// a write that Postgres refuses or changes (the time to live is kept in microseconds there).
#[test]
fn a_round_trip_through_postgres_keeps_every_row() {
    let database = test_database();
    let dir = scratch("postgres");
    let from = dir.join("leases.toml.jobs");
    file_with_owned(&from, &job_file_rows(rows::PAGE.get()));
    let store = store_file(&dir, &database.url());
    let jobs_to = dir.join("back.jobs");
    let (copied, exported) = round_trip(&from, &store, (&jobs_to, &dir.join("back-usage.db")));
    assert_eq!(file_rows(&jobs_to), file_rows(&from));
    assert!(file_rows(&jobs_to).contains(&("cordon/host-c".into(), b"lease".to_vec(), true)));
    assert_eq!(tally_lines(&exported), tally_lines(&copied));
}

/// Asserts that a key under none of the known prefixes is counted under its first segment, or
/// under itself when it has no `/`, so a copy reports every row of a file whatever it holds.
#[test]
fn a_key_under_no_known_prefix_is_counted_under_its_first_segment() {
    let mut tally = Tally::default();
    tally.add("other/x", b"1");
    tally.add("other/y", b"2");
    tally.add("loose", b"3");
    tally.add("job/a", b"one");
    let prefixes: Vec<String> = tally
        .lines()
        .iter()
        .map(|line| {
            line.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(prefixes, ["job/ rows=1", "loose rows=1", "other/ rows=2"]);
    assert_eq!(tally.rows(), 4);
}

/// Asserts that every key space the job store names, and the ended rows it writes beside an
/// allocation's free, is a prefix of the job rows, and that no usage prefix is one, so each
/// row of a store goes to one file. It catches a prefix dropped from the export's list.
#[test]
fn every_key_space_of_the_job_store_is_exported_with_the_job_rows() {
    use js_jobs::keys;
    let named = [
        keys::JOB,
        keys::ALLOCATION,
        keys::MARK,
        keys::TAINT,
        keys::CORDON,
        keys::HOSTMARK,
        keys::POOL,
        keys::HOSTBOOK,
        keys::PERIODIC,
        keys::DISPATCH,
        keys::CALLBACK,
        keys::NODE,
        keys::ATTEMPT,
        keys::CORDONS_RESTORED,
        keys::SCHEMA,
        "idx/job/active/",
        js_usage::ENDED_PREFIX,
        js_usage::ENDED_ID_PREFIX,
    ];
    for prefix in named {
        assert!(
            JOB_ROWS.iter().any(|row| prefix.starts_with(row)),
            "{prefix}"
        );
    }
    for usage in USAGE_ROWS {
        assert!(
            !JOB_ROWS
                .iter()
                .any(|row| usage.starts_with(row) || row.starts_with(usage)),
            "{usage}"
        );
    }
}

/// Asserts that the verb without a command is a usage error (status 2, the words on standard
/// error) and that `--help` prints the usage on standard output with status 0.
#[test]
fn the_verb_without_a_command_is_a_usage_error_and_its_help_goes_to_standard_output() {
    let (code, out, err) = verb(&[]);
    assert_eq!((code, out.as_str()), (status(2), ""));
    assert!(
        err.contains("Usage: js-lease-server job-store <COMMAND>"),
        "{err}"
    );
    let (code, out, err) = verb(&[&"--help"]);
    assert_eq!((code, err.as_str()), (status(0), ""));
    assert!(
        out.contains("Usage: js-lease-server job-store <COMMAND>"),
        "{out}"
    );
}

/// Asserts that a store that fails stops a plan, a write and an export with the store's own
/// reason. It catches a failure taken for an empty store, which would copy over rows a plan
/// never saw or export a file with nothing in it.
#[test]
fn a_store_that_fails_stops_the_copy_and_the_export_with_its_reason() {
    use js_jobs::interfere::INJECTED;
    let dir = scratch("failing");
    let from = dir.join("leases.toml.jobs");
    file_with(&from, &[("job/a", b"one", None)]);
    let source = sqlite(&from);
    let (store, _jobs) = tables();
    store.fail_read_after(0);
    let planned = rows::plan(&source, &*store).err().unwrap().to_string();
    assert!(planned.contains(INJECTED), "{planned}");
    store.fail_read_after(0);
    let wrote = rows::write(&source, &*store).err().unwrap().to_string();
    assert!(wrote.contains(INJECTED), "{wrote}");
    let file = sqlite(&dir.join("back.jobs"));
    store.fail_read_after(0);
    let exported = rows::export(&*store, JOB_ROWS, &file)
        .err()
        .unwrap()
        .to_string();
    assert!(exported.contains(INJECTED), "{exported}");
}

/// Asserts that a clock or a commit that fails stops a write and an export with the store's
/// own reason: the clock of the store read (a row's end is a time of that clock), and the
/// commit of the store written. It catches a failed commit taken for a written page, which
/// would report rows as copied that are not.
#[test]
fn a_failed_clock_or_commit_stops_the_write_and_the_export() {
    use js_jobs::interfere::INJECTED;
    let dir = scratch("failing_writes");
    let from = dir.join("leases.toml.jobs");
    file_with(&from, &[("job/a", b"one", None)]);
    let file = sqlite(&from);
    // The fake store holds the job store's schema row from its open.
    let (store, _jobs) = tables();
    let empty = sqlite(&dir.join("empty.jobs"));
    store.fail_clock(true);
    let clock = rows::write(&*store, &empty).err().unwrap().to_string();
    assert!(clock.contains(INJECTED), "{clock}");
    let back = sqlite(&dir.join("back.jobs"));
    let clock = rows::export(&*store, JOB_ROWS, &back)
        .err()
        .unwrap()
        .to_string();
    assert!(clock.contains(INJECTED), "{clock}");
    store.fail_clock(false);
    store.fail_next();
    let commit = rows::write(&file, &*store).err().unwrap().to_string();
    assert!(commit.contains(INJECTED), "{commit}");
    store.fail_next();
    let commit = rows::export(&file, JOB_ROWS, &*store)
        .err()
        .unwrap()
        .to_string();
    assert!(commit.contains(INJECTED), "{commit}");
    assert_eq!(store.get("job/a").unwrap(), None);
}

/// Asserts that a refusal of the rows reads to the operator as the rows' own words, after the
/// verb's name. It catches a refusal that loses the key it names.
#[test]
fn a_refusal_of_the_rows_says_the_rows_reason() {
    let refusal = super::Refusal::from(rows::RowsError::Raced("job/a".into()));
    assert_eq!(
        refusal.to_string(),
        "\"job/a\" was written by another process after it was read as missing: the rows \
         before it are written; run the dry run again"
    );
}

/// Asserts that a store file that is owner-only but does not name a store the server accepts
/// is refused naming `--store`, for both commands, and that the export makes neither file. It
/// catches a verb that opens some other store than the server's own `--store` would.
#[test]
fn a_store_file_that_names_no_store_is_refused_naming_the_flag() {
    let dir = scratch("no_store");
    let from = dir.join("leases.toml.jobs");
    file_with(&from, &[("job/a", b"one", None)]);
    let store = store_file(&dir, "mysql://x");
    let (code, _, err) = verb(&[&"copy", &"--from", &from, &"--store", &store]);
    assert_eq!(code, status(1));
    assert!(
        err.starts_with("js-lease-server job-store: --store: "),
        "{err}"
    );
    let jobs_to = dir.join("back.jobs");
    let usage_to = dir.join("back-usage.db");
    let words: [&dyn AsRef<std::ffi::OsStr>; 7] = [
        &"export",
        &"--store",
        &store,
        &"--jobs-to",
        &jobs_to,
        &"--usage-to",
        &usage_to,
    ];
    let (code, _, err) = verb(&words);
    assert_eq!(code, status(1));
    assert!(
        err.starts_with("js-lease-server job-store: --store: "),
        "{err}"
    );
    assert!(!jobs_to.exists() && !usage_to.exists());
}

/// Asserts that an export to a file that cannot be made (its directory is not there) is
/// refused naming that file, before any row is written.
#[test]
fn an_export_to_a_file_that_cannot_be_made_is_refused_naming_it() {
    let dir = scratch("unmade");
    let (store, db) = sqlite_store(&dir);
    file_with(&db, &[("job/a", b"one", None)]);
    let jobs_to = dir.join("no-such-dir").join("back.jobs");
    let usage_to = dir.join("back-usage.db");
    let words: [&dyn AsRef<std::ffi::OsStr>; 7] = [
        &"export",
        &"--store",
        &store,
        &"--jobs-to",
        &jobs_to,
        &"--usage-to",
        &usage_to,
    ];
    let (code, out, err) = verb(&words);
    assert_eq!((code, out.as_str()), (status(1), ""));
    let named = format!("js-lease-server job-store: {}: ", jobs_to.display());
    assert!(err.starts_with(&named), "{err}");
    assert!(!usage_to.exists());
}
