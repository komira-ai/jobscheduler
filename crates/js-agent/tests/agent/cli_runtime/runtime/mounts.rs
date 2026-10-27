//! The mounts read back after `create` and before `start`.

use js_agent::lifecycle::ContainerRuntime;

use super::super::scripted::{Bind, Change};
use super::super::{docker, mount, open, spec};
use super::{start, strings};

/// Asserts that the mounts are read back after `create` and before `start`: every bind
/// the daemon made has the source the agent resolved, the destination and the access it
/// asked for, and there is no bind it did not ask for. A job that swapped a directory of
/// a mount root for a link while the daemon bound it (the daemon binds by path, and the
/// agent cannot give it a descriptor) leaves a different source; the container is then
/// removed, `start` is not called and the error says the mounts differ. Other mounts
/// (the tmpfs) are not looked at. It catches a mount outside its root that goes
/// unnoticed.
#[test]
fn the_mounts_are_read_back_before_the_start() {
    let swap = |index: usize, change: fn(&mut Bind)| {
        move |binds: &mut Vec<Bind>| change(&mut binds[index])
    };
    let cases: Vec<(&str, Change)> = vec![
        (
            "source swapped",
            Box::new(swap(1, |b| b.0 = "/etc".to_owned())),
        ),
        (
            "scratch moved",
            Box::new(swap(0, |b| b.0 = "/home/x".to_owned())),
        ),
        ("made writable", Box::new(swap(1, |b| b.2 = true))),
        (
            "other target",
            Box::new(swap(1, |b| b.1 = "/mnt/data/x".to_owned())),
        ),
        (
            "extra bind",
            Box::new(|binds: &mut Vec<Bind>| {
                binds.push(("/".to_owned(), "/host".to_owned(), true))
            }),
        ),
        (
            "bind missing",
            Box::new(|binds: &mut Vec<Bind>| {
                binds.pop();
            }),
        ),
    ];
    for (name, change) in cases {
        let world = docker();
        let mut rt = open(&world);
        world.tamper(change);
        let mut job = spec("r1");
        job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
        let said = rt.create_and_start(&job).unwrap_err();
        assert_eq!(
            said.0, "the mounts of the container are not the ones the agent resolved",
            "{name}"
        );
        assert!(world.ids().is_empty(), "{name}: the container was removed");
        assert!(world.calls_of("start").is_empty(), "{name}");
        assert_eq!(world.calls_of("rm").len(), 1, "{name}");
    }
}

/// Asserts the call that reads the mounts back (the container type, the format that asks
/// for them as JSON, the id after `--`), that a container whose mounts are as asked is
/// started, and what happens when the answer cannot be used: not JSON, entries without
/// the fields, a container the runtime lost in between, or a removal that fails after a
/// mismatch. It catches a read-back that trusts an answer it did not understand.
#[test]
fn the_mount_read_back_and_its_failures() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    let inspect = &world.calls_of("inspect")[0];
    assert_eq!(
        inspect[..],
        strings(&[
            "inspect",
            "--type",
            "container",
            "--format",
            "{{json .Mounts}}",
            "--",
            &id.0
        ])[..]
    );

    for printed in [
        "garbage",
        "{\"a\":1}",
        "[1]",
        "[{}]",
        "[{\"Type\":\"bind\"}]",
        "[{\"Type\":\"bind\",\"Source\":5,\"Destination\":\"/b\",\"RW\":true}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"RW\":true}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"Destination\":\"/b\"}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"Destination\":\"/b\",\"RW\":\"yes\"}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"RW\":true,\"Destination\":5}]",
        "[{\"Type\":1,\"Source\":\"/a\",\"Destination\":\"/b\",\"RW\":true}]",
    ] {
        let world = docker();
        let mut rt = open(&world);
        world.say("inspect", printed);
        let said = rt.create_and_start(&spec("r2")).unwrap_err();
        assert_eq!(said.0, "inspect: unreadable output", "{printed:?}");
        assert!(world.ids().is_empty(), "{printed:?}");
    }

    let world = docker();
    let mut rt = open(&world);
    world.say("inspect", "null\n");
    let said = rt.create_and_start(&spec("r3")).unwrap_err();
    assert_eq!(
        said.0,
        "the mounts of the container are not the ones the agent resolved"
    );

    let world = docker();
    let mut rt = open(&world);
    world.fail("inspect", 1);
    let said = rt.create_and_start(&spec("r4")).unwrap_err();
    assert_eq!(said.0, "inspect: /bin/sh: exit status: 1");
    assert!(world.ids().is_empty(), "removed when the read-back failed");

    // The runtime lost the container between the create and the read-back.
    let world = docker();
    let mut rt = open(&world);
    world.fail("inspect", 1);
    world.say("ps", "");
    let said = rt.create_and_start(&spec("r6")).unwrap_err();
    assert_eq!(said.0, "inspect: unreadable output");
    assert!(world.calls_of("start").is_empty());

    let world = docker();
    let mut rt = open(&world);
    world.tamper(|binds| binds[0].0 = "/etc".to_owned());
    world.fail("rm", 1);
    let said = rt.create_and_start(&spec("r5")).unwrap_err();
    assert!(
        said.0
            .contains("; the container could not be removed: rm: "),
        "{}",
        said.0
    );
    assert_eq!(world.ids().len(), 1, "the leak is reported, not hidden");
}

/// Asserts that the read-back compares the binds as a set: an engine that lists them in
/// another order than the agent asked for them (here the reverse, and also the order the
/// agent asked in, which is not the sorted order) is no mismatch. It catches a comparison
/// of two lists in the order they happen to come in.
#[test]
fn the_order_of_the_binds_does_not_matter() {
    let reversed: [fn(&mut Vec<Bind>); 2] = [|binds| binds.reverse(), |_| ()];
    for reorder in reversed {
        let world = docker();
        let mut rt = open(&world);
        world.tamper(reorder);
        let mut job = spec("r1");
        job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
        rt.create_and_start(&job).unwrap();
        assert_eq!(world.calls_of("start").len(), 1);
    }
}
