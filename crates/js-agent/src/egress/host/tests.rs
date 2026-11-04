//! The host side against a scripted machine: the whole sequence of calls, a failure at each
//! one, and what is left behind (nothing). The kernel's side of it is `tests/egress`.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use super::*;
use crate::egress::Flags;
use js_types::JobNetwork;

const PID: u32 = 77;
const NET: &str = "/proc/77/ns/net";
const NETFD: &str = "/proc/1/fd/5";
const OWN: &str = "/proc/self/ns/net";
const FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

const INET: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"inet","name":"js_crun_egress","handle":1}},{"set":{"name":"jobifs","type":"ifname","handle":2}},{"rule":{"chain":"forward","handle":3,"comment":"reply","expr":[{"counter":{"packets":0,"bytes":0}},{"accept":null}]}}]}"#;
const INET_CHANGED: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"inet","name":"js_crun_egress","handle":1}},{"set":{"name":"jobifs","type":"ifname","handle":2}},{"rule":{"chain":"forward","handle":3,"comment":"reply","expr":[{"counter":{"packets":0,"bytes":0}},{"drop":null}]}}]}"#;
const NAT: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"ip","name":"js_crun_egress_nat","handle":1}}]}"#;

/// Stand-ins for the three programs: executable files in a directory of their own.
fn programs() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("js-egress-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("directory");
        for name in ["ip", "nft", "nsenter"] {
            let path = dir.join(name);
            std::fs::write(&path, b"").expect("program");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        }
        dir
    })
}

fn program(name: &str) -> String {
    programs().join(name).to_string_lossy().into_owned()
}

/// A configuration whose programs are the stand-ins: the default pool, or `pool` if given.
fn config_with(mode: &str, pool: Option<&str>) -> Config {
    let (ip, nft, nsenter) = (program("ip"), program("nft"), program("nsenter"));
    let allow = vec!["192.0.2.10:9300".to_owned()];
    let given = Flags {
        mode: Some(mode),
        allow: &allow,
        pool,
        ip: Some(&ip),
        nft: Some(&nft),
        nsenter: Some(&nsenter),
        ..Flags::default()
    };
    // Mode none takes no other flag, so it gets none.
    let flags = match mode {
        "none" => Flags::default(),
        _ => given,
    };
    Config::from_flags(&flags).expect("a sound configuration")
}

fn allow_list() -> Config {
    config_with("allow-list", None)
}

/// What a path's identity reads as, one entry per read ; the last repeats.
type Seen = Vec<(u64, u64)>;

/// What the scripted machine keeps.
#[derive(Debug, Default)]
struct State {
    log: Vec<String>,
    stdins: Vec<String>,
    calls: usize,
    fail: Option<usize>,
    answers: Vec<(String, Result<String, String>)>,
    files: HashMap<String, String>,
    write_errors: HashMap<String, ErrorKind>,
    identities: HashMap<String, (Seen, usize)>,
    /// Whether another agent holds the host's egress.
    held: bool,
    /// Whether the host's egress cannot be asked for at all.
    lock_fails: bool,
    /// The hold this machine gave, held while it is alive (as the kernel's socket is).
    hold: Option<std::sync::Weak<()>>,
}

/// A machine that records every call and answers as scripted.
#[derive(Debug, Default)]
struct Fake {
    state: Mutex<State>,
}

impl Fake {
    /// A machine with no interfaces, both tables as `INET` and `NAT`, forwarding on, and a
    /// container in a namespace of its own.
    fn new() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        fake.answer("link show", Ok("[]".to_owned()));
        fake.answer("list table inet", Ok(INET.to_owned()));
        fake.answer("list table ip", Ok(NAT.to_owned()));
        fake.state()
            .files
            .insert(FORWARD.to_owned(), "1\n".to_owned());
        fake.identity(OWN, vec![(1, 1)]);
        fake.identity(NET, vec![(1, 2)]);
        fake
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the script lock")
    }

    /// Answers the calls whose logged line contains `needle` with `result` (the latest
    /// script wins).
    fn answer(&self, needle: &str, result: Result<String, String>) {
        self.state().answers.insert(0, (needle.to_owned(), result));
    }

    fn identity(&self, path: &str, seen: Seen) {
        self.state().identities.insert(path.to_owned(), (seen, 0));
    }

    fn fail_call(&self, n: usize) {
        self.state().fail = Some(n);
    }

    fn log(&self) -> Vec<String> {
        self.state().log.clone()
    }

    fn calls(&self) -> usize {
        self.state().calls
    }

    /// Counts a call that changes the machine, and says whether it is the one that fails.
    fn counted(state: &mut State) -> bool {
        let n = state.calls;
        state.calls += 1;
        let failing = state.fail == Some(n);
        state.fail = state.fail.filter(|_| !failing);
        failing
    }
}

impl Sys for Fake {
    fn run(&self, program: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
        let mut state = self.state();
        let name = program.rsplit('/').next().expect("a name");
        let line = format!("{name} {}", args.join(" "));
        state.log.push(format!(
            "{line}{}",
            stdin.map_or(String::new(), |text| {
                format!(" << {}", text.lines().next().unwrap_or_default())
            })
        ));
        state.stdins.push(stdin.unwrap_or_default().to_owned());
        if Self::counted(&mut state) {
            return Err("injected".to_owned());
        }
        state
            .answers
            .iter()
            .find(|(needle, _)| line.contains(needle.as_str()))
            .map_or(Ok(String::new()), |(_, answer)| answer.clone())
    }

    fn write(&self, path: &str, text: &str) -> std::io::Result<()> {
        let mut state = self.state();
        state.log.push(format!("write {path} {}", text.trim()));
        if Self::counted(&mut state) {
            return Err(std::io::Error::other("injected"));
        }
        if let Some(kind) = state.write_errors.get(path) {
            return Err(std::io::Error::from(*kind));
        }
        state.files.insert(path.to_owned(), text.to_owned());
        Ok(())
    }

    fn read(&self, path: &str) -> std::io::Result<String> {
        let state = self.state();
        state
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::from(ErrorKind::NotFound))
    }

    fn lock_host(&self, _name: &str) -> std::io::Result<Option<Held>> {
        let mut state = self.state();
        if state.lock_fails {
            return Err(std::io::Error::other("injected"));
        }
        let alive = state.hold.as_ref().and_then(std::sync::Weak::upgrade);
        if state.held || alive.is_some() {
            return Ok(None);
        }
        let hold = Arc::new(());
        state.hold = Some(Arc::downgrade(&hold));
        Ok(Some(hold as Held))
    }

    fn open_netns(&self, pid: u32) -> std::io::Result<Netns> {
        let identity = Sys::identity(self, &format!("/proc/{pid}/ns/net"))?;
        Ok(Netns {
            identity,
            path: NETFD.to_owned(),
            _held: None,
        })
    }

    fn identity(&self, path: &str) -> std::io::Result<(u64, u64)> {
        let mut state = self.state();
        let (seen, cursor) = state
            .identities
            .get_mut(path)
            .ok_or_else(|| std::io::Error::from(ErrorKind::NotFound))?;
        let at = (*cursor).min(seen.len() - 1);
        *cursor += 1;
        Ok(seen[at])
    }
}

fn started(fake: &Arc<Fake>, config: Config) -> Egress {
    Egress::start_on(config, fake.clone()).expect("starts")
}

/// The expected calls of the first attach (block 0) to the container `PID`.
fn attach_calls() -> Vec<String> {
    let ip = program("ip");
    vec![
        "ip link add jsv0 type veth peer name jsp0".to_owned(),
        "write /proc/sys/net/ipv6/conf/jsv0/disable_ipv6 1".to_owned(),
        "ip addr add 198.18.0.1/30 dev jsv0".to_owned(),
        "ip link set jsv0 up".to_owned(),
        "nft add element inet js_crun_egress jobifs { \"jsv0\" }".to_owned(),
        format!("ip link set jsp0 netns {NETFD} name eth0"),
        format!("nsenter --net={NETFD} -- {ip} -batch - << link set lo up"),
    ]
}

fn release_calls(n: u32) -> Vec<String> {
    vec![
        format!("nft delete element inet js_crun_egress jobifs {{ \"jsv{n}\" }}"),
        format!("ip link del jsv{n}"),
    ]
}

/// Catches mode none doing anything at all: it runs no program, reads and writes no file,
/// needs none of the three programs, passes the check, and an attach is nothing whose drop
/// makes no call.
#[test]
fn mode_none_does_nothing() {
    let fake = Fake::new();
    let egress = started(&fake, config_with("none", None));
    assert_eq!(fake.log(), Vec::<String>::new());
    assert_eq!(egress.config().mode(), Mode::None);
    egress.check().expect("mode none has nothing to check");
    let attachment = egress
        .attach("run", PID, JobNetwork::None)
        .expect("nothing to wire");
    assert_eq!(attachment.interface(), None);
    drop(attachment);
    assert_eq!(fake.calls(), 0, "no call for a mode that wires nothing");
}

/// Catches a sweep that removes what is not the agent's or leaves what is: of the
/// interfaces, `jsv0` and `jsp3` (a name that is already gone with its pair is not an
/// error) are deleted, and `jspare0`, `jsvx`, `jsv`, `eth0`, `lo` and an entry with no name
/// are not; both tables go in one transaction.
#[test]
fn the_sweep_removes_exactly_the_agents_numbered_interfaces() {
    let fake = Fake::new();
    fake.answer(
        "link show",
        Ok(r#"[{"ifname":"lo"},{"ifname":"jsv0"},{"ifname":"jspare0"},{"ifname":"jsvx"},{"ifname":"jsv"},{"ifname":"jsp3"},{"mtu":1500},{"ifname":"eth0"}]"#
            .to_owned()),
    );
    fake.answer(
        "link del jsp3",
        Err("Cannot find device \"jsp3\"".to_owned()),
    );
    let _egress = started(&fake, allow_list());
    assert_eq!(
        fake.log()[..4],
        [
            "ip -j link show",
            "ip link del jsv0",
            "ip link del jsp3",
            "nft -f - << add table inet js_crun_egress",
        ]
    );
}

/// Catches an allow-list that starts without the firewall: after the sweep it switches IPv4
/// forwarding on, loads the ruleset the module renders (in one transaction, the stdin of
/// `nft -f -`) and reads both tables back for the baseline the check compares with.
#[test]
fn an_allow_list_loads_the_firewall_and_reads_it_back() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    assert_eq!(
        fake.log(),
        [
            "ip -j link show",
            "nft -f - << add table inet js_crun_egress",
            "write /proc/sys/net/ipv4/ip_forward 1",
            "nft -f - << add table inet js_crun_egress",
            "nft -j list table inet js_crun_egress",
            "nft -j list table ip js_crun_egress_nat",
        ]
    );
    let loaded = fake.state().stdins[2].clone();
    assert_eq!(loaded, ruleset::render(&allow_list()));
    assert_eq!(egress.config().allow().len(), 1);
    egress.check().expect("as loaded");
}

/// Catches a start that goes on past a failure: each of its calls, failing in turn, ends
/// the start with an error that says what failed; and so does a list of interfaces that is
/// not JSON, a leftover that cannot be deleted for a reason other than being gone, and a
/// listing of the tables that is not what `nft -j` prints.
#[test]
fn every_failure_of_the_start_is_an_error_that_names_it() {
    let calls = {
        let fake = Fake::new();
        let _ = started(&fake, allow_list());
        fake.calls()
    };
    assert_eq!(calls, 6);
    for n in 0..calls {
        let fake = Fake::new();
        fake.fail_call(n);
        let error = Egress::start_on(allow_list(), fake.clone()).expect_err("fails");
        assert!(error.to_string().contains("injected"), "{n}: {error}");
    }
    let start = |fake: &Arc<Fake>| Egress::start_on(allow_list(), fake.clone()).expect_err("fails");
    let fake = Fake::new();
    fake.answer("link show", Ok("nope".to_owned()));
    assert!(start(&fake).to_string().contains("listing the interfaces"));
    let fake = Fake::new();
    fake.answer("link show", Ok(r#"[{"ifname":"jsv0"}]"#.to_owned()));
    fake.answer("link del", Err("Operation not permitted".to_owned()));
    assert!(start(&fake).to_string().contains("Operation not permitted"));
    let fake = Fake::new();
    fake.answer("list table inet", Ok("nope".to_owned()));
    assert!(
        start(&fake)
            .to_string()
            .contains("reading the firewall back")
    );
}

/// Catches a check that passes what the agent did not load: forwarding switched off or the
/// switch unreadable, a rule that changed (counters, handles and the set's members are not
/// a change), and a listing that cannot be read are each an error of their own kind.
#[test]
fn the_check_tells_forwarding_off_a_changed_rule_and_an_unreadable_listing() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    egress.check().expect("as loaded");
    fake.answer(
        "list table inet",
        Ok(INET
            .replace("\"packets\":0", "\"packets\":9")
            .replace("\"handle\":3", "\"handle\":8")),
    );
    egress
        .check()
        .expect("counters and handles are not a change");
    fake.answer("list table inet", Ok(INET_CHANGED.to_owned()));
    assert_eq!(egress.check(), Err(EgressError::Rules));
    fake.answer("list table inet", Ok("nope".to_owned()));
    let error = egress.check().expect_err("unreadable");
    assert!(
        error.to_string().contains("reading the firewall back"),
        "{error}"
    );
    fake.state()
        .files
        .insert(FORWARD.to_owned(), "0\n".to_owned());
    assert_eq!(egress.check(), Err(EgressError::Forwarding));
    fake.state().files.remove(FORWARD);
    assert_eq!(egress.check(), Err(EgressError::Forwarding));
}

/// Catches the wiring of a run being anything but the documented sequence, in order: the
/// pair, the host end's switches and address, the firewall set, the move into the
/// container's namespace, and the batch that configures the job end inside it (no
/// link-local IPv6 address, the address, the default route last), then the removal on
/// release; and a second run holding another block while the first lives, the first block
/// back once it is released.
#[test]
fn a_run_is_wired_in_order_and_unwired_on_release() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.log().len();
    let first = egress
        .attach("one", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(first.interface(), Some("jsv0"));
    assert_eq!(fake.log()[before..], attach_calls()[..]);
    let batch = fake.state().stdins.last().expect("a batch").clone();
    assert_eq!(
        batch,
        "link set lo up\nlink set eth0 addrgenmode none\naddr add 198.18.0.2/30 dev eth0\n\
         link set eth0 up\nroute add default via 198.18.0.1\n"
    );
    let second = egress
        .attach("two", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(second.interface(), Some("jsv1"));
    let after_second = fake.log().len();
    first.release().expect("release");
    assert_eq!(fake.log()[after_second..], release_calls(0)[..]);
    let third = egress
        .attach("three", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(
        third.interface(),
        Some("jsv0"),
        "the first block is free again"
    );
    drop((second, third));
}

/// Catches a failure at any step of the wiring leaving anything behind: with each call of an
/// attach failing in turn the attach is an error that names the run and the step, the
/// removal of the set member and the veth follows, and the block is free for the next run.
#[test]
fn a_failure_at_every_step_of_the_wiring_is_undone() {
    let calls = {
        let fake = Fake::new();
        let egress = started(&fake, allow_list());
        let before = fake.calls();
        let _attachment = egress
            .attach("count", PID, JobNetwork::AllowList)
            .expect("attach");
        fake.calls() - before
    };
    assert_eq!(calls, 7);
    for n in 0..calls {
        let fake = Fake::new();
        let egress = started(&fake, allow_list());
        let (before, logged) = (fake.calls(), fake.log().len());
        fake.fail_call(before + n);
        let error = egress
            .attach("victim", PID, JobNetwork::AllowList)
            .expect_err("fails");
        let text = error.to_string();
        assert!(
            text.contains("wiring run victim") && text.contains("injected"),
            "{n}: {text}"
        );
        let log = fake.log();
        assert!(log[logged..].ends_with(&release_calls(0)), "{n}: {log:?}");
        let again = egress
            .attach("next", PID, JobNetwork::AllowList)
            .expect("the block is free");
        assert_eq!(again.interface(), Some("jsv0"), "{n}");
    }
}

/// Catches the wiring of a namespace it must not touch: a container that is gone, one in the
/// host's own namespace, an agent that cannot read its own namespace, and none holds a
/// block.
#[test]
fn a_namespace_that_is_not_the_containers_is_refused() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.log().len();

    fake.state().identities.remove(NET);
    let error = egress
        .attach("gone", PID, JobNetwork::AllowList)
        .expect_err("gone");
    assert!(
        matches!(error, EgressError::Container { pid: PID, .. }),
        "{error}"
    );

    fake.identity(NET, vec![(1, 1)]);
    let error = egress
        .attach("host", PID, JobNetwork::AllowList)
        .expect_err("the host's own");
    assert!(
        error
            .to_string()
            .contains("shares the host's network namespace"),
        "{error}"
    );
    assert!(
        fake.log()[before..]
            .iter()
            .all(|call| !call.contains("link add")),
        "a pair was made for the host's own namespace"
    );

    fake.identity(NET, vec![(1, 2)]);
    fake.state().identities.remove(OWN);
    let error = egress
        .attach("blind", PID, JobNetwork::AllowList)
        .expect_err("own namespace unreadable");
    assert!(
        error
            .to_string()
            .contains("reading the agent's own network namespace"),
        "{error}"
    );
    fake.identity(OWN, vec![(1, 1)]);

    fake.identity(NET, vec![(1, 2)]);
    let attachment = egress
        .attach("fine", PID, JobNetwork::AllowList)
        .expect("no block was held by the failures");
    assert_eq!(attachment.interface(), Some("jsv0"));
}

/// Catches IPv6 switches that are errors on a host that has none: a missing IPv6 switch is
/// fine (IPv6 is off), but the same error for the IPv4 forwarding switch, and any other error
/// for the IPv6 one, are not.
#[test]
fn a_missing_ipv6_switch_is_tolerated_and_nothing_else_is() {
    let ipv6 = "/proc/sys/net/ipv6/conf/jsv0/disable_ipv6";
    for (kind, works) in [
        (ErrorKind::NotFound, true),
        (ErrorKind::PermissionDenied, false),
    ] {
        let fake = Fake::new();
        let egress = started(&fake, allow_list());
        fake.state().write_errors.insert(ipv6.to_owned(), kind);
        let result = egress.attach("run", PID, JobNetwork::AllowList);
        assert_eq!(result.is_ok(), works, "{kind:?}: {result:?}");
    }
    let fake = Fake::new();
    fake.state()
        .write_errors
        .insert(FORWARD.to_owned(), ErrorKind::NotFound);
    let error = Egress::start_on(allow_list(), fake.clone()).expect_err("no forwarding switch");
    assert!(error.to_string().contains("ip_forward"), "{error}");
}

/// Catches a release that fails for what is not a failure, or hides one that is: a set
/// member and a veth that are gone already (the container ended first) release cleanly, a
/// real error is reported (and the block is freed anyway), and an attachment dropped without
/// a release takes itself down once, as does one released and then dropped.
#[test]
fn a_release_forgives_what_is_gone_and_reports_what_is_not() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let first = egress
        .attach("one", PID, JobNetwork::AllowList)
        .expect("attach");
    fake.answer(
        "delete element",
        Err("Error: No such file or directory".to_owned()),
    );
    fake.answer("link del", Err("Cannot find device \"jsv0\"".to_owned()));
    first.release().expect("gone is not an error");

    let second = egress
        .attach("two", PID, JobNetwork::AllowList)
        .expect("the block is free");
    fake.answer("link del", Err("Operation not permitted".to_owned()));
    let error = second.release().expect_err("a real error");
    assert!(
        error.to_string().contains("Operation not permitted"),
        "{error}"
    );

    let third = egress
        .attach("three", PID, JobNetwork::AllowList)
        .expect("freed even by a failed release");
    assert_eq!(third.interface(), Some("jsv0"));
    fake.answer("link del", Ok(String::new()));
    let logged = fake.log().len();
    drop(third);
    assert_eq!(
        fake.log()[logged..],
        release_calls(0)[..],
        "a drop is a release"
    );
}

/// Catches a pool that hands out more blocks than it has: a pool of 64 blocks serves 64
/// runs at once and refuses the 65th, naming the pool, and takes it again when one ends.
#[test]
fn a_full_pool_refuses_the_next_run() {
    let fake = Fake::new();
    let egress = started(&fake, config_with("allow-list", Some("10.1.2.0/24")));
    let mut held: Vec<Attachment> = (0..64)
        .map(|n| {
            egress
                .attach(&format!("run{n}"), PID, JobNetwork::AllowList)
                .expect("a free block")
        })
        .collect();
    assert_eq!(held[63].interface(), Some("jsv63"));
    let error = egress
        .attach("late", PID, JobNetwork::AllowList)
        .expect_err("full");
    assert_eq!(error, EgressError::Exhausted);
    held.remove(5);
    let again = egress
        .attach("again", PID, JobNetwork::AllowList)
        .expect("one is free");
    assert_eq!(again.interface(), Some("jsv5"));
}

/// Catches an error that says nothing: every kind of [`EgressError`] reads as what it is.
#[test]
fn each_error_reads_as_what_it_is() {
    let kinds = [
        (command("loading", "why"), "egress: loading: why"),
        (
            EgressError::Exhausted,
            "egress: every address block of the pool is in use",
        ),
        (
            EgressError::Container {
                pid: 4,
                why: "gone".to_owned(),
            },
            "egress: container 4: gone",
        ),
        (
            EgressError::Forwarding,
            "egress: net.ipv4.ip_forward is not 1",
        ),
        (
            EgressError::Rules,
            "egress: the firewall tables are not what the agent loaded",
        ),
    ];
    for (error, text) in kinds {
        assert_eq!(error.to_string(), text);
    }
}

/// Catches the real machine behind the seam doing something other than what the scripted one
/// does: a program's output on success and its words on failure, input given to it, a program
/// that cannot be started, files written and read, and the identity of a namespace file
/// (stable for the same one, an error for a missing one). `Egress::start` on the real
/// machine runs the first call of the start and reports what it printed.
#[test]
fn the_real_machine_runs_programs_and_reads_files() {
    let system = System;
    assert_eq!(
        system.run("/bin/echo", &["hi"], None),
        Ok("hi\n".to_owned())
    );
    assert_eq!(
        system.run("/bin/cat", &[], Some("typed")),
        Ok("typed".to_owned())
    );
    let failed = system
        .run("/bin/ls", &["/no/such/directory"], None)
        .expect_err("fails");
    assert!(failed.contains("No such file or directory"), "{failed}");
    let missing = system
        .run("/no/such/program", &[], None)
        .expect_err("not started");
    assert!(missing.starts_with("/no/such/program: "), "{missing}");

    let file = programs().join("note");
    let path = file.to_string_lossy().into_owned();
    system.write(&path, "text").expect("write");
    assert_eq!(system.read(&path).expect("read"), "text");
    assert!(system.read("/no/such/file").is_err());
    let one = system.identity(OWN).expect("identity");
    assert_eq!(system.identity(OWN).expect("identity"), one);
    assert!(system.identity("/no/such/file").is_err());
    let own_pid = system
        .open_netns(std::process::id())
        .expect("own namespace");
    assert_eq!(own_pid.identity, one);
    assert!(own_pid.path.starts_with("/proc/"));
    assert_eq!(system.identity(&own_pid.path).expect("by path"), one);
    assert!(system.open_netns(u32::MAX).is_err());

    // A name of this test's own: the host's (`HOST_LOCK`) may be held by an agent of this
    // machine, and no test may depend on that.
    let name = format!("{HOST_LOCK}-unit-{}", std::process::id());
    let lock = system.lock_host(&name).expect("bound").expect("free");
    assert!(
        system.lock_host(&name).expect("asked").is_none(),
        "held once"
    );
    drop(lock);
    let again = system
        .lock_host(&name)
        .expect("bound")
        .expect("free once dropped");
    drop(again);

    let echo = ["/bin/echo".to_owned()];
    let allow = ["192.0.2.10:9300".to_owned()];
    let flags = Flags {
        mode: Some("allow-list"),
        allow: &allow,
        ip: Some(&echo[0]),
        nft: Some(&echo[0]),
        nsenter: Some(&echo[0]),
        ..Flags::default()
    };
    let config = Config::from_flags(&flags).expect("sound");
    let error = Egress::start_holding(config, Arc::new(System), &name).expect_err("not JSON");
    assert!(
        error.to_string().contains("listing the interfaces"),
        "{error}"
    );
}

/// Catches the crun runtime's port doing anything but the host side's own steps, and an
/// equality that is not "the same pool": through `EgressPort` the check reads the tables
/// back and tells a changed rule, an attach makes the calls of a direct one, and the release
/// of what it gave removes them; a clone is equal to its original and another start is not.
#[test]
fn the_crun_runtimes_port_is_the_host_side() {
    use crate::crun::EgressPort;
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let port: Arc<dyn EgressPort> = crate::crun::port_of(&egress);
    assert_eq!(port.ceiling(), egress.config().ceiling());
    assert_eq!(port.ceiling().level, JobNetwork::AllowList);
    port.check().expect("as loaded");
    let before = fake.log().len();
    let wired = port
        .attach("one", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(fake.log()[before..], attach_calls()[..]);
    let attached = fake.log().len();
    crate::crun::Wired::release(wired).expect("release");
    assert_eq!(fake.log()[attached..], release_calls(0)[..]);
    let again = port.attach("two", PID, JobNetwork::AllowList);
    assert!(again.is_ok(), "the block is free again");
    drop(again);
    fake.answer("list table inet", Ok(INET_CHANGED.to_owned()));
    assert_eq!(port.check(), Err(EgressError::Rules));
    assert_eq!(egress, egress.clone());
    assert_ne!(egress, started(&Fake::new(), allow_list()));
}

/// Catches a start that sweeps the host while another agent holds its egress, or when the
/// hold cannot be asked for: with the host held the start is refused as `Held`, and with the
/// socket unusable it is an error naming the hold; either way nothing else is run (no
/// interface listed or removed, no table touched). Mode none starts whoever holds it.
#[test]
fn a_held_host_refuses_an_allow_list_and_sweeps_nothing() {
    let fake = Fake::new();
    fake.state().held = true;
    let refused = Egress::start_on(allow_list(), fake.clone()).map(drop);
    assert_eq!(refused, Err(EgressError::Held));
    assert_eq!(fake.log(), Vec::<String>::new(), "nothing swept");
    assert!(
        EgressError::Held.to_string().contains(HOST_LOCK),
        "{}",
        EgressError::Held
    );
    let broken = Fake::new();
    broken.state().lock_fails = true;
    let failed = Egress::start_on(allow_list(), broken.clone()).map(drop);
    assert_eq!(
        failed,
        Err(EgressError::Command {
            what: "holding the host's egress".to_owned(),
            why: "injected".to_owned(),
        })
    );
    assert_eq!(broken.log(), Vec::<String>::new(), "nothing swept");
    let none = Fake::new();
    none.state().held = true;
    started(&none, config_with("none", None));
    assert_eq!(none.log(), Vec::<String>::new());
}

/// Catches a runtime opened from the agent's plan that gets an egress of its own (a second
/// start, which the host's lock refuses) or none: the started egress put in the crun plan is
/// the one every clone of the settings holds (the worker's runtime and the rescue's are opened
/// from clones), and the ports made of it take their blocks from one pool.
#[test]
fn every_runtime_opened_from_a_crun_plan_shares_its_egress() {
    use crate::cli_runtime::Settings;
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let crun = PathBuf::from("/usr/bin/crun");
    let mut settings = Settings::for_crun(crun, "job", (7, 8), Vec::new());
    settings.crun_mut().expect("crun settings").egress = Some(egress.clone());
    let rescue = settings.clone();
    let plans = [&settings, &rescue].map(|settings| settings.crun().expect("a plan"));
    assert_eq!(plans[0].egress.as_ref(), Some(&egress));
    assert_eq!(plans[1].egress.as_ref(), Some(&egress));
    let ports = plans.map(|plan| crate::crun::port_of(plan.egress.as_ref().expect("egress")));
    let first = ports[0]
        .attach("worker", PID, JobNetwork::AllowList)
        .expect("attach");
    let second = ports[1]
        .attach("rescue", PID, JobNetwork::AllowList)
        .expect("attach");
    let made: Vec<String> = fake
        .log()
        .into_iter()
        .filter(|line| line.starts_with("ip link add "))
        .collect();
    assert_eq!(
        made,
        [
            "ip link add jsv0 type veth peer name jsp0",
            "ip link add jsv1 type veth peer name jsp1"
        ],
        "one pool"
    );
    drop((first, second));
}

/// Catches a hold on the host that ends while a run of it is still wired: with the last
/// `Egress` dropped and an attachment alive, another start is refused as `Held` and sweeps
/// nothing (it would remove the run's live veth and could take its name, which the run's
/// release would then delete); once the attachment is released the hold ends with it and a
/// start succeeds.
#[test]
fn an_attachment_keeps_the_host_held_after_the_last_egress_is_dropped() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let attachment = egress
        .attach("run", PID, JobNetwork::AllowList)
        .expect("attach");
    drop(egress);
    let logged = fake.log().len();
    let refused = Egress::start_on(allow_list(), fake.clone()).map(drop);
    assert_eq!(refused, Err(EgressError::Held));
    assert_eq!(fake.log().len(), logged, "nothing swept");
    attachment.release().expect("release");
    started(&fake, allow_list());
}

/// The table `inet js_crun_egress` of an `open` ceiling as `nft -j` lists it, cut to the rules
/// the check must tell apart: the reply, the metadata refusal and the open acceptance.
const OPEN_INET: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"inet","name":"js_crun_egress","handle":1}},{"set":{"name":"jobifs","type":"ifname","handle":2}},{"set":{"name":"openifs","type":"ifname","handle":3,"elem":["jsv0"]}},{"rule":{"chain":"forward","handle":4,"comment":"reply","expr":[{"counter":{"packets":0,"bytes":0}},{"accept":null}]}},{"rule":{"chain":"forward","handle":5,"comment":"metadata","expr":[{"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"daddr"}},"right":{"prefix":{"addr":"169.254.0.0","len":16}}}},{"counter":{"packets":0,"bytes":0}},{"reject":{"type":"icmp","expr":"admin-prohibited"}}]}},{"rule":{"chain":"forward","handle":6,"comment":"open","expr":[{"match":{"op":"==","left":{"meta":{"key":"iifname"}},"right":"@openifs"}},{"counter":{"packets":0,"bytes":0}},{"accept":null}]}}]}"#;

fn open() -> Config {
    config_with("open", None)
}

/// Catches an `open` ceiling whose firewall can be changed under a job unseen: the start
/// loads the `open` ruleset and reads it back; a listing with other counters and another
/// member of `openifs` is still as loaded, and the same listing with the metadata refusal
/// removed is `Rules`, through the crun runtime's port too, which the runtime's check turns
/// into a refusal of the start (`egress-unchecked`) before anything is created.
#[test]
fn a_tampered_open_ruleset_is_refused_by_check() {
    let fake = Fake::new();
    fake.answer("list table inet", Ok(OPEN_INET.to_owned()));
    let egress = started(&fake, open());
    let loaded = fake.state().stdins[2].clone();
    assert_eq!(loaded, ruleset::render(&open()));
    assert!(loaded.contains("comment \"open\""), "{loaded}");
    egress.check().expect("as loaded");
    fake.answer(
        "list table inet",
        Ok(OPEN_INET
            .replace("\"packets\":0", "\"packets\":7")
            .replace("[\"jsv0\"]", "[\"jsv3\"]")),
    );
    egress
        .check()
        .expect("counters and members are not a change");
    let value: serde_json::Value = serde_json::from_str(OPEN_INET).expect("json");
    let mut items = value["nftables"].as_array().expect("items").clone();
    items.retain(|item| item["rule"]["comment"] != "metadata");
    assert_eq!(items.len(), 6, "the metadata refusal is gone");
    let tampered = serde_json::json!({ "nftables": items }).to_string();
    fake.answer("list table inet", Ok(tampered));
    assert_eq!(egress.check(), Err(EgressError::Rules));
    let port = crate::crun::port_of(&egress);
    assert_eq!(port.check(), Err(EgressError::Rules));
}

/// Catches a run wired into the set of another level, or a level above the ceiling wired at
/// all: under `open` an `open` job's interface goes in `openifs` and leaves it on release, an
/// `allow-list` job's in `jobifs`; under `allow-list` an `open` job is `AboveCeiling` and
/// makes no call.
#[test]
fn each_level_is_wired_into_its_own_set() {
    let fake = Fake::new();
    let egress = started(&fake, open());
    let before = fake.log().len();
    let wide = egress
        .attach("wide", PID, JobNetwork::Open)
        .expect("attach");
    assert_eq!(wide.interface(), Some("jsv0"));
    let calls: Vec<String> = attach_calls()
        .into_iter()
        .map(|call| call.replace(" jobifs ", " openifs "))
        .collect();
    assert_eq!(fake.log()[before..], calls[..]);
    let attached = fake.log().len();
    wide.release().expect("release");
    assert_eq!(
        fake.log()[attached..],
        [
            "nft delete element inet js_crun_egress openifs { \"jsv0\" }",
            "ip link del jsv0",
        ]
    );
    let before = fake.log().len();
    let listed = egress
        .attach("listed", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(fake.log()[before..], attach_calls()[..]);
    drop(listed);

    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.calls();
    let above = egress.attach("above", PID, JobNetwork::Open);
    assert_eq!(
        above.map(|a| a.interface().map(str::to_owned)),
        Err(EgressError::AboveCeiling {
            asked: JobNetwork::Open,
            ceiling: JobNetwork::AllowList,
        })
    );
    assert_eq!(fake.calls(), before, "nothing was made");
    let said = EgressError::AboveCeiling {
        asked: JobNetwork::Open,
        ceiling: JobNetwork::None,
    }
    .to_string();
    assert_eq!(
        said,
        "egress: a open job cannot be wired under the ceiling none"
    );
}

/// Catches a default that wires a job, and a `none` job given an interface under a ceiling
/// that has them: a start decoded with no `network` reads as `none`, and attaching it under
/// `open` (and under `allow-list`) makes no call (no veth, no set member, no rule) and gives
/// an attachment with no interface whose drop makes none either. The root test
/// `a_none_job_has_loopback_only` shows the namespace holds `lo` alone.
#[test]
fn none_is_the_default_and_has_loopback_only() {
    let start = serde_json::json!({
        "run_id": "r1",
        "lease_id": "1-1",
        "image": "app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "limits": {"cpu_millis": 1, "memory_bytes": 1, "wall_ms": 1},
    });
    let spec: js_agent_wire::JobSpec = serde_json::from_value(start).expect("a start");
    assert_eq!(spec.network, JobNetwork::None);
    assert!(spec.ports.is_empty());
    for config in [open(), allow_list()] {
        let fake = Fake::new();
        let egress = started(&fake, config);
        let before = fake.calls();
        let attachment = egress
            .attach("quiet", PID, spec.network)
            .expect("nothing to wire");
        assert_eq!(attachment.interface(), None);
        drop(attachment);
        assert_eq!(fake.calls(), before, "a none job is wired with nothing");
    }
}
