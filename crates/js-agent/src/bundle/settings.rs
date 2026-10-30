//! The bundle flags, checked together.

use std::path::{Path, PathBuf};

use js_agent_wire::ReadModel;
use js_agent_wire::limits::MAX_CAPABILITY_VALUE_LEN;

use super::host::BundleHost;
use super::list::parse_digests;
use crate::cli::Cli;
use crate::facts::{GIB, Platform};

/// When a bundle that is not in the store is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    /// Never: the operator or the updater puts it there first.
    Never,
    /// When a job names one (only with [`Trust::List`]).
    Missing,
}

/// Which digests may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Those on the allow list.
    List,
    /// Any a job names (developer hosts).
    Digest,
}

/// A registry and namespace a bundle may be fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    /// The host, lower case, with its port when it has one.
    pub host: String,
    /// The namespace prefix below the host, without a trailing `/`.
    pub prefix: String,
    /// The registry is on a private address, and may be reached for that registry only.
    pub private: bool,
}

impl Registry {
    /// Whether the bundle `name` (a pin's name, with its tag if it has one) is in this
    /// registry's namespace: the same host, and a path that is the prefix or below it.
    #[must_use]
    pub fn covers(&self, name: &str) -> bool {
        let Some((host, path)) = name.split_once('/') else {
            return false;
        };
        // A tag follows the last `:` after the last `/`.
        let repository = match path.rsplit_once(':') {
            Some((before, tag)) if !tag.contains('/') => before,
            _ => path,
        };
        let below = repository
            .strip_prefix(self.prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        host.eq_ignore_ascii_case(&self.host) && below
    }
}

/// The bundle flags, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `--bundle-store`.
    pub store: PathBuf,
    /// `--scratch-root`, which the store shares a volume with.
    pub scratch: PathBuf,
    /// `--bundle-budget-gib`, in bytes.
    pub budget_bytes: u64,
    /// `--bundle-low-water-gib`, in bytes.
    pub low_water_bytes: u64,
    /// `--bundle-pull`.
    pub pull: Pull,
    /// `--bundle-registry-allow` and `--bundle-registry-allow-private`.
    pub registries: Vec<Registry>,
    /// `--bundle-min-age-min`.
    pub min_age_min: u64,
    /// `--bundle-keep-file`.
    pub keep_file: Option<PathBuf>,
    /// `--bundle-deny-file`.
    pub deny_file: Option<PathBuf>,
    /// `--bundle-prefetch-file`.
    pub prefetch_file: Option<PathBuf>,
    /// `--bundle-trust`.
    pub trust: Trust,
    /// `--bundle-allow-file`; present exactly when the trust is [`Trust::List`].
    pub allow_file: Option<PathBuf>,
    /// `--read-model`.
    pub read_model: ReadModel,
}

/// The default of `--bundle-min-age-min`.
pub const DEFAULT_MIN_AGE_MIN: u64 = 30;

/// A flag that only means something with a store, by name, when it is given.
fn without_store(cli: &Cli) -> Option<&'static str> {
    let given = [
        ("--bundle-budget-gib", cli.bundle_budget_gib.is_some()),
        ("--bundle-low-water-gib", cli.bundle_low_water_gib.is_some()),
        ("--bundle-pull", cli.bundle_pull.is_some()),
        (
            "--bundle-registry-allow",
            !cli.bundle_registry_allow.is_empty(),
        ),
        (
            "--bundle-registry-allow-private",
            !cli.bundle_registry_allow_private.is_empty(),
        ),
        ("--bundle-min-age-min", cli.bundle_min_age_min.is_some()),
        ("--bundle-keep-file", cli.bundle_keep_file.is_some()),
        ("--bundle-deny-file", cli.bundle_deny_file.is_some()),
        ("--bundle-prefetch-file", cli.bundle_prefetch_file.is_some()),
        ("--bundle-trust", cli.bundle_trust.is_some()),
        ("--bundle-allow-file", cli.bundle_allow_file.is_some()),
        ("--read-model", cli.read_model.is_some()),
    ];
    given
        .into_iter()
        .find(|(_, set)| *set)
        .map(|(flag, _)| flag)
}

/// One of `choices`, by the name the flag was given, or an error naming the flag.
fn choice<T: Copy>(flag: &str, given: Option<&str>, choices: &[(&str, T)]) -> Result<T, String> {
    let wanted = given.unwrap_or(choices[0].0);
    let found = choices.iter().find(|(name, _)| *name == wanted);
    found.map(|(_, value)| *value).ok_or_else(|| {
        let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
        format!("{flag}: {wanted:?} is none of {}", names.join(", "))
    })
}

/// `HOST[:PORT]/PREFIX`: a host of dot-separated labels and an optional port, then a
/// namespace of one or more `/`-separated lower case components, optionally followed by one
/// `/`. A bare host is refused: it would admit every account's packages.
fn registry(flag: &str, text: &str, private: bool) -> Result<Registry, String> {
    let bad = |why: &str| format!("{flag}: {text:?} {why}");
    let (host, prefix) = text
        .split_once('/')
        .ok_or_else(|| bad("names a bare host; give HOST[:PORT]/PREFIX"))?;
    let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
    let (name, port) = host
        .split_once(':')
        .map_or((host, None), |(name, port)| (name, Some(port)));
    let labels = name.split('.').all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    let port_ok = port.is_none_or(|digits| {
        !digits.is_empty() && digits.len() <= 5 && digits.bytes().all(|b| b.is_ascii_digit())
    });
    if !labels || !port_ok {
        return Err(bad("has a host that is not a registry host"));
    }
    let components = prefix.split('/').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    });
    if !components {
        return Err(bad("has a namespace that is empty or not lower case"));
    }
    Ok(Registry {
        host: host.to_ascii_lowercase(),
        prefix: prefix.to_owned(),
        private,
    })
}

/// Reads a list file through the host and checks that it parses; returns how many digests
/// it holds.
fn list(flag: &str, host: &dyn BundleHost, path: &Path) -> Result<usize, String> {
    let text = host
        .read_file(path)
        .map_err(|why| format!("{flag}: {why}"))?;
    let digests = parse_digests(&text).map_err(|why| format!("{flag}: {why}"))?;
    Ok(digests.len())
}

impl Settings {
    /// Checks the bundle flags of `cli`. `None` when none is given.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault: a bundle flag without `--bundle-store`; a store
    /// off a Mac, without `--scratch-root`, without a budget, on a volume the checks refuse
    /// or not absolute; a choice that is none of its values; `--read-model allow`; a
    /// registry entry that is not `HOST[:PORT]/PREFIX`; `--bundle-pull missing` with trust
    /// `digest` or without a registry; trust `list` without an allow list that holds a
    /// digest, or `digest` with one; a list file that cannot be read or is malformed.
    pub fn from_cli(
        cli: &Cli,
        platform: &Platform,
        host: &dyn BundleHost,
    ) -> Result<Option<Self>, String> {
        let Some(store) = &cli.bundle_store else {
            return match without_store(cli) {
                Some(flag) => Err(format!("{flag}: needs --bundle-store")),
                None => Ok(None),
            };
        };
        if !platform.is_macos() {
            return Err("--bundle-store: bundles are for macOS hosts".to_owned());
        }
        if !store.is_absolute() {
            return Err("--bundle-store: must be an absolute path".to_owned());
        }
        let Some(scratch) = &cli.scratch_root else {
            return Err(
                "--bundle-store: needs --scratch-root: the store and the scratch root must \
                 be on one volume"
                    .to_owned(),
            );
        };
        let budget = match cli.bundle_budget_gib {
            Some(gib) if gib > 0 => gib,
            _ => {
                return Err("--bundle-budget-gib: required with --bundle-store, above 0".to_owned());
            }
        };
        let pull = choice(
            "--bundle-pull",
            cli.bundle_pull.as_deref(),
            &[("never", Pull::Never), ("missing", Pull::Missing)],
        )?;
        let trust = choice(
            "--bundle-trust",
            cli.bundle_trust.as_deref(),
            &[("list", Trust::List), ("digest", Trust::Digest)],
        )?;
        let read_model = choice(
            "--read-model",
            cli.read_model.as_deref(),
            &[("open", ReadModel::Open), ("allow", ReadModel::Allow)],
        )?;
        if read_model == ReadModel::Allow {
            return Err(
                "--read-model: allow is not available yet: the allow-list profile has not \
                 landed"
                    .to_owned(),
            );
        }
        let public = cli
            .bundle_registry_allow
            .iter()
            .map(|text| registry("--bundle-registry-allow", text, false));
        let private = cli
            .bundle_registry_allow_private
            .iter()
            .map(|text| registry("--bundle-registry-allow-private", text, true));
        let registries = public.chain(private).collect::<Result<Vec<_>, _>>()?;
        if (pull, trust) == (Pull::Missing, Trust::Digest) {
            return Err(
                "--bundle-pull missing: not with --bundle-trust digest: any job could make \
                 this host download and unpack a bundle of its choosing"
                    .to_owned(),
            );
        }
        if pull == Pull::Missing && registries.is_empty() {
            return Err("--bundle-pull missing: needs --bundle-registry-allow or \
                 --bundle-registry-allow-private"
                .to_owned());
        }
        let allow_file = match (trust, &cli.bundle_allow_file) {
            (Trust::List, None) => {
                return Err("--bundle-trust list: needs --bundle-allow-file".to_owned());
            }
            (Trust::Digest, Some(_)) => {
                return Err("--bundle-allow-file: needs --bundle-trust list".to_owned());
            }
            (_, file) => file.clone(),
        };
        if let Some(file) = &allow_file
            && list("--bundle-allow-file", host, file)? == 0
        {
            return Err("--bundle-allow-file: lists no digest".to_owned());
        }
        for (flag, file) in [
            ("--bundle-deny-file", &cli.bundle_deny_file),
            ("--bundle-keep-file", &cli.bundle_keep_file),
            ("--bundle-prefetch-file", &cli.bundle_prefetch_file),
        ] {
            if let Some(file) = file {
                list(flag, host, file)?;
            }
        }
        host.check_start(store, scratch)
            .map_err(|why| format!("--bundle-store: {why}"))?;
        // The path is also the `bundle-root` capability, which the wire holds to a length.
        let shown = store.to_string_lossy();
        if shown.len() > MAX_CAPABILITY_VALUE_LEN {
            return Err("--bundle-store: the path is too long to report as bundle-root".to_owned());
        }
        Ok(Some(Self {
            store: store.clone(),
            scratch: scratch.clone(),
            budget_bytes: budget.saturating_mul(GIB),
            low_water_bytes: cli.bundle_low_water_gib.unwrap_or(0).saturating_mul(GIB),
            pull,
            registries,
            min_age_min: cli.bundle_min_age_min.unwrap_or(DEFAULT_MIN_AGE_MIN),
            keep_file: cli.bundle_keep_file.clone(),
            deny_file: cli.bundle_deny_file.clone(),
            prefetch_file: cli.bundle_prefetch_file.clone(),
            trust,
            allow_file,
            read_model,
        }))
    }
}
