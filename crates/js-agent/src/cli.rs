//! The command line: flags only, never environment variables.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{CommandFactory, Parser};

use crate::identity::IdentityFlags;
#[cfg(target_os = "macos")]
use crate::platform::LINUX_ONLY as OTHER_PLATFORM;
use crate::platform::LONG_VERSION;
#[cfg(target_os = "linux")]
use crate::platform::MACOS_ONLY as OTHER_PLATFORM;

/// The flags of `js-agent`.
#[derive(Debug, Parser)]
#[command(
    name = "js-agent",
    version,
    long_version = LONG_VERSION,
    about = "Registers this machine with a lease server and keeps it registered"
)]
pub struct Cli {
    /// The lease server, such as `https://leases.example:7300`. Plain `http://` is
    /// accepted for a loopback host only: the join token never crosses a network in
    /// clear.
    #[arg(long, value_name = "URL")]
    pub server: String,
    /// A file of more flags, one argument word per line (`--server=https://leases.example:7300`;
    /// empty lines and lines that start with `#` are skipped), read in place of this flag
    /// before the other flags are parsed. A regular file owned by the user running the agent
    /// or by root, not writable by group or others, at most 64 KiB; given once, and not
    /// inside a flags file. For a unit or job that is the same on every host and so cannot
    /// name the host's server and pool.
    #[arg(long, value_name = "PATH")]
    pub flags_file: Option<PathBuf>,
    /// A file holding the pool's join token, or the one-time token minted for this
    /// instance: a regular file owned by the user running the agent, mode 0600 or 0400.
    /// The token is sent only in the `Authorization` header. Without `--state-root` a
    /// one-time token is this host's credential for as long as the host stays registered,
    /// so the agent leaves the file in place: it reads it again at every start. With a
    /// state root, the first hello of a host that joins with a one-time token is answered
    /// with a host credential that replaces the token (the server retires it): the agent
    /// keeps the credential in the state root and sends it from then on. Required unless
    /// `--state-root` holds a usable `host-credential` file, which the agent then sends in
    /// place of a token; a token file given beside it is read as before.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// A file holding the instance this machine runs on, as one line `PROVIDER:INSTANCE_ID`
    /// (`lambdalabs:0920582c...`), read when the agent starts. The hello names the
    /// instance, which a one-time join token minted for that instance requires: the server
    /// refuses a hello that names another instance or none. The file is not secret.
    #[arg(long, value_name = "PATH")]
    pub instance_file: Option<PathBuf>,
    /// The cloud's signed identity of this machine, sent in the hello (see
    /// [`crate::identity`]).
    #[command(flatten)]
    pub identity: IdentityFlags,
    /// A PEM file with the certificate to trust for `https://` (the server's, or the
    /// authority that issued it); only those certificates are trusted then. Optional:
    /// without it an `https://` server is verified against the public roots bundled in the
    /// agent (a Mozilla set), never the platform's store.
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<PathBuf>,
    /// The pool this host asks to join.
    #[arg(long, value_name = "NAME")]
    pub pool: String,
    /// The id this host registers under. By default a salted hash of the machine id
    /// (`/etc/machine-id`, or the platform UUID on a Mac), never the machine id itself. With
    /// the crun runtime one agent may hold a host id: the claim is a lock file in the
    /// directory above `--scratch-root`, so agents that share a host id must share that
    /// directory to be refused.
    #[arg(long, value_name = "ID")]
    pub host_id: Option<String>,
    /// A label to put on the host, as `key=value`. May be given more than once; a key
    /// given twice takes its last value.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = key_value)]
    pub labels: Vec<(String, String)>,
    /// A capability to report, as `key=value`. May be given more than once. `os` and
    /// `arch` are detected and cannot be set.
    #[arg(long = "capability", value_name = "KEY=VALUE", value_parser = key_value)]
    pub capabilities: Vec<(String, String)>,
    /// Offer at most this many thousandths of a CPU core. A flag only lowers what is
    /// detected.
    #[arg(long, value_name = "N")]
    pub cpu_millis: Option<u64>,
    /// Offer at most this many bytes of memory. A flag only lowers what is detected.
    #[arg(long, value_name = "N")]
    pub memory_bytes: Option<u64>,
    /// Offer at most this many GPUs; `0` offers none. A flag only lowers what is
    /// detected.
    #[arg(long, value_name = "N")]
    pub gpus: Option<u64>,
    /// Name the offered GPUs' class, instead of the one derived from the detected model.
    /// Allowed only when at least one GPU is offered.
    #[arg(long, value_name = "CLASS")]
    pub gpu_class: Option<String>,
    /// A directory jobs may mount from, as `name=path`; the name is what a mount in a
    /// start refers to. May be given more than once. The path must be an existing
    /// directory; it is canonicalised when the agent starts. No root, no mount. With
    /// `--scratch-quota` it may not be on the scratch root's file system; that check sees
    /// the root's own device only, not a scratch device mounted below it (a submount a
    /// recursive bind brings along), which the operator must see to be absent.
    #[arg(long = "mount-root", value_name = "NAME=PATH", value_parser = named_path)]
    pub mount_roots: Vec<(String, PathBuf)>,
    /// A directory jobs may mount from read-only, as `name=path`; like `--mount-root`, and
    /// it shares its namespace of names, except that a start with a read-write mount from
    /// it is refused. Unlike `--mount-root`, it may be on the scratch root's file system
    /// when `--scratch-quota` is given. May be given more than once. The comparison of file
    /// systems sees the root's own device only: a scratch device mounted below a root (a
    /// submount that a recursive bind brings along) is not seen, and the operator must
    /// see to it that there is none.
    #[arg(long = "mount-root-ro", value_name = "NAME=PATH", value_parser = named_path)]
    pub mount_roots_ro: Vec<(String, PathBuf)>,
    /// Bound each run's disk with a project quota on the scratch root's file system. At
    /// start the agent probes the root and refuses to start unless the file system
    /// enforces project quota; then it advertises `scratch-quota=project` and
    /// `scratch-default-gib`, books `--scratch-default-gib` for a run that names no
    /// `disk_bytes`, and gives each run's scratch directory a block limit of the bytes it
    /// books and an inode limit of `--scratch-max-inodes`. With the crun runtime the same
    /// project covers the run's lease directory, made beside the run's directory in the
    /// scratch root, so one bound holds what a job writes to `/scratch`, `/work` and
    /// `/root`. It also refuses to start when a
    /// `--mount-root` is on the same file system as the scratch root (a read-write mount
    /// there would be an unbounded path to its disk); `--mount-root-ro` roots may be there.
    /// Needs `--scratch-root`.
    #[arg(long)]
    pub scratch_quota: bool,
    /// The GiB a run that names no `disk_bytes` books and is bounded to, and the
    /// `scratch-default-gib` capability. From 1; default 2. Needs `--scratch-quota`.
    #[arg(long, value_name = "GIB")]
    pub scratch_default_gib: Option<u64>,
    /// The inode limit of each run's scratch directory, so a flood of empty files cannot
    /// exhaust the file system's inodes. From 1; default 1000000. Needs `--scratch-quota`.
    #[arg(long, value_name = "N")]
    pub scratch_max_inodes: Option<u64>,
    /// The quota calls (`apply`, a usage read, `clear`) that may fail in a row before the
    /// agent probes the scratch root again; if it no longer enforces project quota the agent
    /// starts no run until it does, and says so every minute. From 1; default 3. Needs
    /// `--scratch-quota`.
    #[arg(long, value_name = "N")]
    pub scratch_quota_failures: Option<u64>,
    /// The seconds a finished run's slot is held for its project's usage to read zero (the
    /// file system frees a removed tree's inodes in the background). After that the slot is
    /// parked, the run's place and booked bytes are given back, and the quota counts one
    /// failure. From 1 to 86400; default 600. Needs `--scratch-quota`.
    #[arg(long, value_name = "SECS")]
    pub scratch_usage_wait_secs: Option<u64>,
    /// The slots the agent may give up on (their usage never read zero within
    /// `--scratch-usage-wait-secs`) over its life before the quota is unavailable: no run
    /// starts, each start is refused `quota-unavailable`, until the agent is restarted. A
    /// run that ends well does not take one back. From 1; default 8. Needs
    /// `--scratch-quota`.
    #[arg(long, value_name = "N")]
    pub scratch_abandon_limit: Option<u64>,
    /// The most GiB of the scratch root's free disk the agent keeps back from jobs: the
    /// disk it offers is the free space of that volume when the agent starts minus this
    /// (0 when this is more). Needs `--scratch-root`. Without it, the free space is offered.
    #[arg(long, value_name = "GIB")]
    pub scratch_low_water_gib: Option<u64>,
    /// The directory under which every run gets a private scratch directory (`0700`). It
    /// must be owned by the user running the agent, with no permission for group or
    /// others. Required unless `--no-jobs`. With the crun runtime the image store and the claim
    /// files are kept in the directory above it, which must be on the same file system, and
    /// every directory above the root must be owned by root or by the agent's user with no
    /// group or other write.
    #[arg(long, value_name = "PATH")]
    pub scratch_root: Option<PathBuf>,
    /// The directory under which each run's files (`files` of a start: URLs and tokens) are
    /// written, `0555` with `0444` files, and mounted read-only at `/run/js-files`. Made
    /// `0700` when missing. It must be on a `tmpfs` so the secrets never reach a disk; the
    /// agent refuses to start when it is not. A reboot clears it, which is fine: a run does
    /// not survive a reboot. A tmpfs can be swapped to disk when the host has swap, so run hosts
    /// that handle these secrets with swap off or encrypted. It may not be, hold or lie in
    /// the state, scratch or a mount root. Not with `--no-jobs`. Without the flag
    /// `/run/js-agent/files` is used when it can be
    /// made; when it cannot, the agent runs without run files and does not report the
    /// `run-files` capability. With the native runtime (a Mac, which has no tmpfs) the
    /// root is used only when this flag is given and is not checked for a tmpfs: the files are
    /// on disk unless the flag points at a RAM-backed volume. Each run's directory is given to
    /// the run's user (`0500`, files `0400`) before its program starts and is named by
    /// `JS_FILES_DIR`; the root may not be, hold or lie in the homes of the native users or a
    /// path native runs may not read.
    #[arg(long, value_name = "PATH")]
    pub run_files_root: Option<PathBuf>,
    /// The directory the agent keeps its taint marker and its host credential in: it must be
    /// owned by the user running the agent, with no permission for group or others.
    /// Required unless `--no-jobs`. Giving it also lets a server issue the host a
    /// credential at its first hello (a file `host-credential`, mode 0600, which the agent
    /// writes before it uses it and reads at start; a file that is there and unusable stops
    /// the agent). A host that a post-job check found unclean is tainted for good:
    /// the marker survives a restart, and no code of the agent removes it. It may not be,
    /// or lie in, a mount root or the scratch root (a job with a read-write mount could
    /// delete the marker). It must be on a disk of its own: a job that fills a disk the
    /// state root shares makes the marker unwritable. The agent compares the device number
    /// (`st_dev`) of the state root with that of every mount root and the scratch root and
    /// refuses to start when one is equal. That is a best-effort guard, not a guarantee: the
    /// engine's data root and the container layers are not compared, and file systems that
    /// share free space under distinct device numbers (btrfs subvolumes, for one) pass it.
    /// The operator provides the separate disk. The taint belongs to this directory, not to
    /// the machine: a second agent, another state root, or a state root on a file system
    /// that is lost at a reboot (a `tmpfs`) does not know of it, so give each machine one
    /// agent and one state root.
    #[arg(long, value_name = "PATH")]
    pub state_root: Option<PathBuf>,
    /// Start an agent that runs no job: it registers, says hello and heartbeats, and reports
    /// none of the runtime capabilities. It takes no flag of the runtime this build runs
    /// (Linux: crun, or the container engine; macOS: the native runtime).
    #[arg(long)]
    pub no_jobs: bool,
    /// The flags of the runtime this build runs, which exist on this platform only (see
    /// [`crate::platform`]).
    #[command(flatten)]
    pub os: crate::platform::Flags,
    /// The bytes of each stream of a crun job with a live-log sink that the
    /// capture keeps in its file in the lease directory, from 1 MiB to 1 GiB: the file is
    /// what the agent uploads while the store is down, and bytes past it are a gap in the
    /// job's live logs. A job without a sink keeps the capture's 1 MiB. For a native job with
    /// a sink it is the most bytes of each of its log files the agent uploads; the rest is a
    /// gap, and the files themselves are as before.
    #[arg(
        long,
        value_name = "BYTES",
        default_value_t = crate::logs::DEFAULT_FILE_CAP,
        value_parser = clap::value_parser!(u64).range(crate::logs::MIN_FILE_CAP..=crate::logs::MAX_FILE_CAP)
    )]
    pub log_file_cap: u64,
    /// The absolute path of a program the agent runs while it prepares the host for a
    /// whole-node lease, after its own checks: exit 0 reports the host ready, other exit
    /// statuses refuse the lease with a typed reason (75 `noise-over-bound`, 76
    /// `not-quiet`, 77 `setup-failed`, 78 `unsupported`; any other `setup-failed`), and
    /// the first line it prints is the refusal's detail, sent to the scheduler verbatim (cut
    /// to a byte bound), so it must not print secrets. It runs in a process group of its own,
    /// which is killed when the hook overruns. It gets an environment of its
    /// own and nothing of the scheduler's, and `prepare_timeout` for its output to end and
    /// as long again for its exit (twice that in the worst case).
    #[arg(long, value_name = "PATH")]
    pub readiness_hook: Option<String>,
    /// An argument of `--readiness-hook`, in order. May be given more than once. Needs
    /// `--readiness-hook`.
    #[arg(
        long = "readiness-hook-arg",
        value_name = "ARG",
        allow_hyphen_values = true
    )]
    pub readiness_hook_args: Vec<String>,
    /// A registry and namespace an image may be pulled from, `HOST[:PORT]/PREFIX` such as
    /// `ghcr.io/komira-ai/`; repeatable. The prefix is whole path components, and a bare
    /// host (every account on it) is refused at start. An image on no entry is never
    /// pulled. The runtime that pulls with js-oci reads the list; the Docker and Podman
    /// runtimes do not. A Mac agent with `--bundle-store` reads it too: a start named by its
    /// image (with a `run`) is a bundle, and its image must be on an entry or it is refused
    /// `image-not-allowed`.
    #[arg(long, value_name = "HOST/PREFIX")]
    pub image_allow: Vec<String>,
    /// The credentials an allow-list entry sends to its registry's token realm, for a
    /// private package: `ENTRY=PATH`, where `ENTRY` is an `--image-allow`,
    /// `--bundle-registry-allow` or `--bundle-registry-allow-private` entry as given there
    /// and `PATH` a file holding one line `USER:PASSWORD`; repeatable, once per entry. The
    /// file must be a regular file (not a link) owned by root with no permission for group
    /// or others, at most 4096 bytes, in a root-owned directory that group and others
    /// cannot write; it is read at start, and a file or entry that breaks a rule stops the
    /// agent. The credentials go only to the token realm on the registry's own host, over
    /// https; never to manifest or blob requests, redirects or other hosts.
    #[arg(long = "registry-credentials", value_name = "HOST[:PORT]/PREFIX=PATH")]
    pub registry_credentials: Vec<String>,
    /// How many seconds a container gets to stop on its own before it is killed, whenever
    /// the agent stops one: a `Stop` or `Cancel` from the server, a job past its wall-time
    /// limit, the end of a drain. By default 10; at most 3600. Not with `--no-jobs`.
    #[arg(long, value_name = "SECONDS")]
    pub stop_grace: Option<u64>,
    /// How many seconds SIGTERM and SIGINT let the running jobs finish before the agent
    /// stops them. The agent announces that it drains at once, takes no new job, and sends
    /// the results of the jobs as they end. By default the stop grace; at most 3600. Not with
    /// `--no-jobs`.
    #[arg(long, value_name = "SECONDS")]
    pub drain_wait: Option<u64>,
    /// How many seconds the agent goes on running its jobs when it hears nothing from the
    /// server (no welcome and no accepted heartbeat reply): then it stops every job it
    /// holds, as a stop from the server would (the stop grace, then the kill), and reports
    /// each `killed` with the detail `silence-stop`. By default 300; from 1 to 7200; it must
    /// be above the stop grace plus the server's default grace of 15 seconds, and a welcome
    /// whose grace plus the stop grace is not below it ends the agent. There is no value
    /// that turns the stop off. Not with `--no-jobs`.
    #[arg(long, value_name = "SECONDS")]
    pub silence_stop: Option<u64>,
    /// The bundle store: the directory a Mac keeps unpacked toolchain bundles in (`--bundle-*`
    /// flags need it). Absolute; on the same APFS volume as `--scratch-root` (cloning needs
    /// one volume) mounted with ownership honoured and not `noexec`, and the kernel's
    /// securelevel must be 0 or less; the agent refuses to start otherwise, and on a host
    /// that is not a Mac. Needs `--scratch-root` and `--bundle-budget-gib`. The disk the agent
    /// offers (`disk-gib`) is reduced by the part of the budget the store has not used yet.
    /// The hello reports `bundle-root` (this path) and `bundle` (the manifest digests the
    /// store holds, that the deny and allow lists let through) only when the agent can run a
    /// bundle: it runs native jobs (`--native-helper-socket`) and the store opened; without
    /// the helper it reports neither. A job that pins a bundle runs `@bundle/<entrypoint>`
    /// from the store's trees, under the same profile as any native job. Until a server says it
    /// takes a longer value, `bundle` holds one digest. Roll out servers first, then agents,
    /// then this flag, then placement by `bundle`.
    #[arg(long, value_name = "PATH")]
    pub bundle_store: Option<PathBuf>,
    /// The most GiB the store's trees (and the trees waiting to be deleted) may take
    /// together. Required with `--bundle-store`; above 0.
    #[arg(long, value_name = "GIB")]
    pub bundle_budget_gib: Option<u64>,
    /// New fetches are refused (`bundle-no-space`) while the store volume has less than this
    /// many GiB free. By default 0. Needs `--bundle-store`.
    #[arg(long, value_name = "GIB")]
    pub bundle_low_water_gib: Option<u64>,
    /// Whether a bundle that is not in the store is fetched: `never` (the default: the
    /// operator, or the updater, puts it there first, and a job that names another is
    /// refused `bundle-not-held`) or `missing` (only with `--bundle-trust list`, and at
    /// least one `--image-allow`, `--bundle-registry-allow` or
    /// `--bundle-registry-allow-private` entry): the first start that names one, by a
    /// `bundle` pin or by its image, begins a fetch in the background (`bundle-pending` until the bundle is held; every byte is checked against
    /// its digest, and a fetch that fails is reported once as `bundle-fetch-failed`). Needs
    /// `--bundle-store`.
    #[arg(long, value_name = "never|missing")]
    pub bundle_pull: Option<String>,
    /// A registry and namespace a bundle may be fetched from, `HOST[:PORT]/PREFIX`
    /// (`ghcr.io/example/`); not a bare host. A job's `bundle` pin (the legacy form; an
    /// image reference is held to `--image-allow` instead) must name one when any is given.
    /// May be given more than once. Needs `--bundle-store`.
    #[arg(long = "bundle-registry-allow", value_name = "HOST[:PORT]/PREFIX")]
    pub bundle_registry_allow: Vec<String>,
    /// Like `--bundle-registry-allow`, for a registry on a private address, and for that
    /// registry only. May be given more than once. Needs `--bundle-store`.
    #[arg(
        long = "bundle-registry-allow-private",
        value_name = "HOST[:PORT]/PREFIX"
    )]
    pub bundle_registry_allow_private: Vec<String>,
    /// Minutes a bundle's trees are kept after their last use before they may be evicted.
    /// By default 30. Needs `--bundle-store`.
    #[arg(long, value_name = "MINUTES")]
    pub bundle_min_age_min: Option<u64>,
    /// A root-owned file of manifest digests (`sha256:<hex>`, one a line, `#` comments) the
    /// store never evicts. Needs `--bundle-store`.
    #[arg(long, value_name = "PATH")]
    pub bundle_keep_file: Option<PathBuf>,
    /// A file of manifest digests (same form) that are refused (`bundle-denied`) even when
    /// listed. Read at every start of a job, so a change takes effect at once. Needs
    /// `--bundle-store`.
    #[arg(long, value_name = "PATH")]
    pub bundle_deny_file: Option<PathBuf>,
    /// A file of manifest digests (same form) a fetcher puts in the store ahead of the jobs
    /// that name them. Checked at start; this build has no fetcher. Needs `--bundle-store`.
    #[arg(long, value_name = "PATH")]
    pub bundle_prefetch_file: Option<PathBuf>,
    /// Which digests may run: `list` (the default: only those in `--bundle-allow-file`) or
    /// `digest` (any a job names; for developer hosts only, and never with
    /// `--bundle-pull missing`). Needs `--bundle-store`.
    #[arg(long, value_name = "list|digest")]
    pub bundle_trust: Option<String>,
    /// The allow list for `--bundle-trust list`: manifest digests (same form), one a line.
    /// Required with `list` and must hold a digest; refused with `digest`. Read at every
    /// start of a job, so a change takes effect at once. A job whose digest is not in it is
    /// refused `bundle-not-allowed`.
    #[arg(long, value_name = "PATH")]
    pub bundle_allow_file: Option<PathBuf>,
    /// The read model of a run's profile: `open` (the default) or `allow`. `allow` is not
    /// available yet: the agent refuses it at start.
    #[arg(long, value_name = "open|allow")]
    pub read_model: Option<String>,
}

/// The flags that were removed, without their leading `--`, each with the refusal that names
/// what to give instead. A command line that gives one is refused by [`removed_flag`] before
/// clap reads it, so the operator is told the replacement and not only that the flag is
/// unknown.
static REMOVED: [(&str, &str); 4] = [
    (
        "container-network",
        "--container-network: removed. A job run through the container engine now always has \
         network none. For networking run crun (the Linux build's runtime) with --egress, and \
         ask for a network per job in the job's network field",
    ),
    (
        "container-egress-allow",
        "--container-egress-allow: removed. Use --egress allow-list with --egress-allow on the \
         crun runtime, or the job's network field",
    ),
    (
        "container-nft",
        "--container-nft: removed together with --container-network. The crun runtime's nft \
         program is --egress-nft",
    ),
    (
        "runtime",
        "--runtime: removed. The runtime is the build's: a Linux build runs crun (or, with \
         --allow-container-engine and --container-bin, the container engine), a macOS build the \
         native runtime of --native-helper-socket; --no-jobs starts an agent that runs no job",
    ),
];

/// The refusal of the first removed flag (see [`REMOVED`]) that `args` gives, the program
/// name first, as `--flag value` or `--flag=value`; none when it gives none. The words are
/// read the way clap reads them, so a removed name that is the value of another flag is not
/// taken for the flag: a flag of [`Cli`] that takes a value takes the next word, unless that
/// word starts with `-` and the flag does not take values that start with one, in which case
/// clap reads the word as a flag too. Nothing after a bare `--` is a flag. The words are read
/// as bytes, so a value that is not UTF-8 (attached with `=` or not) never hides the flag
/// name before it. A flag that only the other platform's build has (see [`crate::platform`])
/// is refused the same way, with what this build runs instead.
pub fn removed_flag(args: &[OsString]) -> Option<String> {
    let mut command = Cli::command();
    command.build();
    let mut words = args
        .iter()
        .skip(1)
        .map(|word| word.as_encoded_bytes())
        .peekable();
    while let Some(word) = words.next() {
        let Some(long) = word.strip_prefix(b"--") else {
            continue;
        };
        if long.is_empty() {
            return None;
        }
        let (name, attached) = match long.iter().position(|&byte| byte == b'=') {
            Some(at) => (&long[..at], true),
            None => (long, false),
        };
        let removed = REMOVED.iter().find(|(flag, _)| flag.as_bytes() == name);
        if let Some(&(_, refusal)) = removed {
            return Some(refusal.to_owned());
        }
        let foreign = OTHER_PLATFORM.iter().find(|flag| flag.as_bytes() == name);
        if let Some(flag) = foreign {
            return Some(crate::platform::foreign_flag(flag));
        }
        let takes = command
            .get_arguments()
            .find(|arg| arg.get_long().map(str::as_bytes) == Some(name))
            .filter(|arg| !attached && arg.get_action().takes_values());
        if let Some(arg) = takes {
            let hyphen = arg.is_allow_hyphen_values_set();
            let _value = words.next_if(|next| hyphen || !next.starts_with(b"-"));
        }
    }
    None
}

/// `name=path` split at the first `=`.
fn named_path(text: &str) -> Result<(String, PathBuf), String> {
    key_value(text).map(|(name, path)| (name, PathBuf::from(path)))
}

/// `key=value` split at the first `=`.
fn key_value(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("{text:?} is not key=value"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use super::{REMOVED, removed_flag};

    /// The command line `js-agent` then `words`.
    fn args(words: Vec<OsString>) -> Vec<OsString> {
        std::iter::once(OsString::from("js-agent"))
            .chain(words)
            .collect()
    }

    fn of(words: &[&str]) -> Vec<OsString> {
        args(words.iter().map(OsString::from).collect())
    }

    /// The refusal of the removed flag `name`.
    fn refusal(name: &str) -> Option<&'static str> {
        REMOVED
            .iter()
            .find(|(removed, _)| *removed == name)
            .map(|&(_, refusal)| refusal)
    }

    /// Catches a scan that misreads a word: a removed flag is found in both spellings, with no
    /// value and with an empty attached one, after a word that is not a flag, a word that is
    /// not UTF-8, a flag that takes no value, a flag clap does not know and the value of a
    /// flag; a name that only starts like a removed one is not it; a removed name is a value
    /// after a flag that takes values starting with `-` (and the scan goes on after that
    /// value, a `--` included), attached with `=` to another flag, and after a bare `--`, but
    /// is the flag after one whose value cannot start with `-`; a value that is not UTF-8,
    /// separate or attached, neither hides the flag before it nor ends the scan; and the
    /// first removed flag given is the one named.
    #[test]
    fn the_scan_reads_the_words_as_clap_does() {
        let network = refusal("container-network");
        let allow = refusal("container-egress-allow");
        let nft = refusal("container-nft");
        assert!(network.is_some(), "container-network");
        assert!(allow.is_some(), "container-egress-allow");
        assert!(nft.is_some(), "container-nft");
        let found: [(&[&str], Option<&str>); 22] = [
            (&[], None),
            (&["--container-network", "jobs"], network),
            (&["--container-network=jobs"], network),
            (&["--container-network"], network),
            (&["--container-network="], network),
            (&["--container-nft=/usr/sbin/nft"], nft),
            (&["--container-network-extra", "x"], None),
            (&["--container-nftx=/x"], None),
            (&["--container-egress-allow=192.0.2.1:443"], allow),
            (&["stray", "--container-nft", "/usr/sbin/nft"], nft),
            (&["--scratch-quota", "--container-nft", "/x"], nft),
            (&["--no-such-flag", "--container-network", "x"], network),
            (&["--label", "k=v", "--container-network", "x"], network),
            (&["--label", "--container-network", "x"], network),
            (&["--label"], None),
            (&["--readiness-hook-arg", "--container-network"], None),
            (
                &[
                    "--readiness-hook-arg",
                    "--container-network",
                    "--container-nft=/usr/sbin/nft",
                ],
                nft,
            ),
            (
                &["--readiness-hook-arg", "--", "--container-network", "x"],
                network,
            ),
            (
                &["--readiness-hook-arg", "x", "--", "--container-network"],
                None,
            ),
            (&["--label=k=--container-network"], None),
            (&["--", "--container-network", "x"], None),
            (&["--container-nft", "/x", "--container-network", "x"], nft),
        ];
        for (words, want) in found {
            assert_eq!(removed_flag(&of(words)).as_deref(), want, "{words:?}");
        }
        let bad = || OsString::from_vec(b"\xff".to_vec());
        let flag = |word: &str| OsString::from(word);
        let unreadable = args(vec![bad(), flag("--container-network"), flag("x")]);
        assert_eq!(removed_flag(&unreadable).as_deref(), network);
        let value = args(vec![flag("--label"), bad(), flag("--container-nft")]);
        assert_eq!(removed_flag(&value).as_deref(), nft);
        let attached = |name: &str| {
            let mut word = format!("--{name}=").into_bytes();
            word.push(0xff);
            OsString::from_vec(word)
        };
        let nft_first = args(vec![
            attached("container-nft"),
            flag("--container-network=jobs"),
        ]);
        assert_eq!(removed_flag(&nft_first).as_deref(), nft);
        let label_first = args(vec![attached("label"), flag("--container-egress-allow=x")]);
        assert_eq!(removed_flag(&label_first).as_deref(), allow);
    }

    /// Asserts that `--runtime` is refused by name, with every value it took and in both
    /// spellings, with the text that says the runtime is the build's and names `--no-jobs`;
    /// and that a word that only starts like it, or is the value of another flag, is not. Catches a config package or flags file
    /// that still picks a runtime being run with the build's own, or refused only as unknown.
    #[test]
    fn the_runtime_flag_is_refused_by_name() {
        let removed = refusal("runtime").expect("--runtime is removed");
        assert!(removed.starts_with("--runtime: removed."), "{removed}");
        assert!(removed.contains("--no-jobs"), "{removed}");
        for value in ["crun", "docker", "none", "native"] {
            let spaced = removed_flag(&of(&["--runtime", value]));
            assert_eq!(spaced.as_deref(), Some(removed), "{value}");
            let attached = removed_flag(&of(&[&format!("--runtime={value}")]));
            assert_eq!(attached.as_deref(), Some(removed), "{value}");
        }
        assert_eq!(removed_flag(&of(&["--runtimes", "crun"])), None);
        assert_eq!(removed_flag(&of(&["--label=--runtime"])), None);
        assert_eq!(
            removed_flag(&of(&["--readiness-hook-arg", "--runtime"])),
            None
        );
    }

    /// Asserts that every flag only the other platform's build has (its runtime's) is refused
    /// by name with this build's refusal, in both spellings, and that clap does not know it
    /// here. Catches a flag of the other platform that this build would silently accept, and a
    /// flags file written for the other platform that fails only as an unknown flag.
    #[test]
    fn the_other_platforms_flags_are_refused_by_name() {
        use clap::CommandFactory as _;
        let command = super::Cli::command();
        for name in super::OTHER_PLATFORM {
            let known = command
                .get_arguments()
                .any(|arg| arg.get_long() == Some(name));
            assert!(!known, "--{name} is a flag of this build");
            let refused = crate::platform::foreign_flag(name);
            assert!(refused.starts_with(&format!("--{name}: ")), "{refused}");
            let spaced = removed_flag(&of(&[&format!("--{name}"), "x"]));
            assert_eq!(spaced.as_deref(), Some(refused.as_str()), "{name}");
            let attached = removed_flag(&of(&[&format!("--{name}=x")]));
            assert_eq!(attached.as_deref(), Some(refused.as_str()), "{name}");
        }
    }

    /// Asserts that this build's runtime flags ([`crate::platform::Flags`]) are exactly the
    /// names its platform list holds (which the other build refuses by name), and that the
    /// set an agent with `--no-jobs` refuses ([`crate::platform::Flags::given`]) names every one
    /// of them once. Catches a runtime flag added to one side and not the others: one the other
    /// build would call unknown, or one `--no-jobs` would let pass.
    #[test]
    fn the_platform_lists_name_every_runtime_flag_of_the_build() {
        use std::collections::BTreeSet;

        use clap::{Args as _, Parser as _};

        let command = crate::platform::Flags::augment_args(clap::Command::new("flags"));
        let known: BTreeSet<&str> = command
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .collect();
        #[cfg(target_os = "linux")]
        let own = crate::platform::LINUX_ONLY;
        #[cfg(target_os = "macos")]
        let own = crate::platform::MACOS_ONLY;
        let listed: BTreeSet<&str> = own.into_iter().collect();
        assert_eq!(listed.len(), own.len(), "a name is listed twice");
        assert_eq!(known, listed);
        let cli = super::Cli::try_parse_from(["js-agent", "--server", "s", "--pool", "p"]);
        let given = cli.expect("a command line").os.given();
        let named: BTreeSet<&str> = given
            .iter()
            .map(|(flag, given)| {
                assert!(!given, "{flag} is given by default");
                flag.strip_prefix("--").expect("a long flag")
            })
            .collect();
        assert_eq!(named.len(), given.len(), "a flag is named twice");
        assert_eq!(named, listed);
    }

    /// Asserts, on the Linux build, that the macOS build's runtime flags (the fields of its
    /// `Flags`, read from `platform/macos.rs`, each by its `long` name or its field name in
    /// kebab case) are exactly the names [`crate::platform::MACOS_ONLY`] holds, which this build
    /// refuses by name. Catches a flag added to the macOS build and not to the list: a Linux
    /// build would call it unknown instead of naming the platform that has it, and no Linux pull
    /// request would see it.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_macos_list_names_every_field_of_the_macos_flags() {
        use std::collections::BTreeSet;

        let source = include_str!("platform/macos.rs");
        let start = source.find("pub struct Flags {").expect("the macOS flags");
        let end = start + source[start..].find("\n}\n").expect("their end");
        let mut long = None;
        let mut fields = BTreeSet::new();
        for line in source[start..end].lines().skip(1).map(str::trim) {
            if let Some(rest) = line.strip_prefix("#[arg(long = \"") {
                long = rest.split('"').next().map(str::to_owned);
            } else if let Some(rest) = line.strip_prefix("pub ") {
                let field = rest.split(':').next().expect("a field");
                let name = long.take().unwrap_or_else(|| field.replace('_', "-"));
                fields.insert(name);
            }
        }
        let listed: BTreeSet<String> = crate::platform::MACOS_ONLY
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(fields, listed);
    }
}
