//! The wiring an earlier process of the agent left, taken over (see the module documentation
//! of the host side): what a start keeps, from the records of the egress store ([`found`],
//! [`Left`]); a run's claim on the wiring its record names ([`Egress::adopt`]); and the removal
//! of what no run claimed ([`Egress::settle`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use js_types::{JobNetwork, PortMap};
use serde_json::Value;

use super::super::forward::{self, Forward};
use super::super::ruleset::{IF_PREFIX, IFACE_SET, OPEN_SET, TABLE};
use super::super::store::{Record, Store};
use super::{
    Attachment, Config, Egress, EgressError, PEER_PREFIX, Sys, call, command, forwards, listing,
    lock, remove, unwire,
};

/// One run's wiring a start kept, as its record names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Kept {
    /// The run's block of the pool.
    index: u32,
    /// The level its job asked for.
    level: JobNetwork,
    /// The set its interface is in: the level's.
    set: &'static str,
    /// The ports it declared, sorted by host port.
    ports: Vec<PortMap>,
    /// Their forwards, sorted.
    forwards: Vec<Forward>,
}

/// What a start found of an earlier process's wiring and kept, until runs take it over and
/// [`Egress::settle`] removes the rest.
#[derive(Debug, Default)]
pub(super) struct Left {
    /// The wiring each record names, by run.
    kept: BTreeMap<String, Kept>,
    /// The records that cannot be used (unreadable, or naming a block, a level or ports this
    /// agent cannot keep), each with why; their files go with the settle.
    unusable: Vec<(String, String)>,
    /// The blocks whose interfaces (`jsvN`, `jspN`) are on the host and no record names: a
    /// start killed before it recorded them, or a process without a store.
    strays: BTreeSet<u32>,
}

impl Left {
    /// The `nft -f` lines that follow the ruleset in the start's one transaction: each kept
    /// interface back in its set, then each kept forward.
    pub(super) fn text(&self) -> String {
        let mut out = String::new();
        for kept in self.kept.values() {
            let _ = writeln!(
                out,
                "add element inet {TABLE} {} {{ \"{IF_PREFIX}{}\" }}",
                kept.set, kept.index
            );
        }
        out.push_str(&forward::transaction(&self.forwards()));
        out
    }

    /// The blocks the start keeps out of use: every one found.
    pub(super) fn blocks(&self) -> BTreeSet<u32> {
        let kept = self.kept.values().map(|kept| kept.index);
        kept.chain(self.strays.iter().copied()).collect()
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

/// What a start keeps: the wiring each usable record of `store` names, and the blocks of the
/// interfaces `jsv<digits>` and `jsp<digits>` on the host that no record names. Without a
/// store nothing is kept, and every such interface is a stray.
///
/// # Errors
///
/// The store cannot be listed, or `ip` could not run or printed what it does not print.
pub(super) fn found(
    sys: &dyn Sys,
    config: &Config,
    store: Option<&Store>,
) -> Result<Left, EgressError> {
    let mut left = Left::default();
    let records = match store {
        Some(store) => store
            .records()
            .map_err(|why| command("reading the egress store", why))?,
        None => Vec::new(),
    };
    for (run, record) in records {
        let kept = record.and_then(|record| keep(config, &record));
        let taken = left.kept.values().map(|kept| kept.index);
        match kept {
            Ok(kept) if taken.clone().any(|index| index == kept.index) => left
                .unusable
                .push((run, format!("block {} is another run's", kept.index))),
            Ok(kept) => {
                left.kept.insert(run, kept);
            }
            Err(why) => left.unusable.push((run, why)),
        }
    }
    let what = "listing the interfaces";
    let text = call(sys, what, config.ip(), &["-j", "link", "show"], None)?;
    let links: Vec<Value> = serde_json::from_str(&text).map_err(|why| command(what, why))?;
    let kept: BTreeSet<u32> = left.kept.values().map(|kept| kept.index).collect();
    for link in &links {
        let name = link.get("ifname").and_then(Value::as_str).unwrap_or("");
        let index = numbered(name, IF_PREFIX).or_else(|| numbered(name, PEER_PREFIX));
        left.strays
            .extend(index.filter(|index| !kept.contains(index)));
    }
    for (run, why) in &left.unusable {
        tracing::warn!(run, "egress: the record of the run is not used: {why}");
    }
    Ok(left)
}

/// The wiring `record` names, when this agent can keep it: a block of the pool, a level the
/// ceiling allows, and ports within the ceiling's.
fn keep(config: &Config, record: &Record) -> Result<Kept, String> {
    let set = match record.level {
        JobNetwork::AllowList => IFACE_SET,
        JobNetwork::Open if config.mode().level() == JobNetwork::Open => OPEN_SET,
        level => {
            return Err(format!(
                "the ceiling {} does not allow {level}",
                config.mode()
            ));
        }
    };
    let ranges = config.forward_ports();
    let outside = record
        .ports
        .iter()
        .find(|port| !ranges.iter().any(|range| range.contains(port.host)));
    if let Some(port) = outside {
        return Err(format!(
            "host port {} is not among the ports this agent forwards",
            port.host
        ));
    }
    let forwards = forwards::wanted(config, record.index, &record.ports)
        .map_err(|_| format!("block {} is not one of the pool", record.index))?;
    Ok(Kept {
        index: record.index,
        level: record.level,
        set,
        ports: sorted(&record.ports),
        forwards,
    })
}

/// `ports` sorted by host port.
fn sorted(ports: &[PortMap]) -> Vec<PortMap> {
    let mut sorted = ports.to_vec();
    sorted.sort_by_key(|port| (port.host, port.container));
    sorted
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

impl Egress {
    /// Takes over the wiring an earlier process of the agent made for the run `run` (its
    /// container's name), whose job asked for `level` and declared `ports`: the wiring the
    /// run's record in the egress store names, which the start kept in place. The record must
    /// name the same level and ports. Nothing is changed on the host; the attachment returned
    /// holds the wiring as one [`Egress::attach`] made, and releases it (and its record) the
    /// same way. A `none` job, and mode none, have nothing to take over.
    ///
    /// # Errors
    ///
    /// [`EgressError::AboveCeiling`] for a level above the ceiling;
    /// [`EgressError::PortTaken`] for a host port another run forwards; and
    /// [`EgressError::Unadoptable`] when the run has no record the start kept, or one that
    /// names another level or other ports. Nothing is taken: what the start kept stays for
    /// [`Egress::settle`].
    pub fn adopt(
        &self,
        run: &str,
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
        if level.is_none() {
            return Ok(Attachment::none(inner));
        }
        let unfit = |why: String| EgressError::Unadoptable {
            run: run.to_owned(),
            why,
        };
        let mut left = lock(&inner.kept);
        let mut held = lock(&inner.forwards);
        let kept = left
            .kept
            .remove(run)
            .ok_or_else(|| unfit("the egress store holds no record of it".to_owned()))?;
        // A refusal puts the record's wiring back, for the settle.
        if let Err(why) = fits(&kept, (level, ports), &held).map_err(|why| match why {
            Unfit::Record(why) => unfit(why),
            Unfit::Port(port) => EgressError::PortTaken { port },
        }) {
            left.kept.insert(run.to_owned(), kept);
            return Err(why);
        }
        let hosts: Vec<u16> = kept.forwards.iter().map(|forward| forward.host).collect();
        held.extend(
            kept.forwards
                .into_iter()
                .map(|forward| (forward.host, forward)),
        );
        Ok(Attachment {
            inner: inner.clone(),
            wired: Some((kept.index, format!("{IF_PREFIX}{}", kept.index), kept.set)),
            forwards: hosts,
            record: Some(run.to_owned()),
        })
    }

    /// Removes what the start kept and no run took over ([`Egress::adopt`]): for each such
    /// record its forwards, its set member and its interface (as a release does) and then the
    /// record; each record that cannot be used; and each interface no record named. A block is
    /// given out again once all of its wiring is gone. The runtime calls it once, after its
    /// runs are adopted; [`Egress::check`] holds only after it, since a forward no run took over
    /// is one of no run. A second call, and one in mode none, finds nothing to do.
    ///
    /// # Errors
    ///
    /// Something could not be removed (the first error; every removal is tried). Its block
    /// stays out of use, and its record stays for the next start.
    pub fn settle(&self) -> Result<(), EgressError> {
        let inner = &self.inner;
        let left = std::mem::take(&mut *lock(&inner.kept));
        let mut result = Ok(());
        for (run, kept) in left.kept {
            let hosts: Vec<u16> = kept.forwards.iter().map(|forward| forward.host).collect();
            let name = format!("{IF_PREFIX}{}", kept.index);
            let unwired = unwire(
                inner,
                (kept.index, &name, Some(kept.set)),
                (&hosts, Some(&run)),
            );
            result = result.and(unwired);
        }
        for (run, _) in left.unusable {
            result = result.and(forget(inner.store.as_ref(), &run));
        }
        let (ip, what) = (inner.config.ip(), "deleting a leftover veth");
        for index in left.strays {
            // Deleting one end of a pair deletes the other: the second is then not there.
            let mut removed = Ok(());
            for name in [
                format!("{IF_PREFIX}{index}"),
                format!("{PEER_PREFIX}{index}"),
            ] {
                removed = removed.and(remove(&*inner.sys, what, ip, &["link", "del", &name], None));
            }
            if removed.is_ok() {
                lock(&inner.used).remove(&index);
            }
            result = result.and(removed);
        }
        result
    }
}

/// Why a kept wiring is not a run's.
enum Unfit {
    /// The record names another level or other ports than the run's job.
    Record(String),
    /// A host port of its forwards is another run's.
    Port(u16),
}

/// Whether `kept` is the wiring of a run whose job asked for `level` and declared `ports`,
/// with none of its host ports in `held` (the forwards of other runs).
fn fits(
    kept: &Kept,
    (level, ports): (JobNetwork, &[PortMap]),
    held: &BTreeMap<u16, Forward>,
) -> Result<(), Unfit> {
    if kept.level != level {
        return Err(Unfit::Record(format!(
            "its record names the level {}, its job {level}",
            kept.level
        )));
    }
    if kept.ports != sorted(ports) {
        return Err(Unfit::Record(
            "its record names other ports than its job declared".to_owned(),
        ));
    }
    let mut hosts = kept.forwards.iter().map(|forward| forward.host);
    match hosts.find(|host| held.contains_key(host)) {
        Some(port) => Err(Unfit::Port(port)),
        None => Ok(()),
    }
}

/// Removes the record of `run` from `store`, when there is one.
///
/// # Errors
///
/// The record could not be removed.
pub(super) fn forget(store: Option<&Store>, run: &str) -> Result<(), EgressError> {
    store.map_or(Ok(()), |store| {
        store
            .remove(run)
            .map_err(|why| command(format!("removing the egress record of {run}"), why))
    })
}

#[cfg(test)]
#[path = "adopt_tests.rs"]
mod tests;
