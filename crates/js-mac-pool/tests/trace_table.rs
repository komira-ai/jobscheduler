//! The pure half of the base-set trace: the table format, the removal of one base-set
//! line from a generated profile, the ablation loop over a fake runner, and the reading
//! of `fs_usage` output. The host half is `trace_base_set.rs`.

mod common;

use std::ffi::OsString;

use common::trace::{
    Kind, Line, Mark, Row, TraceError, Workload, ablate, begin_marker, covers, end_marker, lines,
    mark_touched, parse, parse_fs_usage, profile_of, render, retarget, setuid_rows, windows,
    with_profile, without,
};
use js_mac_pool::seatbelt::{Class, MacosMajor, Match, Outcome, Role, SelfTest};

/// A step's argv under the macOS 15 profile of the fixture without grants.
fn sandboxed(class: Class) -> Vec<OsString> {
    let test = SelfTest::new(MacosMajor::Macos15, common::fixture(Vec::new())).expect("steps");
    test.steps()
        .iter()
        .find(|step| step.class() == class && step.role() == Role::Allowed)
        .expect("the step")
        .argv()
}

fn profile() -> String {
    profile_of(&sandboxed(Class::RunTrue)).expect("a profile")
}

fn base() -> &'static js_mac_pool::seatbelt::BaseSet {
    MacosMajor::Macos15.base()
}

fn line(kind: Kind, how: Match, path: &str) -> Line {
    Line {
        kind,
        how,
        path: path.to_owned(),
    }
}

#[test]
fn a_table_reads_back_as_it_was_written() {
    let mut needed = Row::new(Kind::Read, "subpath /usr/lib");
    needed.touched = Mark::Yes;
    needed.needed = Mark::Yes;
    needed.workloads = vec!["compile".to_owned(), "run-true".to_owned()];
    needed.touched_by = vec!["compile/clang".to_owned()];
    let mut changed = Row::new(Kind::Name, "literal /etc");
    changed.touched = Mark::Unknown;
    changed.needed = Mark::No;
    changed.denial_changed = vec!["denied:read-keychain".to_owned()];
    let rows = vec![
        Row::baseline("run-true", Outcome::Success),
        Row::baseline("denied:read-keychain", Outcome::Refused),
        needed,
        changed,
        Row::new(Kind::Setuid, "/usr/bin/su"),
        Row::new(Kind::SetuidNew, "/usr/bin/new"),
    ];
    let meta = vec![("macos".to_owned(), "27.0.1".to_owned())];
    let text = render(&meta, &rows);
    assert!(text.starts_with("# macos=27.0.1\nkind\tline\t"));
    assert_eq!(parse(&text), Ok(rows));
}

#[test]
fn a_row_that_is_not_in_the_table_format_is_refused() {
    assert_eq!(
        parse("read\tonly\tfour\tcolumns"),
        Err(TraceError::BadRow("read\tonly\tfour\tcolumns".to_owned()))
    );
    let unknown = "bogus\tx\tyes\tyes\t-\t-\t-";
    assert_eq!(parse(unknown), Err(TraceError::BadRow(unknown.to_owned())));
}

#[test]
fn taking_out_a_line_leaves_the_rest_of_the_profile_as_it_was() {
    let text = profile();
    let all = lines(base());
    assert_eq!(all.len(), base().names().len() + base().reads().len());
    for line in &all {
        let cut = without(&text, base(), line).expect("the line is in the profile");
        assert_eq!(
            cut.lines().count(),
            text.lines().count(),
            "{}",
            line.label()
        );
        let before: Vec<&str> = text.lines().collect();
        let after: Vec<&str> = cut.lines().collect();
        let changed: Vec<usize> = (0..before.len())
            .filter(|i| before[*i] != after[*i])
            .collect();
        assert_eq!(changed.len(), 1, "{}: one rule changes", line.label());
        let rule = after[changed[0]];
        assert!(
            !rule.contains(&format!(
                "({} \"{}\")",
                match line.how {
                    Match::Literal => "literal",
                    Match::Subpath => "subpath",
                },
                line.path
            )),
            "{}: {rule}",
            line.label()
        );
    }
}

#[test]
fn taking_out_a_device_file_keeps_the_write_grant_of_dev_null() {
    let cut = without(
        &profile(),
        base(),
        &line(Kind::Read, Match::Literal, "/dev/null"),
    )
    .expect("cut");
    let write = cut
        .lines()
        .find(|rule| rule.starts_with("(allow file-write*"))
        .expect("the write grant");
    assert!(write.contains("(literal \"/dev/null\")"));
    let read = cut
        .lines()
        .find(|rule| rule.starts_with("(allow file-read* (literal \"/\")"))
        .expect("the reads");
    assert!(!read.contains("/dev/null"));
    assert!(read.contains("(literal \"/dev/zero\")"));
}

#[test]
fn a_profile_without_the_rule_of_the_base_set_is_refused() {
    let other = "(version 1)\n(allow default)\n";
    let result = without(other, base(), &line(Kind::Read, Match::Subpath, "/bin"));
    assert!(matches!(result, Err(TraceError::LineNotFound(_))));
}

#[test]
fn a_sandboxed_argv_can_swap_its_profile_or_its_program() {
    let argv = sandboxed(Class::RunTrue);
    let swapped = with_profile(&argv, "(version 1)\n").expect("swapped");
    assert_eq!(profile_of(&swapped).as_deref(), Some("(version 1)\n"));
    assert_eq!(swapped.len(), argv.len());
    let program = retarget(&argv, "/bin/cat", &[OsString::from("/etc/localtime")]).expect("cat");
    assert_eq!(program[program.len() - 2], OsString::from("/bin/cat"));
    assert_eq!(program[program.len() - 1], OsString::from("/etc/localtime"));
    assert_eq!(profile_of(&program), profile_of(&argv));
    let plain = vec![OsString::from("/usr/bin/true")];
    assert_eq!(profile_of(&plain), None);
    assert_eq!(retarget(&plain, "/bin/cat", &[]), None);
    assert_eq!(with_profile(&plain, "x"), None);
}

fn workload(name: &str, allowed: bool) -> Workload {
    Workload {
        name: name.to_owned(),
        allowed,
        argv: sandboxed(Class::RunTrue),
        env: Vec::new(),
        cwd: "/".to_owned(),
    }
}

/// A runner that fails an allowed workload without the lines it names, and a denied one
/// that stops being refused when a line is missing.
fn fake<'a>(
    needs: &'a [(&'a str, &'a str)],
    flips: &'a [(&'a str, &'a str)],
) -> impl FnMut(&Workload, &[OsString]) -> Outcome + 'a {
    move |work, argv| {
        let text = profile_of(argv).expect("a profile");
        let has = |path: &str| text.contains(&format!("\"{path}\""));
        let lacks = |table: &[(&str, &str)]| {
            table
                .iter()
                .any(|(name, path)| *name == work.name && !has(path))
        };
        if work.allowed && lacks(needs) {
            return Outcome::Failed;
        }
        if !work.allowed && lacks(flips) {
            return Outcome::Success;
        }
        if work.allowed {
            Outcome::Success
        } else {
            Outcome::Refused
        }
    }
}

#[test]
fn only_the_line_a_workload_fails_without_is_needed_and_names_that_workload() {
    let workloads = vec![
        workload("run-true", true),
        workload("compile", true),
        workload("denied:read-keychain", false),
    ];
    let needs = [("compile", "/System/Volumes/Preboot/Cryptexes")];
    let rows = ablate(base(), &workloads, &mut fake(&needs, &[])).expect("rows");
    let baselines: Vec<&Row> = rows.iter().filter(|r| r.kind == Kind::Baseline).collect();
    assert_eq!(baselines.len(), 3);
    assert_eq!(baselines[2].workloads, ["refused"]);
    let table: Vec<&Row> = rows.iter().filter(|r| r.kind != Kind::Baseline).collect();
    assert_eq!(table.len(), lines(base()).len());
    for row in table {
        if row.line == "subpath /System/Volumes/Preboot/Cryptexes" {
            assert_eq!(row.needed, Mark::Yes);
            assert_eq!(row.workloads, ["compile"]);
        } else {
            assert_eq!(row.needed, Mark::No, "{}", row.line);
            assert!(row.workloads.is_empty(), "{}", row.line);
        }
        assert!(row.denial_changed.is_empty(), "{}", row.line);
    }
}

#[test]
fn a_denied_read_that_changes_without_a_line_is_listed_not_counted_as_need() {
    let workloads = vec![
        workload("run-true", true),
        workload("denied:read-keychain", false),
    ];
    let flips = [("denied:read-keychain", "/bin")];
    let rows = ablate(base(), &workloads, &mut fake(&[], &flips)).expect("rows");
    let bin = rows.iter().find(|r| r.line == "subpath /bin").expect("row");
    assert_eq!(bin.needed, Mark::No);
    assert_eq!(bin.denial_changed, ["denied:read-keychain"]);
}

#[test]
fn a_workload_that_fails_with_the_whole_set_is_blamed_on_no_line() {
    let workloads = vec![workload("compile", true)];
    let mut always_fails = |_: &Workload, _: &[OsString]| Outcome::Failed;
    let rows = ablate(base(), &workloads, &mut always_fails).expect("rows");
    assert_eq!(rows[0].workloads, ["failed"]);
    assert!(rows.iter().skip(1).all(|row| row.needed == Mark::No));
}

#[test]
fn a_workload_without_a_profile_stops_the_ablation() {
    let mut plain = workload("run-true", true);
    plain.argv = vec![OsString::from("/usr/bin/true")];
    let mut ok = |_: &Workload, _: &[OsString]| Outcome::Success;
    let result = ablate(base(), &[plain], &mut ok);
    assert_eq!(result, Err(TraceError::NoProfile("run-true".to_owned())));
}

#[test]
fn fs_usage_lines_give_the_path_and_the_process() {
    let text = "\
12:00:01.000001  open              F=4    (R_____)  /usr/lib/dyld                       0.000020   true.1234
12:00:01.000002  stat64                             /var/db/timezone/zoneinfo/UTC       0.000010 W cat.99
12:00:01.000003  getattrlist                        /Users/Shared/some dir/file x       0.000010   cat.99
12:00:01.000004  fsgetpath                                                               0.000010   mds.5
";
    let touches = parse_fs_usage(text);
    let shown: Vec<(&str, &str)> = touches
        .iter()
        .map(|t| (t.path.as_str(), t.process.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("/usr/lib/dyld", "true"),
            ("/var/db/timezone/zoneinfo/UTC", "cat"),
            ("/Users/Shared/some dir/file x", "cat"),
        ]
    );
}

fn touch(path: &str, process: &str) -> common::trace::Touch {
    common::trace::Touch {
        path: path.to_owned(),
        process: process.to_owned(),
    }
}

#[test]
fn a_window_is_what_lies_between_its_markers_and_only_when_both_were_seen() {
    let touches = vec![
        touch("/noise", "mds"),
        touch(&begin_marker("run-true"), "tracer"),
        touch("/usr/lib/dyld", "true"),
        touch(&end_marker("run-true"), "tracer"),
        touch("/noise2", "mds"),
        touch(&begin_marker("compile"), "tracer"),
        touch("/usr/bin/cc", "cc"),
        touch(&begin_marker("codesign"), "tracer"),
        touch("/usr/bin/codesign", "codesign"),
        touch(&end_marker("codesign"), "tracer"),
        touch(&begin_marker("run-true"), "tracer"),
        touch("/bin/sh", "true"),
        touch(&end_marker("run-true"), "tracer"),
    ];
    let seen = windows(&touches);
    let names: Vec<&String> = seen.keys().collect();
    assert_eq!(names, ["codesign", "run-true"]);
    let paths: Vec<&str> = seen["run-true"].iter().map(|t| t.path.as_str()).collect();
    assert_eq!(paths, ["/usr/lib/dyld", "/bin/sh"]);
}

#[test]
fn a_line_covers_the_paths_it_grants_and_a_link_spelling_of_them() {
    let name = line(Kind::Name, Match::Literal, "/var");
    assert!(covers(&name, "/var/db/x"));
    assert!(covers(&name, "/var"));
    assert!(!covers(&name, "/private/var/db/x"));
    assert!(!covers(&name, "/variable"));
    let tz = line(Kind::Read, Match::Subpath, "/private/var/db/timezone");
    assert!(covers(&tz, "/var/db/timezone/zoneinfo/UTC"));
    assert!(covers(&tz, "/private/var/db/timezone"));
    assert!(!covers(&tz, "/private/var/db/timezoned"));
    let file = line(Kind::Read, Match::Literal, "/dev/null");
    assert!(covers(&file, "/dev/null"));
    assert!(!covers(&file, "/dev/nullx"));
    let root = line(Kind::Read, Match::Literal, "/");
    assert!(covers(&root, "/anything/at/all"));
}

#[test]
fn touched_is_unknown_for_an_untouched_line_when_a_workload_has_no_window() {
    let all = lines(base());
    let mut rows: Vec<Row> = all.iter().map(|l| Row::new(l.kind, &l.label())).collect();
    let touches = vec![
        touch(&begin_marker("run-true"), "tracer"),
        touch("/usr/lib/dyld", "true"),
        touch(&end_marker("run-true"), "tracer"),
    ];
    let seen = windows(&touches);
    mark_touched(&mut rows, base(), &seen, &["run-true".to_owned()]);
    let lib = rows
        .iter()
        .find(|r| r.line == "subpath /usr/lib")
        .expect("row");
    assert_eq!(lib.touched, Mark::Yes);
    assert_eq!(lib.touched_by, ["run-true/true"]);
    let usr = rows.iter().find(|r| r.line == "literal /usr").expect("row");
    assert_eq!(usr.kind, Kind::Name);
    assert_eq!(usr.touched, Mark::Yes);
    let bin = rows.iter().find(|r| r.line == "subpath /bin").expect("row");
    assert_eq!(bin.touched, Mark::No);
    assert!(bin.touched_by.is_empty());

    let both = ["run-true".to_owned(), "compile".to_owned()];
    mark_touched(&mut rows, base(), &seen, &both);
    let bin = rows.iter().find(|r| r.line == "subpath /bin").expect("row");
    assert_eq!(bin.touched, Mark::Unknown);
}

#[test]
fn setuid_files_are_split_into_the_listed_and_the_new() {
    let found = vec!["/usr/bin/su".to_owned(), "/usr/bin/brand-new".to_owned()];
    let rows = setuid_rows(&found, &["/usr/bin/su"]);
    assert_eq!(rows[0].kind, Kind::Setuid);
    assert_eq!(rows[1].kind, Kind::SetuidNew);
    assert_eq!(rows[1].line, "/usr/bin/brand-new");
}
