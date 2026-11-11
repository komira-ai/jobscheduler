//! The checks of a job's layers, mounts and host ports (`rootfs.rs`), called directly: the
//! bound of each rule at both sides, the fault each refusal names, and the explicit form each
//! default is stored in.

use super::*;

/// A layer from the host path `path`.
fn host_layer(path: &str) -> v1::Layer {
    v1::Layer {
        source: Some(v1::layer::Source::HostPath(path.to_owned())),
    }
}

/// A layer from the image `image`.
fn image_layer(image: &str) -> v1::Layer {
    v1::Layer {
        source: Some(v1::layer::Source::Image(image.to_owned())),
    }
}

/// A mount of the host path `/src` at `target`, every other field unset.
fn mount_at(target: &str) -> v1::Mount {
    v1::Mount {
        source: Some(v1::mount::Source::HostPath("/src".to_owned())),
        target: target.to_owned(),
        ..v1::Mount::default()
    }
}

/// A mount of `/src` at `/data`, changed by `change`.
fn mount_with(change: impl FnOnce(&mut v1::Mount)) -> v1::Mount {
    let mut mount = mount_at("/data");
    change(&mut mount);
    mount
}

/// A host port, with the job port when given.
fn port(host: u32, job: Option<u32>) -> v1::HostPort {
    v1::HostPort {
        host_port: host,
        container_port: job,
        protocol: v1::PortProtocol::Unspecified.into(),
    }
}

/// An absolute path of exactly `len` bytes.
fn path_of(len: usize) -> String {
    format!("/{}", "a".repeat(len - 1))
}

/// A declared forward from `host` to `container`.
fn declared(host: u16, container: u16) -> DeclaredPort {
    DeclaredPort { host, container }
}

/// The fault of the mounts `list`.
fn mounts_fault(list: &[v1::Mount]) -> Fault {
    mounts(list).expect_err("the mounts are refused")
}

/// The fault of the host ports `list`, beside no declared forward.
fn ports_fault(list: &[v1::HostPort]) -> Fault {
    host_ports(list, &[]).expect_err("the host ports are refused")
}

/// Asserts that 16 layers are taken in order, each as its source, and 17 are refused naming
/// `spec.layers`. It catches a cap off by one, and layers reordered or dropped.
#[test]
fn sixteen_layers_are_taken_and_seventeen_refused() {
    let image = format!("team/app:v1@sha256:{}", "b".repeat(64));
    let sixteen: Vec<v1::Layer> = (0..16)
        .map(|at| {
            if at % 2 == 0 {
                host_layer(&format!("/layer/{at}"))
            } else {
                image_layer(&image)
            }
        })
        .collect();
    let taken = layers(&sixteen).unwrap();
    assert_eq!(taken.len(), 16);
    assert_eq!(
        taken[0],
        Layer::HostPath {
            path: "/layer/0".to_owned()
        }
    );
    assert_eq!(taken[1], Layer::Image { image });
    let mut seventeen = sixteen;
    seventeen.push(host_layer("/layer/16"));
    assert_eq!(layers(&seventeen), Err(Fault::Field("spec.layers")));
    assert_eq!(layers(&[]), Ok(Vec::new()));
}

/// Asserts that a layer with no source is refused as `NoSource` naming its index, the
/// first bad entry being the one named. It catches an empty layer taken as a host path of "".
#[test]
fn a_layer_without_a_source_is_refused_at_its_index() {
    let empty = v1::Layer { source: None };
    assert_eq!(
        layers(&[host_layer("/a"), empty.clone()]),
        Err(Fault::NoSource("layers", 1))
    );
    assert_eq!(
        layers(&[empty, host_layer("relative")]),
        Err(Fault::NoSource("layers", 0))
    );
}

/// Asserts the host paths of a layer: normal-form absolute paths of up to 4096 bytes are
/// taken, and a relative one, an empty one, one with a trailing `/`, `//`, `.` or `..`, a NUL
/// or 4097 bytes is refused naming `hostPath` at its index. It catches a path that would
/// escape its root or be opened as another directory than the one named.
#[test]
fn a_layer_host_path_is_absolute_and_in_normal_form() {
    for good in ["/a", "/a/b", "/a:b,c", &path_of(4096)] {
        let taken = layers(&[host_layer(good)]).unwrap();
        assert_eq!(
            taken,
            [Layer::HostPath {
                path: good.to_owned()
            }]
        );
    }
    let long = path_of(4097);
    for bad in [
        "", "a", "a/b", "/", "/a/", "//a", "/a//b", "/a/./b", "/.", "/a/..", "/../a", "/a\0b",
        &long,
    ] {
        assert_eq!(
            layers(&[host_layer("/ok"), host_layer(bad)]),
            Err(Fault::Entry("layers", 1, "hostPath")),
            "{bad:?}"
        );
    }
}

/// Asserts that a layer image is a digest reference taken as given, and that a tag without a
/// digest, an empty image and a malformed digest are refused naming `image`. It catches a
/// floating tag taken as a pinned image.
#[test]
fn a_layer_image_must_be_pinned_by_digest() {
    let pinned = format!("registry.example/app@sha256:{}", "c".repeat(64));
    assert_eq!(
        layers(&[image_layer(&pinned)]),
        Ok(vec![Layer::Image {
            image: pinned.clone()
        }])
    );
    for bad in [
        "registry.example/app:latest",
        "",
        "registry.example/app@sha256:abc",
    ] {
        assert_eq!(
            layers(&[image_layer(bad)]),
            Err(Fault::Entry("layers", 0, "image")),
            "{bad:?}"
        );
    }
}

/// Asserts that 64 mounts are taken and 65 refused naming `spec.mounts`. It catches a cap
/// off by one.
#[test]
fn sixty_four_mounts_are_taken_and_sixty_five_refused() {
    let sixty_four: Vec<v1::Mount> = (0..64).map(|at| mount_at(&format!("/m{at}"))).collect();
    assert_eq!(mounts(&sixty_four).unwrap().len(), 64);
    let mut sixty_five = sixty_four;
    sixty_five.push(mount_at("/m64"));
    assert_eq!(mounts(&sixty_five), Err(Fault::Field("spec.mounts")));
    assert_eq!(mounts(&[]), Ok(Vec::new()));
}

/// Asserts that a mount with no source is refused as `NoSource` at its index, and that a
/// host path of a mount is held to the rules of a layer's, for both sides of 4096 bytes. It
/// catches a mount source that skips the path rules.
#[test]
fn a_mount_needs_a_source_and_a_host_path_in_normal_form() {
    let none = v1::Mount {
        source: None,
        ..mount_at("/data")
    };
    assert_eq!(
        mounts(&[mount_at("/ok"), none]),
        Err(Fault::NoSource("mounts", 1))
    );
    let from = |path: &str| v1::Mount {
        source: Some(v1::mount::Source::HostPath(path.to_owned())),
        ..mount_at("/data")
    };
    for good in ["/a", "/a/b", path_of(4096).as_str()] {
        let taken = mounts(&[from(good)]).unwrap();
        assert_eq!(
            taken[0].source,
            MountSource::HostPath {
                path: good.to_owned()
            }
        );
    }
    let long = path_of(4097);
    for bad in [
        "", "a", "/", "/a/", "//a", "/a/./b", "/a/../b", "/a\0b", &long,
    ] {
        assert_eq!(
            mounts(&[from(bad)]),
            Err(Fault::Entry("mounts", 0, "hostPath")),
            "{bad:?}"
        );
    }
}

/// Asserts the targets taken: an absolute path in normal form of up to 4096 bytes, its
/// leading `/` counted, a path below another mount's, and one beside a runtime place that
/// only shares a prefix with it. It catches a limit counted without the `/`, and a prefix
/// test that refuses `/tmpfiles` as if it were `/tmp`.
#[test]
fn a_target_is_taken_at_4096_bytes_and_below_another_mount() {
    for good in [
        "/a",
        "/data/deep/er",
        "/tmpfiles",
        "/running",
        "/var/tmpx",
        "/var",
        "/runx/js-files",
        path_of(4096).as_str(),
    ] {
        let taken = mounts(&[mount_at(good)]).unwrap();
        assert_eq!(taken[0].target, good);
    }
    let both = mounts(&[mount_at("/data"), mount_at("/data/below")]).unwrap();
    let targets: Vec<&str> = both.iter().map(|mount| mount.target.as_str()).collect();
    assert_eq!(targets, ["/data", "/data/below"]);
}

/// Asserts that a target of 4097 bytes, `/`, an empty one, a relative one, one with `..`, `.`
/// or an empty part, a trailing `/`, a non-ASCII byte, `:`, `,`, `\` or a NUL is refused
/// naming `target` at its index. It catches a target that could add a field to the mount
/// options or leave the job's root.
#[test]
fn a_bad_target_is_refused_naming_target() {
    let long = path_of(4097);
    for bad in [
        long.as_str(),
        "/",
        "",
        "data",
        "/a/../b",
        "/..",
        "/a/./b",
        "/a//b",
        "/a/",
        "//a",
        "/é",
        "/a:b",
        "/a,b",
        "/a\\b",
        "/a\0b",
    ] {
        assert_eq!(
            mounts_fault(&[mount_at("/ok"), mount_at(bad)]),
            Fault::Entry("mounts", 1, "target"),
            "{bad:?}"
        );
    }
}

/// Asserts that each place the runtime keeps, in either case, and any path below one is
/// refused naming `target`. It catches a mount over the runtime's own directories.
#[test]
fn a_target_in_a_runtime_place_is_refused() {
    for place in js_types::path::RUNTIME_PLACES {
        for target in [
            place.to_owned(),
            format!("{place}/below"),
            place.to_ascii_uppercase(),
        ] {
            assert_eq!(
                mounts_fault(&[mount_at(&target)]),
                Fault::Entry("mounts", 0, "target"),
                "{target}"
            );
        }
    }
}

/// Asserts that two targets equal in different case are refused naming the second entry's
/// target, and that two equal in the same case are too, while a third distinct target after
/// them changes nothing. It catches a duplicate that reaches an agent on a case-insensitive
/// file system.
#[test]
fn two_targets_equal_in_lower_case_are_refused_at_the_second() {
    assert_eq!(
        mounts_fault(&[mount_at("/Data"), mount_at("/other"), mount_at("/dAtA")]),
        Fault::Entry("mounts", 2, "target")
    );
    assert_eq!(
        mounts_fault(&[mount_at("/data"), mount_at("/data")]),
        Fault::Entry("mounts", 1, "target")
    );
}

/// Asserts that an unspecified mode is stored read-only and each known mode as itself, and
/// that an unknown one is refused naming `mode`. It catches a default stored as unspecified
/// (a resubmit that spells it either way would differ) and a read-write mount read as
/// read-only.
#[test]
fn a_mode_is_stored_explicitly_and_an_unknown_one_refused() {
    for (given, stored) in [
        (v1::MountMode::Unspecified, MountMode::ReadOnly),
        (v1::MountMode::ReadOnly, MountMode::ReadOnly),
        (v1::MountMode::ReadWrite, MountMode::ReadWrite),
        (v1::MountMode::CopyOnWrite, MountMode::CopyOnWrite),
    ] {
        let taken = mounts(&[mount_with(|mount| mount.mode = given.into())]).unwrap();
        assert_eq!(taken[0].mode, stored, "{given:?}");
    }
    assert_eq!(
        mounts_fault(&[mount_with(|mount| mount.mode = 9)]),
        Fault::Entry("mounts", 0, "mode")
    );
}

/// Asserts that a `subPath` that is empty is stored as none, a relative one in normal form
/// (up to 4096 bytes) as given, and an absolute one, one with `..`, `.`, an empty part, a
/// trailing `/`, a NUL or 4097 bytes is refused naming `subPath`. It catches an empty
/// subPath stored as `Some("")`, which would make a default two spellings, and a sub path
/// that leaves its source.
#[test]
fn a_sub_path_is_relative_in_normal_form_and_empty_is_none() {
    let stored = |sub: &str| {
        mounts(&[mount_with(|mount| mount.sub_path = sub.to_owned())])
            .map(|m| m[0].sub_path.clone())
    };
    assert_eq!(stored(""), Ok(None));
    for good in ["a", "a/b", "a.b", &"a".repeat(4096)] {
        assert_eq!(stored(good), Ok(Some(good.to_owned())), "{good:?}");
    }
    let long = "a".repeat(4097);
    for bad in [
        "/a", "/", "..", "a/..", "../a", ".", "a/./b", "a//b", "a/", "a\0b", "a:b", &long,
    ] {
        assert_eq!(
            stored(bad),
            Err(Fault::Entry("mounts", 0, "subPath")),
            "{bad:?}"
        );
    }
}

/// Asserts that `noExec` is stored as given, both ways, and that an unspecified or private
/// propagation is stored private while an unknown one is refused naming `propagation`. It
/// catches a flag dropped on the way to the record.
#[test]
fn no_exec_is_kept_and_propagation_is_explicit() {
    for no_exec in [false, true] {
        let taken = mounts(&[mount_with(|mount| mount.no_exec = no_exec)]).unwrap();
        assert_eq!(taken[0].no_exec, no_exec);
    }
    for given in [
        v1::MountPropagation::Unspecified,
        v1::MountPropagation::Private,
    ] {
        let taken = mounts(&[mount_with(|mount| mount.propagation = given.into())]).unwrap();
        assert_eq!(taken[0].propagation, Propagation::Private, "{given:?}");
    }
    assert_eq!(
        mounts_fault(&[mount_with(|mount| mount.propagation = 9)]),
        Fault::Entry("mounts", 0, "propagation")
    );
}

/// Asserts that a full mount is stored whole, in explicit form, and that the first bad field
/// of a mount in the order target, mode, subPath, propagation is the one named. It catches a
/// field lost in the copy and a refusal that names a later field than the first wrong one.
#[test]
fn a_mount_is_stored_whole_and_its_first_bad_field_is_named() {
    let full = v1::Mount {
        source: Some(v1::mount::Source::HostPath("/srv/data".to_owned())),
        target: "/mnt/data".to_owned(),
        mode: v1::MountMode::CopyOnWrite.into(),
        sub_path: "x/y".to_owned(),
        no_exec: true,
        propagation: v1::MountPropagation::Private.into(),
    };
    assert_eq!(
        mounts(std::slice::from_ref(&full)),
        Ok(vec![JobMount {
            source: MountSource::HostPath {
                path: "/srv/data".to_owned()
            },
            target: "/mnt/data".to_owned(),
            mode: MountMode::CopyOnWrite,
            sub_path: Some("x/y".to_owned()),
            no_exec: true,
            propagation: Propagation::Private,
        }])
    );
    let all_bad = v1::Mount {
        target: "bad".to_owned(),
        mode: 9,
        sub_path: "/bad".to_owned(),
        propagation: 9,
        ..full.clone()
    };
    let faults: Vec<Fault> = [
        all_bad.clone(),
        v1::Mount {
            target: "/ok".to_owned(),
            ..all_bad.clone()
        },
        v1::Mount {
            target: "/ok".to_owned(),
            mode: 1,
            ..all_bad.clone()
        },
        v1::Mount {
            target: "/ok".to_owned(),
            mode: 1,
            sub_path: String::new(),
            ..all_bad
        },
    ]
    .iter()
    .map(|mount| mounts_fault(std::slice::from_ref(mount)))
    .collect();
    assert_eq!(
        faults,
        [
            Fault::Entry("mounts", 0, "target"),
            Fault::Entry("mounts", 0, "mode"),
            Fault::Entry("mounts", 0, "subPath"),
            Fault::Entry("mounts", 0, "propagation"),
        ]
    );
}

/// Asserts that 16 host ports are taken and 17 refused naming `spec.hostPorts`. It catches a
/// cap off by one.
#[test]
fn sixteen_host_ports_are_taken_and_seventeen_refused() {
    let sixteen: Vec<v1::HostPort> = (1..=16).map(|at| port(at, None)).collect();
    assert_eq!(host_ports(&sixteen, &[]).unwrap().len(), 16);
    let mut seventeen = sixteen;
    seventeen.push(port(17, None));
    assert_eq!(
        host_ports(&seventeen, &[]),
        Err(Fault::Field("spec.hostPorts"))
    );
    assert_eq!(host_ports(&[], &[]), Ok(Vec::new()));
}

/// Asserts that a host port of 0 or 65536 is refused naming `hostPort`, 1 and 65535 are
/// taken, and the same bounds hold for a job port naming `containerPort`. It catches a port
/// truncated to 16 bits (65536 read as 0, which then passes as the job port).
#[test]
fn ports_are_one_to_65535_at_both_sides() {
    for good in [1, 65_535] {
        let taken = host_ports(&[port(good, Some(good))], &[]).unwrap();
        assert_eq!(
            taken,
            [HostPort {
                host_port: u16::try_from(good).unwrap(),
                container_port: Some(u16::try_from(good).unwrap()),
                protocol: PortProtocol::Tcp,
            }]
        );
    }
    for bad in [0, 65_536, u32::MAX] {
        assert_eq!(
            ports_fault(&[port(bad, None)]),
            Fault::Entry("hostPorts", 0, "hostPort"),
            "{bad}"
        );
        assert_eq!(
            ports_fault(&[port(8080, Some(bad))]),
            Fault::Entry("hostPorts", 0, "containerPort"),
            "{bad}"
        );
    }
}

/// Asserts that an absent job port is stored as the host port, a given one as given, and
/// that two host ports of one job port are refused naming the second entry's `containerPort`
/// whether both are given, or one is given and the other absent at the same number, while
/// two job ports of one host port are taken. It catches a duplicate job port that would
/// listen twice at one address, and a default compared before it is filled in.
#[test]
fn a_job_port_is_explicit_and_not_taken_twice() {
    let taken = host_ports(&[port(8080, None), port(9090, Some(80))], &[]).unwrap();
    let ports: Vec<(u16, Option<u16>)> = taken
        .iter()
        .map(|port| (port.host_port, port.container_port))
        .collect();
    assert_eq!(ports, [(8080, Some(8080)), (9090, Some(80))]);
    assert_eq!(
        ports_fault(&[port(1, Some(80)), port(2, Some(80))]),
        Fault::Entry("hostPorts", 1, "containerPort")
    );
    assert_eq!(
        ports_fault(&[port(1, Some(80)), port(80, None)]),
        Fault::Entry("hostPorts", 1, "containerPort")
    );
    assert_eq!(
        ports_fault(&[port(80, None), port(1, Some(80))]),
        Fault::Entry("hostPorts", 1, "containerPort")
    );
    let same_host = host_ports(&[port(5, Some(80)), port(5, Some(81))], &[]).unwrap();
    assert_eq!(same_host.len(), 2);
}

/// Asserts that a job port equal to a declared forward's container port is refused naming
/// `containerPort`, by an explicit one and by an absent one that defaults to it, and that
/// the forward's host port alone does not matter. It catches a job port that would collide
/// with a port the job already serves.
#[test]
fn a_job_port_may_not_be_a_declared_containers_port() {
    let forwards = [declared(8080, 80)];
    assert_eq!(
        host_ports(&[port(1, Some(80))], &forwards),
        Err(Fault::Entry("hostPorts", 0, "containerPort"))
    );
    assert_eq!(
        host_ports(&[port(2, Some(81)), port(80, None)], &forwards),
        Err(Fault::Entry("hostPorts", 1, "containerPort"))
    );
    let taken = host_ports(&[port(8080, Some(81))], &forwards).unwrap();
    assert_eq!(taken[0].container_port, Some(81));
}

/// Asserts that an unspecified protocol is stored TCP, TCP as itself and an unknown one is
/// refused naming `protocol`, and that the first bad field of a port is the one named
/// (`hostPort`, then `containerPort`, then `protocol`). It catches a default left
/// unspecified in the record.
#[test]
fn a_protocol_is_explicit_and_an_unknown_one_refused() {
    for given in [v1::PortProtocol::Unspecified, v1::PortProtocol::Tcp] {
        let taken = host_ports(
            &[v1::HostPort {
                protocol: given.into(),
                ..port(80, None)
            }],
            &[],
        )
        .unwrap();
        assert_eq!(taken[0].protocol, PortProtocol::Tcp, "{given:?}");
    }
    let unknown = |host, job| v1::HostPort {
        protocol: 9,
        ..port(host, job)
    };
    assert_eq!(
        ports_fault(&[unknown(80, None)]),
        Fault::Entry("hostPorts", 0, "protocol")
    );
    assert_eq!(
        ports_fault(&[unknown(80, Some(0))]),
        Fault::Entry("hostPorts", 0, "containerPort")
    );
    assert_eq!(
        ports_fault(&[unknown(0, Some(0))]),
        Fault::Entry("hostPorts", 0, "hostPort")
    );
}

/// Asserts when a job must run in a container: with a layer, with a host port at another
/// job port than its host port (given or by default), and not with an unremapped host port
/// (given at its own number or absent) or with nothing. It catches a remapped port that
/// would run where no network of its own exists.
#[test]
fn layers_and_a_remapped_port_need_a_container() {
    let layer = [Layer::HostPath {
        path: "/l".to_owned(),
    }];
    let host_port = |host, job| HostPort {
        host_port: host,
        container_port: job,
        protocol: PortProtocol::Tcp,
    };
    assert!(needs_container(&layer, &[]));
    assert!(needs_container(&[], &[host_port(1, Some(2))]));
    assert!(needs_container(
        &layer,
        &[host_port(1, Some(2)), host_port(3, None)]
    ));
    assert!(!needs_container(&[], &[host_port(1, Some(1))]));
    assert!(!needs_container(&[], &[host_port(1, None)]));
    assert!(!needs_container(&[], &[]));
}
