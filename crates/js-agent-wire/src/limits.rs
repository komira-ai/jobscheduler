//! The caps every untrusted field is held to.
//!
//! A message crosses a trust boundary: an agent is a semi-trusted machine, and a server
//! may be older or newer than the agent it talks to. Each cap is a named constant so a
//! caller (and the tests) can refer to it, and each is generous for real use: a value
//! at the cap is accepted, one above it is refused. No transport exists in this crate,
//! so these caps are the only bound on what a decoded message can hold; a transport
//! still bounds the body it reads before it decodes.
//!
//! | field | cap |
//! |---|---|
//! | ids (`run_id`, `lease_id`, `host_id`, `pool`, `instance_id`, taint `run_id`) | [`MAX_ID_LEN`] bytes of `[A-Za-z0-9._-]`, leading alphanumeric |
//! | `session` | [`MAX_SESSION_LEN`] bytes of `[A-Za-z0-9._-]`, leading alphanumeric |
//! | `image` | at most [`MAX_IMAGE_LEN`] bytes in all (not docker's 255-character name limit) of the `name[:tag][@digest]` reference grammar (see `JobSpec::image`), no transport prefix |
//! | `args` | [`MAX_ARGS`] entries of [`MAX_ARG_LEN`] bytes, [`MAX_ARGS_TOTAL_BYTES`] in all, no NUL |
//! | mounts | [`MAX_MOUNTS`] entries; root name [`MAX_ROOT_NAME_LEN`] bytes; `rel` [`MAX_REL_LEN`] bytes |
//! | labels | [`MAX_LABELS`] entries; key [`MAX_LABEL_KEY_LEN`] bytes in all, its name at most [`MAX_LABEL_NAME_LEN`] and each dot-separated label of its prefix at most [`MAX_DNS_LABEL_LEN`]; value [`MAX_LABEL_VALUE_LEN`] bytes |
//! | capabilities | [`MAX_CAPABILITIES`] entries; key [`MAX_CAPABILITY_KEY_LEN`]; value [`MAX_CAPABILITY_VALUE_LEN`] |
//! | free text (`agent_version`, taint, `detail`) | [`MAX_VERSION_LEN`], [`MAX_TAINT_TEXT_LEN`], [`MAX_DETAIL_LEN`] bytes, no control or format character |
//! | exclusive refusal `detail` | [`MAX_REFUSAL_DETAIL`] bytes, no control or format character |
//! | GPUs | [`MAX_GPUS`] devices or indices; uuid and model [`MAX_GPU_TEXT_LEN`] bytes |
//! | lists in a heartbeat or reply | [`MAX_HELD`] held runs, [`MAX_RESULTS`] results or acks, [`MAX_COMMANDS`] commands |
//! | node load in a heartbeat | [`MAX_LOAD_LEASES`] ids in each of `covered` and `clocks`, [`MAX_LOAD_MINUTES`] minute records, minute index 0 to [`TAIL_MINUTE`], lease ids [`MAX_LEASE_REF_LEN`] bytes |
//! | shape and limits | [`MAX_CPU_MILLIS`], [`MAX_MEMORY_BYTES`], [`MAX_PIDS`], [`MAX_WALL_MS`], [`MAX_LEASE_CAP`] |
//! | session timing | [`MIN_HEARTBEAT_INTERVAL_MS`], [`MAX_HEARTBEAT_INTERVAL_MS`], [`MAX_GRACE_MS`] |

/// Longest id, in bytes: run, lease, host, pool, instance and provider ids.
pub const MAX_ID_LEN: usize = 128;
/// Longest container image reference, in bytes: the whole `name[:tag][@digest]` string,
/// not the 255-character limit docker puts on the name alone.
pub const MAX_IMAGE_LEN: usize = 512;
/// Most arguments after the image.
pub const MAX_ARGS: usize = 1024;
/// Longest single argument, in bytes.
pub const MAX_ARG_LEN: usize = 16 * 1024;
/// Most bytes all arguments of one job may hold together.
pub const MAX_ARGS_TOTAL_BYTES: usize = 256 * 1024;
/// Most mounts on one job.
pub const MAX_MOUNTS: usize = 64;
/// Longest mount root name, in bytes.
pub const MAX_ROOT_NAME_LEN: usize = 64;
/// Longest path below a mount root, in bytes.
pub const MAX_REL_LEN: usize = 4096;
/// Most labels on a hello or a job.
pub const MAX_LABELS: usize = 64;
/// Longest label key, in bytes.
pub const MAX_LABEL_KEY_LEN: usize = 128;
/// Longest label name (the part after any `prefix/`), in bytes; the same 63 as a label value.
pub const MAX_LABEL_NAME_LEN: usize = 63;
/// Longest single dot-separated DNS label of a label-key prefix, in bytes (RFC 1035).
pub const MAX_DNS_LABEL_LEN: usize = 63;
/// Longest label value, in bytes; this is the limit `js-types` puts on a label value.
pub const MAX_LABEL_VALUE_LEN: usize = 63;
/// Most capabilities a hello may report.
pub const MAX_CAPABILITIES: usize = 64;
/// Longest capability key, in bytes.
pub const MAX_CAPABILITY_KEY_LEN: usize = 64;
/// Longest capability value, in bytes.
pub const MAX_CAPABILITY_VALUE_LEN: usize = 128;
/// Longest session secret, in bytes (see [`crate::Welcome::session`]). A secret the
/// registry issues is far shorter; the cap only bounds what a peer can send.
pub const MAX_SESSION_LEN: usize = 128;
/// Longest `agent_version` text, in bytes.
pub const MAX_VERSION_LEN: usize = 64;
/// Longest taint reason or check text, in bytes.
pub const MAX_TAINT_TEXT_LEN: usize = 256;
/// Longest result `detail` text, in bytes.
pub const MAX_DETAIL_LEN: usize = 1024;
/// Longest `detail` of an exclusive refusal, in bytes; the scheduler never parses it.
pub const MAX_REFUSAL_DETAIL: usize = 256;
/// Longest refusal code, in bytes.
pub const MAX_CODE_LEN: usize = 64;
/// Most GPUs one host may report, and most device indices one job may name.
pub const MAX_GPUS: u64 = 256;
/// Longest GPU uuid or model text, in bytes.
pub const MAX_GPU_TEXT_LEN: usize = 128;
/// Most runs a hello or heartbeat may list as held.
pub const MAX_HELD: usize = 4096;
/// Most results in a heartbeat, and most acknowledgements in a reply.
pub const MAX_RESULTS: usize = 4096;
/// Most commands in one reply.
pub const MAX_COMMANDS: usize = 1024;
/// Most thousandths of a core a host or job may name (about 4 million cores). With
/// [`MAX_MEMORY_BYTES`] it keeps the sum over 2^18 hosts below `u64::MAX`.
pub const MAX_CPU_MILLIS: u64 = 1 << 32;
/// Most bytes of memory a host or job may name (32 TiB); see [`MAX_CPU_MILLIS`].
pub const MAX_MEMORY_BYTES: u64 = 1 << 45;
/// Most processes a job may be limited to (the Linux default `pid_max` ceiling).
pub const MAX_PIDS: u64 = 4_194_304;
/// Longest wall-time limit of one job, in milliseconds (seven days).
pub const MAX_WALL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Largest `valid_for_ms` of a start, in milliseconds (seven days).
pub const MAX_VALID_FOR_MS: u64 = MAX_WALL_MS;
/// Largest lease-count cap a host may announce.
pub const MAX_LEASE_CAP: u64 = 4096;
/// Shortest heartbeat interval a welcome may set, in milliseconds; a smaller one would
/// make the agent heartbeat in a busy loop.
pub const MIN_HEARTBEAT_INTERVAL_MS: u64 = 250;
/// Longest heartbeat interval a welcome may set, in milliseconds (one hour).
pub const MAX_HEARTBEAT_INTERVAL_MS: u64 = 60 * 60 * 1000;
/// Longest grace a welcome may set, in milliseconds (one day).
pub const MAX_GRACE_MS: u64 = 24 * 60 * 60 * 1000;

/// Most lease ids in a load's `covered` list, and most records in its `clocks` list. A
/// node of 88 threads has 88 leases at most; an agent with more sends the 96 oldest.
pub const MAX_LOAD_LEASES: usize = 96;
/// Most minute records in one load. A full node closes about 96 minutes a minute and
/// beats 12 times a minute, so this is 12 times the need; a backlog drains over beats.
pub const MAX_LOAD_MINUTES: usize = 64;
/// The last minute index a load names. It means "this minute or later": the agent folds
/// every later minute into it with the larger value of each field, and a receiver merges
/// repeats of it the same way. It is one less than the 64 points of a curve.
pub const TAIL_MINUTE: u16 = 63;
/// Longest lease id in a load, in bytes: `u64::MAX-u64::MAX`. A load's lease id is held to
/// this and to the `term-seq` form, not to [`MAX_ID_LEN`].
pub const MAX_LEASE_REF_LEN: usize = 41;
/// The body a server reads for a heartbeat, in bytes; the server's own limit must equal
/// it. A load at every cap here is about 25 KB, and beside 96 held runs and 8 results at
/// the longest ids a beat stays under three quarters of it; a test fixes those totals.
/// This is not a guarantee for every valid beat: `MAX_HELD` and `MAX_RESULTS` allow far
/// more, and a beat over the server's limit is refused there.
pub const HEARTBEAT_BODY_BUDGET: usize = 65_536;
