//! The mounts read back after `create` and before `start`.

use js_agent::lifecycle::Runtime;

use super::super::scripted::{Bind, Change, Scripted, Volume};
use super::super::{docker, in_another_pid_namespace, mount, open, open_with, settings, spec};
use super::create::mountinfo_beside_srv_in;
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
            Box::new(swap(0, |b| b.0 = "/etc".to_owned())),
        ),
        ("made writable", Box::new(swap(0, |b| b.2 = true))),
        (
            "other target",
            Box::new(swap(0, |b| b.1 = "/mnt/data/x".to_owned())),
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
        "[{\"Type\":\"volume\"}]",
        "[{\"Type\":\"volume\",\"Name\":\"v\",\"Destination\":\"/scratch\",\"Driver\":\"local\"}]",
        "[{\"Type\":\"volume\",\"Name\":5,\"Destination\":\"/scratch\",\"Driver\":\"local\",\"RW\":true}]",
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
    world.tamper_volume(|volume| {
        volume
            .options
            .insert("device".to_owned(), "/etc".to_owned());
    });
    world.fail("rm", 1);
    let said = rt.create_and_start(&spec("r5")).unwrap_err();
    assert!(
        said.0
            .contains("; the container could not be removed: rm: "),
        "{}",
        said.0
    );
    assert_eq!(world.ids().len(), 1, "the leak is reported, not hidden");
    assert_eq!(world.volumes().len(), 1, "and so is its volume");
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

/// Asserts that the scratch directory is read back as the volume the agent asked for: the
/// one mount at `/scratch`, of the local driver, writable, whose options (read with
/// `volume inspect`) are exactly `type=none`, the scratch directory as `device` and
/// `o=bind,nosuid,nodev`. A volume that binds without `nosuid` or `nodev`, binds another
/// directory, adds an option, is of another driver, is read-only or is at another
/// destination leaves the container removed, with its volume, and `start` not called.
/// It catches a scratch directory that is writable by a set-uid file or a device node
/// because the engine did not apply what was asked, and one that is not the directory
/// the agent resolved.
#[test]
fn the_scratch_volume_is_read_back_before_the_start() {
    type Case = (&'static str, fn(&mut Volume));
    let cases: [Case; 8] = [
        ("no nosuid", |v| {
            v.options.insert("o".to_owned(), "bind,nodev".to_owned());
        }),
        ("no nodev", |v| {
            v.options.insert("o".to_owned(), "bind,nosuid".to_owned());
        }),
        ("device moved", |v| {
            v.options.insert("device".to_owned(), "/etc".to_owned());
        }),
        ("other type", |v| {
            v.options.insert("type".to_owned(), "nfs".to_owned());
        }),
        ("extra option", |v| {
            v.options.insert("size".to_owned(), "1g".to_owned());
        }),
        ("other driver", |v| v.driver = "other".to_owned()),
        ("read-only", |v| v.rw = false),
        ("other destination", |v| v.dst = "/other".to_owned()),
    ];
    for (name, change) in cases {
        let world = docker();
        let mut rt = open(&world);
        world.tamper_volume(change);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert_eq!(
            said.0, "the mounts of the container are not the ones the agent resolved",
            "{name}"
        );
        assert!(world.ids().is_empty(), "{name}: the container was removed");
        assert!(world.volumes().is_empty(), "{name}: and its volume");
        assert!(world.calls_of("start").is_empty(), "{name}");
        assert_eq!(world.calls_of("rm").len(), 1, "{name}");
    }
}

/// Asserts the call that reads the options of the scratch volume back (`volume inspect`,
/// the options as JSON, the volume's name after `--`, made after the mounts are read and
/// before `start`), and its failures: a call that fails, and text that is not JSON, are
/// errors that remove the container; options that are valid JSON and not the asked ones
/// are a mismatch. It catches a read-back that passes when it could not look.
#[test]
fn the_scratch_volume_read_back_and_its_failures() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    let volume = world.volume_of(&id.0).unwrap();
    assert_eq!(volume.driver, "local");
    assert_eq!(volume.dst, "/scratch");
    let name = world.volumes().pop().unwrap();
    assert_eq!(
        world.calls_of("volume"),
        [strings(&[
            "volume",
            "inspect",
            "--format",
            "{{json .Options}}",
            "--",
            &name
        ])]
    );

    for (printed, expected) in [
        ("garbage", "volume: unreadable output"),
        (
            "null",
            "the mounts of the container are not the ones the agent resolved",
        ),
        (
            "{\"device\":5}",
            "the mounts of the container are not the ones the agent resolved",
        ),
    ] {
        let world = docker();
        let mut rt = open(&world);
        world.say("volume", printed);
        let said = rt.create_and_start(&spec("r2")).unwrap_err();
        assert_eq!(said.0, expected, "{printed:?}");
        assert!(world.ids().is_empty(), "{printed:?}");
        assert!(world.calls_of("start").is_empty(), "{printed:?}");
    }

    let world = docker();
    let mut rt = open(&world);
    world.fail("volume", 1);
    let said = rt.create_and_start(&spec("r3")).unwrap_err();
    assert_eq!(said.0, "volume: /bin/sh: exit status: 1");
    assert!(world.ids().is_empty());
}

/// Asserts that the scratch volume is removed with the container: the volume exists
/// while the container does, is the one the agent asked for, and is gone after `remove`,
/// which is `rm --force --volumes` (the volume is anonymous, so no `volume rm` is
/// needed and none is made). It catches a volume left behind for every job.
#[test]
fn the_scratch_volume_goes_with_the_container() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    assert_eq!(world.volumes().len(), 1);
    let volume = world.volume_of(&id.0).unwrap();
    let options: Vec<(&str, &str)> = volume
        .options
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        options,
        [
            ("device", "/work/scratch/r1"),
            ("o", "bind,nosuid,nodev"),
            ("type", "none")
        ]
    );
    rt.remove(&id).unwrap();
    assert!(world.volumes().is_empty());
    assert_eq!(
        world.calls_of("rm"),
        [strings(&["rm", "--force", "--volumes", "--", &id.0])]
    );
    assert!(world.calls_of("volume").len() == 1, "no `volume rm`");
}

/// Asserts that on Podman the options of every bind are read back too: the scratch bind
/// and each job mount were asked `nosuid,nodev`, and `.Mounts[].Options` must name both.
/// A bind that lacks either one, that has no `Options` at all, or whose `Options` is not
/// a list leaves the container removed, `start` not called and the error saying the
/// mounts differ; binds that carry both (with other options besides) are started. It
/// catches an engine that ignored an option it was given, and a read-back that assumes
/// the option instead of looking.
#[test]
fn podman_binds_are_read_back_with_nosuid_and_nodev() {
    type Case = (&'static str, fn(&mut Vec<Vec<String>>));
    let cases: [Case; 4] = [
        ("scratch without nosuid", |o| o[0].retain(|x| x != "nosuid")),
        ("scratch without nodev", |o| o[0].retain(|x| x != "nodev")),
        ("job bind without nosuid", |o| {
            o[1].retain(|x| x != "nosuid")
        }),
        ("job bind without nodev", |o| o[1].retain(|x| x != "nodev")),
    ];
    for (name, change) in cases {
        let world = Scripted::new("podman version 5.2.1");
        let mut rt = open_with(&world, "linux", settings())
            .with_proc_files(mountinfo_beside_srv_in, in_another_pid_namespace);
        world.tamper_options(change);
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

    let scratch = "/work/scratch/r1";
    let bind = |options: &str| {
        format!(
            r#"[{{"Type":"bind","Source":"{scratch}","Destination":"/scratch","RW":true{options}}}]"#
        )
    };
    for printed in [
        bind(""),
        bind(r#","Options":"nosuid,nodev""#),
        bind(r#","Options":["nosuid",5]"#),
    ] {
        let world = Scripted::new("podman version 5.2.1");
        let mut rt = open_with(&world, "linux", settings())
            .with_proc_files(mountinfo_beside_srv_in, in_another_pid_namespace);
        world.say_once("inspect", &printed);
        let said = rt.create_and_start(&spec("r2")).unwrap_err();
        assert_eq!(
            said.0, "the mounts of the container are not the ones the agent resolved",
            "{printed}"
        );
        assert!(world.ids().is_empty(), "{printed}");
    }

    let world = Scripted::new("podman version 5.2.1");
    let mut rt = open_with(&world, "linux", settings())
        .with_proc_files(mountinfo_beside_srv_in, in_another_pid_namespace);
    world.tamper_options(|o| o[0].extend(["rprivate".to_owned(), "relatime".to_owned()]));
    rt.create_and_start(&spec("r3")).unwrap();
    assert_eq!(world.calls_of("start").len(), 1);
}
