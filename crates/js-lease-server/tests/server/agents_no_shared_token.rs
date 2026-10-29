//! A registering pool with no shared token file: only hosts whose one-time join token
//! verifies are admitted.

use std::ffi::OsString;

use crate::agents_support::{
    JOIN, JOIN_B, agent_args, hello_body, hello_with, hosts, listed, post, session,
};
use crate::joins::{beat_with, hello, minted};
use crate::support::{Harness, Scratch, TOKEN};

/// The flags of the agent tests without `--agent-token-file`.
fn without_shared_token(scratch: &Scratch) -> Vec<OsString> {
    let mut args = agent_args(scratch, false);
    let at = args.iter().position(|arg| arg == "--agent-token-file");
    args.drain(at.unwrap()..at.unwrap() + 2);
    args
}

async fn start_without_shared_token(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = without_shared_token(&scratch);
    Harness::with(scratch, args, None).await
}

/// Asserts the whole path of a pool with no shared token file: a hello with a token a
/// shared file would have held, with the client token, or with no token is a 401 with no
/// body; a hello with a one-time token minted for its instance registers the host; the
/// host heartbeats with it, also after a tick (which has no token file to read again); and
/// the host is listed. It catches a pool that admits a hello it has no token for, a start
/// that is still refused for lacking the file, and a tick that treats the missing file as
/// a failure.
#[tokio::test]
async fn only_a_one_time_token_registers_a_host() {
    let h = start_without_shared_token("agents_no_shared_token").await;
    for refused in [
        hello_with(&h, JOIN, "burst-1").await,
        hello_with(&h, JOIN_B, "burst-1").await,
        hello_with(&h, TOKEN, "burst-1").await,
        post(&h, "/v1/agent/hello", None, &hello_body("burst-1")).await,
    ] {
        assert_eq!(refused.status, 401, "{}", refused.body);
        assert!(refused.body.is_empty(), "{}", refused.body);
    }
    assert!(listed(&hosts(&h).await).iter().all(|id| id != "burst-1"));

    let token = minted(&h, "i-1").await;
    let welcome = hello(&h, &token, "burst-1", "i-1").await;
    let held = session(&welcome, "burst-1");
    h.ticks.step().await;
    assert_eq!(beat_with(&h, &token, &held).await.status, 200);
    assert!(listed(&hosts(&h).await).contains(&"burst-1".to_string()));
}
