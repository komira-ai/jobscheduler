//! What is made for a run is removed when the run ends, however it ends, and two runs never
//! share anything; and the same through the agent's `CrunRuntime`, which makes the calls.

use std::net::SocketAddr;
use std::time::Duration;

use js_agent::egress::{Config, Egress, EgressError, Flags};
use js_agent::lifecycle::{Exit, RunHandle, Runtime, Status};
use js_agent_wire::LogTail;
use js_types::JobNetwork;

use crate::container::{PROBE, RuntimeHost, section};
use crate::sys::{
    Far, Holder, Reach, Server, allow_list, eventually, far, ip, ip_in, links, named_namespaces,
    nft, packets, serial, tcp, text, wired,
};

fn config() -> Config {
    allow_list(&["192.0.2.10:9300"], &[])
}

/// Asserts the cleanup of a run that ends well: while the run is wired the host has its
/// veth end and the firewall set has its name; releasing it removes both, nothing is added
/// to `/run/netns` at any point (the container's namespace is the kernel's, not a name on
/// disk), and the next run gets the first block of the pool again, which works.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn releasing_a_run_removes_its_veth_and_its_set_member() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let named = named_namespaces();
    let egress = Egress::start(config()).expect("start");
    assert!(links("jsv").is_empty() && wired().is_empty());
    let holder = Holder::new();
    let attachment = egress
        .attach("one", holder.pid(), JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(links("jsv"), ["jsv0"]);
    assert_eq!(wired(), ["jsv0"]);
    assert_eq!(named_namespaces(), named);
    attachment.release().expect("release");
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert!(wired().is_empty());
    assert_eq!(named_namespaces(), named);
    let again = egress
        .attach("two", holder.pid(), JobNetwork::AllowList)
        .expect("the first block is free again");
    assert_eq!(again.interface(), Some("jsv0"));
    assert!(tcp(&holder.ns, far::ALLOWED).connected());
    assert_eq!(allowed.accepted(), 1);
}

/// Asserts the cleanup of a run that ends by itself: when the container's last process
/// goes, the kernel frees its namespace and with it the job end of the veth, and the host end
/// goes too; what is left is the set member, and releasing the run after the fact succeeds
/// (the veth is already gone, which is not an error) and removes it.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn a_run_that_ends_leaves_only_a_set_member_and_release_removes_it() {
    let _serial = serial();
    let _far = Far::new();
    let egress = Egress::start(config()).expect("start");
    let holder = Holder::new();
    let attachment = egress
        .attach("ended", holder.pid(), JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(links("jsv"), ["jsv0"]);
    drop(holder);
    assert!(
        eventually(Duration::from_secs(10), || links("jsv").is_empty()),
        "the host end outlived the namespace: {:?}",
        links("jsv")
    );
    assert_eq!(wired(), ["jsv0"], "the set member is the agent's to remove");
    attachment
        .release()
        .expect("a release after the namespace ended is not an error");
    assert!(wired().is_empty());
}

/// Asserts the restart sweep: a run that was wired when the agent died (its attachment never
/// released, nothing of the old process left) and a pair that was made but never moved are
/// removed by the next start, along with the set's members, while the process that was in
/// the old namespace is untouched; the new agent then wires the same namespace again from the
/// first block.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn a_restart_removes_what_a_crash_left() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let old = Egress::start(config()).expect("start");
    let holder = Holder::new();
    // What a run wired when its agent died: the host end, the job end in the run's
    // namespace and the set member. A crash leaves nothing of the process, its hold on the
    // host included; dropping the old egress, which holds no run, stands for that.
    ip(&[
        "link", "add", "jsv0", "type", "veth", "peer", "name", "jsp0",
    ]);
    ip(&["link", "set", "jsp0", "netns", &holder.pid().to_string()]);
    nft(&[
        "add",
        "element",
        "inet",
        "js_crun_egress",
        "jobifs",
        "{ \"jsv0\" }",
    ]);
    assert_eq!(wired(), ["jsv0"]);
    drop(old);
    ip(&[
        "link", "add", "jsv7", "type", "veth", "peer", "name", "jsp7",
    ]);
    // Names that only look like the agent's are not its.
    ip(&[
        "link", "add", "jspare0", "type", "veth", "peer", "name", "jspare1",
    ]);
    assert_eq!(links("jsv"), ["jsv0", "jsv7"]);
    assert_eq!(links("jsp"), ["jsp7", "jspare0", "jspare1"]);

    let new = Egress::start(config()).expect("a restart");
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert_eq!(
        links("jsp"),
        ["jspare0", "jspare1"],
        "only the numbered ones went"
    );
    ip(&["link", "del", "jspare0"]);
    assert!(wired().is_empty(), "{:?}", wired());
    assert!(std::path::Path::new(&format!("/proc/{}", holder.pid())).exists());
    let attachment = new
        .attach("again", holder.pid(), JobNetwork::AllowList)
        .expect("wired again");
    assert_eq!(attachment.interface(), Some("jsv0"));
    assert!(tcp(&holder.ns, far::ALLOWED).connected());
    assert_eq!(allowed.accepted(), 1);
}

/// Asserts that wiring refuses what it must not touch and leaves nothing behind when it
/// fails: a pid whose network namespace is the host's own (wiring it would put the job on the
/// host's network and the veth in the host's own namespace) and a pid that is not there are
/// refused with the container error, no interface is made, and the block that was taken is
/// free for the next run.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn wiring_refuses_the_hosts_own_namespace_and_a_missing_process() {
    let _serial = serial();
    let _far = Far::new();
    let egress = Egress::start(config()).expect("start");
    let own = egress.attach("own", std::process::id(), JobNetwork::AllowList);
    assert!(matches!(own, Err(EgressError::Container { .. })), "{own:?}");
    let missing = egress.attach("missing", u32::MAX, JobNetwork::AllowList);
    assert!(
        matches!(missing, Err(EgressError::Container { .. })),
        "{missing:?}"
    );
    assert!(links("jsv").is_empty() && links("jsp").is_empty());
    assert!(wired().is_empty());
    let holder = Holder::new();
    let attachment = egress
        .attach("fine", holder.pid(), JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(
        attachment.interface(),
        Some("jsv0"),
        "the failed attempts released their block"
    );
}

/// Asserts that two runs at once are isolated. They get different blocks, interfaces and
/// addresses; both reach the allowed endpoint; neither reaches a listener in the other (the
/// other's address is not allowed, so the firewall refuses it, and the listeners count
/// nothing); and ending one leaves the other wired and working.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn two_runs_at_once_cannot_reach_each_other() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let egress = Egress::start(config()).expect("start");
    let (a, b) = (Holder::new(), Holder::new());
    let first = egress
        .attach("a", a.pid(), JobNetwork::AllowList)
        .expect("attach a");
    let second = egress
        .attach("b", b.pid(), JobNetwork::AllowList)
        .expect("attach b");
    assert_eq!(links("jsv"), ["jsv0", "jsv1"]);
    assert_eq!(wired(), ["jsv0", "jsv1"]);
    let address = |holder: &Holder| {
        text(&ip_in(
            &holder.ns,
            &["-4", "-o", "addr", "show", "dev", "eth0"],
        ))
    };
    assert!(address(&a).contains("198.18.0.2/30"), "{}", address(&a));
    assert!(address(&b).contains("198.18.0.6/30"), "{}", address(&b));

    let on_a = Server::tcp(&a.ns, "0.0.0.0:7000".parse::<SocketAddr>().unwrap());
    let on_b = Server::tcp(&b.ns, "0.0.0.0:7000".parse::<SocketAddr>().unwrap());
    assert!(tcp(&a.ns, far::ALLOWED).connected());
    assert!(tcp(&b.ns, far::ALLOWED).connected());
    assert_eq!(allowed.accepted(), 2);
    let refused = packets("inbound");
    assert!(matches!(
        tcp(&a.ns, "198.18.0.6:7000".parse().unwrap()),
        Reach::Failed(_)
    ));
    assert!(matches!(
        tcp(&b.ns, "198.18.0.2:7000".parse().unwrap()),
        Reach::Failed(_)
    ));
    assert_eq!(
        on_a.accepted() + on_b.accepted(),
        0,
        "a run reached another run's listener"
    );
    assert!(
        packets("inbound") >= refused + 2,
        "the firewall refused both, as traffic to another job"
    );

    first.release().expect("release a");
    assert_eq!(links("jsv"), ["jsv1"]);
    assert_eq!(wired(), ["jsv1"]);
    assert!(
        tcp(&b.ns, far::ALLOWED).connected(),
        "b stopped working when a ended"
    );
    assert_eq!(allowed.accepted(), 3);
    drop(second);
}

/// Asserts that one agent at a time holds the host's egress: while an agent's egress lives
/// (with a run wired), a second start, as of another agent with its own host id and scratch
/// root, is refused as `Held` and removes nothing (the first run's veth and set member stay,
/// and it still reaches the allowed endpoint). The hold lasts while the run is wired even
/// after the egress itself is dropped (the second start is still `Held`), and ends with the
/// run's release, after which a start succeeds. Catches a second agent that sweeps the first
/// one's live wiring, and a hold that ends before the last run's release.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn a_second_agent_cannot_start_while_one_holds_the_hosts_egress() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let first = Egress::start(config()).expect("start");
    let holder = Holder::new();
    let attachment = first
        .attach("first", holder.pid(), JobNetwork::AllowList)
        .expect("attach");
    let second = Egress::start(config()).map(drop);
    assert_eq!(second, Err(EgressError::Held));
    assert_eq!(links("jsv"), ["jsv0"]);
    assert_eq!(wired(), ["jsv0"]);
    assert!(tcp(&holder.ns, far::ALLOWED).connected());
    assert_eq!(allowed.accepted(), 1);
    drop(first);
    let still = Egress::start(config()).map(drop);
    assert_eq!(
        still,
        Err(EgressError::Held),
        "the wired run holds the host"
    );
    assert_eq!(links("jsv"), ["jsv0"]);
    attachment.release().expect("release");
    let next = Egress::start(config()).expect("free once the last run is released");
    next.check().expect("loaded");
}

/// Waits for the run `id` to end, reads its end through the runtime (which releases its
/// wiring), and returns the end and what the job printed.
fn ended(host: &mut RuntimeHost, id: &RunHandle) -> (Status, LogTail) {
    host.runtime.wait_ended(id);
    let status = host.runtime.poll(id).expect("polled");
    let tail = host
        .runtime
        .log_tail(id)
        .expect("a tail")
        .unwrap_or_default();
    (status, tail)
}

/// The probe line of `target` in the section `reach` of `out`.
fn probed(out: &str, target: &str) -> String {
    section(out, "reach")
        .lines()
        .find(|line| line.starts_with(&format!("reach {target} ")))
        .unwrap_or_else(|| panic!("no probe of {target} in\n{out}"))
        .to_owned()
}

/// Asserts the allow-list end to end through the agent's runtime: the firewall checks as
/// loaded through `Runtime::check_egress` (which the lifecycle calls before a create), and a
/// job started by `CrunRuntime::create_and_start` (which starts the container, asks for its
/// pid and wires it while the init holds the job) connects to the allowed endpoint
/// on the far host, and the far host's other address and the metadata address are refused
/// (neither listener sees a connection); the job ends well, and its removal leaves no veth
/// and no set member. Catches a runtime that starts its jobs with loopback only, or wired
/// beside the firewall.
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn a_crun_job_through_the_runtime_reaches_an_allowed_endpoint_and_not_a_denied_one() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let other_host = Server::tcp(&far_host.ns, far::OTHER_HOST);
    let metadata = Server::tcp(&far_host.ns, far::METADATA);
    let egress = Egress::start(config()).expect("start");
    let mut host = RuntimeHost::new("reach", &egress);
    host.runtime
        .check_egress()
        .expect("the firewall is as loaded");
    let script = format!(
        "{PROBE}echo '--reach'; p 192.0.2.10 9300; p 192.0.2.40 9300; p 169.254.169.254 80; \
         echo '--end'"
    );
    let spec = host.spec("reach", &script);
    let id = host
        .runtime
        .create_and_start(&spec)
        .unwrap_or_else(|why| panic!("started: {why}"));
    let (status, tail) = ended(&mut host, &id);
    assert!(
        matches!(status, Status::Exited(Exit { code: 0, .. })),
        "{status:?}: {tail:?}"
    );
    let out = &tail.stdout;
    assert!(probed(out, "192.0.2.10:9300").contains("pong"), "{tail:?}");
    for refused in ["192.0.2.40:9300", "169.254.169.254:80"] {
        let line = probed(out, refused);
        assert!(!line.contains("pong"), "{line}");
    }
    assert!(allowed.accepted() >= 1);
    assert_eq!(other_host.accepted() + metadata.accepted(), 0);
    assert!(packets("metadata") >= 1, "the metadata range, by name");
    host.runtime.remove(&id).expect("removed");
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert!(wired().is_empty(), "{:?}", wired());
}

/// Asserts that the runtime leaves nothing of a run's network on the host, however the run
/// goes: a job that ends by itself (it writes a marker to its scratch directory first, the
/// control of the test below) leaves no veth and no set member once the runtime has read
/// its end, before it is removed; one that is removed while it runs is wired while it runs
/// and leaves neither after its removal. Catches a runtime path that forgets to release.
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn a_crun_runtime_job_leaves_no_veth_and_no_set_member_after_removal() {
    let _serial = serial();
    let _far = Far::new();
    let egress = Egress::start(config()).expect("start");
    let mut host = RuntimeHost::new("clean", &egress);
    let spec = host.spec("ends", "echo ran > /scratch/marker");
    let id = host
        .runtime
        .create_and_start(&spec)
        .unwrap_or_else(|why| panic!("started: {why}"));
    let (status, tail) = ended(&mut host, &id);
    assert!(
        matches!(status, Status::Exited(Exit { code: 0, .. })),
        "{status:?}: {tail:?}"
    );
    assert!(spec.scratch.join("marker").exists(), "the job ran");
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert!(wired().is_empty(), "released at its end: {:?}", wired());
    host.runtime.remove(&id).expect("removed");

    let spec = host.spec("held", "sleep 600");
    let id = host
        .runtime
        .create_and_start(&spec)
        .unwrap_or_else(|why| panic!("started: {why}"));
    assert_eq!(links("jsv"), ["jsv0"]);
    assert_eq!(wired(), ["jsv0"]);
    host.runtime.remove(&id).expect("removed while it runs");
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert!(wired().is_empty(), "{:?}", wired());
}

/// Asserts that a container the runtime cannot wire never starts its job: with an `nsenter`
/// that fails (the last step of the wiring, which adds the route) the start fails naming the
/// step, the job's marker is never written (the init held the job, and the runtime ended
/// the container), nothing of the wiring is left on the host, and the runtime holds no run.
/// The test above is the control: the same job, wired, writes it. Catches a job that runs
/// unwired, and a failed start that leaves its container or its veth.
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn a_failed_wiring_never_starts_the_job() {
    let _serial = serial();
    let _far = Far::new();
    let allow = vec!["192.0.2.10:9300".to_owned()];
    let config = Config::from_flags(&Flags {
        mode: Some("allow-list"),
        allow: &allow,
        nsenter: Some("/bin/false"),
        ..Flags::default()
    })
    .expect("a sound configuration");
    let egress = Egress::start(config).expect("start");
    let mut host = RuntimeHost::new("unwired", &egress);
    let spec = host.spec("unwired", "echo ran > /scratch/marker");
    let why = host
        .runtime
        .create_and_start(&spec)
        .map(|_| ())
        .expect_err("the wiring fails");
    assert!(why.to_string().contains("configuring the job end"), "{why}");
    assert!(!spec.scratch.join("marker").exists(), "the job ran unwired");
    assert!(links("jsv").is_empty() && links("jsp").is_empty());
    assert!(wired().is_empty(), "{:?}", wired());
    assert!(host.runtime.list_labelled().expect("listed").is_empty());
}

/// Asserts that runs the runtime holds at once each keep their own wiring: two running jobs
/// have a veth and a set member each; removing the first takes only its own, and the second
/// keeps its; a third job then gets the first one's block again. Catches a removal that takes
/// another run's wiring, and a block that is never given back.
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn runtime_jobs_at_once_keep_their_own_wiring() {
    let _serial = serial();
    let _far = Far::new();
    let egress = Egress::start(config()).expect("start");
    let mut host = RuntimeHost::new("overlap", &egress);
    let start = |host: &mut RuntimeHost, run: &str| {
        let spec = host.spec(run, "sleep 600");
        host.runtime
            .create_and_start(&spec)
            .unwrap_or_else(|why| panic!("started {run}: {why}"))
    };
    let first = start(&mut host, "first");
    let second = start(&mut host, "second");
    assert_eq!(links("jsv"), ["jsv0", "jsv1"]);
    assert_eq!(wired(), ["jsv0", "jsv1"]);
    host.runtime.remove(&first).expect("removed");
    assert_eq!(links("jsv"), ["jsv1"], "the second run keeps its veth");
    assert_eq!(wired(), ["jsv1"], "and its set member");
    let third = start(&mut host, "third");
    assert_eq!(
        links("jsv"),
        ["jsv0", "jsv1"],
        "the first block is free again"
    );
    for id in [&second, &third] {
        host.runtime.remove(id).expect("removed");
    }
    assert!(links("jsv").is_empty() && wired().is_empty());
}
