use super::*;
use js_agent_wire::{JobMount, Layer, MountSource};

/// A runtime that keeps every default of the trait.
struct Bare;

impl Runtime for Bare {
    fn create_and_start(&mut self, _: &RunSpec) -> Result<RunHandle, RuntimeError> {
        Err(RuntimeError::new("bare"))
    }
    fn poll(&mut self, _: &RunHandle) -> Result<Status, RuntimeError> {
        Ok(Status::Gone)
    }
    fn stop(&mut self, _: &RunHandle, _: Duration) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn remove(&mut self, _: &RunHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        Ok(Vec::new())
    }
}

/// Asserts the defaults of a runtime that says nothing of native bodies and image starts:
/// it refuses a native body `native-unsupported` (every runtime but the Mac one), runs a
/// start named by its image (every container runtime) and runs no image run. Catches a
/// default that would let a Linux runtime take a native job, or refuse the image starts
/// every container runtime runs.
#[test]
fn a_runtime_that_says_nothing_runs_images_and_no_native_body() {
    let bare = Bare;
    let native = NativeSpec {
        program: "/bin/true".to_owned(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    };
    let refused = bare.check_native(&native).unwrap_err();
    assert_eq!(refused.code, refusal::NATIVE_UNSUPPORTED);
    assert_eq!(refused.why, "this agent runs no native jobs");
    assert!(bare.check_image().is_ok());
    assert!(!bare.runs_image_runs());
}

/// Catches a runtime that gives no layers (the container engine's, a Mac's, a VM's)
/// answering as if it did: it gives none, allows any image of a layer (it is never
/// asked), binds the mounts of named roots, has nothing to prepare, discard or check,
/// and mounts nothing.
#[test]
fn a_runtime_that_gives_no_layers_prepares_nothing() {
    let mut bare = Bare;
    let id = RunHandle("c1".to_owned());
    assert!(bare.create_and_start(&spec()).is_err());
    assert_eq!(bare.poll(&id), Ok(Status::Gone));
    assert_eq!(bare.stop(&id, Duration::ZERO), Ok(()));
    assert_eq!(bare.remove(&id), Ok(()));
    assert_eq!(bare.list_labelled(), Ok(Vec::new()));
    assert!(!bare.layered_rootfs());
    assert_eq!(
        bare.check_layer_image("registry.example/a@sha256:0"),
        Ok(())
    );
    assert!(bare.binds_named_mounts());
    let fs = JobFs::default();
    assert_eq!(bare.resolve_fs("r1", "", &fs, 0), FsPrepared::Ready);
    assert_eq!(bare.assemble_fs("r1", 0), FsPrepared::Ready);
    assert_eq!(bare.discard_fs("r1"), Ok(()));
    assert_eq!(bare.mounts_of("r1"), None);
    assert_eq!(bare.input_failure(&id), None);
    assert!(!bare.ends_held(&id));
}

/// Asserts that a runtime that keeps the trait's defaults creates on the caller's thread:
/// `begin_create` answers what `create_and_start` does, at once and never pending, and
/// `poll_create` answers a failure naming the run, since none of its creates is under way.
/// Catches a default that answers pending for a create nobody runs, or one that loses the
/// create's error.
#[test]
fn a_runtime_that_says_nothing_creates_on_the_callers_thread() {
    let mut bare = Bare;
    assert_eq!(
        bare.begin_create(&spec()),
        Created::Done(Err(RuntimeError::new("bare")))
    );
    assert_eq!(
        bare.poll_create("r7"),
        Created::Done(Err(RuntimeError::new("no create of r7 is under way")))
    );
}

/// Asserts that a runtime that keeps the trait's defaults removes on the caller's thread:
/// `removing` answers what `remove` does, at once and never pending. Catches a default that
/// answers pending for a removal nobody runs, which would hold the run's slot for good.
#[test]
fn a_runtime_that_says_nothing_removes_on_the_callers_thread() {
    let mut bare = Bare;
    let id = RunHandle("c1".to_owned());
    assert_eq!(bare.removing(&id), Removal::Done(Ok(())));
}

/// Catches the host paths of a start counted in another order than the runtime's sources
/// (a layer's after a mount's), a read-write mount not marked as writing, a `sub_path`
/// lost, an image layer taken for a host path or the other way round, and a layer or a
/// mount a provider makes listed as a host path (it has none until the provider answers)
/// or taken for an image.
#[test]
fn a_starts_host_paths_are_its_layers_then_its_mounts() {
    use js_agent_wire::{MountMode, Propagation};
    let mount = |path: &str, mode, sub_path: Option<&str>| JobMount {
        source: MountSource::HostPath {
            path: path.to_owned(),
        },
        target: "/t".to_owned(),
        mode,
        sub_path: sub_path.map(str::to_owned),
        no_exec: false,
        propagation: Propagation::Private,
    };
    let fs = JobFs {
        layers: vec![
            Layer::Image {
                image: "registry.example/a@sha256:0".to_owned(),
            },
            Layer::Provider {
                provider: "tier".to_owned(),
                reference: "k1".to_owned(),
            },
            Layer::HostPath {
                path: "/l".to_owned(),
            },
        ],
        mounts: vec![
            mount("/rw", MountMode::ReadWrite, Some("sub")),
            JobMount {
                source: MountSource::Provider {
                    provider: "tier".to_owned(),
                    reference: "k2".to_owned(),
                },
                ..mount("/unused", MountMode::ReadWrite, None)
            },
            mount("/cow", MountMode::CopyOnWrite, None),
        ],
    };
    assert!(!fs.is_empty());
    assert!(JobFs::default().is_empty());
    let source = |path, sub_path, writes| HostSource {
        path,
        sub_path,
        writes,
    };
    assert_eq!(
        fs.host_paths(),
        [
            source("/l", None, false),
            source("/rw", Some("sub"), true),
            source("/cow", None, false),
        ]
    );
    assert_eq!(fs.images(), ["registry.example/a@sha256:0"]);
}

/// Asserts that only a removal that left a native run's account held says how many accounts
/// are held, and that the text is the runtime's own whatever the kind: a plain failure and a
/// blind one say none. Catches a kind that reads as a held account for any failure, which
/// would let the lifecycle skip the taint of a removal that failed.
#[test]
fn only_a_held_account_says_how_many_accounts_are_held() {
    let held = RuntimeError::account_held("user-delete: refused", 3);
    assert_eq!(held.held_accounts(), Some(3));
    assert_eq!(held.0, "user-delete: refused");
    assert_eq!(RuntimeError::new("remove failed").held_accounts(), None);
    assert_eq!(RuntimeError::blind("not seen").held_accounts(), None);
}

/// Asserts that a runtime that makes no accounts holds none. Catches a default that would
/// taint every host.
#[test]
fn a_runtime_that_makes_no_accounts_holds_none() {
    assert_eq!(Bare.held_accounts(), 0);
}

/// A run of no consequence.
fn spec() -> RunSpec {
    RunSpec {
        run_id: "r1".to_owned(),
        body: Body::Container {
            image: String::new(),
            args: Vec::new(),
        },
        env: Vec::new(),
        cpu_millis: 0,
        memory_bytes: 0,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch: PathBuf::new(),
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        fuse: false,
        network: JobNetwork::None,
        ports: Vec::new(),
        host_ports: Vec::new(),
        labels: BTreeMap::new(),
        bundle: None,
        log_sink: None,
        xcode: None,
    }
}
