//! A run's output committed to the put ingest for real, as root, by the production runtime:
//! the run's job writes its pack, the removal commits it, and the owners, groups and modes the
//! system then shows are read back (`native/put.rs`, `mac_put::hand_over`). The node is a uid
//! and gid that differ from the agent's (root), so the agent's and the node's places can be
//! told apart.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use js_agent::lifecycle::{FsPrepared, JobFs, Runtime};
use js_agent::mac_put::{DIR_MODE, Node};
use js_agent::native::PutConfig;
use js_agent_wire::{JobMount, MountMode, MountSource, Propagation};

use super::{exited, fixture, runtime, settle, spec};

/// The node of the test: a uid and gid no account needs to hold, outside the pool's slots.
const NODE: Node = Node {
    uid: 4322,
    gid: 4322,
};

/// Asserts, as root, that a run with the build service's put mount has its pack committed by
/// its removal with the owners the node needs: the key's folders the node's user and group, the
/// job's directory root's (the agent's) with the node's group and mode 0770, `pack` root's of
/// that group, mode 0640, holding the job's bytes, and the commit reported once by its key.
/// Catches the agent's and the node's ids swapped or confused in the commit (which no test as
/// a plain user can tell apart), and a commit that the real supervisor's run never reaches.
#[test]
#[ignore = "needs root and creates users"]
fn as_root_a_runs_pack_is_committed_with_the_owners_the_node_needs() {
    let fixture = fixture();
    let put_root = fixture.root.join("put");
    fs::create_dir(&put_root).unwrap();
    let put_root = fs::canonicalize(&put_root).unwrap();
    let put = PutConfig {
        root: put_root.clone(),
        node: NODE,
    };
    let mut rt = runtime(fixture).with_put(Some(put));
    let dir = put_root.join("cas/b/b1/n1");
    let fs_ = JobFs {
        layers: Vec::new(),
        mounts: vec![JobMount {
            source: MountSource::HostPath {
                path: dir.display().to_string(),
            },
            target: "/work/.kbf-out".to_owned(),
            mode: MountMode::ReadWrite,
            sub_path: None,
            no_exec: true,
            propagation: Propagation::Private,
        }],
    };
    assert_eq!(rt.check_job_fs(&fs_, &[]), Ok(()));
    assert_eq!(rt.resolve_fs("put", "", &fs_, 0), FsPrepared::Ready);
    let script = "mkdir .kbf-out && printf 'the pack' > .kbf-out/pack";
    let id = rt
        .create_and_start(&spec(fixture, "put", "/bin/sh", &["-c", script]))
        .unwrap();
    assert_eq!(settle(&mut rt, &id), exited(0));
    rt.remove(&id).unwrap();

    for part in ["cas", "cas/b", "cas/b/b1"] {
        let meta = fs::symlink_metadata(put_root.join(part)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (NODE.uid, NODE.gid), "{part}");
    }
    let job_dir = fs::symlink_metadata(&dir).unwrap();
    assert_eq!(
        (job_dir.uid(), job_dir.gid()),
        (0, NODE.gid),
        "the job's directory"
    );
    assert_eq!(job_dir.permissions().mode() & 0o7777, DIR_MODE);
    let pack = fs::symlink_metadata(dir.join("pack")).unwrap();
    assert_eq!((pack.uid(), pack.gid()), (0, NODE.gid), "pack");
    assert_eq!(pack.permissions().mode() & 0o7777, 0o640);
    assert_eq!(fs::read(dir.join("pack")).unwrap(), b"the pack");
    let report = rt.outputs_report(&id).expect("the commit's report");
    assert_eq!(report.key, "cas/b/b1/n1/pack");
    assert_eq!(report.bytes, 8);
    assert_eq!(rt.outputs_report(&id), None, "reported once");
}
