//! `memory_schedulable_percent`: the share of the memory an operator configured that the
//! server books, for a `[[pool]]` of the configuration file and the agent pool file, and
//! what the routes show of it.

use serde_json::{Value, json};

use crate::agents_support::{JOIN, POOL, agent_args, entry, hello_body, hosts, post, session};
use crate::config_support::{
    PoolText, file, load, quote, quoted, refusal, server, server_with_agents,
};
use crate::jobs_support::{JobBody, submit_body};
use crate::support::{Harness, Scratch, config, replace, request};

const GIB: u64 = 1 << 30;

/// The memory the agent pool lists: 16 GiB, as `agents_support::POOL` has it.
const POOLED: u64 = 16 * GIB;

/// What 90 percent of the agent pool's memory books, rounded down.
const POOLED_NINETY: u64 = POOLED * 90 / 100;

/// Asserts that a percent of 0, 101 or a word is refused when the configuration is
/// checked, naming the pool and the field, for a joined pool and for an elastic one. It
/// catches a pool that starts booking nothing, or more than the host has.
#[test]
fn a_percent_outside_one_to_a_hundred_is_refused() {
    let scratch = Scratch::new("mempct_refusals");
    let key = "memory_schedulable_percent";
    let cases = [
        ("0", "must be from 1 to 100"),
        ("101", "must be from 1 to 100"),
        ("-1", "must not be negative"),
        ("\"ninety\"", "must be an integer"),
    ];
    for (value, reason) in cases {
        let pool = PoolText::joined("hosts").set(key, value);
        let said = refusal(&scratch, &file(&server(&scratch, ""), &[&pool]));
        assert_eq!(said, format!("pool \"hosts\": {key}: {reason}"));
        let elastic = PoolText::elastic(&scratch).set(key, value);
        let text = file(&server_with_agents(&scratch), &[&elastic]);
        assert_eq!(
            refusal(&scratch, &text),
            format!("pool \"gpu\": {key}: {reason}")
        );
    }
}

/// The agent pool file with `memory_schedulable_percent = 90` before its `[shape]`.
fn ninety_pool() -> String {
    POOL.replace(
        "max_machines = 2\n",
        "max_machines = 2\nmemory_schedulable_percent = 90\n",
    )
}

/// A server with agent registration on and the job service on a loopback port, whose agent
/// pool file is `text`.
async fn registering(name: &str, text: &str) -> Harness {
    let scratch = Scratch::new(name);
    let path = scratch.write("agent-pool-memory.toml", text, 0o644);
    let mut args = replace(
        agent_args(&scratch),
        "--agent-pool-config",
        path.into_os_string(),
    );
    args.push("--job-listen".into());
    args.push("127.0.0.1:0".into());
    Harness::with(scratch, args).await
}

/// Submits a job `id` that needs `memory_bytes` and answers the status and the error code,
/// or "queued".
async fn ask(h: &Harness, id: &str, memory_bytes: u64) -> (u16, String) {
    let reply = submit_body(h, &JobBody::new(id).needs(1000, memory_bytes).json()).await;
    let said = match reply.status {
        201 => "queued".to_owned(),
        _ => reply.json()["error"].as_str().unwrap_or("").to_owned(),
    };
    (reply.status, said)
}

/// A hello of `host` claiming `memory_bytes`.
fn claiming(host: &str, memory_bytes: u64) -> Value {
    let mut body = hello_body(host);
    body["shape"]["memory_bytes"] = json!(memory_bytes);
    body
}

/// Asserts that an agent pool with 90 books exactly floor(16 GiB x 90 / 100) of its
/// `[shape]` memory for a host, whatever the host claims: a host that claims 100 GiB is
/// booked at the cut all the same (a job needing one byte more is refused `no-capable-host`,
/// one needing exactly the cut is admitted), so an agent cannot inflate what it offers. It catches a booking that follows the
/// claim and a cut that is not applied to the pool file's shape.
#[tokio::test]
async fn the_agent_pool_percent_books_the_cut_whatever_the_claim() {
    let h = registering("mempct_agents_90", &ninety_pool()).await;
    let reply = post(
        &h,
        "/v1/agent/hello",
        Some(JOIN),
        &claiming("big-1", 100 * GIB),
    )
    .await;
    session(&reply, "big-1");
    assert_eq!(
        ask(&h, "over-by-one", POOLED_NINETY + 1).await,
        (422, "no-capable-host".to_owned())
    );
    assert_eq!(ask(&h, "whole-listing", POOLED).await.1, "no-capable-host");
    assert_eq!(
        ask(&h, "at-the-cut", POOLED_NINETY).await,
        (201, "queued".to_owned())
    );
}

/// Asserts that a hello is held to the memory the pool file lists, not to the cut: a host
/// that claims the cut is refused `shape-not-accepted` and one that claims the listing is
/// admitted. The host must really have the memory; the pool books less of it.
#[tokio::test]
async fn a_hello_must_meet_the_listed_memory_not_the_cut() {
    let h = registering("mempct_agents_floor", &ninety_pool()).await;
    let short = post(
        &h,
        "/v1/agent/hello",
        Some(JOIN),
        &claiming("short-1", POOLED_NINETY),
    )
    .await;
    assert_eq!(short.status, 403, "{}", short.body);
    assert_eq!(
        short.json(),
        json!({ "error": "shape-not-accepted", "reason": "below-floor" })
    );
    let exact = post(
        &h,
        "/v1/agent/hello",
        Some(JOIN),
        &claiming("exact-1", POOLED),
    )
    .await;
    session(&exact, "exact-1");
}

/// Asserts that an agent pool file without the key books all of its shape memory, and
/// that 0 and 101 are refused at start naming the flag and the key.
#[tokio::test]
async fn the_agent_pool_default_is_unchanged_and_bad_values_are_refused() {
    let h = registering("mempct_agents_default", POOL).await;
    session(
        &post(&h, "/v1/agent/hello", Some(JOIN), &claiming("a-1", POOLED)).await,
        "a-1",
    );
    assert_eq!(
        ask(&h, "over-by-one", POOLED + 1).await,
        (422, "no-capable-host".to_owned())
    );
    assert_eq!(
        ask(&h, "at-the-listing", POOLED).await,
        (201, "queued".to_owned())
    );
    for bad in ["0", "101"] {
        let scratch = Scratch::new(&format!("mempct_agents_bad_{bad}"));
        let text = POOL.replace(
            "max_machines = 2\n",
            &format!("max_machines = 2\nmemory_schedulable_percent = {bad}\n"),
        );
        let path = scratch.write("agent-pool-memory.toml", &text, 0o644);
        let args = replace(
            agent_args(&scratch),
            "--agent-pool-config",
            path.clone().into_os_string(),
        );
        let said = config(args).err().unwrap().to_string();
        assert!(said.contains("--agent-pool-config"), "{said}");
        assert!(
            said.ends_with("memory_schedulable_percent must be from 1 to 100"),
            "{said}"
        );
    }
}

/// Asserts that `GET /v1/hosts` shows, for a registered host, the memory listed, the percent
/// and the bookable memory, and that `shape.memory_bytes` is
/// the bookable memory. It catches a listing that shows the unreduced figure as what is
/// booked.
#[tokio::test]
async fn hosts_show_the_listed_memory_the_percent_and_the_bookable_memory() {
    let h = registering("mempct_hosts_registered", &ninety_pool()).await;
    session(
        &post(
            &h,
            "/v1/agent/hello",
            Some(JOIN),
            &claiming("big-1", 100 * GIB),
        )
        .await,
        "big-1",
    );
    let listing = hosts(&h).await;
    let shown = entry(&listing, "big-1");
    assert_eq!(shown["shape"]["memory_bytes"], POOLED_NINETY);
    assert_eq!(
        shown["memory"],
        json!({
            "configured_bytes": POOLED,
            "schedulable_percent": 90,
            "bookable_bytes": POOLED_NINETY,
        })
    );
}

/// Asserts that `GET /v1/pools` and `/status` show an elastic pool's floor memory (1 GiB),
/// the percent and the bookable memory, and that a pool without the key shows 100 and the
/// whole floor.
#[tokio::test]
async fn the_pool_status_shows_the_floor_memory_the_percent_and_the_bookable_memory() {
    for (name, percent, bookable) in [
        ("mempct_pool_status_50", Some("50"), GIB / 2),
        ("mempct_pool_status_default", None, GIB),
    ] {
        let scratch = Scratch::new(name);
        let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
        let mut pool = PoolText::elastic(&scratch)
            .set("exec", &quote("none"))
            .set("join_token_file", &quoted(&joins));
        if let Some(percent) = percent {
            pool = pool.set("memory_schedulable_percent", percent);
        }
        let text = file(&server_with_agents(&scratch), &[&pool]);
        let config = load(&scratch, &text).unwrap();
        let h = Harness::over(scratch, config).await;
        let reply = request(h.addr(), "GET", "/v1/pools", None, None).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let shown = reply.json()["pools"][0]["memory"].clone();
        let expected = json!({
            "configured_bytes": GIB,
            "schedulable_percent": percent.map_or(100, |p| p.parse::<u64>().unwrap()),
            "bookable_bytes": bookable,
        });
        assert_eq!(shown, expected);
        let page = request(h.addr(), "GET", "/status", None, None).await;
        assert_eq!(page.status, 200, "{}", page.body);
        let line = format!(
            "{bookable} of {GIB} bytes ({}%)",
            expected["schedulable_percent"]
        );
        assert!(page.body.contains(&line), "{}", page.body);
    }
}
