//! `--placement-curves` and `server.placement_curves`: whether the placement engine learns
//! a CPU and a memory profile for each job class from the series its runs recorded, and
//! whether it books by them. `off` learns nothing; `shadow` learns and logs one line per run
//! it checks, and places as `off` does; `on` learns and logs as `shadow` does and books a
//! class that passes its checks by its CPU curve, on the hosts `--curve-hosts` or
//! `server.curve_hosts` names (or every host), for at most `--curve-max-wait-seconds` or
//! `server.curve_max_wait_seconds` of a job's wait. The mode is read by
//! [`js_sched_placer::sizes::Curves::parse`]; this is where its refusal is worded, and where
//! the host list and the wait bound are read and refused, for the flags and the keys alike.

use std::collections::BTreeSet;
use std::time::Duration;

use clap::Args;
use js_sched_placer::curves::{
    CurveHosts, CurveSettings, DEFAULT_CURVE_MAX_WAIT, MAX_CURVE_MAX_WAIT_SECS,
};
use js_sched_placer::sizes::Curves;
use js_types::HostId;

use crate::config::{Cli, Config};
use crate::error::StartupError;

/// The curve mode of the placement engine: its `mode` (`--placement-curves`, or
/// `server.placement_curves`) and the `settings` that book under `on` and that `shadow` judges
/// by (`--curve-hosts` and `--curve-max-wait-seconds`, or `server.curve_hosts` and
/// `server.curve_max_wait_seconds`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CurveSetting {
    /// `off` unless given.
    pub(crate) mode: Curves,
    /// The hosts that book by curve and the wait bound, read in every mode.
    pub(crate) settings: CurveSettings,
}

/// The flags of the live curve mode's canary and wait bound, the flags of `server.curve_hosts`
/// and `server.curve_max_wait_seconds`. Both are read in every mode and act only under
/// `--placement-curves on`; both need `--agent-pool-config`.
#[derive(Debug, Clone, Default, Args)]
pub struct CurveFlags {
    /// The hosts that book by curve under `--placement-curves on`, as host ids separated by
    /// commas (or the flag given again), such as `mac-1,mac-2`: a job placed on any other host
    /// is booked flat. Without the flag, `on` books by curve on every host. An empty name, a
    /// name twice and a name that is not a host id (1 to 63 characters of `a-z`, `0-9`, `.`,
    /// `_` and `-`) are refused at start, naming it. Needs `--agent-pool-config`.
    #[arg(long, value_name = "HOST,...", value_delimiter = ',')]
    pub curve_hosts: Option<Vec<String>>,
    /// How long, in whole seconds from 0 to 300, a queued job of a class booked by curve waits
    /// with no host fitting its curve before it is tested with its flat number, under
    /// `--placement-curves on`; 30 without the flag, and at 0 a curve never delays a job. Needs
    /// `--agent-pool-config`.
    #[arg(long, value_name = "SECONDS")]
    pub curve_max_wait_seconds: Option<u64>,
}

impl Config {
    /// The hosts that book by curve and the wait bound (`--curve-hosts` and
    /// `--curve-max-wait-seconds`, or `server.curve_hosts` and `server.curve_max_wait_seconds`):
    /// every host and 30 s unless the flags or the file name others. They book only when
    /// [`Config::placement_curves`] is `on`; under `shadow` the hosts are the ones its log line
    /// judges by curve.
    #[must_use]
    pub fn curve_settings(&self) -> CurveSettings {
        self.agents.curves.settings.clone()
    }
}

/// `text` as the mode it names, or the refusal naming the three values that are accepted.
pub(crate) fn parse(text: &str) -> Result<Curves, String> {
    Curves::parse(text)
        .ok_or_else(|| format!("placement_curves must be off, shadow or on; got {text:?}"))
}

/// The hosts `names` names, the list of `--curve-hosts` or `server.curve_hosts`: every host
/// when the list is not given (`None`). Refused, in this order for each name, is an empty name,
/// a name given twice and a name that is not a host id, the refusal naming it; then a list that
/// names none.
pub(crate) fn hosts(names: Option<&[String]>) -> Result<CurveHosts, String> {
    let Some(names) = names else {
        return Ok(CurveHosts::default());
    };
    let mut named = BTreeSet::new();
    for name in names {
        if name.is_empty() {
            return Err("holds an empty name".to_owned());
        }
        let id = HostId::new(name).map_err(|err| err.to_string())?;
        if !named.insert(id) {
            return Err(format!("names {name:?} twice"));
        }
    }
    match named.is_empty() {
        true => Err("must name at least one".to_owned()),
        false => Ok(CurveHosts::new(named)),
    }
}

/// The wait bound `secs` gives, the value of `--curve-max-wait-seconds` or
/// `server.curve_max_wait_seconds`: [`DEFAULT_CURVE_MAX_WAIT`] when it is not given, and refused
/// above [`MAX_CURVE_MAX_WAIT_SECS`].
pub(crate) fn max_wait(secs: Option<u64>) -> Result<Duration, String> {
    match secs {
        None => Ok(DEFAULT_CURVE_MAX_WAIT),
        Some(secs) if secs <= MAX_CURVE_MAX_WAIT_SECS => Ok(Duration::from_secs(secs)),
        Some(_) => Err(format!("must be from 0 to {MAX_CURVE_MAX_WAIT_SECS}")),
    }
}

/// The curve mode the flags of `cli` give (`--placement-curves`, `--curve-hosts` and
/// `--curve-max-wait-seconds`), or the refusal naming the flag.
pub(crate) fn of_flags(cli: &Cli) -> Result<CurveSetting, StartupError> {
    let flags = &cli.curve;
    let hosts = hosts(flags.curve_hosts.as_deref())
        .map_err(|said| StartupError::new("--curve-hosts", said))?;
    let max_wait = max_wait(flags.curve_max_wait_seconds)
        .map_err(|said| StartupError::new("--curve-max-wait-seconds", said))?;
    Ok(CurveSetting {
        mode: cli.placement_curves,
        settings: CurveSettings { hosts, max_wait },
    })
}

#[cfg(test)]
#[path = "placement_curves_canary_tests.rs"]
mod canary_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that `off`, `shadow` and `on` are read as the mode they name, and that an
    /// upper-case spelling, a padded value and an empty one are refused with the whole
    /// message, which names the three accepted values and quotes the value given. Catches a
    /// parse that ignores case or trims, a refusal that does not name `on`, and an `on` that
    /// is refused.
    #[test]
    fn the_three_values_are_read_and_any_other_is_refused_in_full() {
        assert_eq!(parse("off"), Ok(Curves::Off));
        assert_eq!(parse("shadow"), Ok(Curves::Shadow));
        assert_eq!(parse("on"), Ok(Curves::On));
        for bad in ["On", " on", "Shadow", " off", ""] {
            assert_eq!(
                parse(bad),
                Err(format!(
                    "placement_curves must be off, shadow or on; got {bad:?}"
                )),
                "{bad:?}"
            );
        }
    }

    /// The names `names` as a list given.
    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Asserts the host list's answers, each refusal by its whole text: no list is every host;
    /// a list is the hosts it names; an empty name, a name twice (the second time it is
    /// written, though the first is valid), a name that is not a host id (a capital, 64
    /// characters) and an empty list are refused, the name quoted; the first bad name of a list
    /// is the one refused. Catches a check left out of the one path both the flag and the key
    /// take, and a refusal that names a name other than the bad one.
    #[test]
    fn the_host_list_is_read_and_refused_by_one_path() {
        let any = HostId::new("any").unwrap();
        assert!(hosts(None).unwrap().books(&any));
        let named = hosts(Some(list(&["h-1", "a.b_c-1"]).as_slice())).unwrap();
        let expected = [HostId::new("h-1").unwrap(), HostId::new("a.b_c-1").unwrap()];
        assert_eq!(named, CurveHosts::new(expected.into_iter().collect()));
        let long = "a".repeat(64);
        let not_id = "must be 1 to 63 characters of [a-z0-9._-]";
        let cases = [
            (list(&["a", ""]), "holds an empty name".to_owned()),
            (list(&["a", "b", "a"]), "names \"a\" twice".to_owned()),
            (list(&["a", "Host"]), format!("host id \"Host\" {not_id}")),
            (list(&[&long]), format!("host id \"{long}\" {not_id}")),
            (list(&["Bad", ""]), format!("host id \"Bad\" {not_id}")),
            (Vec::new(), "must name at least one".to_owned()),
        ];
        for (names, said) in cases {
            assert_eq!(hosts(Some(names.as_slice())), Err(said), "{names:?}");
        }
    }

    /// The curve setting of the command line `--placement-curves on` and `extra`, or the refusal.
    fn of_line(extra: &[&str]) -> Result<CurveSetting, String> {
        let mut line = vec![
            "js-lease-server",
            "--state-file",
            "state",
            "--boot",
            "resume",
        ];
        line.extend(["--token-file", "token", "--placement-curves", "on"]);
        line.extend_from_slice(extra);
        let cli = Cli::try_parse_from(line).unwrap();
        of_flags(&cli).map_err(|err| {
            let said = err.to_string();
            assert!(said.starts_with(&format!("{}: ", err.flag())), "{said}");
            said
        })
    }

    /// Asserts what the command line gives: the mode with every host and 30 s without the two
    /// flags, the hosts and the wait given with them, and each flag's refusal naming the flag, as
    /// its text and as the refusal's flag. Catches a flag read from the other's value and a
    /// refusal naming the key, or the configuration file, not the flag.
    #[test]
    fn the_command_line_gives_the_mode_hosts_and_wait_or_names_the_flag() {
        let none = of_line(&[]).unwrap();
        let on = CurveSetting {
            mode: Curves::On,
            settings: CurveSettings::default(),
        };
        assert_eq!(none, on);
        let given = of_line(&["--curve-hosts", "h-1", "--curve-max-wait-seconds", "0"]).unwrap();
        let hosts = CurveHosts::new([HostId::new("h-1").unwrap()].into_iter().collect());
        let settings = CurveSettings {
            hosts,
            max_wait: Duration::ZERO,
        };
        assert_eq!(given.settings, settings);
        let said = of_line(&["--curve-hosts", "h-1,h-1"]).unwrap_err();
        assert_eq!(said, "--curve-hosts: names \"h-1\" twice");
        let said = of_line(&["--curve-max-wait-seconds", "301"]).unwrap_err();
        assert_eq!(said, "--curve-max-wait-seconds: must be from 0 to 300");
    }

    /// Asserts the wait bound's answers: none is 30 s, 0 and 300 are read as given, and 301 and
    /// the largest number are refused with the whole text naming the range. Catches a range off
    /// by one at its top and a default other than 30 s.
    #[test]
    fn the_wait_bound_is_read_from_0_to_300_by_one_path() {
        assert_eq!(max_wait(None), Ok(Duration::from_secs(30)));
        assert_eq!(max_wait(Some(0)), Ok(Duration::ZERO));
        assert_eq!(max_wait(Some(300)), Ok(Duration::from_secs(300)));
        for bad in [301, u64::MAX] {
            let said = "must be from 0 to 300".to_owned();
            assert_eq!(max_wait(Some(bad)), Err(said), "{bad}");
        }
    }
}
