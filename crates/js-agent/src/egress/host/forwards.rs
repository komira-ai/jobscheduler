//! The forwards of a wired run's declared ports on the host: step 7 of the attach (the rules
//! loaded in one transaction, read back and recorded) and their removal at release. The text
//! of the rules and their reading back are [`super::super::forward`]'s.

use js_types::PortMap;
use serde_json::Value;

use super::super::forward::{self, Forward};
use super::{
    Config, EgressError, IF_PREFIX, Inner, NAT_TABLE, RunError, Sys, TABLE, call, command, gone,
    listing, lock,
};

/// Step 7 of the module documentation: the forwards of `ports` to the job of block `index`,
/// whose wiring is up; returns their host ports. Nothing is left when it fails.
pub(super) fn add(inner: &Inner, index: u32, ports: &[PortMap]) -> Result<Vec<u16>, EgressError> {
    if ports.is_empty() {
        return Ok(Vec::new());
    }
    let (sys, config) = (&*inner.sys, &inner.config);
    let (_, job) = config.pool().block(index).ok_or(EgressError::Exhausted)?;
    let iface = format!("{IF_PREFIX}{index}");
    let mut wanted: Vec<Forward> = ports
        .iter()
        .map(|port| Forward {
            host: port.host,
            job,
            container: port.container,
            iface: iface.clone(),
        })
        .collect();
    wanted.sort();
    let hosts: Vec<u16> = wanted.iter().map(|forward| forward.host).collect();
    // Held from the look at the ports in use to their record (or the removal of what was
    // loaded): no other run takes one of them meanwhile.
    let mut held = lock(&inner.forwards);
    // A port twice in the list, or one another run forwards. (The wire refuses a host port
    // declared twice; this does not depend on it.)
    let twice = hosts
        .windows(2)
        .find(|pair| pair[0] == pair[1])
        .map(|pair| pair[0]);
    let taken = twice.or_else(|| hosts.iter().copied().find(|host| held.contains_key(host)));
    if let Some(port) = taken {
        return Err(EgressError::PortTaken { port });
    }
    call(
        sys,
        "loading the forwards",
        config.nft(),
        &["-f", "-"],
        Some(&forward::transaction(&wanted)),
    )?;
    let read = listing(sys, config).and_then(|now| {
        let in_order = forward::split(&now).is_some();
        match in_order && forward::decode(&now, &iface) == wanted {
            true => Ok(()),
            false => Err(EgressError::Rules),
        }
    });
    if let Err(why) = read {
        // The load's own error is the one reported; a rule that could not be removed is a
        // forward of no run, which the check refuses.
        let _ = delete_forwards(sys, config, &hosts);
        return Err(why);
    }
    held.extend(wanted.into_iter().map(|forward| (forward.host, forward)));
    Ok(hosts)
}

/// Removes the forwards of the host ports `hosts` (a released run's): their rules, and their
/// record, which goes whatever became of the rules.
pub(super) fn remove(inner: &Inner, hosts: &[u16]) -> Result<(), EgressError> {
    if hosts.is_empty() {
        return Ok(());
    }
    let mut held = lock(&inner.forwards);
    for host in hosts {
        held.remove(host);
    }
    delete_forwards(&*inner.sys, &inner.config, hosts)
}

/// Deletes the rules of the forwards of the host ports `hosts` from both tables, by handle: a
/// listing of each table finds them by their comment, and one `nft -f` transaction deletes
/// them all. A rule or a table that is gone already is not an error.
fn delete_forwards(sys: &dyn Sys, config: &Config, hosts: &[u16]) -> Result<(), EgressError> {
    let comments: Vec<String> = hosts.iter().map(|host| forward::comment(*host)).collect();
    let mut script = String::new();
    for (family, table) in [("inet", TABLE), ("ip", NAT_TABLE)] {
        let args = ["-j", "list", "table", family, table];
        let text = match sys.run(config.nft(), &args, None) {
            // The table is gone, and the rules with it.
            Err(RunError::Failed(said)) if gone(&said) => continue,
            other => other.map_err(|why| {
                command(
                    "listing the forwards",
                    format!("{} {args:?}: {why}", config.nft()),
                )
            })?,
        };
        let value: Value =
            serde_json::from_str(&text).map_err(|why| command("listing the forwards", why))?;
        for (chain, handle) in handles(&value, &comments) {
            script.push_str(&format!(
                "delete rule {family} {table} {chain} handle {handle}\n"
            ));
        }
    }
    match script.is_empty() {
        true => Ok(()),
        false => super::remove(
            sys,
            "removing the forwards",
            config.nft(),
            &["-f", "-"],
            Some(&script),
        ),
    }
}

/// The chain and the handle of each rule of `listing` (what `nft -j list table` printed)
/// whose comment is one of `comments`.
fn handles<'a>(
    listing: &'a Value,
    comments: &'a [String],
) -> impl Iterator<Item = (&'a str, u64)> + 'a {
    let items = listing
        .get("nftables")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| item.get("rule"))
        .filter(|rule| {
            let comment = rule.get("comment").and_then(Value::as_str);
            comments
                .iter()
                .any(|wanted| Some(wanted.as_str()) == comment)
        })
        .filter_map(|rule| Some((rule.get("chain")?.as_str()?, rule.get("handle")?.as_u64()?)))
}
