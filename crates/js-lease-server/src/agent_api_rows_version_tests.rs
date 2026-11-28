//! The agent version a hello gave, as the host's row `node/<host>` keeps it: a server that did
//! not take the hello (one restarted since it) lists the version once a heartbeat of the
//! session reaches it, in its registry and in the job service's hosts listing, with no new
//! hello; and the heartbeats of the session keep it in the row.

use super::*;
use crate::api::Shared;
use crate::config::parse_token;
use crate::job_api::fleet::Hosts;

/// The version the hellos here give: no other hello of these tests gives it.
const VERSION: &str = "3.1.4";

/// The hello of `host` through the route of `server`, giving the agent version `version`, as
/// the session it opened.
async fn join_as(server: &Server, host: &str, version: &str) -> Sess {
    let mut hello: Value = serde_json::from_slice(&hello_body(host)).unwrap();
    hello["agent_version"] = json!(version);
    let body = Ok(Bytes::from(hello.to_string()));
    let welcome = serve_hello(&server.agents, host, body, &|| ());
    let Json(welcome) = welcome.await.unwrap();
    Sess {
        host: host.to_owned(),
        epoch: welcome.epoch,
        secret: welcome.session,
    }
}

/// The agent version the registry of `server` lists for `host`, which it must list.
fn in_registry(server: &Server, host: &str) -> Option<String> {
    let hosts = server.agents.registry.hosts();
    let view = hosts.into_iter().find(|view| view.id == id(host));
    view.expect("the registry lists the host").agent_version
}

/// The agent version the job service's hosts listing (`ListHosts`) of `server` gives `host`,
/// which it must list.
async fn in_listing(server: &Server, host: &str) -> Option<String> {
    let shared = Shared::new(
        parse_token(&[b'q'; 40]).unwrap(),
        Arc::clone(&server.agents),
    );
    let listed = Hosts::hosts(&shared).await.hosts;
    let entry = listed.into_iter().find(|entry| entry.id == id(host));
    entry.expect("the listing lists the host").agent_version
}

/// Asserts that a host whose hello one server took is listed with that hello's agent version by
/// a server started after it (as a restarted server is), through its registry and through the
/// job service's hosts listing, as soon as a heartbeat of the session reaches it and with no new
/// hello; that the row keeps the version through the heartbeats; and that a third server
/// started later lists it too. Catches a server that lists the version only of the hellos it
/// took itself, so that every host lists none after a restart until its agent says hello again.
#[tokio::test]
async fn a_server_started_after_the_hello_lists_its_agent_version() {
    let world = World::new();
    let before = world.server("rows_version_before", Kind::JOBS);
    let session = join_as(&before, "host-a", VERSION).await;
    let kept = |world: &World| world.row("host-a").record.agent_version;
    assert_eq!(kept(&world).as_deref(), Some(VERSION), "the hello's row");

    let after = world.server("rows_version_after", Kind::JOBS);
    after.beat(&session, Some(1), Vec::new()).await.unwrap();
    assert_eq!(after.holds("host-a"), Some(session.epoch));
    assert_eq!(in_registry(&after, "host-a").as_deref(), Some(VERSION));
    assert_eq!(in_listing(&after, "host-a").await.as_deref(), Some(VERSION));

    after.beat(&session, Some(2), Vec::new()).await.unwrap();
    assert_eq!(kept(&world).as_deref(), Some(VERSION), "after the beats");
    assert_eq!(in_registry(&after, "host-a").as_deref(), Some(VERSION));

    let later = world.server("rows_version_later", Kind::JOBS);
    later.beat(&session, Some(3), Vec::new()).await.unwrap();
    assert_eq!(in_listing(&later, "host-a").await.as_deref(), Some(VERSION));
}
