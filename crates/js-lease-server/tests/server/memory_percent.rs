//! `memory_schedulable_percent`: the share of the memory an operator configured that the
//! server books, for a static hosts file, a `[[pool]]` of the configuration file and the
//! agent pool file, and what the routes show of it.

use serde_json::{Value, json};

use crate::agents_support::{JOIN, POOL, agent_args, entry, hello_body, hosts, post, session};
use crate::config_support::{
    PoolText, file, load, quote, quoted, refusal, server, server_with_agents,
};
use crate::support::{Harness, Scratch, base_args, config, replace, request};

const GIB: u64 = 1 << 30;

/// The memory the static hosts list: 251 GiB.
const LISTED: u64 = 251 * GIB;

/// What 90 percent of it books, rounded down.
const NINETY: u64 = LISTED * 90 / 100;

/// The memory the agent pool lists: 16 GiB, as `agents_support::POOL` has it.
const POOLED: u64 = 16 * GIB;

/// What 90 percent of the agent pool's memory books, rounded down.
const POOLED_NINETY: u64 = POOLED * 90 / 100;

/// A hosts file of one 251 GiB host with `line` (a `memory_schedulable_percent` line, or
/// nothing) before it.
fn hosts_text(line: &str) -> String {
    format!(
        "{line}\n[[host]]\nid = \"big-01\"\ngpu_class = \"nvidia-a100\"\n\n[host.resources]\n\
         cpu_millis = 64000\nmemory_bytes = {LISTED}\ngpus = 8\n\n[host.capabilities]\n\
         isa = \"x86-64-v3\"\n"
    )
}

/// A request for one host of the pool with `memory_bytes`.
fn lease(memory_bytes: u64) -> Value {
    json!({
        "class": "gpu-host",
        "resources": {
            "cpu_millis": 1000,
            "memory_bytes": memory_bytes,
            "gpus": 1,
            "gpu_class": "nvidia-a100",
        },
        "capabilities": { "isa": "x86-64-v3" },
        "max_wall_seconds": 7200,
    })
}

/// Asks `h` for a lease with `memory_bytes` and answers the status and the error code,
/// or "granted".
async fn ask(h: &Harness, memory_bytes: u64) -> (u16, String) {
    let reply = h
        .call("POST", "/v1/leases", Some(&lease(memory_bytes)))
        .await;
    let said = match reply.status {
        201 => "granted".to_owned(),
        _ => reply.json()["error"].as_str().unwrap_or("").to_owned(),
    };
    (reply.status, said)
}

/// A server over a static hosts file given by the flag `--pool-config`.
async fn flagged(name: &str, hosts_line: &str) -> Harness {
    let scratch = Scratch::new(name);
    let path = scratch.write("big-hosts.toml", &hosts_text(hosts_line), 0o644);
    let args = replace(base_args(&scratch), "--pool-config", path.into_os_string());
    Harness::with(scratch, args, None).await
}

/// A server over the configuration file whose only pool is a static pool over a hosts
/// file with `hosts_line`, and the `[[pool]]` key `pool_percent` when given.
async fn configured(name: &str, hosts_line: &str, pool_percent: Option<&str>) -> Harness {
    let scratch = Scratch::new(name);
    let path = scratch.write("big-hosts.toml", &hosts_text(hosts_line), 0o644);
    let mut pool = PoolText::static_pool(&scratch, "big").set(
        "provider",
        &format!("{{ kind = \"static\", hosts_file = {} }}", quoted(&path)),
    );
    if let Some(percent) = pool_percent {
        pool = pool.set("memory_schedulable_percent", percent);
    }
    let text = file(&server(&scratch, "listen = \"127.0.0.1:0\""), &[&pool]);
    let config = load(&scratch, &text).unwrap();
    Harness::over(scratch, config).await
}

/// Asserts that a static hosts file with 90 books exactly floor(251 GiB x 90 / 100) of
/// its host: a request of one byte more is refused `no-fit` and books nothing, and a
/// request of exactly that is granted. It catches a key that is read and dropped, a cut
/// that rounds up, and one applied to the request.
#[tokio::test]
async fn the_hosts_file_percent_books_the_cut() {
    let h = flagged("mempct_flag_90", "memory_schedulable_percent = 90").await;
    assert_eq!(ask(&h, NINETY + 1).await, (409, "no-fit".to_owned()));
    assert_eq!(ask(&h, NINETY).await, (201, "granted".to_owned()));
}

/// Asserts that without the key the whole of the listed memory is booked, exactly as it
/// was: the default of 100 is unchanged.
#[tokio::test]
async fn without_the_key_all_of_the_memory_is_booked() {
    let h = flagged("mempct_flag_default", "").await;
    assert_eq!(ask(&h, LISTED + 1).await, (409, "no-fit".to_owned()));
    assert_eq!(ask(&h, LISTED).await, (201, "granted".to_owned()));
    let h = flagged("mempct_flag_100", "memory_schedulable_percent = 100").await;
    assert_eq!(ask(&h, LISTED).await, (201, "granted".to_owned()));
}

/// Asserts that the `[[pool]]` key books the cut for a static pool, and that it is the
/// pool's key that counts when the hosts file says another: 90 in the pool over 50 in the
/// file books 90, once (not 45). It catches the two being multiplied and a pool key that
/// is ignored for a static pool.
#[tokio::test]
async fn the_pool_key_books_the_cut_of_a_static_pool() {
    let h = configured("mempct_pool_90", "", Some("90")).await;
    assert_eq!(ask(&h, NINETY + 1).await, (409, "no-fit".to_owned()));
    assert_eq!(ask(&h, NINETY).await, (201, "granted".to_owned()));
    let both = configured(
        "mempct_pool_over_file",
        "memory_schedulable_percent = 50",
        Some("90"),
    );
    let both = both.await;
    assert_eq!(ask(&both, NINETY).await, (201, "granted".to_owned()));
    let file_only = configured("mempct_file_only", "memory_schedulable_percent = 50", None).await;
    assert_eq!(ask(&file_only, NINETY).await, (409, "no-fit".to_owned()));
    assert_eq!(
        ask(&file_only, LISTED / 2).await,
        (201, "granted".to_owned())
    );
}

/// Asserts that a percent of 0, 101 or a word is refused when the configuration is
/// checked, naming the pool and the field, for a static pool and for an elastic one, and
/// that a hosts file with such a value is refused naming the field and the file. It
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
        let pool = PoolText::static_pool(&scratch, "hosts").set(key, value);
        let said = refusal(&scratch, &file(&server(&scratch, ""), &[&pool]));
        assert_eq!(said, format!("pool \"hosts\": {key}: {reason}"));
        let elastic = PoolText::elastic(&scratch).set(key, value);
        let text = file(&server_with_agents(&scratch), &[&elastic]);
        assert_eq!(
            refusal(&scratch, &text),
            format!("pool \"gpu\": {key}: {reason}")
        );
    }
    let hosts = scratch.write(
        "zero-hosts.toml",
        &hosts_text("memory_schedulable_percent = 0"),
        0o644,
    );
    let pool = PoolText::static_pool(&scratch, "hosts").set(
        "provider",
        &format!("{{ kind = \"static\", hosts_file = {} }}", quoted(&hosts)),
    );
    let said = refusal(&scratch, &file(&server(&scratch, ""), &[&pool]));
    assert_eq!(
        said,
        format!(
            "pool \"hosts\": provider.hosts_file: {}: memory_schedulable_percent must be from 1 to 100",
            hosts.display()
        )
    );
}

/// The agent pool file with `memory_schedulable_percent = 90` before its `[shape]`.
fn ninety_pool() -> String {
    POOL.replace(
        "max_machines = 2\n",
        "max_machines = 2\nmemory_schedulable_percent = 90\n",
    )
}

/// A server with agent registration on and no static pool, whose agent pool file is `text`.
async fn registering(name: &str, text: &str) -> Harness {
    let scratch = Scratch::new(name);
    let path = scratch.write("agent-pool-memory.toml", text, 0o644);
    let args = replace(
        agent_args(&scratch, false),
        "--agent-pool-config",
        path.into_os_string(),
    );
    Harness::with(scratch, args, None).await
}

/// A hello of `host` claiming `memory_bytes`.
fn claiming(host: &str, memory_bytes: u64) -> Value {
    let mut body = hello_body(host);
    body["shape"]["memory_bytes"] = json!(memory_bytes);
    body
}

/// Asserts that an agent pool with 90 books exactly floor(16 GiB x 90 / 100) of its
/// `[shape]` memory for a host, whatever the host claims: a host that claims 100 GiB is
/// booked at the cut all the same (a request of one byte more is `no-fit`, exactly the cut
/// is granted), so an agent cannot inflate a lease. It catches a booking that follows the
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
    assert_eq!(ask(&h, POOLED_NINETY + 1).await, (409, "no-fit".to_owned()));
    assert_eq!(ask(&h, POOLED).await.1, "no-fit");
    assert_eq!(ask(&h, POOLED_NINETY).await, (201, "granted".to_owned()));
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
    assert_eq!(short.json(), json!({ "error": "shape-not-accepted" }));
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
    assert_eq!(ask(&h, POOLED + 1).await, (409, "no-fit".to_owned()));
    assert_eq!(ask(&h, POOLED).await, (201, "granted".to_owned()));
    for bad in ["0", "101"] {
        let scratch = Scratch::new(&format!("mempct_agents_bad_{bad}"));
        let text = POOL.replace(
            "max_machines = 2\n",
            &format!("max_machines = 2\nmemory_schedulable_percent = {bad}\n"),
        );
        let path = scratch.write("agent-pool-memory.toml", &text, 0o644);
        let args = replace(
            agent_args(&scratch, false),
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

/// Asserts that `GET /v1/hosts` shows, for a registered host and for a static one, the
/// memory listed, the percent and the bookable memory, and that `shape.memory_bytes` is
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
    let h = flagged("mempct_hosts_static", "memory_schedulable_percent = 90").await;
    let shown = entry(&hosts(&h).await, "big-01");
    assert_eq!(shown["shape"]["memory_bytes"], NINETY);
    assert_eq!(
        shown["memory"],
        json!({
            "configured_bytes": LISTED,
            "schedulable_percent": 90,
            "bookable_bytes": NINETY,
        })
    );
    let h = flagged("mempct_hosts_static_default", "").await;
    let shown = entry(&hosts(&h).await, "big-01");
    assert_eq!(shown["memory"]["schedulable_percent"], 100);
    assert_eq!(shown["memory"]["bookable_bytes"], LISTED);
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
