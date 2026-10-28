//! The settings of the load predicate and the flags that set some of them.

use std::fmt;
use std::time::Duration;

use js_types::sched::{FarmTime, Resources};

/// Whether placement reads the load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlacementLoad {
    /// Admit and rank by measured and predicted load.
    #[default]
    On,
    /// Place as `Off` does, and count what `On` would have decided
    /// ([`LoadCounters::would_admit`], [`LoadCounters::would_block`]).
    Shadow,
    /// Admit by the booked rule: free booked CPUs, memory never summed.
    Off,
}

/// The settings of the load predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadConfig {
    /// Whether placement reads the load. Default `On`.
    pub mode: PlacementLoad,
    /// The heartbeat interval `W`; readings are stale after three of them and the cold
    /// cap counts per window. Default 5 s.
    pub window: Duration,
    /// `f`, in thousandths: the covered leases are charged at least this share of their
    /// predicted cores. Default 750; kept within 500 to 1000.
    pub cpu_floor_permille: u32,
    /// `s_cpu`, in thousandths: the CPU the predicate lets a worker be charged above its
    /// capacity. Default 100.
    pub s_cpu_permille: u32,
    /// `s_mem`, in thousandths. Default 0.
    pub s_mem_permille: u32,
    /// The most cold leases admitted per worker per window; `None` is one per 32 cores
    /// of the worker, rounded up. At least 1.
    pub cold_per_window: Option<u32>,
    /// How much of a worker's memory, in thousandths, its active file pages may count for
    /// in the working set (see `WorkerLoad::working_set_counted`). Default 250; at most
    /// 1000.
    pub file_share_permille: u32,
    /// Profile admission: a key with a learned curve is charged per minute and the
    /// predicate is checked over the next 30 minutes. The operator's off switch for CPU
    /// (memory has its own, [`LoadConfig::profile_memory`]); with both off the flat rule
    /// applies. Default on.
    pub profile: bool,
    /// Memory is charged by the curve's remaining maximum. Needs [`LoadConfig::profile`].
    /// Default on.
    pub profile_memory: bool,
    /// `g`, in thousandths: a curve charges a lease at least this share of its flat
    /// cores. Default 750; kept within 500 and `f`.
    pub profile_floor_permille: u32,
    /// The node guard on flat memory (`d`, in thousandths): a lease charged by a memory
    /// curve is admitted only if the flat memory costs of every lease on the worker,
    /// its own included, stay within `(1 + d) * C_mem`. Never applies to a whole-host
    /// lease. `None` (the default) turns it off; the suggested value is 250.
    pub flat_memory_slack_permille: Option<u32>,
    /// End a new lease's charge at its expected life: the P90 of its key's wall minutes
    /// (read through [`CurveSource::wall_minutes_p90`]) plus this many minutes, instead
    /// of charging it for the whole horizon. The lease's own present minute is always
    /// charged. `None` (the default) turns it off; the suggested value is 2.
    pub expected_life_delta_minutes: Option<u32>,
    /// `--placement-score-balance`: how much of the other resource's leftover the fit
    /// counts, as `b/2` of the binding term ([`crate::Scorer`], key 5). Default 1; kept
    /// within 0 to 4.
    pub score_balance: u32,
    /// `--placement-score-slack-tol-ppm`: the overshoot of a worker's capacity, in parts per
    /// million of it, that key 1 ignores. Default 10,000 (1%).
    pub score_slack_tol_ppm: u64,
    /// `--placement-key-share`: the share of a worker's CPU, in percent, that one action
    /// key may take before key 4 prefers another worker. Default 25; at most 100.
    pub key_share_percent: u32,
}

/// Why [`LoadConfig::apply_flag`] refused a flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagError {
    /// Not a placement score flag.
    Unknown(String),
    /// The value is not a number in the flag's range.
    Invalid {
        /// The flag.
        flag: String,
        /// The value as given.
        value: String,
    },
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(flag) => write!(f, "unknown placement flag {flag}"),
            Self::Invalid { flag, value } => write!(f, "{flag}: invalid value {value:?}"),
        }
    }
}

impl std::error::Error for FlagError {}

impl Default for LoadConfig {
    fn default() -> Self {
        Self {
            mode: PlacementLoad::On,
            window: Duration::from_secs(5),
            cpu_floor_permille: 750,
            s_cpu_permille: 100,
            s_mem_permille: 0,
            cold_per_window: None,
            file_share_permille: 250,
            profile: true,
            profile_memory: true,
            profile_floor_permille: 750,
            flat_memory_slack_permille: None,
            expected_life_delta_minutes: None,
            score_balance: 1,
            score_slack_tol_ppm: 10_000,
            key_share_percent: 25,
        }
    }
}

impl LoadConfig {
    /// These settings with each value brought into its range.
    #[must_use]
    pub fn normalised(self) -> Self {
        let floor = self.cpu_floor_permille.clamp(500, 1000);
        Self {
            window: self.window.max(Duration::from_millis(1)),
            cpu_floor_permille: floor,
            cold_per_window: self.cold_per_window.map(|n| n.max(1)),
            file_share_permille: self.file_share_permille.min(1000),
            profile_floor_permille: self.profile_floor_permille.clamp(500, floor),
            score_balance: self.score_balance.min(crate::load::MAX_BALANCE),
            key_share_percent: self.key_share_percent.min(100),
            ..self
        }
    }

    /// Sets the score flag `name` (`--placement-score-balance`, `--placement-score-slack-tol-ppm`
    /// or `--placement-key-share`, with or without the leading dashes) to `value`. Nothing
    /// changes on an error. The balance is 0 to 4 and the key share 0 to 100 (a trailing `%`
    /// is allowed); a value outside is refused rather than clamped, so a typo is not run.
    ///
    /// # Errors
    /// [`FlagError`] for another flag or a value out of range.
    pub fn apply_flag(&mut self, name: &str, value: &str) -> Result<(), FlagError> {
        let invalid = || FlagError::Invalid {
            flag: name.to_owned(),
            value: value.to_owned(),
        };
        match name.trim_start_matches('-') {
            "placement-score-balance" => {
                let n: u32 = value.parse().map_err(|_| invalid())?;
                if n > crate::load::MAX_BALANCE {
                    return Err(invalid());
                }
                self.score_balance = n;
            }
            "placement-score-slack-tol-ppm" => {
                self.score_slack_tol_ppm = value.parse().map_err(|_| invalid())?;
            }
            "placement-key-share" => {
                let n: u32 = value
                    .strip_suffix('%')
                    .unwrap_or(value)
                    .parse()
                    .map_err(|_| invalid())?;
                if n > 100 {
                    return Err(invalid());
                }
                self.key_share_percent = n;
            }
            _ => return Err(FlagError::Unknown(name.to_owned())),
        }
        Ok(())
    }

    fn window_millis(&self) -> u64 {
        u64::try_from(self.window.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    pub(crate) fn window_usec(&self) -> u64 {
        self.window_millis().saturating_mul(1000)
    }

    /// The number of the window `now` falls in.
    pub(crate) fn window_index(&self, now: FarmTime) -> u64 {
        now.as_millis() / self.window_millis()
    }

    /// `f * cores`, rounded up.
    pub(crate) fn floor_of(&self, cores: u64) -> u64 {
        scale_up(cores, u64::from(self.cpu_floor_permille), 1000)
    }

    /// `C_cpu * (1 + s_cpu)`.
    pub(crate) fn cpu_limit(&self, capacity: &Resources) -> u64 {
        scale(
            capacity.cpu_millis,
            1000 + u64::from(self.s_cpu_permille),
            1000,
        )
    }

    /// `C_mem * (1 + s_mem)`.
    pub(crate) fn mem_limit(&self, capacity: &Resources) -> u64 {
        scale(
            capacity.memory_bytes,
            1000 + u64::from(self.s_mem_permille),
            1000,
        )
    }

    /// The cold leases a worker of `capacity` admits per window.
    pub(crate) fn cold_cap(&self, capacity: &Resources) -> u32 {
        self.cold_per_window.map_or_else(
            || {
                u32::try_from(capacity.cpu_millis.div_ceil(32_000))
                    .unwrap_or(u32::MAX)
                    .max(1)
            },
            |n| n.max(1),
        )
    }
}

pub(crate) fn scale(value: u64, num: u64, den: u64) -> u64 {
    u64::try_from(u128::from(value) * u128::from(num) / u128::from(den)).unwrap_or(u64::MAX)
}

pub(crate) fn scale_up(value: u64, num: u64, den: u64) -> u64 {
    u64::try_from((u128::from(value) * u128::from(num)).div_ceil(u128::from(den)))
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: settings out of range reaching the predicate: a floor below a half or above
    /// one, a zero window (every window the same index, a zero staleness limit), a
    /// cold cap of 0 (no cold key ever runs) and a file share above the whole node.
    #[test]
    fn settings_are_brought_into_range() {
        let wild = LoadConfig {
            window: Duration::ZERO,
            cpu_floor_permille: 10,
            cold_per_window: Some(0),
            file_share_permille: 9_000,
            ..LoadConfig::default()
        }
        .normalised();
        assert_eq!(wild.window, Duration::from_millis(1));
        assert_eq!(wild.cpu_floor_permille, 500);
        assert_eq!(wild.cold_per_window, Some(1));
        assert_eq!(wild.file_share_permille, 1000);
        let high = LoadConfig {
            cpu_floor_permille: 5_000,
            ..LoadConfig::default()
        }
        .normalised();
        assert_eq!(high.cpu_floor_permille, 1000);
        assert_eq!(LoadConfig::default().normalised(), LoadConfig::default());
        let wild = LoadConfig {
            score_balance: 9,
            key_share_percent: 400,
            ..LoadConfig::default()
        }
        .normalised();
        assert_eq!((wild.score_balance, wild.key_share_percent), (4, 100));
    }

    /// Catches: the profile floor `g` outside 0.5 to `f` (a curve charged below half of
    /// the flat cost, or above the floor the covered flat leases get), the switches not
    /// on and the two owner-decision settings not off by default.
    #[test]
    fn the_profile_floor_stays_between_a_half_and_the_flat_floor() {
        let with = |f, g| {
            LoadConfig {
                cpu_floor_permille: f,
                profile_floor_permille: g,
                ..LoadConfig::default()
            }
            .normalised()
        };
        assert_eq!(with(750, 100).profile_floor_permille, 500);
        assert_eq!(with(750, 600).profile_floor_permille, 600);
        assert_eq!(with(750, 900).profile_floor_permille, 750, "not above f");
        assert_eq!(
            with(600, 750).profile_floor_permille,
            600,
            "f moved: g follows"
        );
        assert_eq!(
            with(100, 100).profile_floor_permille,
            500,
            "f is clamped first"
        );
        let d = LoadConfig::default();
        assert!(d.profile);
        assert!(d.profile_memory);
        assert_eq!(d.profile_floor_permille, d.cpu_floor_permille);
        assert_eq!(d.flat_memory_slack_permille, None);
        assert_eq!(d.expected_life_delta_minutes, None);
    }

    /// Catches: the default cold cap not being one per 32 cores rounded up (3 on 88
    /// threads), a worker of under 32 cores getting none, and an explicit setting being
    /// ignored.
    #[test]
    fn the_cold_cap_is_one_per_32_cores_rounded_up() {
        let cfg = LoadConfig::default();
        let cap = |cores: u64| cfg.cold_cap(&Resources::new(cores * 1000, 0));
        assert_eq!(cap(88), 3);
        assert_eq!(cap(64), 2);
        assert_eq!(cap(65), 3);
        assert_eq!(cap(16), 1);
        assert_eq!(cap(0), 1);
        let set = LoadConfig {
            cold_per_window: Some(12),
            ..cfg
        };
        assert_eq!(set.cold_cap(&Resources::new(88_000, 0)), 12);
    }

    /// Catches: limits computed with the slack on the wrong resource, overflowing, or
    /// the floor rounded down (an under-charge).
    #[test]
    fn limits_floor_and_window_arithmetic() {
        let cfg = LoadConfig::default();
        let cap = Resources::new(88_000, 1000);
        assert_eq!(cfg.cpu_limit(&cap), 96_800);
        assert_eq!(cfg.mem_limit(&cap), 1000);
        let loose = LoadConfig {
            s_mem_permille: 500,
            ..cfg
        };
        assert_eq!(loose.mem_limit(&cap), 1500);
        assert_eq!(cfg.cpu_limit(&Resources::new(u64::MAX, 0)), u64::MAX);
        assert_eq!(cfg.floor_of(1000), 750);
        assert_eq!(cfg.floor_of(1), 1, "rounded up");
        assert_eq!(cfg.floor_of(u64::MAX), 13_835_058_055_282_163_712);
        assert_eq!(cfg.window_usec(), 5_000_000);
        assert_eq!(cfg.window_index(FarmTime::from_millis(4_999)), 0);
        assert_eq!(cfg.window_index(FarmTime::from_millis(5_000)), 1);
    }

    /// Catches: a score flag parsed into the wrong field, a value out of range clamped
    /// silently instead of refused (a typo would run), a failed parse changing the
    /// config, and a `%` suffix on the key share not accepted.
    #[test]
    fn the_score_flags_parse_into_their_fields() {
        let mut cfg = LoadConfig::default();
        assert_eq!(cfg.apply_flag("--placement-score-balance", "3"), Ok(()));
        assert_eq!(
            cfg.apply_flag("--placement-score-slack-tol-ppm", "2500"),
            Ok(())
        );
        assert_eq!(cfg.apply_flag("--placement-key-share", "40%"), Ok(()));
        assert_eq!(
            (
                cfg.score_balance,
                cfg.score_slack_tol_ppm,
                cfg.key_share_percent
            ),
            (3, 2_500, 40)
        );
        assert_eq!(cfg.apply_flag("placement-key-share", "100"), Ok(()));
        assert_eq!(cfg.key_share_percent, 100);
        let before = cfg;
        for (flag, value) in [
            ("--placement-score-balance", "5"),
            ("--placement-score-balance", "-1"),
            ("--placement-score-slack-tol-ppm", "1%"),
            ("--placement-key-share", "101"),
            ("--placement-key-share", ""),
        ] {
            assert_eq!(
                cfg.apply_flag(flag, value),
                Err(FlagError::Invalid {
                    flag: flag.to_owned(),
                    value: value.to_owned()
                })
            );
        }
        assert_eq!(
            cfg.apply_flag("--placement-s-cpu", "100"),
            Err(FlagError::Unknown("--placement-s-cpu".to_owned()))
        );
        assert_eq!(cfg, before);
        assert_eq!(
            FlagError::Unknown("--placement-s-cpu".to_owned()).to_string(),
            "unknown placement flag --placement-s-cpu"
        );
        assert_eq!(
            FlagError::Invalid {
                flag: "--placement-key-share".to_owned(),
                value: "101".to_owned()
            }
            .to_string(),
            "--placement-key-share: invalid value \"101\""
        );
    }
}
