//! Native jobs through the run API: a body with `native` (and optionally a pinned
//! `bundle`) is queued, goes to the agent's start unchanged, and is refused when it is
//! wrong or when the host cannot run it. A body of the old shape reads and answers as it
//! always did.

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, hello_body, post, session};
use crate::runs_support::{beat, lease, queue, run_body, runs, words};
use crate::support::Harness;

const BUNDLE: &str =
    "toolchain:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A host that joins with the capabilities `extra` names on top of the usual ones.
async fn join_host(h: &Harness, extra: &[(&str, &str)]) -> Session {
    let mut body = hello_body("burst-1");
    for (key, value) in extra {
        body["capabilities"][*key] = json!(value);
    }
    let reply = post(h, "/v1/agent/hello", Some(JOIN), &body).await;
    session(&reply, "burst-1")
}

/// The capabilities of a macOS agent that runs native jobs and holds a bundle store.
const NATIVE_HOST: [(&str, &str); 2] = [("exec", "sandbox-exec"), ("bundle-root", "/opt/bundles")];

/// A native run of `program`: no image, the limits of `run_body`.
fn native_body(program: &str) -> Value {
    let mut body = run_body(1000);
    body.as_object_mut().unwrap().remove("image");
    body.as_object_mut().unwrap().remove("args");
    body["native"] = json!({ "program": program, "args": ["--version"], "network": "loopback" });
    body
}

/// Asserts the path of a native job end to end: the POST is accepted and stored, the next
/// heartbeat reply carries a start whose `native` body has the program, the arguments
/// and the network level as sent, whose `image` is empty, and which has no `bundle`. It
/// catches a handler that drops the body or builds the job with `native: None`.
#[tokio::test]
async fn a_native_body_is_queued_and_reaches_the_start() {
    let h = crate::agents_support::start_agents("run_native_flow", false).await;
    let me = join_host(&h, &NATIVE_HOST).await;
    let lease = lease(&h).await;
    let queued = queue(&h, &lease, &native_body("/usr/bin/true")).await;
    assert_eq!(queued.status, 202, "{}", queued.body);
    assert_eq!(queued.json()["state"], "queued");
    assert_eq!(runs(&h, &lease).await.len(), 1);
    let reply = beat(&h, &me, &[], &[]).await;
    assert_eq!(words(&reply), ["start run-1-1"]);
    let start = &reply["commands"][0];
    assert_eq!(start["image"], "");
    assert_eq!(start["native"]["program"], "/usr/bin/true");
    assert_eq!(start["native"]["args"], json!(["--version"]));
    assert_eq!(start["native"]["network"], "loopback");
    assert!(start.get("bundle").is_none(), "{start}");
}

/// Asserts that the network level is optional (none means off) and that a bundle digest
/// is carried to the start beside an `@bundle/<entrypoint>` program. It catches a bundle
/// that is validated and then dropped, and a default other than off.
#[tokio::test]
async fn a_bundle_digest_reaches_the_start() {
    let h = crate::agents_support::start_agents("run_native_bundle", false).await;
    let me = join_host(&h, &NATIVE_HOST).await;
    let lease = lease(&h).await;
    let mut body = native_body("@bundle/clang");
    body["native"].as_object_mut().unwrap().remove("network");
    body["bundle"] = json!(BUNDLE);
    let queued = queue(&h, &lease, &body).await;
    assert_eq!(queued.status, 202, "{}", queued.body);
    let reply = beat(&h, &me, &[], &[]).await;
    let start = &reply["commands"][0];
    assert_eq!(start["bundle"], BUNDLE);
    assert_eq!(start["native"]["program"], "@bundle/clang");
    assert_eq!(start["native"]["network"], "off");
}

/// Asserts that an image and a native body together are 422 `invalid-image`, and that a
/// body with neither is 400, with nothing queued either way. It catches a route that
/// lets one win silently.
#[tokio::test]
async fn an_image_and_a_native_body_are_exclusive() {
    let h = crate::agents_support::start_agents("run_native_exclusive", false).await;
    join_host(&h, &NATIVE_HOST).await;
    let lease = lease(&h).await;
    let mut both = native_body("/usr/bin/true");
    both["image"] = run_body(1000)["image"].clone();
    let refused = queue(&h, &lease, &both).await;
    assert_eq!(
        (refused.status, refused.json()),
        (422, json!({ "error": "invalid-image" }))
    );
    let mut neither = native_body("/usr/bin/true");
    neither.as_object_mut().unwrap().remove("native");
    let refused = queue(&h, &lease, &neither).await;
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert!(runs(&h, &lease).await.is_empty());
}

/// Asserts that a native job for a host that did not report `exec` as `sandbox-exec` (a
/// container host, or an agent that predates the report) is 409 `native-unsupported`,
/// with nothing queued and no start sent, while the same host still takes an image job.
/// It catches a native job queued where it could never run.
#[tokio::test]
async fn a_native_job_is_refused_for_a_host_that_is_not_native() {
    for exec in [None, Some("container")] {
        let name = format!("run_native_container_{}", exec.unwrap_or("none"));
        let h = crate::agents_support::start_agents(&name, false).await;
        let extra: Vec<(&str, &str)> = exec.map(|mode| ("exec", mode)).into_iter().collect();
        let me = join_host(&h, &extra).await;
        let lease = lease(&h).await;
        let refused = queue(&h, &lease, &native_body("/usr/bin/true")).await;
        assert_eq!(
            (refused.status, refused.json()),
            (409, json!({ "error": "native-unsupported" })),
            "{exec:?}"
        );
        assert!(runs(&h, &lease).await.is_empty(), "{exec:?}");
        assert!(words(&beat(&h, &me, &[], &[]).await).is_empty(), "{exec:?}");
        let image = queue(&h, &lease, &run_body(1000)).await;
        assert_eq!(image.status, 202, "{exec:?}: {}", image.body);
    }
}

/// Asserts that a bundle for a native host with no bundle store (no `bundle-root`) is 409
/// `bundle-unsupported`, with nothing queued: an agent without a store would refuse the
/// start after it was queued.
#[tokio::test]
async fn a_bundle_is_refused_for_a_host_without_a_store() {
    let h = crate::agents_support::start_agents("run_native_no_store", false).await;
    join_host(&h, &[("exec", "sandbox-exec")]).await;
    let lease = lease(&h).await;
    let mut body = native_body("@bundle/clang");
    body["bundle"] = json!(BUNDLE);
    let refused = queue(&h, &lease, &body).await;
    assert_eq!(
        (refused.status, refused.json()),
        (409, json!({ "error": "bundle-unsupported" }))
    );
    assert!(runs(&h, &lease).await.is_empty());
}

/// Asserts the 422 of each bad native or bundle body: a tag-only bundle
/// (`bundle-digest-required`), a bundle with no native body (`invalid-bundle`), a bundle
/// program with no bundle (`invalid-program`), a program that is not an absolute path
/// (`invalid-program`) and a native body that also has `args` of its own
/// (`invalid-job`); and that nothing is queued. It catches a route that skips the wire's
/// checks for the new body.
#[tokio::test]
async fn bad_native_bodies_are_refused_before_the_host_is_asked() {
    let h = crate::agents_support::start_agents("run_native_bad", false).await;
    join_host(&h, &[]).await;
    let lease = lease(&h).await;
    let mut tag_only = native_body("@bundle/clang");
    tag_only["bundle"] = json!("toolchain:1");
    let mut no_native = run_body(1000);
    no_native["bundle"] = json!(BUNDLE);
    let no_bundle = native_body("@bundle/clang");
    let relative = native_body("true");
    let mut own_args = native_body("/usr/bin/true");
    own_args["args"] = json!(["x"]);
    let cases = [
        (tag_only, "bundle-digest-required"),
        (no_native, "invalid-bundle"),
        (no_bundle, "invalid-program"),
        (relative, "invalid-program"),
        (own_args, "invalid-job"),
    ];
    for (body, code) in cases {
        let reply = queue(&h, &lease, &body).await;
        assert_eq!(
            (reply.status, reply.json()),
            (422, json!({ "error": code })),
            "{body}"
        );
    }
    assert!(runs(&h, &lease).await.is_empty());
}

/// Asserts that the native object takes the program, the arguments and the network level
/// and nothing else: an unknown key, an environment, and an unknown network level are 400.
/// It catches a route that quietly widens what a client can ask of an agent.
#[tokio::test]
async fn the_native_object_takes_only_what_it_names() {
    let h = crate::agents_support::start_agents("run_native_strict", false).await;
    join_host(&h, &NATIVE_HOST).await;
    let lease = lease(&h).await;
    let extras = [
        ("env", json!({ "A": "1" })),
        ("inputs", json!([])),
        ("sandbox", json!("off")),
        ("network", json!("everywhere")),
    ];
    for (key, value) in extras {
        let mut body = native_body("/usr/bin/true");
        body["native"][key] = value;
        let reply = queue(&h, &lease, &body).await;
        assert_eq!(reply.status, 400, "{key}: {}", reply.body);
    }
    let mut missing = native_body("/usr/bin/true");
    missing["native"].as_object_mut().unwrap().remove("program");
    assert_eq!(queue(&h, &lease, &missing).await.status, 400);
    assert!(runs(&h, &lease).await.is_empty());
}

/// Asserts that a body of the old shape reads as it did: it is queued on a native host
/// too, its start has no `native` and no `bundle` key (the bytes an older agent has always
/// read), and a body without an image is still 400 and one with an empty image still 422
/// `invalid-image`. It catches a change that makes an old client's body mean something
/// else.
#[tokio::test]
async fn an_old_body_reads_as_it_always_did() {
    let h = crate::agents_support::start_agents("run_native_old", false).await;
    let me = join_host(&h, &NATIVE_HOST).await;
    let lease = lease(&h).await;
    let queued = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(queued.status, 202, "{}", queued.body);
    let reply = beat(&h, &me, &[], &[]).await;
    let start = &reply["commands"][0];
    assert_eq!(start["image"], run_body(1000)["image"]);
    assert!(start.get("native").is_none(), "{start}");
    assert!(start.get("bundle").is_none(), "{start}");
    let mut no_image = run_body(1000);
    no_image.as_object_mut().unwrap().remove("image");
    assert_eq!(queue(&h, &lease, &no_image).await.status, 400);
    let mut empty = run_body(1000);
    empty["image"] = json!("");
    let refused = queue(&h, &lease, &empty).await;
    assert_eq!(
        (refused.status, refused.json()),
        (422, json!({ "error": "invalid-image" }))
    );
}
