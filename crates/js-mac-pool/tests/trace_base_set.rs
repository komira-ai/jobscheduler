//! The trace that derives the read base set of a macOS major version that has none yet.
//!
//! One `#[ignore]` test, run as root on a provisioned Mac through the
//! `js-mac-pool-privileged-tests` workflow's binary (the variables are those of
//! [`common::priv_root`]): `./trace_base_set --ignored --test-threads=1 --nocapture trace`.
//! It runs under the macOS 15 base set, the nearest one, whatever the host's major
//! version is: the profile comes from [`SelfTest::new`] with the major given, so no
//! production code knows the host's version.
//!
//! The workloads are the self-test's allowed steps (`true`, a granted-tree read, a
//! compile and an ad-hoc `codesign` with the Xcode of the host), a bundle entrypoint and a
//! read through the time zone link, as the pool user; and the self-test's denied reads.
//! Each runs once with the whole set and once with each line of the set removed in turn
//! (the ablation). The allowed workloads then run again under `fs_usage`, which lists the
//! paths they touch.
//!
//! It prints a table (format in `common/trace.rs`) and writes it to
//! `<work root>/trace-table.tsv`, with the paths each workload touched in
//! `<work root>/trace-touches.tsv`. It asserts nothing about the host: a workload that
//! fails is a row, not a failure. It writes only below the work root and makes the two
//! canary files the fixture makes, which it removes.

mod common;

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use common::mac::Mac;
use common::priv_root::{fixture, ids, work_root};
use common::run;
use common::trace::{
    Row, Touch, Workload, ablate, begin_marker, end_marker, mark_touched, parse_fs_usage, render,
    retarget, setuid_rows, windows,
};
use js_bundle::ReadGrantKind;
use js_mac_pool::seatbelt::{Class, MacosMajor, ReadGrant, Role, SelfTest, scan_privileged};

/// How many times the allowed workloads run under `fs_usage`. The tracer is not known to be
/// attached when the first run starts; a window it missed is dropped, and a later run
/// supplies it.
const CAPTURE_PASSES: usize = 3;

fn tool(program: &str, args: &[&str]) -> String {
    let out = Command::new(program).args(args).output();
    match out {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Err(error) => format!("unavailable: {error}"),
    }
}

/// A file made new, below the work root, with the table.
fn write_new(path: &Path, text: &str) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .unwrap_or_else(|error| panic!("cannot make {}: {error}", path.display()));
    file.write_all(text.as_bytes()).expect("write");
}

/// The allowed and denied steps of the self-test as workloads, and the two more the
/// self-test does not have.
fn workloads(test: &SelfTest, mac: &Mac) -> Vec<Workload> {
    let mut all = Vec::new();
    for step in test.steps() {
        let allowed = match step.role() {
            Role::Control => continue,
            Role::Allowed => true,
            Role::Denied => false,
        };
        let name = if allowed {
            step.class().to_string()
        } else {
            format!("denied:{}", step.class())
        };
        all.push(Workload {
            name,
            allowed,
            argv: step.argv(),
            env: step.env().to_vec(),
            cwd: step.cwd().to_owned(),
        });
    }
    let sandboxed = test
        .steps()
        .iter()
        .find(|step| step.class() == Class::RunTrue)
        .expect("the self-test runs true")
        .argv();
    let cwd = mac.run.to_str().expect("text").to_owned();
    let entrypoint = mac.pinned_file("bin/read");
    let data = mac.pinned_file("data.txt").into_os_string();
    let extra = [
        (
            "bundle-entrypoint",
            entrypoint.to_str().expect("text"),
            vec![data],
        ),
        (
            "time-zone-link",
            "/bin/cat",
            vec!["/private/etc/localtime".into()],
        ),
    ];
    for (name, program, args) in extra {
        all.push(Workload {
            name: name.to_owned(),
            allowed: true,
            argv: retarget(&sandboxed, program, &args).expect("a sandboxed argv"),
            env: Vec::new(),
            cwd: cwd.clone(),
        });
    }
    all
}

/// Runs the allowed workloads under `fs_usage` and returns what it printed, or nothing when
/// it could not be started.
fn capture(allowed: &[&Workload], ids: (u32, u32), raw: &Path) -> String {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(raw)
        .unwrap_or_else(|error| panic!("cannot make {}: {error}", raw.display()));
    let Ok(mut tracer) = Command::new("/usr/bin/fs_usage")
        .args(["-w", "-f", "pathname"])
        .stdout(file)
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    for _ in 0..CAPTURE_PASSES {
        for work in allowed {
            // The markers are paths that do not exist: the tracer sees them being looked up.
            let _ = std::fs::metadata(begin_marker(&work.name));
            run(&work.argv, &work.env, Some(&work.cwd), Some(ids));
            let _ = std::fs::metadata(end_marker(&work.name));
        }
    }
    // SIGINT makes it flush what it buffered; it is our own child.
    let _ = Command::new("/bin/kill")
        .args(["-INT", &tracer.id().to_string()])
        .status();
    let _ = tracer.wait();
    std::fs::read(raw)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn touches_table(seen: &std::collections::BTreeMap<String, Vec<Touch>>) -> String {
    let mut out = String::from("workload\tprocess\tpath\n");
    for (name, inside) in seen {
        for touch in inside {
            out.push_str(&format!("{name}\t{}\t{}\n", touch.process, touch.path));
        }
    }
    out
}

#[test]
#[ignore = "needs root on a provisioned Mac (js-mac-pool-privileged-tests); prints the table"]
fn trace_the_base_set_of_this_host_under_the_macos_15_set() {
    let ids = ids();
    let root = work_root();
    let mac = fixture("trace-base-set");
    let traced = MacosMajor::Macos15;
    let mut fixture = mac.fixture(scan_privileged(&[]));
    fixture
        .grants
        .push(ReadGrant::Kind(ReadGrantKind::Timezone));
    let test = SelfTest::new(traced, fixture).expect("steps");
    let all = workloads(&test, &mac);

    // The compile's output must exist for `codesign` to sign, whatever the ablation does.
    if let Some(step) = test.steps().iter().find(|s| s.class() == Class::Compile) {
        let (program, args) = step.command();
        let mut argv = vec![program.into()];
        argv.extend(args.iter().cloned());
        run(&argv, step.env(), Some(step.cwd()), Some(ids));
    }

    let mut rows: Vec<Row> = Vec::new();
    for step in test.steps().iter().filter(|s| s.role() == Role::Control) {
        let ran = run(&step.argv(), step.env(), Some(step.cwd()), Some(ids));
        rows.push(Row::baseline(
            &format!("control:{}", step.class()),
            ran.outcome(),
        ));
    }
    let mut runner = |work: &Workload, argv: &[std::ffi::OsString]| {
        run(argv, &work.env, Some(&work.cwd), Some(ids)).outcome()
    };
    rows.extend(ablate(traced.base(), &all, &mut runner).expect("the ablation"));

    let allowed: Vec<&Workload> = all.iter().filter(|work| work.allowed).collect();
    let raw = root.join("fs_usage-raw.txt");
    let output = capture(&allowed, ids, &raw);
    let seen = windows(&parse_fs_usage(&output));
    let expected: Vec<String> = allowed.iter().map(|work| work.name.clone()).collect();
    mark_touched(&mut rows, traced.base(), &seen, &expected);

    let roots: Vec<&str> = MacosMajor::ALL
        .iter()
        .flat_map(|major| major.base().reads().iter())
        .filter(|(how, _)| *how == js_mac_pool::seatbelt::Match::Subpath)
        .map(|(_, path)| *path)
        .collect();
    let found = scan_privileged(&roots);
    rows.extend(setuid_rows(found.paths(), traced.base().setuid()));

    let meta = [
        ("tool", "js-mac-pool trace_base_set".to_owned()),
        ("traced-set", "macos15".to_owned()),
        (
            "product-version",
            tool("/usr/bin/sw_vers", &["-productVersion"]),
        ),
        (
            "build-version",
            tool("/usr/bin/sw_vers", &["-buildVersion"]),
        ),
        ("arch", tool("/usr/bin/uname", &["-m"])),
        (
            "xcode",
            tool("/usr/bin/xcodebuild", &["-version"]).replace('\n', " "),
        ),
        ("fs-usage-windows", seen.len().to_string()),
    ]
    .map(|(key, value)| (key.to_owned(), value));
    let table = render(&meta, &rows);
    println!("{table}");
    write_new(&root.join("trace-table.tsv"), &table);
    write_new(&root.join("trace-touches.tsv"), &touches_table(&seen));
    println!("table: {}", root.join("trace-table.tsv").display());
}
