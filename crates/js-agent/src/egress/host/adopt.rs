//! The wiring an earlier process of the agent left, taken over (see the module documentation
//! of the host side): what a start finds and keeps ([`found`], [`Left`]), a run's claim on its
//! own wiring ([`Egress::adopt`]), and the removal of what no run claimed ([`Egress::settle`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::Ipv4Addr;

use js_types::{JobNetwork, PortMap};
use serde_json::Value;

use super::super::forward::{self, Forward};
use super::super::ruleset::{self, IF_PREFIX, IFACE_SET, NAT_TABLE, OPEN_SET, TABLE};
use super::{
    Attachment, Config, Egress, EgressError, INSIDE_NAME, Mode, PEER_PREFIX, RunError, Sys, call,
    command, forwards, gone, listing, lock, namespace, remove, unwire,
};

/// One block's wiring a start kept: what the kernel held for it then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Kept {
    /// The interface index of the host end (`jsvN`).
    ifindex: u64,
    /// The set the host end is a member of, as the start put it back; none when it was in no
    /// set, in both, or in a set the ceiling no longer has (its packets then meet the
    /// default refusal: a run that keeps such a wiring reaches nothing).
    set: Option<&'static str>,
    /// Its whole forwards, sorted.
    forwards: Vec<Forward>,
}

/// What a start found of an earlier process's wiring and kept, until runs take it over and
/// [`Egress::settle`] removes the rest.
#[derive(Debug, Default)]
pub(super) struct Left {
    /// The host ends still there, by block.
    kept: BTreeMap<u32, Kept>,
    /// The blocks whose job end (`jspN`) is still on the host with no host end beside it.
    strays: BTreeSet<u32>,
}

impl Left {
    /// The `nft -f` lines that follow the ruleset in the start's one transaction: each kept
    /// member back in its set, then each kept forward.
    pub(super) fn text(&self) -> String {
        let mut out = String::new();
        for (index, kept) in &self.kept {
            if let Some(set) = kept.set {
                let _ = writeln!(
                    out,
                    "add element inet {TABLE} {set} {{ \"{IF_PREFIX}{index}\" }}"
                );
            }
        }
        out.push_str(&forward::transaction(&self.forwards()));
        out
    }

    /// The blocks the start keeps out of use: every one found.
    pub(super) fn blocks(&self) -> BTreeSet<u32> {
        self.kept.keys().chain(&self.strays).copied().collect()
    }

    /// Every kept forward, sorted.
    fn forwards(&self) -> Vec<Forward> {
        let mut all: Vec<Forward> = self
            .kept
            .values()
            .flat_map(|kept| kept.forwards.iter().cloned())
            .collect();
        all.sort();
        all
    }

    /// Reads both tables back after the start's load: the baseline [`Egress::check`]
    /// compares with (the tables without the forwards' rules), once the forwards read back as
    /// exactly the kept ones, each whole and ahead of the other rules of `forward`.
    ///
    /// # Errors
    ///
    /// [`EgressError::Rules`] when they do not, or the tables cannot be read.
    pub(super) fn read_back(&self, sys: &dyn Sys, config: &Config) -> Result<Value, EgressError> {
        let now = listing(sys, config)?;
        let wanted = self.forwards();
        match forward::split(&now) {
            Some((rest, count)) if count == 2 * wanted.len() && forward::all(&now) == wanted => {
                Ok(rest)
            }
            _ => Err(EgressError::Rules),
        }
    }
}

/// Reads what an earlier process left on the host: the interfaces `jsv<digits>` (with their
/// interface index) and `jsp<digits>`, the members of the sets and the whole forwards of the
/// tables, if both tables are there. A member or a forward of an interface that is gone is
/// not kept (its run's container ended and took the pair with it).
///
/// # Errors
///
/// A program could not run or printed what it does not print.
pub(super) fn found(sys: &dyn Sys, config: &Config) -> Result<Left, EgressError> {
    let what = "listing the interfaces";
    let text = call(sys, what, config.ip(), &["-j", "link", "show"], None)?;
    let links: Vec<Value> = serde_json::from_str(&text).map_err(|why| command(what, why))?;
    let (mut hosts, mut strays) = (BTreeMap::new(), BTreeSet::new());
    for link in &links {
        let name = link.get("ifname").and_then(Value::as_str).unwrap_or("");
        if let Some(index) = numbered(name, IF_PREFIX) {
            let ifindex = link.get("ifindex").and_then(Value::as_u64);
            hosts.insert(
                index,
                ifindex.ok_or_else(|| command(what, format!("{name} has no ifindex")))?,
            );
        } else if let Some(index) = numbered(name, PEER_PREFIX) {
            strays.insert(index);
        }
    }
    // A job end beside its host end goes with it.
    strays.retain(|index| !hosts.contains_key(index));
    let (members, forwards) = match tables(sys, config)? {
        Some((inet, both)) => (members(&inet, config.mode()), forward::all(&both)),
        None => (BTreeMap::new(), Vec::new()),
    };
    let kept = hosts
        .into_iter()
        .map(|(index, ifindex)| {
            let name = format!("{IF_PREFIX}{index}");
            let forwards = forwards.iter().filter(|f| f.iface == name).cloned();
            let kept = Kept {
                ifindex,
                set: members.get(&name).copied().flatten(),
                forwards: forwards.collect(),
            };
            (index, kept)
        })
        .collect();
    Ok(Left { kept, strays })
}

/// Both tables as `nft -j list table` prints them: the filter table as printed, and both
/// normalized (`[inet, nat]`, as [`listing`] reads them); none when either is not there.
fn tables(sys: &dyn Sys, config: &Config) -> Result<Option<(Value, Value)>, EgressError> {
    let what = "reading the firewall an earlier process left";
    let mut texts = Vec::new();
    for (family, table) in [("inet", TABLE), ("ip", NAT_TABLE)] {
        let args = ["-j", "list", "table", family, table];
        match sys.run(config.nft(), &args, None) {
            Err(RunError::Failed(said)) if gone(&said) => return Ok(None),
            other => texts.push(
                other.map_err(|why| command(what, format!("{} {args:?}: {why}", config.nft())))?,
            ),
        }
    }
    let inet: Value = serde_json::from_str(&texts[0]).map_err(|why| command(what, why))?;
    let mut both = Vec::new();
    for text in &texts {
        both.push(ruleset::normalize(text).map_err(|why| command(what, why))?);
    }
    Ok(Some((inet, Value::Array(both))))
}

/// The set each member of the sets of `inet` (the filter table as printed) is in, when the
/// ceiling `mode` has that set; a member of both sets, or of a set the ceiling does not have,
/// is in none.
fn members(inet: &Value, mode: Mode) -> BTreeMap<String, Option<&'static str>> {
    let sets: &[&'static str] = match mode {
        Mode::Open => &[IFACE_SET, OPEN_SET],
        _ => &[IFACE_SET],
    };
    let items = inet
        .get("nftables")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut found: BTreeMap<String, Option<&'static str>> = BTreeMap::new();
    for set in items.iter().filter_map(|item| item.get("set")) {
        let name = set.get("name").and_then(Value::as_str).unwrap_or("");
        let known = [IFACE_SET, OPEN_SET]
            .into_iter()
            .find(|known| *known == name);
        let Some(known) = known else { continue };
        let elems = set.get("elem").and_then(Value::as_array);
        for member in elems.into_iter().flatten().filter_map(Value::as_str) {
            let kept = sets.contains(&known).then_some(known);
            found
                .entry(member.to_owned())
                .and_modify(|seen| *seen = None)
                .or_insert(kept);
        }
    }
    found
}

/// The block of `name` when it is `prefix` and then digits, nothing else (`jsv12`, not
/// `jsvx` or `jspare0`).
fn numbered(name: &str, prefix: &str) -> Option<u32> {
    let digits = name.strip_prefix(prefix)?;
    digits
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| digits.parse().ok())
        .flatten()
}

/// The interface index of the host end of the pair whose job end is the `eth0` of the
/// network namespace of `pid`, and the job end's address: what `ip -j addr show dev eth0`
/// prints there.
///
/// # Errors
///
/// [`EgressError::Container`] for a container that is gone or shares the host's namespace;
/// [`EgressError::Unadoptable`] for one whose `eth0` is missing or not a pair's end with one
/// /30 address.
fn job_end(sys: &dyn Sys, config: &Config, pid: u32) -> Result<(u64, Ipv4Addr), EgressError> {
    let netns = namespace(sys, pid)?;
    let unfit = |why: String| EgressError::Unadoptable { pid, why };
    let net = format!("--net={}", netns.path);
    let args = [
        net.as_str(),
        "--",
        config.ip(),
        "-j",
        "addr",
        "show",
        "dev",
        INSIDE_NAME,
    ];
    let text = call(sys, "reading the job end", config.nsenter(), &args, None)
        .map_err(|why| unfit(why.to_string()))?;
    let links: Vec<Value> =
        serde_json::from_str(&text).map_err(|why| unfit(format!("reading the job end: {why}")))?;
    let link = links.first();
    let peer = link.and_then(|link| link.get("link_index")?.as_u64());
    let addresses = link.and_then(|link| link.get("addr_info")?.as_array().cloned());
    let inet: Vec<(Option<Ipv4Addr>, Option<u64>)> = addresses
        .unwrap_or_default()
        .iter()
        .filter(|addr| addr.get("family").and_then(Value::as_str) == Some("inet"))
        .map(|addr| {
            let local = addr.get("local").and_then(Value::as_str);
            let len = addr.get("prefixlen").and_then(Value::as_u64);
            (local.and_then(|text| text.parse().ok()), len)
        })
        .collect();
    match (peer, inet.as_slice()) {
        (Some(peer), [(Some(job), Some(30))]) => Ok((peer, *job)),
        _ => Err(unfit(format!(
            "its {INSIDE_NAME} is not the end of a pair with one /30 address"
        ))),
    }
}

impl Egress {
    /// Takes over the wiring an earlier process of the agent made for the container whose
    /// first process is `pid`, for run `run` (used in errors only), whose job asked for `level`
    /// and declared `ports`: the wiring [`Egress::attach`] made for it, which the start kept.
    /// The container's `eth0` must be the job end of a pair whose host end the start kept,
    /// with the address of that end's block; the host end must be in the set of `level` and
    /// carry exactly the forwards of `ports`, each within the ceiling's ports. Nothing is
    /// changed on the host: the wiring is already what the run needs, and the attachment
    /// returned holds it as one `attach` made (it releases it the same way). A `none` job, and
    /// mode none, have nothing to take over.
    ///
    /// # Errors
    ///
    /// [`EgressError::AboveCeiling`] for a level above the ceiling;
    /// [`EgressError::Container`] for a container that is gone or shares the host's
    /// namespace; [`EgressError::PortTaken`] for a host port another run forwards; and
    /// [`EgressError::Unadoptable`] for a wiring that is not the one the run needs. Nothing is
    /// taken: what the start kept stays for [`Egress::settle`].
    pub fn adopt(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Attachment, EgressError> {
        let inner = &self.inner;
        let ceiling = inner.config.mode().level();
        if level > ceiling {
            return Err(EgressError::AboveCeiling {
                asked: level,
                ceiling,
            });
        }
        let set = match level {
            JobNetwork::None => return Ok(Attachment::none(inner)),
            JobNetwork::AllowList => IFACE_SET,
            JobNetwork::Open => OPEN_SET,
        };
        let unfit = |why: String| EgressError::Unadoptable {
            pid,
            why: format!("run {run}: {why}"),
        };
        let outside = ports.iter().find(|port| {
            let ranges = inner.config.forward_ports();
            !ranges.iter().any(|range| range.contains(port.host))
        });
        if let Some(port) = outside {
            return Err(unfit(format!(
                "host port {} is not among the ports this agent forwards",
                port.host
            )));
        }
        let (peer, job) = job_end(&*inner.sys, &inner.config, pid)?;
        let index = inner.config.pool().index_of(job).ok_or_else(|| {
            unfit(format!(
                "its address {job} is not the job end of a block of the pool"
            ))
        })?;
        let name = format!("{IF_PREFIX}{index}");
        let wanted = forwards::wanted(&inner.config, index, ports)?;
        let mut kept = lock(&inner.kept);
        let found = kept
            .kept
            .get(&index)
            .ok_or_else(|| unfit(format!("no {name} was left on the host")))?;
        let wrong = [
            (found.ifindex != peer, "its eth0 is not the job end of"),
            (found.set != Some(set), "the set of its level does not hold"),
            (
                found.forwards != wanted,
                "its declared ports are not the forwards of",
            ),
        ];
        if let Some((_, what)) = wrong.into_iter().find(|(wrong, _)| *wrong) {
            return Err(unfit(format!("{what} {name}")));
        }
        let mut held = lock(&inner.forwards);
        let hosts: Vec<u16> = wanted.iter().map(|forward| forward.host).collect();
        if let Some(port) = hosts.iter().copied().find(|host| held.contains_key(host)) {
            return Err(EgressError::PortTaken { port });
        }
        kept.kept.remove(&index);
        held.extend(wanted.into_iter().map(|forward| (forward.host, forward)));
        Ok(Attachment {
            inner: inner.clone(),
            wired: Some((index, name, set)),
            forwards: hosts,
        })
    }

    /// Removes what the start kept and no run took over ([`Egress::adopt`]): for each such
    /// block its forwards, its set member and its host end (as a release does), and each job
    /// end left alone on the host. A block is given out again once all of its wiring is gone.
    /// The runtime calls it once, after its runs are adopted; [`Egress::check`] holds only
    /// after it, since a forward no run took over is one of no run. A second call, and one in
    /// mode none, finds nothing to do.
    ///
    /// # Errors
    ///
    /// Something could not be removed (the first error; every removal is tried). Its block
    /// stays out of use.
    pub fn settle(&self) -> Result<(), EgressError> {
        let inner = &self.inner;
        let left = std::mem::take(&mut *lock(&inner.kept));
        let mut result = Ok(());
        for (index, kept) in left.kept {
            let hosts: Vec<u16> = kept.forwards.iter().map(|forward| forward.host).collect();
            let name = format!("{IF_PREFIX}{index}");
            result = result.and(unwire(inner, (index, &name, kept.set), &hosts));
        }
        for index in left.strays {
            let name = format!("{PEER_PREFIX}{index}");
            let (ip, what) = (inner.config.ip(), "deleting a leftover veth");
            let removed = remove(&*inner.sys, what, ip, &["link", "del", &name], None);
            if removed.is_ok() {
                lock(&inner.used).remove(&index);
            }
            result = result.and(removed);
        }
        result
    }
}

#[cfg(test)]
#[path = "adopt_tests.rs"]
mod tests;
