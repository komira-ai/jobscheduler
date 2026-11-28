//! `memory_schedulable_percent`: the share of the memory an operator configured that the
//! server books, for a `[[pool]]` of the configuration file and the agent pool file, and
//! what the routes show of it.

use serde_json::{Value, json};

use crate::agents_support::{POOL, agent_args, entry, hello_body, hosts, post, session};
use crate::config_support::{PoolText, file, load, quote, refusal, server, server_with_agents};
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

/// Asserts that an agent pool with 90 books 90 percent of the memory a host claims, not of
/// its `[shape]` memory: a host that claims 100 GiB is booked floor(100 GiB x 90 / 100), so
/// a job needing one byte more is refused `no-capable-host`, one needing exactly that cut
/// is admitted, and a job needing the pool's whole 16 GiB listing fits too. It catches a
/// booking capped at the pool's `[shape]` memory and a cut applied to the pool file's shape
/// instead of the claim.
#[tokio::test]
async fn the_agent_pool_percent_cuts_the_claim_of_a_host_larger_than_the_pool() {
    let h = registering("mempct_agents_90", &ninety_pool()).await;
    let reply = post(&h, "/v1/agent/hello", None, &claiming("big-1", 100 * GIB)).await;
    session(&reply, "big-1");
    let cut = 100 * GIB * 90 / 100;
    assert_eq!(
        ask(&h, "over-by-one", cut + 1).await,
        (422, "no-capable-host".to_owned())
    );
    assert_eq!(ask(&h, "whole-listing", POOLED).await.1, "queued");
    assert_eq!(ask(&h, "at-the-cut", cut).await, (201, "queued".to_owned()));
}

/// Asserts that a hello is held to no memory and that a host is booked the cut of what it
/// reports: a host claiming 14.4 GiB (the cut) in a pool that lists 16 GiB at 90 joins, a job
/// needing one byte more than floor(14.4 GiB x 90 / 100) is refused `no-capable-host`, one
/// needing exactly that is admitted, and `GET /v1/hosts` shows that as the host's memory. It
/// catches a floor that still refuses the smaller host, and a host admitted but booked the
/// pool's cut, which would place work on memory it does not have.
#[tokio::test]
async fn a_host_claiming_less_than_the_listing_joins_and_is_booked_the_cut_of_its_claim() {
    let h = registering("mempct_agents_floor", &ninety_pool()).await;
    let short = post(
        &h,
        "/v1/agent/hello",
        None,
        &claiming("short-1", POOLED_NINETY),
    )
    .await;
    session(&short, "short-1");
    let cut = POOLED_NINETY * 90 / 100;
    assert_eq!(
        ask(&h, "over-by-one", cut + 1).await,
        (422, "no-capable-host".to_owned())
    );
    assert_eq!(ask(&h, "at-the-cut", cut).await, (201, "queued".to_owned()));
    let listing = hosts(&h).await;
    let shown = entry(&listing, "short-1");
    assert_eq!(shown["shape"]["memory_bytes"], cut);
    assert_eq!(
        shown["memory"],
        json!({
            "configured_bytes": POOLED_NINETY,
            "schedulable_percent": 90,
            "bookable_bytes": cut,
        })
    );
}

/// Asserts that a host claiming 15 GiB in a pool whose `[shape]` lists 16 GiB (no percent)
/// joins and is booked against 15 GiB: a job needing one byte more than 15 GiB is refused
/// `no-capable-host` and one needing 15 GiB is admitted. It catches the `below-floor`
/// refusal that kept such a host out, and a booking at the pool's 16 GiB.
#[tokio::test]
async fn a_host_claiming_less_memory_than_the_pool_is_booked_at_its_claim() {
    let h = registering("mempct_agents_less", POOL).await;
    let claimed = POOLED - GIB;
    session(
        &post(&h, "/v1/agent/hello", None, &claiming("a-1", claimed)).await,
        "a-1",
    );
    assert_eq!(
        ask(&h, "over-by-one", claimed + 1).await,
        (422, "no-capable-host".to_owned())
    );
    assert_eq!(
        ask(&h, "at-the-claim", claimed).await,
        (201, "queued".to_owned())
    );
}

/// Asserts that an agent pool file without the key books all of its shape memory, and
/// that 0 and 101 are refused at start naming the flag and the key.
#[tokio::test]
async fn the_agent_pool_default_is_unchanged_and_bad_values_are_refused() {
    let h = registering("mempct_agents_default", POOL).await;
    session(
        &post(&h, "/v1/agent/hello", None, &claiming("a-1", POOLED)).await,
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

/// Asserts that `GET /v1/hosts` shows, for a registered host claiming 100 GiB in a pool
/// listing 16 GiB at 90 percent, the claimed memory as `configured_bytes`, the percent, and
/// 90 percent of the claim as `bookable_bytes`, and that `shape.memory_bytes` is that
/// bookable memory. It catches a listing that shows the pool's `[shape]` memory instead of
/// the claim, and one that shows the unreduced figure as what is booked.
#[tokio::test]
async fn hosts_show_the_claimed_memory_the_percent_and_the_bookable_memory() {
    let h = registering("mempct_hosts_registered", &ninety_pool()).await;
    session(
        &post(&h, "/v1/agent/hello", None, &claiming("big-1", 100 * GIB)).await,
        "big-1",
    );
    let listing = hosts(&h).await;
    let shown = entry(&listing, "big-1");
    let cut = 100 * GIB * 90 / 100;
    assert_eq!(shown["shape"]["memory_bytes"], cut);
    assert_eq!(
        shown["memory"],
        json!({
            "configured_bytes": 100 * GIB,
            "schedulable_percent": 90,
            "bookable_bytes": cut,
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
        let mut pool = PoolText::elastic(&scratch).set("exec", &quote("none"));
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
