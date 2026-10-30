//! The numbers `--scratch-quota` takes: their defaults and bounds, and the check of the flags
//! that set them.

use crate::cli::Cli;

/// The GiB of scratch disk a run without `disk_bytes` books under `--scratch-quota` when
/// `--scratch-default-gib` is not given.
pub const DEFAULT_SCRATCH_GIB: u64 = 2;

/// The failed quota calls in a row after which the quota is probed again, when
/// `--scratch-quota-failures` is not given.
pub const DEFAULT_SCRATCH_QUOTA_FAILURES: u32 = 3;

/// The seconds a finished run's slot is held for its usage to read zero, when
/// `--scratch-usage-wait-secs` is not given.
pub const DEFAULT_SCRATCH_USAGE_WAIT_SECS: u64 = 600;

/// The most seconds `--scratch-usage-wait-secs` takes: a day.
pub const MAX_SCRATCH_USAGE_WAIT_SECS: u64 = 86_400;

/// The slots given up on (see `--scratch-usage-wait-secs`) after which the quota is
/// unavailable until the agent is restarted, when `--scratch-abandon-limit` is not given.
pub const DEFAULT_SCRATCH_ABANDON_LIMIT: u32 = 8;

/// The most `--scratch-abandon-limit` takes.
pub const MAX_SCRATCH_ABANDON_LIMIT: u32 = 1_000_000;

/// The most `--scratch-quota-failures` takes.
pub const MAX_SCRATCH_QUOTA_FAILURES: u32 = 1_000_000;

/// The most GiB `--scratch-default-gib` takes: the bytes must fit the wire's disk limit.
pub const MAX_SCRATCH_GIB: u64 = js_agent_wire::limits::MAX_DISK_BYTES >> 30;

/// The inode limit of a run under `--scratch-quota` when `--scratch-max-inodes` is not
/// given.
pub const DEFAULT_SCRATCH_MAX_INODES: u64 = 1_000_000;

/// The numbers of `--scratch-quota` (default GiB, inode limit, failures, usage wait, abandon limit), checked: its flags need
/// `--scratch-quota`, the default is at least 1 GiB and its bytes fit the wire, and the
/// inode limit is at least 1 and fits what the quota takes.
pub(super) fn scratch_quota_limits(cli: &Cli) -> Result<(u64, u64, u32, u64, u32), String> {
    let given = [
        ("--scratch-default-gib", cli.scratch_default_gib),
        ("--scratch-max-inodes", cli.scratch_max_inodes),
        ("--scratch-quota-failures", cli.scratch_quota_failures),
        ("--scratch-usage-wait-secs", cli.scratch_usage_wait_secs),
        ("--scratch-abandon-limit", cli.scratch_abandon_limit),
    ];
    if let Some((flag, _)) = given
        .iter()
        .find(|(_, value)| value.is_some() && !cli.scratch_quota)
    {
        return Err(format!("{flag}: needs --scratch-quota"));
    }
    let gib = cli.scratch_default_gib.unwrap_or(DEFAULT_SCRATCH_GIB);
    if !(1..=MAX_SCRATCH_GIB).contains(&gib) {
        return Err(format!(
            "--scratch-default-gib: {gib} is not from 1 to {MAX_SCRATCH_GIB}"
        ));
    }
    let inodes = cli.scratch_max_inodes.unwrap_or(DEFAULT_SCRATCH_MAX_INODES);
    if !(1..=crate::quota::MAX_INODE_LIMIT).contains(&inodes) {
        return Err(format!(
            "--scratch-max-inodes: {inodes} is not from 1 to {}",
            crate::quota::MAX_INODE_LIMIT
        ));
    }
    let failures = cli
        .scratch_quota_failures
        .unwrap_or(u64::from(DEFAULT_SCRATCH_QUOTA_FAILURES));
    let failures = u32::try_from(failures)
        .ok()
        .filter(|n| (1..=MAX_SCRATCH_QUOTA_FAILURES).contains(n))
        .ok_or_else(|| {
            format!(
                "--scratch-quota-failures: {failures} is not from 1 to {MAX_SCRATCH_QUOTA_FAILURES}"
            )
        })?;
    let wait = cli
        .scratch_usage_wait_secs
        .unwrap_or(DEFAULT_SCRATCH_USAGE_WAIT_SECS);
    if !(1..=MAX_SCRATCH_USAGE_WAIT_SECS).contains(&wait) {
        return Err(format!(
            "--scratch-usage-wait-secs: {wait} is not from 1 to {MAX_SCRATCH_USAGE_WAIT_SECS}"
        ));
    }
    let abandon = cli
        .scratch_abandon_limit
        .unwrap_or(u64::from(DEFAULT_SCRATCH_ABANDON_LIMIT));
    let abandon = u32::try_from(abandon)
        .ok()
        .filter(|n| (1..=MAX_SCRATCH_ABANDON_LIMIT).contains(n))
        .ok_or_else(|| {
            format!(
                "--scratch-abandon-limit: {abandon} is not from 1 to {MAX_SCRATCH_ABANDON_LIMIT}"
            )
        })?;
    Ok((gib, inodes, failures, wait, abandon))
}
