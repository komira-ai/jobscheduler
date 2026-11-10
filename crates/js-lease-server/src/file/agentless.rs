//! The refusal of the removed agentless hosts-file pool: a `[[pool]]` with no top-level
//! `kind` whose provider is the old kind `static` (`provider = { kind = "static",
//! hosts_file = ... }`). It is refused at load with the migration, before the kind is
//! looked up, so the refusal says what to do instead of naming an unknown kind. The
//! machine provider kind `static` of a `[[machine_provider]]` (the hosts that register) is
//! another table and is not touched.

pub(super) use crate::flags::AGENTLESS_REMOVED;

/// The keys by which `table` is a pool of the removed agentless hosts-file pool, the old
/// provider kind `static`: `provider.kind = "static"`, and `provider.hosts_file` when it
/// is given. `None` for every other pool. The values of the keys are not repeated, so no
/// path reaches a message.
pub(super) fn agentless_keys(table: &toml::Table) -> Option<&'static str> {
    let Some(toml::Value::Table(provider)) = table.get("provider") else {
        return None;
    };
    let Some(toml::Value::String(kind)) = provider.get("kind") else {
        return None;
    };
    match (kind.as_str(), provider.contains_key("hosts_file")) {
        ("static", true) => Some("provider.kind = \"static\", provider.hosts_file"),
        ("static", false) => Some("provider.kind = \"static\""),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    /// Asserts which tables are the old agentless pool and which keys name it: the old
    /// provider kind with and without `hosts_file`, and nothing for a pool with no
    /// provider, a provider that is not a table, a provider with no kind or a kind that is
    /// not a string, or another kind. It catches a check that lets the old kind through to
    /// the kind table, or one that refuses an elastic pool.
    #[test]
    fn only_the_old_static_provider_kind_is_the_agentless_pool() {
        let old = table("provider = { kind = \"static\", hosts_file = \"/x/hosts.toml\" }");
        assert_eq!(
            agentless_keys(&old),
            Some("provider.kind = \"static\", provider.hosts_file")
        );
        let bare = table("provider = { kind = \"static\" }");
        assert_eq!(agentless_keys(&bare), Some("provider.kind = \"static\""));
        for other in [
            "name = \"p\"",
            "provider = \"static\"",
            "provider = { hosts_file = \"/x\" }",
            "provider = { kind = 3 }",
            "provider = { kind = \"lambdalabs\" }",
        ] {
            assert_eq!(agentless_keys(&table(other)), None, "{other}");
        }
    }
}
