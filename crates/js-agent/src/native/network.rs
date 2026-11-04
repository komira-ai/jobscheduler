//! The network of a native job on a Mac (design 5.8, and step 5 of 5.9): the job's level, the
//! agent's ceiling (`--native-network`) and the comparison of the two. Pure: no I/O and no
//! clock, so it is tested on Linux like the rest of the runtime's rules.
//!
//! A Mac has no network namespace, so a job level maps onto what the sandbox and the packet
//! filter can do ([`wanted`]):
//!
//! | Job level | `--native-network` value | Needs |
//! |---|---|---|
//! | `none` | `off` | nothing: the sandbox denies the network |
//! | `allow-list` | `on` | the pf anchor of the pool, checked before each job |
//! | `open` | `open` | nothing: the sandbox allows the network, no filter rule |
//!
//! `loopback` is a ceiling with no job level of its own. A job above the ceiling is refused
//! ([`admit`]), never run with less. A native body says its level as a [`NetworkLevel`]
//! (`off`, `on`, `open`; and `loopback`, which [`of_body`] keeps as a level of its own).
//!
//! The bundle's `seatbelt.network_max` is not a ceiling: it says what the bundle's tools
//! need, and it lowers the level the profile gets after the job was admitted
//! (`native/bundle.rs`).

use std::fmt;

use js_agent_wire::{NetworkCeiling, NetworkLevel, refusal};
use js_mac_pool::seatbelt::Net;
use js_types::{JobNetwork, PortMap};

use crate::egress::NetworkRefusal;

/// What a native job's network is on a Mac: the job level mapped onto the sandbox and the
/// packet filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeNet {
    /// The sandbox denies the network (`none`).
    Off,
    /// The sandbox allows loopback only: what a native body that asks `loopback` gets. No job
    /// level maps to it.
    Loopback,
    /// The sandbox allows the network and the pool's pf anchor limits it (`allow-list`, the
    /// `--native-network on` value).
    OnWithFilter,
    /// The sandbox allows the network and nothing filters it (`open`).
    Open,
}

impl NativeNet {
    /// The level as a job or a native body names it, for the text of a refusal.
    fn name(self) -> &'static str {
        match self {
            Self::Off => JobNetwork::None.name(),
            Self::Loopback => "loopback",
            Self::OnWithFilter => JobNetwork::AllowList.name(),
            Self::Open => JobNetwork::Open.name(),
        }
    }

    /// The least ceiling that admits the level.
    fn least_ceiling(self) -> Ceiling {
        match self {
            Self::Off => Ceiling::Off,
            Self::Loopback => Ceiling::Loopback,
            Self::OnWithFilter => Ceiling::On,
            Self::Open => Ceiling::Open,
        }
    }
}

/// The most network the native jobs of an agent may ask for: its `--native-network`. The
/// values are ordered `Off < Loopback < On < Open`; a job is admitted when its level's least
/// ceiling is not above this one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ceiling {
    /// `off`, the default: no job may use the network.
    #[default]
    Off,
    /// `loopback`: a job may use loopback, which every run of the Mac shares.
    Loopback,
    /// `on`: a job may use the network as the pool's pf anchor allows it; the agent needs the
    /// anchor in force (the `--native-pf-*` flags).
    On,
    /// `open`: a job may use the network with no filter; the scheduler, the cloud metadata
    /// address and the rest of the network are reachable from the job. No anchor is needed.
    Open,
}

impl Ceiling {
    /// Every value, lowest first.
    pub const ALL: [Self; 4] = [Self::Off, Self::Loopback, Self::On, Self::Open];

    /// The value as `--native-network` spells it: `off`, `loopback`, `on` or `open`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Loopback => "loopback",
            Self::On => "on",
            Self::Open => "open",
        }
    }

    /// The value `text` spells, or `None` for any other text.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ceiling| ceiling.name() == text)
    }

    /// The most network the sandbox profile of any run may grant: `on` and `open` both
    /// allow the network (the profile has no network rule); what tells them apart is the
    /// packet filter, which only `on` requires.
    #[must_use]
    pub fn net(self) -> Net {
        match self {
            Self::Off => Net::Off,
            Self::Loopback => Net::Loopback,
            Self::On | Self::Open => Net::On,
        }
    }

    /// The ceiling as a hello reports it (the capability `egress`): `off` and `loopback` are
    /// `none`, `on` is `allow-list` and `open` is `open`, with no port, since a Mac forwards
    /// none.
    #[must_use]
    pub fn advertised(self) -> NetworkCeiling {
        let level = match self {
            Self::Off | Self::Loopback => JobNetwork::None,
            Self::On => JobNetwork::AllowList,
            Self::Open => JobNetwork::Open,
        };
        NetworkCeiling {
            level,
            ports: Vec::new(),
        }
    }
}

impl fmt::Display for Ceiling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a job at `level` gets on a Mac: `none` is [`NativeNet::Off`], `allow-list` is
/// [`NativeNet::OnWithFilter`] and `open` is [`NativeNet::Open`].
pub(crate) fn wanted(level: JobNetwork) -> NativeNet {
    match level {
        JobNetwork::None => NativeNet::Off,
        JobNetwork::AllowList => NativeNet::OnWithFilter,
        JobNetwork::Open => NativeNet::Open,
    }
}

/// The job level a native body's `network` names: `off` is `none`, `on` is `allow-list` and
/// `open` is `open`; `loopback` names none.
fn job_level(level: NetworkLevel) -> Option<JobNetwork> {
    match level {
        NetworkLevel::Off => Some(JobNetwork::None),
        NetworkLevel::Loopback => None,
        NetworkLevel::On => Some(JobNetwork::AllowList),
        NetworkLevel::Open => Some(JobNetwork::Open),
    }
}

/// What a native body that asks `level` gets: the job level it names, mapped by [`wanted`];
/// `loopback`, which names no job level, is [`NativeNet::Loopback`].
pub(crate) fn of_body(level: NetworkLevel) -> NativeNet {
    job_level(level).map_or(NativeNet::Loopback, wanted)
}

/// `wanted`, when `ceiling` allows it; it is never lowered.
///
/// # Errors
///
/// [`refusal::NETWORK_ABOVE_CEILING`] for a level above the ceiling, with a text that names
/// both (`network allow-list asked, the agent allows at most off`).
pub(crate) fn admit(ceiling: Ceiling, wanted: NativeNet) -> Result<NativeNet, NetworkRefusal> {
    match wanted.least_ceiling() <= ceiling {
        true => Ok(wanted),
        false => Err(NetworkRefusal {
            code: refusal::NETWORK_ABOVE_CEILING,
            detail: format!(
                "network {} asked, the agent allows at most {ceiling}",
                wanted.name()
            ),
        }),
    }
}

/// Whether a job at `wanted` needs the pool's pf anchor in force before it starts: only
/// [`NativeNet::OnWithFilter`] does.
pub(crate) fn needs_anchor(wanted: NativeNet) -> bool {
    wanted == NativeNet::OnWithFilter
}

/// What a Mac does with declared ports: it forwards none yet, so a start that declares any is
/// refused, and one that declares none passes.
///
/// # Errors
///
/// [`refusal::PORTS_NOT_SUPPORTED`] for a non-empty `ports`.
pub(crate) fn check_ports(ports: &[PortMap]) -> Result<(), NetworkRefusal> {
    match ports.is_empty() {
        true => Ok(()),
        false => Err(NetworkRefusal {
            code: refusal::PORTS_NOT_SUPPORTED,
            detail: "declared ports are not supported on macOS yet".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOB_LEVELS: [JobNetwork; 3] = [JobNetwork::None, JobNetwork::AllowList, JobNetwork::Open];

    /// Asserts that a job that asks for no network gets the sandbox with the network denied,
    /// which needs no anchor. Catches `none` mapped to loopback or to a level that opens the
    /// network.
    #[test]
    fn none_maps_to_off() {
        assert_eq!(wanted(JobNetwork::None), NativeNet::Off);
        assert!(!needs_anchor(NativeNet::Off));
    }

    /// Asserts that an allow-listed job is the `on` value of `--native-network` and needs the
    /// anchor. Catches allow-list mapped to `Open`, which would run an allow-listed job with
    /// no filter.
    #[test]
    fn allow_list_maps_to_on_with_filter() {
        assert_eq!(wanted(JobNetwork::AllowList), NativeNet::OnWithFilter);
        assert!(needs_anchor(wanted(JobNetwork::AllowList)));
    }

    /// Asserts that an open job is `Open` and needs no anchor, and that neither does a job
    /// with no network or loopback only. Catches `open` demanding the anchor (an operator who
    /// chose `open` could then not run on a Mac without pf).
    #[test]
    fn open_needs_no_anchor() {
        assert_eq!(wanted(JobNetwork::Open), NativeNet::Open);
        assert!(!needs_anchor(NativeNet::Open));
        assert!(!needs_anchor(NativeNet::Off));
        assert!(!needs_anchor(NativeNet::Loopback));
    }

    /// Asserts, for every ceiling and every job level, that a level at or under the ceiling
    /// is admitted unchanged and a level above it is refused `network-above-ceiling` with a
    /// text that names the level and the ceiling, never an `Ok` of a lower level. Catches the
    /// lowering this replaced, a level let through above the ceiling, and a refusal that does
    /// not say which ceiling refused it.
    #[test]
    fn above_ceiling_is_refused_not_lowered() {
        let order = |level: JobNetwork| match level {
            JobNetwork::None => Ceiling::Off,
            JobNetwork::AllowList => Ceiling::On,
            JobNetwork::Open => Ceiling::Open,
        };
        let mut refused = 0;
        for ceiling in Ceiling::ALL {
            for level in JOB_LEVELS {
                let asked = wanted(level);
                let admitted = admit(ceiling, asked);
                if order(level) <= ceiling {
                    assert_eq!(admitted, Ok(asked), "{level} under {ceiling}");
                    continue;
                }
                refused += 1;
                let refusal = admitted.expect_err("above the ceiling");
                assert_eq!(refusal.code, refusal::NETWORK_ABOVE_CEILING);
                assert_eq!(
                    refusal.detail,
                    format!("network {level} asked, the agent allows at most {ceiling}")
                );
            }
        }
        assert_eq!(refused, 5, "4 x 3 pairs, 5 of them above the ceiling");
    }

    /// Asserts that a `loopback` ceiling admits a job with no network and refuses
    /// allow-list and open, and that no job level maps to loopback. Catches a loopback
    /// ceiling read as `on` (an unfiltered network job on a Mac whose operator allowed
    /// loopback only).
    #[test]
    fn loopback_ceiling_admits_only_none() {
        assert_eq!(
            admit(Ceiling::Loopback, wanted(JobNetwork::None)),
            Ok(NativeNet::Off)
        );
        for level in [JobNetwork::AllowList, JobNetwork::Open] {
            let refusal = admit(Ceiling::Loopback, wanted(level)).expect_err("above loopback");
            assert_eq!(refusal.code, refusal::NETWORK_ABOVE_CEILING);
            assert!(
                refusal.detail.ends_with("at most loopback"),
                "{}",
                refusal.detail
            );
        }
        for level in JOB_LEVELS {
            assert_ne!(wanted(level), NativeNet::Loopback, "{level}");
        }
    }

    /// Asserts how a native body's level reads: `off`, `on` and `open` are the job levels
    /// `none`, `allow-list` and `open`, and `loopback` is a level of its own that a `loopback`
    /// ceiling admits and an `off` one refuses, naming `loopback`. Catches a body level mapped
    /// past the job level it names (`on` read as `open` would drop the filter) and a loopback
    /// body run with the network denied instead of refused.
    #[test]
    fn a_native_body_names_its_job_level() {
        assert_eq!(of_body(NetworkLevel::Off), NativeNet::Off);
        assert_eq!(of_body(NetworkLevel::On), NativeNet::OnWithFilter);
        assert_eq!(of_body(NetworkLevel::Open), NativeNet::Open);
        assert_eq!(of_body(NetworkLevel::Loopback), NativeNet::Loopback);
        for ceiling in [Ceiling::Loopback, Ceiling::On, Ceiling::Open] {
            assert_eq!(
                admit(ceiling, NativeNet::Loopback),
                Ok(NativeNet::Loopback),
                "{ceiling}"
            );
        }
        let refusal = admit(Ceiling::Off, NativeNet::Loopback).expect_err("above off");
        assert_eq!(
            refusal.detail,
            "network loopback asked, the agent allows at most off"
        );
    }

    /// Asserts that every ceiling reads back from the word `--native-network` spells it with,
    /// that any other word is none of them, that the profile's network is denied for `off`,
    /// loopback for `loopback` and open for both `on` and `open`, and that a hello reports `off`
    /// and `loopback` as `none`, `on` as `allow-list` and `open` as `open`, never with a port.
    /// Catches `open` mapped to a profile that denies the network, a ceiling advertised above
    /// what the agent runs, and a Mac that claims ports it does not forward.
    #[test]
    fn each_ceiling_reads_maps_and_is_advertised() {
        let table = [
            (Ceiling::Off, "off", Net::Off, JobNetwork::None),
            (
                Ceiling::Loopback,
                "loopback",
                Net::Loopback,
                JobNetwork::None,
            ),
            (Ceiling::On, "on", Net::On, JobNetwork::AllowList),
            (Ceiling::Open, "open", Net::On, JobNetwork::Open),
        ];
        for (ceiling, word, net, level) in table {
            assert_eq!(Ceiling::parse(word), Some(ceiling));
            assert_eq!(ceiling.to_string(), word);
            assert_eq!(ceiling.net(), net, "{word}");
            let advertised = ceiling.advertised();
            assert_eq!(advertised.level, level, "{word}");
            assert!(advertised.ports.is_empty(), "{word}");
        }
        for other in ["", "On", "allow-list", "none", "opened"] {
            assert_eq!(Ceiling::parse(other), None, "{other:?}");
        }
        assert_eq!(Ceiling::default(), Ceiling::Off);
    }

    /// Asserts that declared ports are refused `ports-not-supported` with the exact text, and
    /// that no port passes. Catches a port that is ignored on a Mac, and a refusal on the
    /// field's presence alone.
    #[test]
    fn declared_ports_are_refused_with_the_exact_text() {
        assert_eq!(check_ports(&[]), Ok(()));
        let refused = check_ports(&[PortMap {
            host: 8080,
            container: 80,
        }]);
        assert_eq!(
            refused,
            Err(NetworkRefusal {
                code: refusal::PORTS_NOT_SUPPORTED,
                detail: "declared ports are not supported on macOS yet".to_owned(),
            })
        );
    }
}
