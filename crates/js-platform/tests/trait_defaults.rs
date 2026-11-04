//! The calls a platform does not override.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use js_platform::{
    ContainerPlatform, JobHandle, PlatformCaps, PlatformError, PlatformId, SubmitSpec,
};
use js_types::{ImageDigestRef, JobNetwork, Requirements, Resources};

/// A platform that implements only the three required calls.
struct Minimal;

impl ContainerPlatform for Minimal {
    fn id(&self) -> PlatformId {
        PlatformId::new("minimal")
    }

    fn capabilities(&self) -> PlatformCaps {
        PlatformCaps::default()
    }

    fn can_ever_serve(&self, _r: &Requirements) -> bool {
        false
    }
}

/// The answer of `future`, which must be ready at its first poll: the default calls wait
/// for nothing.
fn answer<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(answer) => answer,
        Poll::Pending => panic!("a default call waited"),
    }
}

fn spec() -> SubmitSpec {
    let image = ImageDigestRef::parse(&format!("team/app:2@sha256:{}", "a".repeat(64))).unwrap();
    let requirements = Requirements {
        resources: Resources::new(1000, 1 << 30, 0, None).unwrap(),
        disk_bytes: None,
        os: None,
        arch: None,
        isa_level: None,
        exec: None,
    };
    SubmitSpec::new(
        "submit-1",
        image,
        requirements,
        JobNetwork::None,
        Duration::from_secs(600),
    )
}

/// Asserts that a platform implementing only `id`, `capabilities` and `can_ever_serve`
/// answers `NotBuilt` to submit, observe, cancel, logs and result, each at once. It
/// catches a default body that answers as if the call had worked (an empty log, no
/// result, a cancelled job), which a hostless caller would take for a real answer.
#[test]
fn undeclared_operations_answer_not_built() {
    let platform = Minimal;
    let handle = JobHandle::new("job-1");
    assert_eq!(
        answer(platform.submit(spec())),
        Err(PlatformError::NotBuilt)
    );
    assert_eq!(
        answer(platform.observe(&handle)),
        Err(PlatformError::NotBuilt)
    );
    assert_eq!(
        answer(platform.cancel(&handle)),
        Err(PlatformError::NotBuilt)
    );
    assert_eq!(answer(platform.logs(&handle)), Err(PlatformError::NotBuilt));
    assert_eq!(
        answer(platform.result(&handle)),
        Err(PlatformError::NotBuilt)
    );
    assert_eq!(platform.id(), PlatformId::new("minimal"));
    assert_eq!(platform.capabilities(), PlatformCaps::default());
    assert!(!platform.can_ever_serve(&spec().requirements));
}
