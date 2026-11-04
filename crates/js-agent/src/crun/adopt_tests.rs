//! The checks that decide whether a lease of an earlier process is adopted, over sidecars
//! built in memory and directories made for the test (the whole pass, over a stand-in
//! `crun`, is `tests/crun_stand_in/adopt.rs`).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::adopt::{agree, fields_of, in_cgroup, is_dir, named_in, report_lines, scratch_fits};
use super::ledger::{Booked, Entry, Failed, Reason, Removed, Supervisor, SweepReport};
use crate::cli_runtime::container_name;
use crate::lifecycle::label;

/// The sidecar of a run of `agent-a` that runs, with every field present and the labels a
/// job start writes.
fn running(run: &str) -> Entry {
    let name = container_name("agent-a", run).expect("a lease name");
    let labels: BTreeMap<String, String> = [
        (label::RUN, run),
        (label::LEASE, "lease-1"),
        (label::WALL_MS, "60000"),
        (label::CPU_MILLIS, "1500"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::DISK_BYTES, "2048"),
        (label::GPUS, "0,1"),
        (label::SLOT, "2"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    let mut entry = Entry::new("agent-a", run, &name, &labels);
    entry.allocation = Some("lease-1".to_owned());
    entry.booked = Some(Booked {
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        disk_bytes: Some(2048),
        gpus: vec![0, 1],
        slot: 2,
    });
    entry.started_wall_ms = Some(1_000);
    entry.wall_ms = Some(60_000);
    entry.lease_dir = Some(PathBuf::from("/leases").join(&name));
    entry.cgroup = Some(PathBuf::from("/cgroup").join(&name));
    entry.scratch = Some(PathBuf::from("/runs").join(run));
    entry.exit_record = Some(PathBuf::from("/leases").join(&name).join("exit.json"));
    entry.image_digest = Some(format!("sha256:{}", "ab".repeat(32)));
    entry.supervisor = Some(Supervisor { pid: 7, start: 99 });
    entry
}

/// Catches an adoption that goes on with a field of the sidecar missing (a part of the
/// record that was never written would be read as a default): a complete sidecar gives its
/// fields, and each field left out is refused with its name.
#[test]
fn a_running_sidecar_with_a_field_missing_is_refused_naming_it() {
    let entry = running("r1");
    fields_of(&entry).expect("every field is there");
    let breaks: [(&str, fn(&mut Entry)); 10] = [
        ("lease id", |entry| entry.allocation = None),
        ("booking", |entry| entry.booked = None),
        ("start", |entry| entry.started_wall_ms = None),
        ("wall-time limit", |entry| entry.wall_ms = None),
        ("lease directory", |entry| entry.lease_dir = None),
        ("cgroup", |entry| entry.cgroup = None),
        ("scratch directory", |entry| entry.scratch = None),
        ("exit record", |entry| entry.exit_record = None),
        ("image", |entry| entry.image_digest = None),
        ("supervisor", |entry| entry.supervisor = None),
    ];
    for (what, break_it) in breaks {
        let mut entry = running("r1");
        break_it(&mut entry);
        let why = fields_of(&entry).expect_err(what);
        assert_eq!(why, format!("its {what} is not recorded"));
    }
}

/// Catches an adoption of a run whose labels are not the ones its record says it was made
/// with (the lifecycle books a run by its labels): the labels of a good record agree with it,
/// and each label changed, or left out, is refused with its key; a disk booking and a disk
/// label must both be there or both be absent.
#[test]
fn labels_that_disagree_with_the_record_are_refused_naming_the_key() {
    let entry = running("r1");
    let fields = fields_of(&entry).expect("fields");
    assert_eq!(agree(&entry, &fields), Ok(()));

    let keys = [
        label::RUN,
        label::LEASE,
        label::AGENT,
        label::WALL_MS,
        label::CPU_MILLIS,
        label::MEMORY_BYTES,
        label::DISK_BYTES,
        label::GPUS,
        label::SLOT,
    ];
    for key in keys {
        for changed in [true, false] {
            let mut entry = running("r1");
            if changed {
                entry.labels.insert(key.to_owned(), "other".to_owned());
            } else {
                entry.labels.remove(key);
            }
            let fields = fields_of(&entry).expect("fields");
            let why = agree(&entry, &fields).expect_err(key);
            assert_eq!(why, format!("the label {key} disagrees with the record"));
        }
    }

    let mut entry = running("r1");
    entry.booked.as_mut().expect("a booking").disk_bytes = None;
    let fields = fields_of(&entry).expect("fields");
    let why = agree(&entry, &fields).expect_err("a label for no booking");
    assert!(why.contains(label::DISK_BYTES), "{why}");
    entry.labels.remove(label::DISK_BYTES);
    let fields = fields_of(&entry).expect("fields");
    assert_eq!(agree(&entry, &fields), Ok(()), "neither is there");
}

/// Catches a container whose init is not in the lease's cgroup being taken for the lease's
/// (the pid `crun state` names could be another process): the pid is found in the
/// `cgroup.procs` of the cgroup itself or of one below it, however deep, and not found in a
/// cgroup that lists others, lists nothing, has no file, or has a link where a child is.
#[test]
fn a_process_is_found_in_a_cgroup_or_below_it_and_nowhere_else() {
    let dir = TestDir::under(std::env::temp_dir(), "adopt-procs");
    let at = |parts: &str| dir.path().join(parts);
    for (cgroup, procs) in [
        ("top", "1\n2\n"),
        ("deep/a/b", "40\n41\n"),
        ("deep/a", ""),
        ("other", "9\n"),
    ] {
        fs::create_dir_all(at(cgroup)).expect("a cgroup");
        fs::write(at(cgroup).join("cgroup.procs"), procs).expect("its processes");
    }
    fs::create_dir_all(at("bare")).expect("a cgroup with no file");
    fs::write(at("top/not-a-cgroup"), "41\n").expect("a file beside the children");
    symlink(at("deep"), at("other/link")).expect("a link to a cgroup");

    assert!(in_cgroup(&at("top"), 2));
    assert!(!in_cgroup(&at("top"), 3));
    assert!(in_cgroup(&at("deep"), 41));
    assert!(!in_cgroup(&at("deep"), 9));
    assert!(!in_cgroup(&at("bare"), 1));
    assert!(!in_cgroup(&at("missing"), 1));
    assert!(!in_cgroup(&at("other"), 40), "a link is not followed");
}

/// Catches a directory check that follows a link (a lease directory replaced by a link
/// would be adopted): a directory is one, a link to it, a file and a missing path are not.
#[test]
fn only_a_real_directory_is_a_directory() {
    let dir = TestDir::under(std::env::temp_dir(), "adopt-dirs");
    let at = |name: &str| dir.path().join(name);
    fs::create_dir(at("real")).expect("dir");
    fs::write(at("file"), "x").expect("file");
    symlink(at("real"), at("link")).expect("link");
    assert!(is_dir(&at("real")));
    for other in ["file", "link", "missing"] {
        assert!(!is_dir(&at(other)), "{other}");
    }
}

/// Catches a scan for the leases nothing records that takes names it should not: only the
/// entries that start with the marker give a run id (what follows it), whether they are
/// directories or not, and a directory that cannot be listed gives none.
#[test]
fn the_names_a_scan_gives_are_the_entries_with_the_marker_less_the_marker() {
    let dir = TestDir::under(std::env::temp_dir(), "adopt-named");
    for name in [".lease-js-x-r1", ".lease-js-x-r2", "js-x-r3", "other"] {
        fs::create_dir(dir.path().join(name)).expect("an entry");
    }
    let runs: Vec<_> = named_in(dir.path(), ".lease-js-x-").into_iter().collect();
    assert_eq!(runs, ["r1", "r2"]);
    assert!(named_in(&dir.path().join("none"), "js-").is_empty());
}

/// Catches reasons that read alike or say nothing: each reason of a removal has its own text.
#[test]
fn each_reason_for_a_removal_reads_differently() {
    let texts = [
        Reason::OldForm.to_string(),
        Reason::NeverRan.to_string(),
        Reason::Inconsistent("the cgroup is gone".to_owned()).to_string(),
        Reason::NoRecord.to_string(),
    ];
    assert!(texts[2].contains("the cgroup is gone"), "{texts:?}");
    for (at, text) in texts.iter().enumerate() {
        for (other, same) in texts.iter().enumerate() {
            assert!(at == other || text != same, "{texts:?}");
        }
    }
}

/// Catches a start that says nothing of what it did to the leases it found: one line for each
/// run adopted, each lease removed with its reason and each lease that could not be removed,
/// and no line for a pass that did nothing. The lines name no host path.
#[test]
fn the_pass_over_the_ledger_is_told_in_one_line_for_each_lease() {
    assert!(report_lines(&SweepReport::default()).is_empty());
    let report = SweepReport {
        adopted: vec!["r1".to_owned()],
        removed: vec![Removed {
            run_id: "r2".to_owned(),
            name: "js-x-r2".to_owned(),
            labels: BTreeMap::new(),
            reason: Reason::NeverRan,
        }],
        failed: vec![Failed {
            run_id: "r3".to_owned(),
            name: "js-x-r3".to_owned(),
            why: "busy".to_owned(),
        }],
        left: Vec::new(),
    };
    assert_eq!(
        report_lines(&report),
        [
            "crun: adopted job r1".to_owned(),
            format!("crun: removed the lease of job r2: {}", Reason::NeverRan),
            "crun: could not remove the lease of job r3: busy".to_owned(),
        ]
    );
}

/// Catches a run adopted on a scratch directory that is not its own (the job would be handed
/// another run's files): the directory must be named for the run, and, when the leases are
/// made beside the runs, be in the same directory as the lease.
#[test]
fn a_scratch_directory_is_the_runs_own_by_name_and_by_place() {
    let runs = Path::new("/runs");
    assert!(scratch_fits(Path::new("/runs/r1"), "r1", None));
    assert!(scratch_fits(Path::new("/elsewhere/r1"), "r1", None));
    assert!(!scratch_fits(Path::new("/runs/r2"), "r1", None));
    assert!(scratch_fits(Path::new("/runs/r1"), "r1", Some(runs)));
    assert!(!scratch_fits(Path::new("/elsewhere/r1"), "r1", Some(runs)));
    assert!(!scratch_fits(Path::new("/runs/r2"), "r1", Some(runs)));
}
