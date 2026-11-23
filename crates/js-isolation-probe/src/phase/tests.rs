use std::fs;

use js_testkit::dir::TestDir;

use super::*;
use crate::host::{Target, test_who};
use crate::ipc;
use crate::tools::fake::Fake;

/// A token no other test or run uses.
fn token(test: &str) -> String {
    format!("{test}{}", std::process::id())
}

/// A home with `exec/`, and a place outside it that the test closes or keeps open.
fn layout(name: &str) -> (TestDir, Who) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    fs::create_dir_all(dir.join("home/exec")).unwrap();
    fs::create_dir(dir.join("shared")).unwrap();
    let who = test_who(&dir.join("home"));
    (dir, who)
}

/// Two places: the run directory and a shared one beside the home.
fn places(who: &Who) -> Vec<Target> {
    vec![
        Target {
            name: "run-dir".to_owned(),
            dir: who.cwd.clone(),
        },
        Target {
            name: "shared".to_owned(),
            dir: who.home.parent().unwrap().join("shared"),
        },
    ]
}

fn host<'a>(tools: &'a Fake, who: Who, writes: &'a dyn Fn(&Who) -> Vec<Target>) -> Host<'a> {
    Host {
        tools,
        who,
        ids: Ids {
            gid: 20,
            egid: 20,
            groups: vec![20, 12],
        },
        writes,
        reads: vec![],
        connect_limit: Duration::from_secs(30),
        many: 20,
        setuid_dirs: vec![],
    }
}

fn printed(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn pinned(probe: &str, leave: &str, same: &str, other: &str) -> Pins {
    Pins::parse(&format!(
        "[probe]\n{probe}\n[leave]\n{leave}\n[find-same-slot]\n{same}\n[find-other-slot]\n{other}\n"
    ))
    .unwrap()
}

#[test]
fn probe_prints_every_probe_in_order_and_judges_by_the_pins() {
    let (dir, who) = layout("phase-probe");
    let secret = dir.join("secret");
    fs::write(&secret, b"never printed").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let phase = Phase::Probe {
        writes: vec![Named {
            name: "absent-store".to_owned(),
            value: dir.join("absent").display().to_string(),
        }],
        reads: vec![Named {
            name: "agent-token".to_owned(),
            value: secret.display().to_string(),
        }],
        connects: vec![Named {
            name: "loopback".to_owned(),
            value: listener.local_addr().unwrap().to_string(),
        }],
        lookups: vec![Named {
            name: "literal".to_owned(),
            value: "127.0.0.1".to_owned(),
        }],
        unix: vec![Named {
            name: "helper-socket".to_owned(),
            value: dir.join("no-socket").display().to_string(),
        }],
    };
    let tools = Fake::default()
        .exit(
            "/usr/sbin/dseditgroup -o checkmember -m pool-1 admin",
            67,
            "",
            "",
        )
        .exit("/usr/bin/crontab -l", 1, "", "no crontab for pool-1");
    let writes = places;
    let host = host(&tools, who.clone(), &writes);
    let pins = pinned(
        "\"write:run-dir\" = \"allowed\"\n\"priv:sudo\" = \"denied\"\n\"read:agent-token\" = \"denied\"",
        "",
        "",
        "",
    );
    let mut out = Vec::new();
    let passed = run(&phase, &host, &pins, &mut out).unwrap();
    assert!(!passed);
    let lines = printed(&out);
    assert_eq!(
        lines[0],
        format!(
            "# js-isolation-probe probe as pool-1 (uid 601, gid 20, egid 20, groups 20 12), home {}, in {}",
            who.home.display(),
            who.cwd.display()
        )
    );
    let names: Vec<&str> = lines[1..]
        .iter()
        .filter_map(|l| l.split_once(": ").map(|(name, _)| name))
        .collect();
    assert_eq!(
        &names[..17],
        [
            "write:run-dir",
            "write:shared",
            "write:absent-store",
            "read:agent-token",
            "connect:loopback",
            "lookup:literal",
            "connect-unix:helper-socket",
            "priv:sudo",
            "priv:su",
            "priv:admin-group",
            "priv:group-wheel",
            "priv:group-admin",
            "setuid:programs",
            "priv:record-same-value",
            "priv:launchd-user",
            "priv:launchd-gui",
            "priv:crontab",
        ]
    );
    let text = lines.join("\n");
    assert!(!text.contains("never printed"), "a read prints no contents");
    assert!(
        text.contains("\nfiles:20: allowed; 20 made, removed\n"),
        "{text}"
    );
    assert!(
        text.contains("\nDIFFERS read:agent-token: pinned denied, got allowed\n"),
        "{text}"
    );
    assert_eq!(lines.last().unwrap(), "verdict: FAIL read:agent-token");
    assert_eq!(
        fs::read_dir(&who.cwd).unwrap().count(),
        0,
        "the probe leaves nothing in the run directory"
    );
}

#[test]
fn leave_keeps_its_markers_and_says_how_to_find_them() {
    let (dir, mut who) = layout("phase-leave");
    who.account_home = Some(dir.join("account"));
    let tag = token("pl");
    let _sweep = ipc::Sweep(tag.clone());
    let tools = Fake::default();
    let writes = places;
    let host = host(&tools, who.clone(), &writes);
    let phase = Phase::Leave {
        token: tag.clone(),
        writes: vec![],
        linger_secs: 30,
    };
    let pins = pinned(
        "",
        "\"leave:named-semaphore\" = \"allowed\"\n\"leave:file:run-dir\" = \"allowed\"\n\"leave:file:shared\" = \"allowed\"",
        "",
        "",
    );
    let mut out = Vec::new();
    assert!(run(&phase, &host, &pins, &mut out).unwrap());
    let lines = printed(&out);
    assert_eq!(
        lines[0],
        format!(
            "# js-isolation-probe leave as pool-1 (uid 601, gid 20, egid 20, groups 20 12), home {}, account home {}, in {}",
            who.home.display(),
            dir.join("account").display(),
            who.cwd.display()
        )
    );
    assert_eq!(
        lines[1],
        format!(
            "# next: find --token {tag} --left-by-uid 601 --left-by-user pool-1 --left-by-home {}",
            who.home.display()
        )
    );
    assert!(
        lines.iter().any(|l| l
            == "leave:background-process: allowed; pid 4242, cannot look back: exit 1: unknown"),
        "{lines:?}"
    );
    assert!(who.cwd.join(files::marker_name(&tag)).exists());
    assert!(dir.join("shared").join(files::marker_name(&tag)).exists());
    assert_eq!(lines.last().unwrap(), "verdict: PASS");
    // The same slot's find removes what it finds.
    let phase = Phase::Find {
        token: tag.clone(),
        writes: vec![],
        uid: 601,
        user: "pool-1".to_owned(),
        home: who.home.clone(),
    };
    let pins = pinned(
        "",
        "",
        "\"find:named-semaphore\" = \"found\"\n\"find:file:run-dir\" = \"found\"\n\"find:sysv-semaphore\" = \"found\"",
        "",
    );
    let mut out = Vec::new();
    assert!(run(&phase, &host, &pins, &mut out).unwrap());
    let lines = printed(&out);
    assert_eq!(lines[1], "# left by uid 601: the same slot");
    assert!(
        lines
            .iter()
            .any(|l| l == "find:named-semaphore: found; opened it, removed"),
        "{lines:?}"
    );
    assert!(!who.cwd.join(files::marker_name(&tag)).exists());
    assert!(!dir.join("shared").join(files::marker_name(&tag)).exists());
    assert!(ipc::semaphore_open(&ipc::semaphore_name(&tag)).is_err());
}

#[test]
fn find_on_another_slot_looks_at_the_leavers_places_and_removes_nothing() {
    let (dir, me) = layout("phase-other");
    let tag = token("po");
    let _sweep = ipc::Sweep(tag.clone());
    let them_home = dir.join("them");
    fs::create_dir_all(them_home.join("exec")).unwrap();
    fs::write(them_home.join("exec").join(files::marker_name(&tag)), b"").unwrap();
    ipc::semaphore_create(&ipc::semaphore_name(&tag)).unwrap();
    let tools = Fake::default();
    let writes = places;
    let host = host(&tools, me, &writes);
    let phase = Phase::Find {
        token: tag.clone(),
        writes: vec![],
        uid: 600,
        user: "pool-0".to_owned(),
        home: them_home.clone(),
    };
    let pins = pinned(
        "",
        "",
        "",
        "\"find:named-semaphore\" = \"found\"\n\"find:file:run-dir\" = \"gone\"",
    );
    let mut out = Vec::new();
    assert!(!run(&phase, &host, &pins, &mut out).unwrap());
    let lines = printed(&out);
    assert_eq!(lines[1], "# left by uid 600: another slot");
    assert!(
        lines
            .iter()
            .any(|l| l == "find:named-semaphore: found; opened it, left for its own slot"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "DIFFERS find:file:run-dir: pinned gone, got found"),
        "{lines:?}"
    );
    assert!(
        them_home
            .join("exec")
            .join(files::marker_name(&tag))
            .exists()
    );
    ipc::semaphore_remove(&ipc::semaphore_name(&tag)).unwrap();
}

#[test]
fn a_failed_write_to_the_output_is_an_error() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (_dir, who) = layout("phase-closed");
    let tools = Fake::default();
    let writes = |_: &Who| Vec::new();
    let host = Host {
        many: 1,
        ..host(&tools, who, &writes)
    };
    let pins = pinned("", "", "", "");
    for phase in [
        Phase::Probe {
            writes: vec![],
            reads: vec![],
            connects: vec![],
            lookups: vec![],
            unix: vec![],
        },
        Phase::Leave {
            token: "t1".to_owned(),
            writes: vec![],
            linger_secs: 0,
        },
    ] {
        assert!(run(&phase, &host, &pins, &mut Closed).is_err());
    }
}
