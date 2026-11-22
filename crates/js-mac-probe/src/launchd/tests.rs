use super::*;

#[test]
fn the_cases_are_each_stop_without_then_with_abandon_process_group() {
    let names: Vec<String> = Case::all().into_iter().map(Case::name).collect();
    assert_eq!(
        names,
        [
            "bootout",
            "kickstart-k",
            "kill-9",
            "bootout-abandon",
            "kickstart-k-abandon",
            "kill-9-abandon"
        ]
    );
}

#[test]
fn only_a_bootout_leaves_the_job_unstarted() {
    assert!(!Stop::Bootout.restarts());
    assert!(Stop::Kickstart.restarts());
    assert!(Stop::Kill.restarts());
}

#[test]
fn the_children_of_another_uid_are_started_only_with_ids_to_drop_to() {
    assert_eq!(
        Kind::started(false),
        [Kind::SameGroup, Kind::OwnGroup, Kind::OwnSession]
    );
    assert_eq!(Kind::launched(false), Kind::started(false));
    assert_eq!(
        Kind::started(true),
        [
            Kind::SameGroup,
            Kind::OwnGroup,
            Kind::OwnSession,
            Kind::Dropped,
            Kind::Supervisor,
            Kind::SupervisedJob
        ]
    );
    // The supervisor starts its own child; the job does not.
    assert_eq!(
        Kind::launched(true),
        [
            Kind::SameGroup,
            Kind::OwnGroup,
            Kind::OwnSession,
            Kind::Dropped,
            Kind::Supervisor
        ]
    );
}

#[test]
fn every_kind_reads_back_from_its_name_and_an_unknown_name_is_none() {
    for kind in Kind::started(true) {
        assert_eq!(Kind::from_name(kind.name()), Some(kind));
    }
    assert_eq!(Kind::from_name("other"), None);
}

#[test]
fn only_the_same_group_child_may_end_with_its_job() {
    let must: Vec<bool> = Kind::started(true)
        .into_iter()
        .map(Kind::must_outlive)
        .collect();
    assert_eq!(must, [false, true, true, true, true, true]);
    let own: Vec<bool> = Kind::started(true)
        .into_iter()
        .map(Kind::own_group)
        .collect();
    assert_eq!(own, [false, true, false, true, true, true]);
}

#[test]
fn ids_read_uid_colon_gid_and_print_back_the_same() {
    let ids: Ids = "4294967294:20".parse().unwrap();
    assert_eq!(
        ids,
        Ids {
            uid: 4_294_967_294,
            gid: 20
        }
    );
    assert_eq!(ids.to_string(), "4294967294:20");
}

#[test]
fn ids_without_a_colon_or_with_a_part_that_is_not_a_number_are_refused() {
    for text in ["600", "x:20", "600:y", "-1:20", ":"] {
        let err = text.parse::<Ids>().unwrap_err();
        assert_eq!(err, BadIds(text.to_owned()));
    }
    assert_eq!(
        BadIds("600".into()).to_string(),
        "expected <uid>:<gid> (two numbers), got \"600\""
    );
}

#[test]
fn the_plist_keeps_the_job_alive_and_sets_abandon_process_group_only_when_asked() {
    let program = ["/p/js-mac-probe".to_owned(), "a&b<c>\"d".to_owned()];
    let plist = Plist {
        label: "js-mac-probe.launchd.7.kill-9",
        program: &program,
        abandon: false,
        log: Path::new("/p/state/job.log"),
    };
    let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>js-mac-probe.launchd.7.kill-9</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/p/js-mac-probe</string>
\t\t<string>a&amp;b&lt;c&gt;&quot;d</string>
\t</array>
\t<key>KeepAlive</key>
\t<true/>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>ThrottleInterval</key>
\t<integer>1</integer>
\t<key>StandardOutPath</key>
\t<string>/p/state/job.log</string>
\t<key>StandardErrorPath</key>
\t<string>/p/state/job.log</string>
</dict>
</plist>
";
    assert_eq!(plist.render(), expected);
    let abandoning = Plist {
        abandon: true,
        ..plist
    };
    let with_key = expected.replace(
        "\t<key>StandardOutPath</key>",
        "\t<key>AbandonProcessGroup</key>\n\t<true/>\n\t<key>StandardOutPath</key>",
    );
    assert_eq!(abandoning.render(), with_key);
}

#[test]
fn a_hello_reads_back_from_its_text() {
    let hello = Hello {
        pid: 41,
        pgid: 40,
        sid: 39,
        uid: 4_294_967_294,
    };
    assert_eq!(hello.to_string(), "pid=41 pgid=40 sid=39 uid=4294967294");
    assert_eq!(Hello::parse(&hello.to_string()), Some(hello));
    assert_eq!(
        Hello::parse("  pid=1\tpgid=2 sid=3 uid=4 \n").map(|h| h.uid),
        Some(4)
    );
}

#[test]
fn a_hello_with_a_missing_reordered_extra_or_unreadable_field_is_none() {
    for line in [
        "",
        "pid=1 pgid=2 sid=3",
        "pid=1 pgid=2 sid=3 uid",
        "pgid=2 pid=1 sid=3 uid=4",
        "pid=1 pgid=2 sid=3 uid=4 more=5",
        "pid=x pgid=2 sid=3 uid=4",
        "pid=1 pgid=x sid=3 uid=4",
        "pid=1 pgid=2 sid=x uid=4",
        "pid=1 pgid=2 sid=3 uid=-4",
    ] {
        assert_eq!(Hello::parse(line), None, "{line:?}");
    }
}

fn hello(pid: i32) -> Hello {
    Hello {
        pid,
        pgid: pid,
        sid: 1,
        uid: 501,
    }
}

#[test]
fn the_children_file_reads_back_and_a_reason_stays_on_its_line() {
    let children = [
        Spawned {
            kind: Kind::SameGroup,
            outcome: Ok(hello(10)),
        },
        Spawned {
            kind: Kind::Dropped,
            outcome: Err("cannot start: two\nlines".to_owned()),
        },
    ];
    let text = render_children(&children);
    assert_eq!(
        text,
        "same-group pid=10 pgid=10 sid=1 uid=501\ndropped error cannot start: two lines\n"
    );
    let read = parse_children(&text);
    assert_eq!(read[0], children[0]);
    assert_eq!(read[1].outcome, Err("cannot start: two lines".to_owned()));
}

#[test]
fn a_children_line_with_an_unknown_kind_no_space_or_a_bad_hello_is_left_out() {
    let text = "other pid=1 pgid=1 sid=1 uid=1\nown-group\nown-group pid=x\nown-session pid=2 pgid=2 sid=2 uid=2\n";
    assert_eq!(
        parse_children(text),
        [Spawned {
            kind: Kind::OwnSession,
            outcome: Ok(Hello {
                pid: 2,
                pgid: 2,
                sid: 2,
                uid: 2
            }),
        }]
    );
}

#[test]
fn the_instances_are_the_pids_in_order_and_other_lines_are_left_out() {
    assert_eq!(parse_instances("12\n x\n 13 \n\n"), [12, 13]);
    assert!(parse_instances("").is_empty());
}

#[test]
fn a_tag_is_the_kind_under_the_state_directory() {
    assert_eq!(
        tag(Path::new("/s/kill-9"), Kind::OwnSession),
        Path::new("/s/kill-9/own-session")
    );
}

#[test]
fn a_uid_ps_prints_negative_is_read_as_the_unsigned_uid() {
    // macOS's `ps` prints nobody (4294967294) as -2.
    let rows = parse_ps("43558 43551 43558    -2 S hold --tag /s/dropped\n");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uid, 4_294_967_294);
    assert_eq!(
        Fate::of(&hello(43558), Path::new("/s/dropped"), &rows),
        Fate::Running {
            ppid: 43551,
            pgid: 43558,
            uid: 4_294_967_294
        }
    );
}

#[test]
fn ps_rows_keep_the_whole_command_and_lines_without_four_numbers_are_left_out() {
    let text = "  101     1   101 4294967294 /p/js-mac-probe child hold --tag /s/own group\n\
PID PPID PGID UID COMMAND\n\
102 1 102\n\
103 1 x 0 cmd\n\
104 1 104 x cmd\n\
105 x 105 0 cmd\n\
x 1 106 0 cmd\n\
108 1 108 -2147483649 cmd\n\
107 1 107 0 \n";
    assert_eq!(
        parse_ps(text),
        [
            Row {
                pid: 101,
                ppid: 1,
                pgid: 101,
                uid: 4_294_967_294,
                command: "/p/js-mac-probe child hold --tag /s/own group".to_owned(),
            },
            Row {
                pid: 107,
                ppid: 1,
                pgid: 107,
                uid: 0,
                command: String::new(),
            }
        ]
    );
}

fn row(pid: i32, ppid: i32, command: &str) -> Row {
    Row {
        pid,
        ppid,
        pgid: pid,
        uid: 501,
        command: command.to_owned(),
    }
}

#[test]
fn a_child_runs_on_only_where_its_pid_carries_its_tag() {
    let state = Path::new("/s");
    let tagged = tag(state, Kind::OwnGroup);
    let rows = [
        row(20, 1, "/p child hold --tag /s/same-group"),
        row(21, 1, "/p child hold --tag /s/own-group --secs 9"),
    ];
    assert_eq!(
        Fate::of(&hello(21), &tagged, &rows),
        Fate::Running {
            ppid: 1,
            pgid: 21,
            uid: 501
        }
    );
    // A reused pid: the row's command is another child's.
    assert_eq!(Fate::of(&hello(20), &tagged, &rows), Fate::Gone);
    assert_eq!(Fate::of(&hello(22), &tagged, &rows), Fate::Gone);
}

#[test]
fn only_a_child_running_under_launchd_has_outlived_its_job() {
    let running = |ppid| Fate::Running {
        ppid,
        pgid: 5,
        uid: 0,
    };
    assert!(running(1).outlived(1));
    assert!(!running(77).outlived(1));
    assert!(running(77).outlived(77));
    assert!(!Fate::Gone.outlived(1));
    assert_eq!(
        running(1).to_string(),
        "runs on, parent pid 1, pgid 5, uid 0"
    );
    assert_eq!(Fate::Gone.to_string(), "gone");
}

#[test]
fn a_child_is_settled_once_gone_or_no_longer_the_jobs() {
    let running = |ppid| Fate::Running {
        ppid,
        pgid: 5,
        uid: 0,
    };
    assert!(Fate::Gone.settled(77));
    assert!(running(1).settled(77));
    assert!(!running(77).settled(77));
}

#[test]
fn fates_are_read_only_for_children_that_started() {
    let state = Path::new("/s");
    let children = [
        Spawned {
            kind: Kind::OwnGroup,
            outcome: Ok(hello(21)),
        },
        Spawned {
            kind: Kind::Dropped,
            outcome: Err("refused".to_owned()),
        },
        Spawned {
            kind: Kind::SameGroup,
            outcome: Ok(hello(20)),
        },
    ];
    let rows = [row(21, 1, "--tag /s/own-group")];
    assert_eq!(
        fates(&children, state, &rows),
        [
            Fate::Running {
                ppid: 1,
                pgid: 21,
                uid: 501
            },
            Fate::Gone
        ]
    );
}

fn judged(children: &[Spawned], rows: &[Row]) -> Judgement {
    judge(&Kind::started(true), children, Path::new("/s"), rows)
}

/// Every kind started, pids 20 to 25 in kind order.
fn all_started() -> Vec<Spawned> {
    Kind::started(true)
        .into_iter()
        .zip(20..)
        .map(|(kind, pid)| Spawned {
            kind,
            outcome: Ok(hello(pid)),
        })
        .collect()
}

/// Every child but the same-group one runs on as it must: the job's under launchd, the
/// supervisor's (25) under the supervisor (24).
fn survivors() -> Vec<Row> {
    vec![
        row(21, 1, "--tag /s/own-group"),
        row(22, 1, "--tag /s/own-session"),
        row(23, 1, "--tag /s/dropped"),
        row(24, 1, "--tag /s/supervisor"),
        row(25, 24, "--tag /s/supervised-job"),
    ]
}

#[test]
fn a_case_holds_when_every_child_but_the_same_group_one_runs_on_under_its_parent() {
    let judgement = judged(&all_started(), &survivors());
    assert!(judgement.held);
    let facts: Vec<(&str, &str)> = judgement
        .facts
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        facts,
        [
            ("same-group", "gone"),
            ("own-group", "runs on, parent pid 1, pgid 21, uid 501"),
            ("own-session", "runs on, parent pid 1, pgid 22, uid 501"),
            ("dropped", "runs on, parent pid 1, pgid 23, uid 501"),
            ("supervisor", "runs on, parent pid 1, pgid 24, uid 501"),
            ("supervised-job", "runs on, parent pid 24, pgid 25, uid 501"),
        ]
    );
}

#[test]
fn a_case_fails_when_a_needed_child_is_gone_kept_by_another_parent_or_never_started() {
    let without = |at: usize| {
        let mut rows = survivors();
        rows.remove(at);
        rows
    };
    // The own-session child gone.
    assert!(!judged(&all_started(), &without(1)).held);
    // The dropped child still the job's.
    let mut kept = survivors();
    kept[2] = row(23, 99, "--tag /s/dropped");
    assert!(!judged(&all_started(), &kept).held);
    // The supervisor's child handed to launchd: its supervisor no longer waits for it.
    let mut orphaned = survivors();
    orphaned[4] = row(25, 1, "--tag /s/supervised-job");
    assert!(!judged(&all_started(), &orphaned).held);
    // The dropped child refused, and then missing from the file.
    let mut children = all_started();
    children[3].outcome = Err("cannot start: EPERM".to_owned());
    let refused = judged(&children, &survivors());
    assert!(!refused.held);
    assert_eq!(
        refused.facts[3],
        (
            "dropped".to_owned(),
            "not started: cannot start: EPERM".to_owned()
        )
    );
    children.remove(3);
    let missing = judged(&children, &survivors());
    assert!(!missing.held);
    assert_eq!(
        missing.facts[3].1,
        "not started: no line in the children file"
    );
}

#[test]
fn a_supervised_job_whose_supervisor_did_not_start_has_no_parent_to_outlive_under() {
    let mut children = all_started();
    children[4].outcome = Err("cannot start".to_owned());
    // Even a row that would pass with the supervisor's pid does not.
    let judgement = judged(&children, &survivors());
    assert!(!judgement.held);
    assert_eq!(
        judgement.facts[5].1,
        "runs on, parent pid 24, pgid 25, uid 501"
    );
}

#[test]
fn a_same_group_child_that_is_missing_or_runs_on_does_not_decide_the_case() {
    let mut rows = survivors();
    rows.push(row(20, 1, "--tag /s/same-group"));
    assert!(judged(&all_started(), &rows).held);
    let without: Vec<Spawned> = all_started().into_iter().skip(1).collect();
    assert!(judged(&without, &rows).held);
}

#[test]
fn an_outcome_line_reads_back_and_a_line_that_is_neither_is_none() {
    let ok = Ok(hello(7));
    assert_eq!(render_outcome(&ok), "pid=7 pgid=7 sid=1 uid=501");
    assert_eq!(parse_outcome("pid=7 pgid=7 sid=1 uid=501\n"), Some(ok));
    let refused: Result<Hello, String> = Err("cannot start: a\nb".to_owned());
    assert_eq!(render_outcome(&refused), "error cannot start: a b");
    assert_eq!(
        parse_outcome("error cannot start: a b"),
        Some(Err("cannot start: a b".to_owned()))
    );
    assert_eq!(parse_outcome(""), None);
    assert_eq!(parse_outcome("pid=x"), None);
}

#[test]
fn an_end_names_the_exit_or_signal_or_why_the_child_was_not_reaped() {
    use std::os::unix::process::ExitStatusExt as _;
    let exited = Ok(std::process::ExitStatus::from_raw(3 << 8));
    assert_eq!(end_text(&exited), "exit status: 3");
    let killed = Ok(std::process::ExitStatus::from_raw(9));
    assert_eq!(end_text(&killed), "signal: 9 (SIGKILL)");
    let failed = Err(std::io::Error::from_raw_os_error(10));
    assert_eq!(
        end_text(&failed),
        "not reaped: No child processes (os error 10)"
    );
}

#[test]
fn the_print_summary_keeps_the_lines_that_say_the_jobs_state() {
    let text = "system/js-mac-probe.launchd.7.bootout = {\n\
\tactive count = 1\n\
\tpath = /p/x.plist\n\
\ttype = LaunchDaemon\n\
\tstate = running\n\
\n\
\tprogram = /p/js-mac-probe\n\
\truns = 2\n\
\tpid = 812\n\
\tlast exit code = (never exited)\n\
\tlast terminating signal = Killed: 9\n\
\tproperties = keepalive | runatload | inferred program\n\
\tabandon process group = 1\n\
}\n";
    assert_eq!(
        print_summary(text),
        "state = running | runs = 2 | pid = 812 | last exit code = (never exited) | \
last terminating signal = Killed: 9 | properties = keepalive | runatload | inferred program | \
abandon process group = 1"
    );
    assert_eq!(print_summary("Could not find service"), "");
}
