//! The layers, mounts and host ports files of `js job submit`, against the in-memory job
//! server of `fake_jobs`: what the request's spec holds, and the refusal of a file that
//! reads wrongly.

use js_proto::v1::{
    HostPort, Layer, Mount, MountMode, MountPropagation, PortProtocol, SubmitJobRequest, layer,
    mount,
};

use super::{call, js, only};
use crate::fake_jobs::Fake;
use crate::support::Scratch;

/// Asserts that `--spec.layers-file`, `--spec.mounts-file` and `--spec.host-ports-file` read
/// proto3 JSON (camelCase names, full enum names, `hostPath` and `image` as the oneof
/// members) and that the request's spec holds exactly those layers, mounts and host ports, in
/// order, with every option the file names and none it leaves out. It catches a file flag
/// that drops a list, reads the enum by another name, loses the oneof member or sends the
/// lists in another order.
#[test]
fn the_layers_mounts_and_host_ports_files_fill_the_spec() {
    let scratch = Scratch::new("jobs_rootfs_files");
    let fake = Fake::new();
    let layers = scratch.write(
        "layers.json",
        r#"[{"image": "base@sha256:00"}, {"hostPath": "/srv/overlay"}]"#,
        0o600,
    );
    let mounts = scratch.write(
        "mounts.json",
        r#"[{"hostPath": "/srv/data", "target": "/data", "mode": "MOUNT_MODE_COPY_ON_WRITE",
             "subPath": "inputs", "noExec": true, "propagation": "MOUNT_PROPAGATION_PRIVATE"},
            {"hostPath": "/srv/more", "target": "/more"}]"#,
        0o600,
    );
    let host_ports = scratch.write(
        "host-ports.json",
        r#"[{"hostPort": 5432, "containerPort": 15432, "protocol": "PORT_PROTOCOL_TCP"},
            {"hostPort": 6379}]"#,
        0o600,
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &[
            "job",
            "submit",
            "--id",
            "rootfs-1",
            "--spec.command",
            "/bin/run",
            "--spec.layers-file",
            layers.to_str().unwrap(),
            "--spec.mounts-file",
            mounts.to_str().unwrap(),
            "--spec.host-ports-file",
            host_ports.to_str().unwrap(),
        ],
    );
    assert_eq!((got.code, got.err.as_str()), (0, ""));
    let (path, request) = only::<SubmitJobRequest>(&fake);
    assert_eq!(path, call("SubmitJob"));
    let spec = request.spec.unwrap();
    let want_layers = vec![
        Layer {
            source: Some(layer::Source::Image("base@sha256:00".to_owned())),
        },
        Layer {
            source: Some(layer::Source::HostPath("/srv/overlay".to_owned())),
        },
    ];
    assert_eq!(spec.layers, want_layers);
    let want_mounts = vec![
        Mount {
            source: Some(mount::Source::HostPath("/srv/data".to_owned())),
            target: "/data".to_owned(),
            mode: MountMode::CopyOnWrite.into(),
            sub_path: "inputs".to_owned(),
            no_exec: true,
            propagation: MountPropagation::Private.into(),
        },
        Mount {
            source: Some(mount::Source::HostPath("/srv/more".to_owned())),
            target: "/more".to_owned(),
            ..Mount::default()
        },
    ];
    assert_eq!(spec.mounts, want_mounts);
    let want_ports = vec![
        HostPort {
            host_port: 5432,
            container_port: Some(15432),
            protocol: PortProtocol::Tcp.into(),
        },
        HostPort {
            host_port: 6379,
            container_port: None,
            protocol: PortProtocol::Unspecified.into(),
        },
    ];
    assert_eq!(spec.host_ports, want_ports);
}

/// Asserts that a mounts file with a mount mode named other than by its full enum name is exit 2
/// naming the flag and sends nothing. It catches a file flag that accepts a mount mode by
/// its short name, which a server would then read differently.
#[test]
fn a_mounts_file_with_an_unknown_enum_name_is_usage_exit_2() {
    let scratch = Scratch::new("jobs_rootfs_bad");
    let fake = Fake::new();
    let short = scratch.write(
        "short.json",
        r#"[{"hostPath": "/srv/data", "target": "/data", "mode": "read-write"}]"#,
        0o600,
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &[
            "job",
            "submit",
            "--spec.mounts-file",
            short.to_str().unwrap(),
        ],
    );
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(
        got.err.starts_with("js: --spec.mounts-file: "),
        "{}",
        got.err
    );
    assert!(fake.seen().is_empty());
}
