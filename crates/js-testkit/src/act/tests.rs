use super::*;

fn args(text: &str) -> Vec<String> {
    text.split(' ').map(str::to_owned).collect()
}

/// Catches a step taken for the wrong verb, its arguments misread (a `chmod` mode
/// read as decimal, or above 7777; a `spawn-child`'s `;;` not turned into the `;`
/// its child splits at, or its `--pid` taken for a step), and a malformed step
/// accepted (it would run with a default the test did not ask for), a child's step
/// included.
#[test]
fn steps_parse_or_are_refused() {
    let run = args("/bin/x a b");
    let all = args(
        "exit 7 ; sleep 0.25 ; echo a b ; echo --stderr c ; append f x y ; env A ; \
         env B dflt ; pwd ; id ; kill-self 9 ; spawn-child sleep 1 ; \
         spawn-child --pid - setsid ;; sleep 1 ;; exit 0 ; spawn-child --pid f spawn-child id ; \
         run /bin/x a b ; setsid ; alloc 32 ; burn 0.5 ; upper o i ; rename a b ; symlink t l ; chmod 750 p ; \
         chmod 7777 q ; wait-for w 1.5 ; sleep-once o 0.5 ; mkdir m/n ; cat c ; mtime t 946684800 ; pgid ; \
         leads ; write-lines w ; write-lines w a b ; copy o i j ; remove r ; mkfifo p ; \
         truncate t 2048 ; zeros 3 ; zeros --stderr 4 ; ignore-signal 15 ; test -e e ; \
         test ! -d d ; test -x x ; points-to l t ; holds h a b ; inode-links o a b ; try-read r ; try-write w ; \
         try-list d ; tcp-listen 127.0.0.1:0 pf 1.5 ; tcp-listen [::1]:8080 pf 0 ; \
         tcp-connect 192.0.2.1:80 ; unix-listen s 30 ; unix-connect s ; procargs 1 ; \
         proc-sees 4242 ; ps-sees m-1 ; shm-owner kt 2 ; shm-owner kt 1 0.5 ; shm-tracker kt 2",
    );
    let want = [
        Step::Exit(7),
        Step::Sleep(Duration::from_millis(250)),
        Step::Echo {
            stderr: false,
            text: "a b".to_owned(),
        },
        Step::Echo {
            stderr: true,
            text: "c".to_owned(),
        },
        Step::Append {
            path: "f",
            text: "x y".to_owned(),
        },
        Step::Env {
            name: "A",
            default: None,
        },
        Step::Env {
            name: "B",
            default: Some("dflt"),
        },
        Step::Pwd,
        Step::Id,
        Step::KillSelf(Signal::KILL),
        Step::SpawnChild {
            pid: None,
            args: vec!["sleep", "1"],
        },
        Step::SpawnChild {
            pid: Some("-"),
            args: vec!["setsid", ";", "sleep", "1", ";", "exit", "0"],
        },
        Step::SpawnChild {
            pid: Some("f"),
            args: vec!["spawn-child", "id"],
        },
        Step::Run(&run),
        Step::Setsid,
        Step::Alloc(32),
        Step::Burn(Duration::from_millis(500)),
        Step::Upper {
            out: "o",
            input: "i",
        },
        Step::Rename { from: "a", to: "b" },
        Step::Symlink {
            target: "t",
            link: "l",
        },
        Step::Chmod {
            mode: 0o750,
            path: "p",
        },
        Step::Chmod {
            mode: 0o7777,
            path: "q",
        },
        Step::WaitFor {
            path: "w",
            within: Duration::from_millis(1500),
        },
        Step::SleepOnce {
            flag: "o",
            time: Duration::from_millis(500),
        },
        Step::Mkdir("m/n"),
        Step::Cat("c"),
        Step::Mtime {
            path: "t",
            secs: 946_684_800,
        },
        Step::Pgid,
        Step::Leads,
        Step::WriteLines {
            path: "w",
            text: String::new(),
        },
        Step::WriteLines {
            path: "w",
            text: "a\nb\n".to_owned(),
        },
        Step::Copy {
            out: "o",
            inputs: vec!["i", "j"],
        },
        Step::Remove("r"),
        Step::Mkfifo("p"),
        Step::Truncate {
            path: "t",
            len: 2048,
        },
        Step::Zeros {
            stderr: false,
            count: 3,
        },
        Step::Zeros {
            stderr: true,
            count: 4,
        },
        Step::IgnoreSignal(Signal::TERM),
        Step::Test {
            not: false,
            what: Predicate::Exists,
            path: "e",
        },
        Step::Test {
            not: true,
            what: Predicate::Dir,
            path: "d",
        },
        Step::Test {
            not: false,
            what: Predicate::Executable,
            path: "x",
        },
        Step::PointsTo {
            link: "l",
            target: "t",
        },
        Step::Holds {
            path: "h",
            text: "a b".to_owned(),
        },
        Step::InodeLinks {
            out: "o",
            paths: vec!["a", "b"],
        },
        Step::Probe(Probe::TryRead("r")),
        Step::Probe(Probe::TryWrite("w")),
        Step::Probe(Probe::TryList("d")),
        Step::Probe(Probe::TcpListen {
            addr: "127.0.0.1:0".parse().expect("address"),
            portfile: "pf",
            hold: Duration::from_millis(1500),
        }),
        Step::Probe(Probe::TcpListen {
            addr: "[::1]:8080".parse().expect("address"),
            portfile: "pf",
            hold: Duration::ZERO,
        }),
        Step::Probe(Probe::TcpConnect("192.0.2.1:80".parse().expect("address"))),
        Step::Probe(Probe::UnixListen {
            path: "s",
            hold: Duration::from_secs(30),
        }),
        Step::Probe(Probe::UnixConnect("s")),
        Step::Probe(Probe::Procargs(1)),
        Step::Probe(Probe::ProcSees(4242)),
        Step::Probe(Probe::PsSees("m-1")),
        Step::Shm(Shm::Owner {
            prefix: "kt",
            count: 2,
            hold: Duration::from_secs(300),
        }),
        Step::Shm(Shm::Owner {
            prefix: "kt",
            count: 1,
            hold: Duration::from_millis(500),
        }),
        Step::Shm(Shm::Tracker {
            prefix: "kt",
            count: 2,
        }),
    ];
    assert_eq!(parse(&all).expect("parses"), want);
    for bad in [
        "",
        "exit",
        "exit 256",
        "exit -1",
        "sleep x",
        "sleep -1",
        "sleep inf",
        "env",
        "env a b c",
        "pwd x",
        "kill-self 0",
        "kill-self TERM",
        "spawn-child",
        "spawn-child nope",
        "alloc x",
        "burn",
        "burn x",
        "upper o",
        "rename a",
        "symlink t",
        "chmod 750",
        "chmod 10000 p",
        "chmod 8 p",
        "chmod rwx p",
        "wait-for w",
        "wait-for w soon",
        "sleep-once o",
        "sleep-once o soon",
        "spawn-child --pid",
        "spawn-child --pid f",
        "spawn-child echo a ;; ;; exit 0",
        "spawn-child exit 0 ;; nope",
        "run",
        "mkdir",
        "mkdir a b",
        "cat",
        "mtime t",
        "mtime t -1",
        "mtime t 1.5",
        "pgid x",
        "leads x",
        "write-lines",
        "copy o",
        "remove",
        "remove a b",
        "mkfifo",
        "truncate t",
        "truncate t x",
        "zeros",
        "zeros x",
        "zeros --stderr",
        "ignore-signal 0",
        "ignore-signal TERM",
        "test -e",
        "test -z p",
        "test ! -e",
        "points-to l",
        "holds",
        "inode-links o",
        "try-read",
        "try-read a b",
        "try-write",
        "try-list",
        "try-list a b",
        "tcp-listen 127.0.0.1:0 pf",
        "tcp-listen 127.0.0.1 pf 1",
        "tcp-listen localhost:80 pf 1",
        "tcp-listen 127.0.0.1:0 pf soon",
        "tcp-listen 127.0.0.1:0 pf -1",
        "tcp-connect",
        "tcp-connect 127.0.0.1",
        "tcp-connect 127.0.0.1:65536",
        "unix-listen s",
        "unix-listen s soon",
        "unix-connect",
        "procargs",
        "procargs 0",
        "procargs -1",
        "procargs x",
        "proc-sees",
        "proc-sees 0",
        "proc-sees 1.5",
        "ps-sees",
        "ps-sees a b",
        "shm-owner",
        "shm-owner kt",
        "shm-owner kt 0",
        "shm-owner kt x",
        "shm-owner kt 1 soon",
        "shm-owner kt 1 1 1",
        "shm-tracker kt 1 1",
        "shm-owner a/b 1",
        "shm-owner abcdefghijklmnopqrstuvwxyz 1",
        "shm-tracker kt 1 2",
        "echo a ; ; exit 0",
        "nope",
    ] {
        let words = args(bad);
        let got = parse(&words);
        assert!(
            matches!(got, Err(Error::Usage(_))),
            "{bad:?} was not refused: {got:?}"
        );
    }
    assert!(matches!(parse(&[]), Err(Error::Usage(_))));
}
