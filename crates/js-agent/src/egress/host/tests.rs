//! The host side against a scripted machine: the whole sequence of calls, a failure at each
//! one, and what is left behind (nothing). The kernel's side of it is `tests/egress`.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use super::*;
use crate::egress::Flags;

const PID: u32 = 77;
const NET: &str = "/proc/77/ns/net";
const OWN: &str = "/proc/self/ns/net";
const FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

const INET: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"inet","name":"js_egress","handle":1}},{"set":{"name":"jobifs","type":"ifname","handle":2}},{"rule":{"chain":"forward","handle":3,"comment":"reply","expr":[{"counter":{"packets":0,"bytes":0}},{"accept":null}]}}]}"#;
const INET_CHANGED: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"inet","name":"js_egress","handle":1}},{"set":{"name":"jobifs","type":"ifname","handle":2}},{"rule":{"chain":"forward","handle":3,"comment":"reply","expr":[{"counter":{"packets":0,"bytes":0}},{"drop":null}]}}]}"#;
const NAT: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"ip","name":"js_egress_nat","handle":1}}]}"#;

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

/// What a path's identity reads as, one entry per read (`None` is an unreadable one); the
/// last repeats.
type Seen = Vec<Option<(u64, u64)>>;

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
        fake.identity(OWN, vec![Some((1, 1))]);
        fake.identity(NET, vec![Some((1, 2))]);
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

    fn identity(&self, path: &str) -> std::io::Result<(u64, u64)> {
        let mut state = self.state();
        let (seen, cursor) = state
            .identities
            .get_mut(path)
            .ok_or_else(|| std::io::Error::from(ErrorKind::NotFound))?;
        let at = (*cursor).min(seen.len() - 1);
        *cursor += 1;
        seen[at].ok_or_else(|| std::io::Error::from(ErrorKind::PermissionDenied))
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
        "write /proc/sys/net/ipv4/conf/jsv0/rp_filter 1".to_owned(),
        "ip addr add 198.18.0.1/30 dev jsv0".to_owned(),
        "ip link set jsv0 up".to_owned(),
        "nft add element inet js_egress jobifs { \"jsv0\" }".to_owned(),
        format!("ip link set jsp0 netns {PID} name eth0"),
        format!("nsenter --net={NET} -- {ip} -batch - << link set lo up"),
    ]
}

fn release_calls(n: u32) -> Vec<String> {
    vec![
        format!("nft delete element inet js_egress jobifs {{ \"jsv{n}\" }}"),
        format!("ip link del jsv{n}"),
    ]
}

/// Catches mode none doing anything but clearing the host of what an earlier agent left:
/// the leftover veth ends (a name that is already gone with its pair is not an error, a
/// foreign interface and an entry with no name are left alone), both tables removed in one
/// transaction, no forwarding switch, no firewall; the check passes, an attach is nothing
/// and its drop makes no call.
#[test]
fn mode_none_only_sweeps_what_an_earlier_agent_left() {
    let fake = Fake::new();
    fake.answer(
        "link show",
        Ok(r#"[{"ifname":"lo"},{"ifname":"jsv0"},{"ifname":"jsp3"},{"mtu":1500},{"ifname":"eth0"}]"#
            .to_owned()),
    );
    fake.answer(
        "link del jsp3",
        Err("Cannot find device \"jsp3\"".to_owned()),
    );
    let egress = started(&fake, config_with("none", None));
    assert_eq!(
        fake.log(),
        [
            "ip -j link show",
            "ip link del jsv0",
            "ip link del jsp3",
            "nft -f - << add table inet js_egress",
        ]
    );
    assert_eq!(egress.config().mode(), Mode::None);
    egress.check().expect("mode none has nothing to check");
    let attachment = egress.attach("run", PID).expect("nothing to wire");
    assert_eq!(attachment.interface(), None);
    drop(attachment);
    assert_eq!(fake.log().len(), 4, "no call for a mode that wires nothing");
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
            "nft -f - << add table inet js_egress",
            "write /proc/sys/net/ipv4/ip_forward 1",
            "nft -f - << add table inet js_egress",
            "nft -j list table inet js_egress",
            "nft -j list table ip js_egress_nat",
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
    let first = egress.attach("one", PID).expect("attach");
    assert_eq!(first.interface(), Some("jsv0"));
    assert_eq!(fake.log()[before..], attach_calls()[..]);
    let batch = fake.state().stdins.last().expect("a batch").clone();
    assert_eq!(
        batch,
        "link set lo up\nlink set eth0 addrgenmode none\naddr add 198.18.0.2/30 dev eth0\n\
         link set eth0 up\nroute add default via 198.18.0.1\n"
    );
    let second = egress.attach("two", PID).expect("attach");
    assert_eq!(second.interface(), Some("jsv1"));
    let after_second = fake.log().len();
    first.release().expect("release");
    assert_eq!(fake.log()[after_second..], release_calls(0)[..]);
    let third = egress.attach("three", PID).expect("attach");
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
        let _attachment = egress.attach("count", PID).expect("attach");
        fake.calls() - before
    };
    assert_eq!(calls, 8);
    for n in 0..calls {
        let fake = Fake::new();
        let egress = started(&fake, allow_list());
        let (before, logged) = (fake.calls(), fake.log().len());
        fake.fail_call(before + n);
        let error = egress.attach("victim", PID).expect_err("fails");
        let text = error.to_string();
        assert!(
            text.contains("wiring run victim") && text.contains("injected"),
            "{n}: {text}"
        );
        let log = fake.log();
        assert!(log[logged..].ends_with(&release_calls(0)), "{n}: {log:?}");
        let again = egress.attach("next", PID).expect("the block is free");
        assert_eq!(again.interface(), Some("jsv0"), "{n}");
    }
}

/// Catches the wiring of a namespace it must not touch: a container that is gone, one in the
/// host's own namespace, an agent that cannot read its own namespace, a namespace that is
/// another one by the time the job end is moved, and one that cannot be read the second
/// time. The first two leave no call behind, the last two undo what was made, and none holds
/// a block.
#[test]
fn a_namespace_that_is_not_the_containers_is_refused() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.log().len();

    fake.state().identities.remove(NET);
    let error = egress.attach("gone", PID).expect_err("gone");
    assert!(
        matches!(error, EgressError::Container { pid: PID, .. }),
        "{error}"
    );

    fake.identity(NET, vec![Some((1, 1))]);
    let error = egress.attach("host", PID).expect_err("the host's own");
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

    fake.identity(NET, vec![Some((1, 2))]);
    fake.state().identities.remove(OWN);
    let error = egress
        .attach("blind", PID)
        .expect_err("own namespace unreadable");
    assert!(
        error
            .to_string()
            .contains("reading the agent's own network namespace"),
        "{error}"
    );
    fake.identity(OWN, vec![Some((1, 1))]);

    let logged = fake.log().len();
    fake.identity(NET, vec![Some((1, 2)), Some((1, 3))]);
    let error = egress.attach("swapped", PID).expect_err("changed");
    assert!(
        error.to_string().contains("its network namespace changed"),
        "{error}"
    );
    assert!(fake.log()[logged..].ends_with(&release_calls(0)));

    fake.identity(NET, vec![Some((1, 2)), None]);
    let error = egress
        .attach("vanished", PID)
        .expect_err("unreadable the second time");
    assert!(matches!(error, EgressError::Container { .. }), "{error}");
    fake.identity(NET, vec![Some((1, 2))]);
    let attachment = egress
        .attach("fine", PID)
        .expect("no block was held by the failures");
    assert_eq!(attachment.interface(), Some("jsv0"));
}

/// Catches IPv6 switches that are errors on a host that has none: a missing IPv6 switch is
/// fine (IPv6 is off), but the same error for the IPv4 switch, and any other error for the
/// IPv6 one, are not.
#[test]
fn a_missing_ipv6_switch_is_tolerated_and_nothing_else_is() {
    let ipv6 = "/proc/sys/net/ipv6/conf/jsv0/disable_ipv6";
    let rp = "/proc/sys/net/ipv4/conf/jsv0/rp_filter";
    for (path, kind, works) in [
        (ipv6, ErrorKind::NotFound, true),
        (rp, ErrorKind::NotFound, false),
        (ipv6, ErrorKind::PermissionDenied, false),
    ] {
        let fake = Fake::new();
        let egress = started(&fake, allow_list());
        fake.state().write_errors.insert(path.to_owned(), kind);
        let result = egress.attach("run", PID);
        assert_eq!(result.is_ok(), works, "{path} {kind:?}: {result:?}");
    }
}

/// Catches a release that fails for what is not a failure, or hides one that is: a set
/// member and a veth that are gone already (the container ended first) release cleanly, a
/// real error is reported (and the block is freed anyway), and an attachment dropped without
/// a release takes itself down once, as does one released and then dropped.
#[test]
fn a_release_forgives_what_is_gone_and_reports_what_is_not() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let first = egress.attach("one", PID).expect("attach");
    fake.answer(
        "delete element",
        Err("Error: No such file or directory".to_owned()),
    );
    fake.answer("link del", Err("Cannot find device \"jsv0\"".to_owned()));
    first.release().expect("gone is not an error");

    let second = egress.attach("two", PID).expect("the block is free");
    fake.answer("link del", Err("Operation not permitted".to_owned()));
    let error = second.release().expect_err("a real error");
    assert!(
        error.to_string().contains("Operation not permitted"),
        "{error}"
    );

    let third = egress
        .attach("three", PID)
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
                .attach(&format!("run{n}"), PID)
                .expect("a free block")
        })
        .collect();
    assert_eq!(held[63].interface(), Some("jsv63"));
    let error = egress.attach("late", PID).expect_err("full");
    assert_eq!(error, EgressError::Exhausted);
    held.remove(5);
    let again = egress.attach("again", PID).expect("one is free");
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
    let error = Egress::start(Config::from_flags(&flags).expect("sound")).expect_err("not JSON");
    assert!(
        error.to_string().contains("listing the interfaces"),
        "{error}"
    );
}
