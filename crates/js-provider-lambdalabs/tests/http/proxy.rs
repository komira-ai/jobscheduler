//! The client reads no proxy setting from the environment. The test runs itself again as
//! a child with every proxy variable pointing at a listener that counts connections.

use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use js_provider_lambdalabs::{ApiKey, HttpApi, LambdaApi};

use crate::support::{PATIENT, Reply, Scratch, Server, fixture};

const BASE: &str = "LAMBDALABS_TEST_PROXY_CHILD_BASE";

#[test]
fn the_agent_reads_no_proxy_setting() {
    if let Ok(base) = std::env::var(BASE) {
        // The child: one read, which must reach the server named.
        let scratch = Scratch::new("proxy-child");
        let key = ApiKey::from_file(&scratch.key_file("proxy-key")).unwrap();
        let api = HttpApi::connect(&base, &key, None, PATIENT).unwrap();
        assert_eq!(api.instance_types().unwrap().len(), 4);
        return;
    }
    let target = Server::plain();
    target.on(
        "GET /api/v1/instance-types",
        Reply::json(200, &fixture("instance_types.json")),
    );
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let reached = Arc::new(AtomicUsize::new(0));
    {
        let reached = Arc::clone(&reached);
        thread::spawn(move || {
            for stream in proxy.incoming() {
                reached.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
    }
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "proxy::the_agent_reads_no_proxy_setting",
            "--test-threads=1",
        ])
        .env(BASE, target.url("http"))
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        child.env(name, &proxy_url);
    }
    let output = child.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        target.seen().len(),
        1,
        "the child's request did not reach the server"
    );
    assert_eq!(
        reached.load(Ordering::SeqCst),
        0,
        "the request went through the proxy"
    );
}
