//! What a pool that launches machines needs of the file besides its kind: the scheduler's
//! id, the agent and Tailscale build the bootstrap downloads (each an `https://` address and
//! a SHA-256), and the address the machines dial; and the `lambdalabs` kind as the server
//! ships it, read offline through the owner-only reader. Each refusal is checked with its
//! exact message, naming the pool and the field.

use std::os::unix::fs::PermissionsExt;

use js_elastic::Artifact;
use js_lease_server::{Config, Kinds, NetworkPlan, OwnerOnlyFiles};

use crate::config_support::{
    PoolText, clock, file, load, oauth_file, quote, refusal, server, server_with_agents,
};
use crate::support::Scratch;

const URL: &str = "https://pkgs.example.org/js-agent-x86_64";
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A `[server]` table that lets an elastic pool start, with `extra` lines after the
/// listeners.
fn with_server(scratch: &Scratch, extra: &str) -> String {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    server(scratch, &format!("{listeners}\n{extra}"))
}

/// A server that tells launched machines where to dial.
fn dialable(scratch: &Scratch) -> String {
    with_server(
        scratch,
        "advertise_url = \"https://sched.example.org:7301\"",
    )
}

fn agent(url: &str, sha: &str) -> String {
    format!("{{ url = {}, sha256 = {} }}", quote(url), quote(sha))
}

/// The refusal of a file whose elastic pool is `pool`, beside a server that can be dialed.
fn elastic(scratch: &Scratch, pool: &PoolText) -> String {
    refusal(scratch, &file(&dialable(scratch), &[pool]))
}

/// Asserts the scheduler's id: `js` when the file names none, the file's when it does, and
/// a refusal naming the alphabet for one that is not an id, without repeating it. The id
/// marks what the scheduler launches, so two schedulers on one account must differ.
#[test]
fn the_scheduler_id_defaults_and_is_checked() {
    let scratch = Scratch::new("launch_scheduler");
    let pool = PoolText::elastic(&scratch);
    let plan = |server: String| {
        let config = load(&scratch, &file(&server, &[&pool])).unwrap();
        config.elastic_pool().unwrap().scheduler.as_str().to_owned()
    };
    assert_eq!(plan(with_server(&scratch, "")), "js");
    assert_eq!(
        plan(with_server(&scratch, "scheduler_id = \"sched.one_2-x\"")),
        "sched.one_2-x"
    );
    for (value, reason) in [
        (
            "\"has space\"",
            "an id is 1 to 64 letters, digits, '.', '_' or '-'",
        ),
        ("\"\"", "an id is 1 to 64 letters, digits, '.', '_' or '-'"),
        ("5", "must be a string"),
    ] {
        let text = file(
            &with_server(&scratch, &format!("scheduler_id = {value}")),
            &[&pool],
        );
        assert_eq!(
            refusal(&scratch, &text),
            format!("server.scheduler_id: {reason}")
        );
    }
}

/// Asserts the `agent` table: a pinned download is read as its address and hash, and each
/// way it can be wrong is refused naming the field: not a table, an unknown key, a missing
/// or non-https address, and a hash that is missing or not 64 lower-case hex digits (short,
/// long, upper case, or a digit that is not hex in either place of a pair). A static pool
/// has no use for it. It catches a download over plain http (the bootstrap refuses it, so
/// the machine would boot and never join) and a hash that is not checked.
#[test]
fn the_agent_pin_is_an_https_address_and_a_hash() {
    let scratch = Scratch::new("launch_agent");
    let pool = |value: &str| PoolText::elastic(&scratch).set("agent", value);
    let config = load(
        &scratch,
        &file(&dialable(&scratch), &[&pool(&agent(URL, SHA))]),
    )
    .unwrap();
    let pinned = config.elastic_pool().unwrap().agent.clone();
    let expected = Artifact {
        url: URL.to_owned(),
        sha256: [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xab, 0xcd, 0xef,
        ],
    };
    assert_eq!(pinned, Some(expected));

    let case = |value: String, said: &str| {
        let got = elastic(&scratch, &pool(&value));
        assert_eq!(got, format!("pool \"gpu\": agent{said}"), "{value}");
    };
    case("5".to_owned(), ": must be a table");
    case(
        format!(
            "{{ url = {}, sha256 = {}, size = 1 }}",
            quote(URL),
            quote(SHA)
        ),
        ".size: unknown field; known: url, sha256",
    );
    case(format!("{{ sha256 = {} }}", quote(SHA)), ".url: required");
    let https = ".url: must be an https:// URL: the host refuses any other";
    for url in [
        "http://pkgs.example.org/agent",
        "https://",
        "pkgs.example.org",
        "https://a b",
    ] {
        case(agent(url, SHA), https);
    }
    case(format!("{{ url = {} }}", quote(URL)), ".sha256: required");
    let hash = ".sha256: must be 64 lower-case hex digits";
    let long = format!("{SHA}0");
    let upper = SHA.replace('a', "A");
    let high = format!("g{}", &SHA[1..]);
    let low = format!("{}g", &SHA[..63]);
    let bads = [
        &SHA[..63],
        long.as_str(),
        upper.as_str(),
        high.as_str(),
        low.as_str(),
        "",
    ];
    for bad in bads {
        case(agent(URL, bad), hash);
    }

    let hosts = PoolText::static_pool(&scratch, "hosts").set("agent", &agent(URL, SHA));
    let text = file(&server(&scratch, ""), &[&hosts]);
    assert_eq!(
        refusal(&scratch, &text),
        "pool \"hosts\": agent: only for an elastic pool"
    );
}

/// Asserts what a pool that launches needs: with `launch = "live"` the agent pin, the
/// Tailscale pin when the network is Tailscale, and the address the machines dial, each
/// refused with its own message when missing, and a pool in `plan` mode needing none of
/// them. It catches a live pool that boots machines which cannot download their agent, and
/// a plan pool that needs what only a launch uses.
#[test]
fn a_live_pool_needs_the_pins_and_the_address_a_plan_pool_does_not() {
    let scratch = Scratch::new("launch_live");
    let live = PoolText::elastic(&scratch).set("launch", &quote("live"));
    let needs = "required with launch = \"live\": the bootstrap downloads it";

    assert_eq!(
        elastic(&scratch, &live),
        format!("pool \"gpu\": agent: {needs}")
    );
    let plain = live.set("agent", &agent(URL, SHA));
    let silent = file(&with_server(&scratch, ""), &[&plain]);
    assert_eq!(
        refusal(&scratch, &silent),
        "server.advertise_url: required with a pool whose launch is live: the hosts it launches dial it"
    );
    let ready = load(&scratch, &file(&dialable(&scratch), &[&plain]));
    assert!(ready.is_ok());

    let oauth = oauth_file(&scratch);
    let tailscale =
        format!("{{ kind = \"tailscale\", oauth_client_file = {oauth}, tag = \"tag:js-agent\" }}");
    let unpinned = plain.set("network", &tailscale);
    assert_eq!(
        elastic(&scratch, &unpinned),
        format!("pool \"gpu\": network.binary: {needs}")
    );
    let binary = format!(
        "{{ kind = \"tailscale\", oauth_client_file = {oauth}, tag = \"tag:js-agent\", \
           api_url = \"http://127.0.0.1:9\", binary = {} }}",
        agent("https://pkgs.example.org/tailscale.tgz", SHA)
    );
    let pinned = unpinned.set("network", &binary);
    let config = load(&scratch, &file(&dialable(&scratch), &[&pinned])).unwrap();
    let plan = config.elastic_pool().unwrap();
    match &plan.network {
        NetworkPlan::Tailscale {
            binary, api_url, ..
        } => {
            let pin = binary.as_ref().unwrap();
            assert_eq!(pin.url, "https://pkgs.example.org/tailscale.tgz");
            assert_eq!(api_url.as_deref(), Some("http://127.0.0.1:9"));
        }
        other => panic!("{other:?}"),
    }
    let bad = binary.replace("https://pkgs", "http://pkgs");
    let wrong = pinned.set("network", &bad);
    assert_eq!(
        elastic(&scratch, &wrong),
        "pool \"gpu\": network.binary.url: must be an https:// URL: the host refuses any other"
    );

    let plan_only = PoolText::elastic(&scratch);
    let tailscale_plan = plan_only.set("network", &tailscale);
    assert!(
        load(
            &scratch,
            &file(&with_server(&scratch, ""), &[&tailscale_plan])
        )
        .is_ok()
    );
}

/// The text of a pool `gpu` of the shipped `lambdalabs` kind, reading `key` and listing
/// `types`, with its API address on the loopback interface, and `more` after them.
fn lambda(scratch: &Scratch, key: &std::path::Path, types: &str, more: &str) -> PoolText {
    let provider = format!(
        "{{ kind = \"lambdalabs\", api_key_file = {:?}, types = {types}, \
          api_url = \"http://127.0.0.1:9\"{more} }}",
        key.display().to_string()
    );
    PoolText::elastic(scratch).set("provider", &provider)
}

const A10: &str = "[{ name = \"gpu_1x_a10\", gpu = \"nvidia-a10\", gpus = 1 }]";

/// Reads `text` with the kinds the server ships.
fn shipped(scratch: &Scratch, text: &str) -> Result<Config, js_lease_server::StartupError> {
    let path = scratch.write("js.toml", text, 0o644);
    Config::from_config_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock())
}

/// Asserts the `lambdalabs` kind as shipped, read through the server's owner-only reader:
/// the design's pool builds in `plan` (with an `api_url` on the loopback interface, and no
/// call made); a key file others can read is refused naming the pool, the field and the
/// mode, never the key; a table the kind refuses (a type listed twice, a second GPU class)
/// is refused naming the row; and `launch = "live"` builds the provider too. It catches a
/// kind that reads its key file other than through the reader, and one the server does not
/// register.
#[test]
fn the_lambdalabs_kind_is_read_as_shipped() {
    let scratch = Scratch::new("launch_lambda");
    let secret = "PLANTED-LAMBDA-KEY-VALUE";
    let key = scratch.write("lambda-key", secret, 0o600);
    let plan = lambda(&scratch, &key, A10, "");
    let built = shipped(&scratch, &file(&server_with_agents(&scratch), &[&plan]));
    let config = built.unwrap();
    assert!(config.elastic_pool().is_some());

    let live = lambda(&scratch, &key, A10, "")
        .set("launch", &quote("live"))
        .set("agent", &agent(URL, SHA));
    assert!(shipped(&scratch, &file(&dialable(&scratch), &[&live])).is_ok());

    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let said = shipped(&scratch, &file(&server_with_agents(&scratch), &[&plan]))
        .err()
        .unwrap()
        .to_string();
    assert!(
        said.starts_with("pool \"gpu\": provider.api_key_file: "),
        "{said}"
    );
    assert!(said.contains("0644"), "{said}");
    assert!(!said.contains(secret), "{said}");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();

    let twice = "[{ name = \"gpu_1x_a10\", gpu = \"nvidia-a10\", gpus = 1 }, \
                 { name = \"gpu_1x_a10\", gpu = \"nvidia-a10\", gpus = 2 }]";
    let said = shipped(
        &scratch,
        &file(
            &server_with_agents(&scratch),
            &[&lambda(&scratch, &key, twice, "")],
        ),
    );
    assert_eq!(
        said.err().unwrap().to_string(),
        "pool \"gpu\": provider.types[2].name: \"gpu_1x_a10\" is listed twice"
    );
    let classes = "[{ name = \"a\", gpu = \"nvidia-a10\", gpus = 1 }, { name = \"b\", gpu = \"nvidia-h100\", gpus = 1 }]";
    let said = shipped(
        &scratch,
        &file(
            &server_with_agents(&scratch),
            &[&lambda(&scratch, &key, classes, "")],
        ),
    );
    assert_eq!(
        said.err().unwrap().to_string(),
        "pool \"gpu\": provider.types[2].gpu: a pool has one gpu class; \"nvidia-a10\" is the class of types[1], not \"nvidia-h100\""
    );
}
