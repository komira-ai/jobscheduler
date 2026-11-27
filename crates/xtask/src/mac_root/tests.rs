use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt as _;

use js_testkit::dir::TestDir;

use super::*;

/// The user list of a Mac with no lease user.
const CLEAN: &str = "_www 70\nadi 501\nnobody -2\n";

/// The output of a binary whose two tests passed.
const PASSED: &str =
    "\nrunning 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed\n";

fn exit(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

/// What a fake binary run as root does.
#[derive(Clone)]
enum Ran {
    /// It ran, printed this and exited with this code.
    Out(i32, String),
    /// It could not be started.
    Spawn,
}

/// The machine of a test: what each call answers, and what it was asked.
struct Fake {
    /// The user lists `dscl` prints, in order; `!` is a failed `dscl`. [`CLEAN`] once used up.
    listings: RefCell<Vec<&'static str>>,
    /// Whether `sw_vers` fails.
    sw_vers_fails: bool,
    /// The package whose build fails.
    build_fails: Option<&'static str>,
    /// Whether the `pf_probe` build names no program.
    no_program: bool,
    /// A root command with this argument exits 1.
    root_fails_on: Option<&'static str>,
    /// What each target's binary does, by its key; [`PASSED`] otherwise.
    ran: BTreeMap<&'static str, Ran>,
    /// Every root command, in order.
    roots: RefCell<Vec<Vec<OsString>>>,
    /// Every binary run: its arguments, directory and log.
    logged: RefCell<Vec<(Vec<OsString>, PathBuf, PathBuf)>>,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            listings: RefCell::new(Vec::new()),
            sw_vers_fails: false,
            build_fails: None,
            no_program: false,
            root_fails_on: None,
            ran: BTreeMap::new(),
            roots: RefCell::new(Vec::new()),
            logged: RefCell::new(Vec::new()),
        }
    }

    fn listing(self, listings: &[&'static str]) -> Fake {
        *self.listings.borrow_mut() = listings.to_vec();
        self
    }

    /// The root commands, each as one line.
    fn roots(&self) -> Vec<String> {
        self.roots
            .borrow()
            .iter()
            .map(|args| {
                let words: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
                words.join(" ")
            })
            .collect()
    }
}

/// The executable cargo builds for `test` of `package` here.
fn exe(package: &str, test: &str) -> String {
    format!("/w/target/debug/deps/{test}-{package}")
}

impl Machine for Fake {
    fn build(&self, package: &str, tests: &[&str]) -> Result<String, Error> {
        if self.build_fails == Some(package) {
            return Err(Error::Failed(format!("{package} did not build")));
        }
        let mut out = String::new();
        for test in tests {
            out.push_str(&format!(
                r#"{{"reason":"compiler-artifact","target":{{"kind":["test"],"name":"{test}"}},"profile":{{"test":true}},"executable":"{}","manifest_path":"/w/crates/{package}/Cargo.toml"}}"#,
                exe(package, test)
            ));
            out.push('\n');
        }
        Ok(out)
    }

    fn build_example(&self, package: &str, example: &str) -> Result<String, Error> {
        if self.no_program {
            return Ok(String::new());
        }
        Ok(format!(
            r#"{{"reason":"compiler-artifact","target":{{"kind":["example"],"name":"{example}"}},"profile":{{"test":false}},"executable":"/w/target/debug/examples/{example}","manifest_path":"/w/crates/{package}/Cargo.toml"}}"#
        ))
    }

    fn read(&self, program: &str, args: &[&str]) -> Result<String, Error> {
        if program == SW_VERS {
            assert!(
                args == ["-productVersion"] || args == ["-buildVersion"],
                "{args:?}"
            );
            if self.sw_vers_fails {
                return Err(Error::Failed("sw_vers failed".to_owned()));
            }
            return Ok(match args[0] {
                "-productVersion" => "27.0.1\n".to_owned(),
                _ => "26A434\n".to_owned(),
            });
        }
        assert_eq!(
            (program, args),
            (DSCL, &[".", "-list", "/Users", "UniqueID"][..])
        );
        let mut listings = self.listings.borrow_mut();
        let listing = match listings.is_empty() {
            true => CLEAN,
            false => listings.remove(0),
        };
        match listing {
            "!" => Err(Error::Failed("dscl failed".to_owned())),
            text => Ok(text.to_owned()),
        }
    }

    fn root(&self, args: &[OsString]) -> Result<ExitStatus, Error> {
        self.roots.borrow_mut().push(args.to_vec());
        let fails = self
            .root_fails_on
            .is_some_and(|word| args.iter().any(|a| a.to_string_lossy() == word));
        Ok(exit(i32::from(fails)))
    }

    fn root_logged(
        &self,
        args: &[OsString],
        cwd: &Path,
        log: &Path,
    ) -> Result<(ExitStatus, String), Error> {
        self.logged
            .borrow_mut()
            .push((args.to_vec(), cwd.to_path_buf(), log.to_path_buf()));
        let key = TARGETS
            .iter()
            .map(|t| t.key)
            .find(|key| {
                args.iter()
                    .any(|a| Path::new(a).ends_with(Path::new("bin").join(key)))
            })
            .expect("a target's binary");
        match self.ran.get(key).cloned() {
            Some(Ran::Spawn) => Err(Error::Failed(format!("{key} could not start"))),
            Some(Ran::Out(code, text)) => Ok((exit(code), text)),
            None => Ok((exit(0), PASSED.to_owned())),
        }
    }
}

/// A scratch directory with a repository root in it, and a work directory that does not
/// exist yet.
struct Place {
    dir: TestDir,
}

impl Place {
    fn new(name: &str) -> Place {
        let dir = TestDir::under(std::env::temp_dir(), &format!("xtask-mac-root-{name}"));
        std::fs::create_dir_all(dir.join("repo")).expect("repo");
        Place { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    fn run(&self, fake: &Fake) -> Result<String, Error> {
        self.run_on("macos", fake, &self.work())
    }

    fn run_on(&self, os: &str, fake: &Fake, work: &Path) -> Result<String, Error> {
        run(&MacRootTests {
            os,
            machine: fake,
            root: &self.root(),
            work,
            owner_uid: 501,
        })
    }

    fn table(&self) -> String {
        std::fs::read_to_string(self.root().join("target/mac-root-tests/table.tsv"))
            .expect("the table")
    }
}

/// Catches a run that skips a target, runs one with the wrong variables, as the wrong user
/// or from the wrong directory, lays the work directory out wrongly (a directory not made,
/// a binary not copied or left without its mode), deletes a user it did not make, or leaves
/// the stated skip out of the table.
#[test]
fn a_clean_mac_runs_every_target_from_the_work_directory_and_passes() {
    let place = Place::new("clean");
    let made =
        "adi 501\nkbf-acct-0 4300\nkbf-acct-1 4309\nkbf-acct-7 650\nkbf-acct-x 4313\nother 4305\n";
    let fake = Fake::new().listing(&[CLEAN, made]);
    let ok = place.run(&fake).expect("every test passed");
    let table_path = place.root().join("target/mac-root-tests/table.tsv");
    assert_eq!(
        ok,
        format!(
            "ok: 16 Mac root tests passed on macOS 27.0.1 (26A434); the table is {}",
            table_path.display()
        )
    );
    let w = place.work();
    let w = w.display();
    let mut expected: Vec<String> = [
        "",
        "/bin",
        "/treestore",
        "/treestore/root",
        "/mac-pool",
        "/mac-pool-trace",
        "/native-privileged",
        "/native-isolation",
        "/native-inbound-pf",
        "/native-fence",
    ]
    .iter()
    .map(|d| format!("/bin/mkdir -m 0755 {w}{d}"))
    .collect();
    for target in &TARGETS {
        expected.push(format!(
            "/bin/cp {} {w}/bin/{}",
            exe(target.package, target.test),
            target.key
        ));
    }
    expected.push(format!(
        "/bin/cp /w/target/debug/examples/pf_probe {w}/bin/pf_probe"
    ));
    let mut chmod = "/bin/chmod 0755".to_owned();
    for target in &TARGETS {
        chmod.push_str(&format!(" {w}/bin/{}", target.key));
    }
    chmod.push_str(&format!(" {w}/bin/pf_probe"));
    expected.push(chmod);
    // Only the users the native_privileged run made: in range and named as the pool's.
    expected.push("/usr/bin/dscl . -delete /Users/kbf-acct-0".to_owned());
    expected.push("/usr/bin/dscl . -delete /Users/kbf-acct-1".to_owned());
    assert_eq!(fake.roots(), expected);

    let logged = fake.logged.borrow();
    assert_eq!(logged.len(), TARGETS.len());
    let logs = place.root().join("target/mac-root-tests");
    for ((args, cwd, log), target) in logged.iter().zip(&TARGETS) {
        assert_eq!(args, &run_args(target.key, &place.work(), 501));
        assert_eq!(cwd, &PathBuf::from(format!("/w/crates/{}", target.package)));
        assert_eq!(log, &logs.join(format!("{}.log", target.key)));
    }
    let table = place.table();
    let mut want = "# macOS 27.0.1 (26A434)\ntarget\ttest\tresult\treason\n".to_owned();
    for target in &TARGETS {
        want.push_str(&format!("{0}\ta\tpass\t\n{0}\tb\tpass\t\n", target.key));
        if target.key == "native_privileged" {
            want.push_str(&format!(
                "native_privileged\t{}\tnot-run\t{}\n",
                VM_TEST.0, VM_TEST.1
            ));
        }
    }
    assert_eq!(table, want);
}

/// Asserts the account the `native_fence` target made (uid 4312, the last of the made range)
/// is deleted after that target, and only after it: the listing taken after the earlier
/// `js-agent` targets holds none. Catches a made range that stops one uid short, which would
/// leave the fence test's account on the Mac.
#[test]
fn the_fence_targets_account_is_deleted_after_it() {
    let place = Place::new("fence-cleanup");
    let fence = "adi 501\nkbf-acct-0 4312\n";
    let fake = Fake::new().listing(&[
        CLEAN, CLEAN, CLEAN, CLEAN, CLEAN, CLEAN, CLEAN, fence, CLEAN,
    ]);
    place.run(&fake).expect("every test passed");
    let roots = fake.roots();
    let deletes: Vec<&String> = roots.iter().filter(|a| a.contains("-delete")).collect();
    assert_eq!(deletes, ["/usr/bin/dscl . -delete /Users/kbf-acct-0"]);
    assert!(place.table().contains("native_fence\ta\tpass\t\n"));
}

/// Asserts a Mac that already holds the fence target's uid 4312 is refused before anything
/// runs as root, as every other uid of the tests' range is. Catches a test range that stops
/// short of the fence target's uid.
#[test]
fn a_mac_with_the_fence_uid_is_refused() {
    let place = Place::new("fence-refused");
    let fake = Fake::new().listing(&["kbf-acct-0 4312\n"]);
    let e = place.run(&fake).expect_err("refused").to_string();
    assert!(e.ends_with("this Mac has kbf-acct-0 (4312)"), "{e}");
    assert!(fake.roots().is_empty());
}

/// Catches a failure that passes for a pass, or that stops the targets after it: a failed
/// test, a binary that crashed before any test, one that could not start, a user left after
/// a cleanup, and a user list that cannot be read after one are each a `FAIL` row, every
/// target still runs, and the verb fails naming each.
#[test]
fn failures_are_rows_every_target_still_runs_and_the_verb_fails() {
    let place = Place::new("failures");
    let left = "kbf-acct-0 4310\n";
    let mut fake = Fake::new().listing(&[CLEAN, CLEAN, CLEAN, left, left, CLEAN, "!"]);
    let failed = "running 2 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\n\nthread 'b' panicked at tests/x.rs:1:2:\nthe pf rule did not load\n\nfailures:\n    b\n";
    fake.ran
        .insert("pf_as_root", Ran::Out(101, failed.to_owned()));
    fake.ran.insert("treestore", Ran::Out(255, String::new()));
    fake.ran.insert("mac_pool", Ran::Spawn);
    let errors = match place.run(&fake) {
        Err(Error::Refused(lines)) => lines,
        other => panic!("{other:?}"),
    };
    let logs = place.root().join("target/mac-root-tests");
    assert_eq!(
        errors,
        vec![
            "pf_as_root b: the pf rule did not load".to_owned(),
            format!(
                "treestore (binary): reported no test; exit status: 255; see {}",
                logs.join("treestore.log").display()
            ),
            "mac_pool (binary): mac_pool could not start".to_owned(),
            "cleanup kbf-acct-0: the user is left after native_isolation".to_owned(),
            "cleanup (listing): after native_inbound_pf: dscl failed".to_owned(),
        ]
    );
    assert_eq!(fake.logged.borrow().len(), TARGETS.len());
    assert!(
        fake.roots()
            .contains(&"/usr/bin/dscl . -delete /Users/kbf-acct-0".to_owned())
    );
}

/// Catches a step before the runs whose failure is passed over: each refusal stops the verb
/// before anything runs as root (or, for a failed setup command, before any binary runs),
/// with its reason.
#[test]
fn each_refusal_before_the_runs_stops_the_verb_with_its_reason() {
    let place = Place::new("refusals");
    let work = place.work();
    let w = work.display().to_string();
    let say = |fake: &Fake, os: &str, work: &Path| {
        place.run_on(os, fake, work).map_err(|e| e.to_string())
    };

    let fake = Fake::new();
    assert_eq!(
        say(&fake, "linux", &work),
        Err("mac-root-tests runs on a Mac; this is linux".to_owned())
    );
    assert_eq!(
        say(&fake, "macos", Path::new("rel/work")),
        Err("--work rel/work is not an absolute path".to_owned())
    );
    assert_eq!(
        say(&fake, "macos", &place.root()),
        Err(format!(
            "--work {} exists; name a new directory",
            place.root().display()
        ))
    );
    let orphan = place.dir.join("missing/work");
    assert_eq!(
        say(&fake, "macos", &orphan),
        Err(format!(
            "--work {}: its parent is not a directory",
            orphan.display()
        ))
    );
    let fake = Fake::new().listing(&["!"]);
    assert_eq!(say(&fake, "macos", &work), Err("dscl failed".to_owned()));
    let fake = Fake::new().listing(&["adi 501 extra\n"]);
    assert_eq!(
        say(&fake, "macos", &work),
        Err("dscl printed a line that is not a user and its uid: \"adi 501 extra\"".to_owned())
    );
    let fake = Fake::new().listing(&["kbf-lease-pool-3 603\n"]);
    let refused = say(&fake, "macos", &work).expect_err("a lease user");
    assert!(
        refused.ends_with("this Mac has kbf-lease-pool-3 (603)"),
        "{refused}"
    );
    let fake = Fake {
        sw_vers_fails: true,
        ..Fake::new()
    };
    assert_eq!(say(&fake, "macos", &work), Err("sw_vers failed".to_owned()));
    let fake = Fake {
        build_fails: Some("js-mac-pool"),
        ..Fake::new()
    };
    assert_eq!(
        say(&fake, "macos", &work),
        Err("js-mac-pool did not build".to_owned())
    );
    let fake = Fake {
        no_program: true,
        ..Fake::new()
    };
    assert_eq!(
        say(&fake, "macos", &work),
        Err("cargo built 0 programs for the example `pf_probe`, not 1".to_owned())
    );
    for fake in [
        &Fake::new().listing(&["!"]),
        &Fake {
            build_fails: Some("js-agent"),
            ..Fake::new()
        },
    ] {
        let _ = say(fake, "macos", &work);
        assert!(
            fake.roots().is_empty(),
            "nothing runs as root before the checks pass"
        );
    }
    let fake = Fake {
        root_fails_on: Some("/bin/cp"),
        ..Fake::new()
    };
    let e = say(&fake, "macos", &work).expect_err("cp fails");
    assert!(
        e.starts_with("sudo -n /bin/cp /w/target/debug/deps/pf_as_root-js-mac-provision ")
            & e.ends_with(&format!("{w}/bin/pf_as_root failed: exit status: 1")),
        "{e}"
    );
    assert!(
        fake.logged.borrow().is_empty(),
        "no binary runs after a failed setup"
    );
}

/// Catches the logs or the table written somewhere they cannot be, passed over: each is an
/// error naming the path.
#[test]
fn a_log_directory_or_table_that_cannot_be_written_is_an_error() {
    let place = Place::new("blocked-logs");
    std::fs::write(place.root().join("target"), "a file").expect("block target/");
    let e = place.run(&Fake::new()).expect_err("logs blocked");
    let logs = place.root().join("target/mac-root-tests");
    assert!(
        e.to_string().starts_with(&format!("{}: ", logs.display())),
        "{e}"
    );

    let place = Place::new("blocked-table");
    let table = place.root().join("target/mac-root-tests/table.tsv");
    std::fs::create_dir_all(&table).expect("block the table");
    let e = place.run(&Fake::new()).expect_err("table blocked");
    assert!(
        e.to_string().starts_with(&format!("{}: ", table.display())),
        "{e}"
    );
}

/// Catches a user list read wrongly: a negative uid, blank lines, a line of one word, of
/// three words or with a uid that is not a number, and an empty list read as "no lease
/// user".
#[test]
fn the_user_list_is_read_or_refused() {
    assert_eq!(
        parse_users("\n_www 70\n\nnobody -2\n").map_err(|e| e.to_string()),
        Ok(vec![("_www".to_owned(), 70), ("nobody".to_owned(), -2)])
    );
    for bad in ["adi\n", "adi 501 x\n", "adi five\n"] {
        let e = parse_users(bad).expect_err(bad).to_string();
        assert!(
            e.starts_with("dscl printed a line that is not a user"),
            "{e}"
        );
    }
    assert_eq!(
        parse_users(" \n").map_err(|e| e.to_string()),
        Err("dscl listed no user".to_owned())
    );
}

/// Catches a lease user let through, at each edge of each range and by name, and a Mac
/// without one refused.
#[test]
fn a_lease_user_is_refused_at_every_edge() {
    let user = |name: &str, uid: i64| vec![(name.to_owned(), uid)];
    for (name, uid) in [
        ("a", 600),
        ("a", 699),
        ("a", 4300),
        ("a", 4312),
        ("a", 4320),
        ("kbf-lease-1-1", 501),
    ] {
        let e = check_users(&user(name, uid)).expect_err(name).to_string();
        assert_eq!(
            e,
            format!(
                "refused: the root tests run only on a Mac with no lease users, because they \
                 make and delete users named kbf-acct-<n> (uids 4300-4320); this Mac has \
                 {name} ({uid})"
            )
        );
    }
    let fine = [
        ("a", 599),
        ("b", 700),
        ("c", 4299),
        ("d", 4321),
        ("kbf-other", 1),
    ];
    let fine: Vec<_> = fine.iter().map(|(n, u)| ((*n).to_owned(), *u)).collect();
    assert_eq!(check_users(&fine).map_err(|e| e.to_string()), Ok(()));
    let two = [("x".to_owned(), 650), ("y".to_owned(), 4310)];
    let e = check_users(&two).expect_err("two").to_string();
    assert!(e.ends_with("this Mac has x (650), y (4310)"), "{e}");
}

/// Catches a cleanup that deletes a user the tests did not make: only account-named users with
/// a uid of the tests' made range, at both edges, are named (an earlier release's pool user
/// is not).
#[test]
fn only_the_tests_pool_users_are_deleted() {
    let users: Vec<(String, i64)> = [
        ("kbf-acct-0", 4300),
        ("kbf-acct-1", 4312),
        ("kbf-acct-2", 4299),
        ("kbf-acct-3", 4313),
        ("kbf-acct-4", 650),
        ("other", 4305),
        ("kbf-lease-pool-5", 4306),
    ]
    .iter()
    .map(|(n, u)| ((*n).to_owned(), *u))
    .collect();
    assert_eq!(to_delete(&users), ["kbf-acct-0", "kbf-acct-1"]);
}

/// Catches a target given another target's variables or directories, a missing filter or
/// skip, and a binary run without `--ignored` or in parallel.
#[test]
fn each_target_gets_its_own_variables_directories_and_arguments() {
    let w = Path::new("/S/w");
    let line = |key: &str| {
        let words: Vec<_> = run_args(key, w, 501)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        words.join(" ")
    };
    assert_eq!(
        line("pf_as_root"),
        "/usr/bin/env JS_PF_PROBE_BIN=/S/w/bin/pf_probe /S/w/bin/pf_as_root --ignored --test-threads=1"
    );
    assert_eq!(
        line("treestore"),
        "/usr/bin/env JS_TREESTORE_PRIV_PARENT=/S/w/treestore JS_TREESTORE_PRIV_ROOT=/S/w/treestore/root JS_TREESTORE_PRIV_UID=4320 JS_TREESTORE_PRIV_GID=4320 /S/w/bin/treestore --ignored --test-threads=1"
    );
    assert_eq!(
        line("mac_pool"),
        "/usr/bin/env JS_MAC_POOL_PRIV_ROOT=/S/w/mac-pool JS_MAC_POOL_PRIV_OWNER_UID=501 JS_MAC_POOL_PRIV_UID=4320 JS_MAC_POOL_PRIV_GID=4320 /S/w/bin/mac_pool --ignored --test-threads=1"
    );
    assert_eq!(
        line("mac_pool_trace"),
        "/usr/bin/env JS_MAC_POOL_PRIV_ROOT=/S/w/mac-pool-trace JS_MAC_POOL_PRIV_OWNER_UID=501 JS_MAC_POOL_PRIV_UID=4320 JS_MAC_POOL_PRIV_GID=4320 /S/w/bin/mac_pool_trace --ignored --test-threads=1 trace"
    );
    assert_eq!(
        line("native_privileged"),
        "/usr/bin/env JS_NATIVE_PRIV_ROOT=/S/w/native-privileged /S/w/bin/native_privileged --ignored --test-threads=1 --skip vm::"
    );
    assert_eq!(
        line("native_isolation"),
        "/usr/bin/env JS_NATIVE_ISO_ROOT=/S/w/native-isolation JS_NATIVE_ISO_UID=4310 /S/w/bin/native_isolation --ignored --test-threads=1"
    );
    assert_eq!(
        line("native_inbound_pf"),
        "/usr/bin/env JS_NATIVE_INBOUND_ROOT=/S/w/native-inbound-pf JS_NATIVE_INBOUND_UID=4311 /S/w/bin/native_inbound_pf --ignored --test-threads=1"
    );
    assert_eq!(
        line("native_fence"),
        "/usr/bin/env JS_NATIVE_FENCE_ROOT=/S/w/native-fence JS_NATIVE_FENCE_UID=4312 JS_NATIVE_FENCE_OWNER_UID=501 /S/w/bin/native_fence --ignored --test-threads=1"
    );
    assert_eq!(dirs_for("native_fence", w), [w.join("native-fence")]);
    assert_eq!(
        line("unknown"),
        "/usr/bin/env /S/w/bin/unknown --ignored --test-threads=1"
    );
    assert!(dirs_for("pf_as_root", w).is_empty());
    assert!(skipped_for("treestore").is_empty());
    assert_eq!(skipped_for("native_privileged"), vec![VM_TEST]);
}

/// Catches result lines misread: each verdict word, an ignored test with its reason, the
/// summary line and an unknown verdict that are not tests, and output that is not libtest's.
#[test]
fn libtest_result_lines_are_read_and_nothing_else() {
    let text = "running 5 tests\ntest a ... ok\ntest m::b ... FAILED\ntest c ... ignored\ntest d ... ignored, needs root\ntest e ... bench\ntest result: FAILED. 1 passed; 1 failed\ntest f ... ok   \nnoise\n";
    assert_eq!(
        results(text),
        vec![
            ("a".to_owned(), Outcome::Pass),
            ("m::b".to_owned(), Outcome::Fail),
            ("c".to_owned(), Outcome::NotRun),
            ("d".to_owned(), Outcome::NotRun),
            ("f".to_owned(), Outcome::Pass),
        ]
    );
    assert!(results("").is_empty());
}

/// Catches the wrong reason for a failure: another test's panic, the panic's location line
/// instead of its message, a panic with no message, output with no panic, a name with no
/// block, and a message longer than the table keeps.
#[test]
fn a_failure_s_reason_is_the_line_after_its_own_panic() {
    let long = "x".repeat(REASON_CHARS + 1);
    let text = format!(
        "failures:\n\n---- a stdout ----\nthread 'a' panicked at x.rs:1:1:\nwrong a\n---- b stdout ----\nprinted\nthread 'b' panicked at x.rs:2:2:\n  wrong b  \n\n---- c stdout ----\nthread 'c' panicked at x.rs:3:3:\n\n---- d stdout ----\nno panic here\n---- e stdout ----\nthread 'e' panicked at x.rs:5:5:\n---- f stdout ----\nthread 'f' panicked at x.rs:6:6:\n{long}\n"
    );
    assert_eq!(reason_of(&text, "a"), "wrong a");
    assert_eq!(reason_of(&text, "b"), "wrong b");
    assert_eq!(
        reason_of(&text, "c"),
        "it panicked; no message (see the log)"
    );
    assert_eq!(
        reason_of(&text, "d"),
        "no panic in its output (see the log)"
    );
    assert_eq!(
        reason_of(&text, "e"),
        "it panicked; no message (see the log)"
    );
    assert_eq!(
        reason_of(&text, "zz"),
        "no panic in its output (see the log)"
    );
    assert_eq!(reason_of(&text, "f"), "x".repeat(REASON_CHARS));
}

/// Catches a silent pass: a binary that reports no test, or that exits with a failure while
/// every test it names passed, is a failure of the binary; one that names its failed test is
/// not reported twice; an ignored test is not run, with the reason.
#[test]
fn a_binary_is_judged_by_its_tests_and_its_exit() {
    let log = Path::new("/l/t.log");
    let row = |test: &str, outcome, reason: &str| Row::new("t", test, outcome, reason);
    assert_eq!(
        judge("t", "", "exit status: 0", true, log),
        vec![row(
            "(binary)",
            Outcome::Fail,
            "reported no test; exit status: 0; see /l/t.log"
        )]
    );
    assert_eq!(
        judge("t", "test a ... ok\n", "exit status: 101", false, log),
        vec![
            row("a", Outcome::Pass, ""),
            row(
                "(binary)",
                Outcome::Fail,
                "exit status: 101 with no failed test; see /l/t.log"
            ),
        ]
    );
    assert_eq!(
        judge(
            "t",
            "test a ... FAILED\ntest i ... ignored\n",
            "exit status: 101",
            false,
            log
        ),
        vec![
            row("a", Outcome::Fail, "no panic in its output (see the log)"),
            row("i", Outcome::NotRun, "ignored by the binary"),
        ]
    );
    assert_eq!(
        judge("t", "test a ... ok\n", "exit status: 0", true, log),
        vec![row("a", Outcome::Pass, "")]
    );
}

/// Catches a table that does not line up, drops a column, or keeps trailing blanks.
#[test]
fn the_table_lines_up_and_its_file_is_tab_separated() {
    let rows = vec![
        Row::new("pf_as_root", "a", Outcome::Pass, ""),
        Row::new("t", "a_long_test_name", Outcome::Fail, "why"),
        Row::new("t", "v", Outcome::NotRun, "needs a VM"),
    ];
    assert_eq!(
        render("27.0.1 (26A434)", &rows),
        "macOS 27.0.1 (26A434)\n\
         target      test              result   reason\n\
         pf_as_root  a                 pass\n\
         t           a_long_test_name  FAIL     why\n\
         t           v                 not-run  needs a VM\n"
    );
    assert_eq!(
        tsv("26.4 (25E1)", &rows),
        "# macOS 26.4 (25E1)\ntarget\ttest\tresult\treason\npf_as_root\ta\tpass\t\nt\ta_long_test_name\tFAIL\twhy\nt\tv\tnot-run\tneeds a VM\n"
    );
    assert_eq!(render("x", &[]), "macOS x\ntarget  test  result   reason\n");
}
