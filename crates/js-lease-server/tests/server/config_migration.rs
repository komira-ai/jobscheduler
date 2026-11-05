//! The examples of "Migrating an agentless pool" in `docs/design/configuration.md`, run
//! through the loaders that read them: the old pool is refused with the migration, and
//! each of the two replacement routes loads.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use crate::agents_support::JOIN;
use crate::config_support::{load, refusal, server, server_with_agents};
use crate::support::{Scratch, base_args, config, take_fake_hosts};

/// Why the old agentless hosts-file pool is refused, as the server words it.
const MIGRATION: &str = "the agentless hosts-file pool was removed: run js-agent on each listed host and use --agent-pool-config, or a [[pool]] of kind = \"static\" (with its shape and max_machines)";

/// The token path the documented pool writes, which the tests swap for a scratch file.
const DOC_TOKEN_PATH: &str = "/run/credentials/js/join-token";

/// The `index`th (from 0) `toml` block under the heading `### <heading>` of the
/// configuration document, as it is written there; the section ends at the next heading.
fn migration_block(heading: &str, index: usize) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/configuration.md");
    let text = fs::read_to_string(path).unwrap();
    let start = text
        .find(&format!("\n### {heading}\n"))
        .expect("the heading is in the document");
    let rest = &text[start + 1..];
    let ends = ["\n## ", "\n### "].map(|mark| rest[4..].find(mark));
    let section = match ends.into_iter().flatten().min() {
        Some(end) => &rest[..end + 4],
        None => rest,
    };
    let fence = "```toml\n";
    let mut blocks = section.split(fence).skip(1);
    let block = blocks.nth(index).expect("the section has the block");
    block[..block.find("```").unwrap()].to_owned()
}

/// Asserts that the old `[[pool]]` of the document, under a `[server]` table, is refused
/// with the exact message naming the pool, `provider.kind` and the migration. It catches an
/// old example that stopped being refused, or a refusal that lost the migration text.
#[test]
fn the_documented_old_pool_is_refused_with_the_migration() {
    let scratch = Scratch::new("migration_old");
    let old = migration_block("The old files", 0);
    let text = format!("{}\n{old}", server(&scratch, ""));
    assert_eq!(
        refusal(&scratch, &text),
        format!("pool \"gpu\": provider.kind = \"static\", provider.hosts_file: {MIGRATION}")
    );
}

/// Asserts that route 1 of the document holds: its `[server]` fragment sets
/// `agent_listen`, and its `[[pool]]` of `kind = "static"` (with the documented token path
/// pointed at an owner-only scratch file holding a join token) loads under a server table
/// with agent listening, with no warning about the old `joined` spelling. It catches a
/// route-1 example that a file cannot load, a fragment that lost `agent_listen`, and a
/// pool spelled with the old kind.
#[test]
fn the_config_file_route_loads() {
    let scratch = Scratch::new("migration_config_route");
    let fragment: toml::Table = migration_block("Route 1: a pool in the `--config` file", 0)
        .parse()
        .unwrap();
    assert!(
        fragment["server"]
            .as_table()
            .unwrap()
            .contains_key("agent_listen")
    );
    let token = scratch.write("pool-token", &format!("{JOIN}\n"), 0o600);
    let pool = migration_block("Route 1: a pool in the `--config` file", 1);
    assert!(pool.contains(DOC_TOKEN_PATH));
    let pool = pool.replace(DOC_TOKEN_PATH, &token.display().to_string());
    let text = format!("{}\n{pool}", server_with_agents(&scratch));
    let loaded = load(&scratch, &text).unwrap();
    assert!(
        loaded
            .warnings()
            .iter()
            .all(|line| !line.contains("joined")),
        "{:?}",
        loaded.warnings()
    );
}

/// Asserts that route 2 of the document holds: its agent pool file, with
/// `--agent-token-file` over an owner-only scratch file holding a join token and no host
/// list, builds a server configuration. It catches an example in a shape the agent pool
/// loader refuses, such as a `[[pool]]` table, a `kind` or a `join_token_file` key.
#[test]
fn the_agent_pool_flags_route_loads() {
    let scratch = Scratch::new("migration_flags_route");
    let pool = migration_block(
        "Route 2: the flags `--agent-pool-config` and `--agent-token-file`",
        0,
    );
    let pool_file = scratch.write("agent-pool.toml", &pool, 0o644);
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    let (mut args, _) = take_fake_hosts(base_args(&scratch));
    let flags: [(&str, OsString); 2] = [
        ("--agent-token-file", tokens.into_os_string()),
        ("--agent-pool-config", pool_file.into_os_string()),
    ];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    config(args).unwrap();
}
