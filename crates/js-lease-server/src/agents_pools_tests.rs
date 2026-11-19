//! The agent side over several pools, assembled in process from the flags as the server
//! assembles it: no listener is bound and no connection is made.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use js_agent_wire::Taint;
use js_registry::{Binding, Credential, PoolConfig};
use js_testkit::dir::TestDir;
use js_types::{ClassId, HostId, ManualClock, ProviderId, Timestamp};

use crate::agent_pool::shipped_class;
use crate::agent_state::BindingStore;
use crate::agents::{assemble, state_path};
use crate::api::rig::{Lines, process_sink};
use crate::clock::SharedClock;
use crate::config::{Cli, Config};

/// A pool `name` of up to two hosts of 8 cores and 16 GiB on x86_64.
fn pool(name: &str) -> String {
    format!(
        "name = \"{name}\"\nclass = \"burst-host\"\nmax_machines = 2\n\n\
         [shape]\ncpu_millis = 8000\nmemory_bytes = 17179869184\n\n\
         [capabilities]\narch = \"x86_64\"\n"
    )
}

fn write(dir: &Path, name: &str, text: &str, mode: u32) -> OsString {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.into_os_string()
}

/// The configuration of a server of the pools `burst` and `macs`, each from its own
/// `--agent-pool-config`.
fn two_pools(dir: &Path) -> Config {
    let mut args: Vec<OsString> = vec!["js-lease-server".into()];
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", dir.join("state").into_os_string()),
        ("--boot", "initialise".into()),
        ("--token-file", write(dir, "token", &"t".repeat(40), 0o600)),
        (
            "--agent-pool-config",
            write(dir, "a.toml", &pool("burst"), 0o644),
        ),
        (
            "--agent-pool-config",
            write(dir, "b.toml", &pool("macs"), 0o644),
        ),
    ];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    Config::from_cli(Cli::try_parse_from(args).unwrap()).unwrap()
}

/// Asserts that the agent side refuses pools of one name that reach it, naming the flag,
/// although the configuration's own check refuses them first: the registry's refusal is the
/// last word on a hello that could not say which pool it joins. With two names it assembles
/// a registry named for the first pool.
#[test]
fn the_agent_side_refuses_two_pools_of_one_name_and_takes_two_names() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents_pools");
    let dir = TestDir::under(root, "two_names");
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(1_000_000)));
    let config = two_pools(dir.path());
    let state = config.state_file.clone();
    let assembled = assemble(config.agents, config.usage, &clock, &state, None).unwrap();
    assert_eq!(assembled.registry.pool_id().as_str(), "burst");
    assert_eq!(assembled.more_tokens[0].pool, "macs");
    drop(assembled);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents_pools");
    let dir = TestDir::under(root, "one_name");
    let mut config = two_pools(dir.path());
    let class = shipped_class(ClassId::new("burst-host").unwrap());
    let name = ProviderId::new("burst").unwrap();
    let again = PoolConfig::new(name, class, config.agents.pool.shape().clone(), 2);
    config.agents.more[0].pool = again;
    let state = config.state_file.clone();
    let refused = assemble(config.agents, config.usage, &clock, &state, None);
    assert_eq!(
        refused.err().unwrap().to_string(),
        "--agent-pool-config: two pools are named \"burst\": a hello could not say which it joins"
    );
}

/// Asserts that a start whose bindings hold a taint, and a cordon still to run, logs once per
/// such host, as info, what is put back (host, `when=start`, the taint's reason, check and
/// run id, and the cordon's end), and nothing for a host whose binding holds neither. It
/// catches a restart that brings back taints and cordons without a word, so that an operator
/// cannot tell why the hosts take no job.
#[test]
fn a_start_logs_the_taints_and_cordons_it_puts_back() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents_pools");
    let dir = TestDir::under(root, "put_back_logged");
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(1_000_000)));
    let config = two_pools(dir.path());
    let state = config.state_file.clone();
    let host = |name: &str| HostId::new(name).unwrap();
    let credential = || Credential::of_token("token");
    let taint = Taint {
        reason: "residue".to_owned(),
        check: Some("tmp-files".to_owned()),
        run_id: Some("r1".to_owned()),
    };
    let kept = BTreeMap::from([
        (
            host("host-a"),
            Binding::new(credential(), None).with_taint(Some(taint)),
        ),
        (
            host("host-b"),
            Binding::new(credential(), None).with_cordon(Some(2_000_000)),
        ),
        (host("host-c"), Binding::new(credential(), None)),
    ]);
    let file = state_path(&state, ".agents");
    BindingStore::new(file, BTreeMap::new())
        .save(|| kept)
        .unwrap();
    let lines = Lines::default();
    process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let assembled = assemble(config.agents, config.usage, &clock, &state, None).unwrap();
    drop(guard);
    assert_eq!(assembled.registry.hosts().len(), 2);
    let text = lines.take();
    let message = "a host's stored taint or cordon is put back";
    let put: Vec<&str> = text.lines().filter(|line| line.contains(message)).collect();
    assert_eq!(put.len(), 2, "{text}");
    let first = "host=host-a when=\"start\" reason=\"residue\" check=\"tmp-files\" run_id=\"r1\"";
    assert!(put[0].ends_with(&format!("{message} {first}")), "{text}");
    let second = "host=host-b when=\"start\" cordon_until_ms=2000000";
    assert!(put[1].ends_with(&format!("{message} {second}")), "{text}");
}
