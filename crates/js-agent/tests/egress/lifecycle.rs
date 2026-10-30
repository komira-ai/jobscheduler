//! What is made for a run is removed when the run ends, however it ends, and two runs never
//! share anything.

use std::net::SocketAddr;
use std::time::Duration;

use js_agent::egress::{Config, Egress, EgressError};

use crate::sys::{
    Far, Holder, Reach, Server, allow_list, eventually, far, ip, ip_in, links, named_namespaces,
    packets, serial, tcp, text, wired,
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
    let attachment = egress.attach("one", holder.pid()).expect("attach");
    assert_eq!(links("jsv"), ["jsv0"]);
    assert_eq!(wired(), ["jsv0"]);
    assert_eq!(named_namespaces(), named);
    attachment.release().expect("release");
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert!(wired().is_empty());
    assert_eq!(named_namespaces(), named);
    let again = egress
        .attach("two", holder.pid())
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
    let attachment = egress.attach("ended", holder.pid()).expect("attach");
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
    let leaked = old.attach("crashed", holder.pid()).expect("attach");
    std::mem::forget(leaked);
    drop(old);
    ip(&[
        "link", "add", "jsv7", "type", "veth", "peer", "name", "jsp7",
    ]);
    assert_eq!(links("jsv"), ["jsv0", "jsv7"]);
    assert_eq!(links("jsp"), ["jsp7"]);

    let new = Egress::start(config()).expect("a restart");
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert!(links("jsp").is_empty(), "{:?}", links("jsp"));
    assert!(wired().is_empty(), "{:?}", wired());
    assert!(std::path::Path::new(&format!("/proc/{}", holder.pid())).exists());
    let attachment = new.attach("again", holder.pid()).expect("wired again");
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
    let own = egress.attach("own", std::process::id());
    assert!(matches!(own, Err(EgressError::Container { .. })), "{own:?}");
    let missing = egress.attach("missing", u32::MAX);
    assert!(
        matches!(missing, Err(EgressError::Container { .. })),
        "{missing:?}"
    );
    assert!(links("jsv").is_empty() && links("jsp").is_empty());
    assert!(wired().is_empty());
    let holder = Holder::new();
    let attachment = egress.attach("fine", holder.pid()).expect("attach");
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
    let first = egress.attach("a", a.pid()).expect("attach a");
    let second = egress.attach("b", b.pid()).expect("attach b");
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
        packets("default-reject") >= refused + 2,
        "the firewall refused both"
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
