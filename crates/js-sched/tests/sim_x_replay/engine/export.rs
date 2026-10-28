//! The input side of the x2 runner: a directory of exported per-minute series, read into
//! the replay's types.
//!
//! # The directory
//!
//! * `key_profile.txt` (required): one line per minute of every run that ended on its own.
//! * `node_minute.txt` (optional): one line per minute of every node.
//!
//! Both are line-oriented text: fields separated by one space, no tabs. Blank lines and
//! lines that start with `#` are skipped (they still count in line numbers). Hosts are
//! opaque numbers and times are offsets, so the files name no machine and no calendar day.
//!
//! ```text
//! kp <key> <platform> <start_ms> <wall_ms> <minute> <cores_milli> <ws_bytes>
//! nm <node> <minute> <mean_cores_milli> <ws_max_bytes> <leases>
//! ```
//!
//! * `key`: 16 lowercase hex digits, the digest of the work key.
//! * `platform`: an opaque token of `a-z`, `0-9`, `_` and `-`, at most 32 bytes.
//! * `start_ms`: when the run started, in ms from the export's own zero; `-` when the
//!   export does not carry starts. All runs of a file agree on which.
//! * `wall_ms`: wall time of the run, at least 1.
//! * `minute`: the minute since the run's start. A run is a block of consecutive lines
//!   that repeat key, platform, start and wall and count minutes `0, 1, 2, ...`; its
//!   block has exactly `wall_ms / 60,000 + 1` lines.
//! * `cores_milli`, `ws_bytes`: the minute's mean cores and largest working set.
//! * `node`: an opaque number. `minute` there is the minute since the export's zero, and
//!   strictly increases within a node.
//!
//! Only runs that ended on their own are exported (a killed or truncated run is not in a
//! key's ring), so every run read here is `Outcome::Ok`.
//!
//! # What it holds
//!
//! [`KeyProfile`] and the node rows keep what the files say, in file order, and
//! [`render_key_profile`] writes the same text back. [`Export`] turns them into the
//! replay's types: [`Export::rows`] (one [`Row`] per run, in start order), and
//! [`Export::beliefs`], the [`Predictor`] built from each key's earlier runs only.
//!
//! # Arrivals
//!
//! With `start_ms` the starts are the recorded ones ([`Arrivals::Recorded`]). Without,
//! the runs are laid out one after another in file order, a gap longer than the longest
//! wall apart ([`Arrivals::Invented`]), so each run's earlier runs are the runs above it
//! in the file. [`Export::header`] says which, and which other inputs are real.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::fixture::{KeyIx, Outcome, Row};
use super::predictor::{Observed, Predictor};

const MINUTE_MS: u64 = 60_000;
const MIB: u64 = 1 << 20;
const PLATFORM_MAX: usize = 32;

/// The file of run-minutes in an export directory.
pub const KEY_PROFILE_FILE: &str = "key_profile.txt";

/// The file of node-minutes in an export directory.
pub const NODE_MINUTE_FILE: &str = "node_minute.txt";

/// What is wrong with one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineError {
    /// The first field is not the tag this file holds.
    UnknownTag,
    /// The line has the wrong number of fields.
    FieldCount {
        /// What the tag takes.
        want: usize,
        /// What the line has.
        got: usize,
    },
    /// The key is not 16 lowercase hex digits.
    BadKey,
    /// The platform is empty, too long or has a character outside `a-z0-9_-`.
    BadPlatform,
    /// A field that is not a plain decimal number of its type (or, for `start_ms`, `-`).
    BadNumber {
        /// The field.
        field: &'static str,
    },
    /// A wall time of 0.
    ZeroWall,
    /// Some runs carry a start and some do not.
    MixedStart,
    /// The minute is not the one that follows the run's previous line (0 for a new run).
    MinuteOrder {
        /// The minute the line should have.
        want: u32,
        /// The minute it has.
        got: u32,
    },
    /// A line of a run repeats key, platform, start or wall differently from the run's
    /// first line.
    RunChanged,
    /// The minute lies past the run's wall time.
    PastWall {
        /// The last minute the wall allows.
        last: u32,
    },
    /// The run ended (here is its last line) with fewer minutes than its wall time has.
    ShortRun {
        /// Minutes it has.
        have: u32,
        /// Minutes the wall time needs.
        want: u32,
    },
    /// More distinct keys than a `u16` index holds.
    TooManyKeys,
    /// A node's minute does not increase.
    NodeMinuteOrder {
        /// The node's previous minute.
        after: u32,
    },
}

/// A malformed line, with its 1-based number in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportError {
    /// The line number, counting skipped lines.
    pub line: usize,
    /// What is wrong with it.
    pub kind: LineError,
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.kind)
    }
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LineError::UnknownTag => write!(f, "unknown tag for this file"),
            LineError::FieldCount { want, got } => {
                write!(f, "wants {want} fields, has {got}")
            }
            LineError::BadKey => write!(f, "key is not 16 lowercase hex digits"),
            LineError::BadPlatform => write!(f, "platform is not 1-{PLATFORM_MAX} of a-z0-9_-"),
            LineError::BadNumber { field } => write!(f, "{field} is not a plain decimal number"),
            LineError::ZeroWall => write!(f, "wall_ms is 0"),
            LineError::MixedStart => write!(f, "some runs carry start_ms and some do not"),
            LineError::MinuteOrder { want, got } => {
                write!(f, "minute is {got}, expected {want}")
            }
            LineError::RunChanged => write!(f, "key, platform, start or wall differs within a run"),
            LineError::PastWall { last } => write!(f, "minute is past the wall (last is {last})"),
            LineError::ShortRun { have, want } => {
                write!(f, "run has {have} minutes, its wall needs {want}")
            }
            LineError::TooManyKeys => write!(f, "more distinct keys than a u16 holds"),
            LineError::NodeMinuteOrder { after } => {
                write!(f, "node minute does not increase past {after}")
            }
        }
    }
}

impl std::error::Error for ExportError {}

/// Why a directory is not an export.
#[derive(Debug)]
pub enum ReadError {
    /// A file could not be read.
    Io {
        /// The path.
        path: PathBuf,
        /// What the system said.
        source: std::io::Error,
    },
    /// A file has a malformed line.
    Parse {
        /// The file's name within the directory.
        file: &'static str,
        /// The line.
        error: ExportError,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            ReadError::Parse { file, error } => write!(f, "{file}: {error}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// One run: a block of `kp` lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// The run's key, an index into [`KeyProfile::keys`].
    pub key: KeyIx,
    /// The opaque platform token.
    pub platform: String,
    /// The recorded start, ms from the export's zero; `None` when the export has none.
    pub start_ms: Option<u64>,
    /// Wall time, ms, at least 1.
    pub wall_ms: u64,
    /// Mean cores of each minute, millicores.
    pub cores_milli: Vec<u32>,
    /// Largest working set of each minute, bytes.
    pub ws_bytes: Vec<u64>,
}

/// The `key_profile` file: runs in file order and the keys they number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyProfile {
    /// The key digests in order of first appearance; `KeyIx(i)` is `keys[i]`.
    pub keys: Vec<u64>,
    /// The runs.
    pub runs: Vec<Run>,
}

/// One minute of one node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeMinute {
    /// The node, an opaque number.
    pub node: u32,
    /// Minute since the export's zero.
    pub minute: u32,
    /// Mean cores over the minute, millicores.
    pub mean_cores_milli: u32,
    /// Largest working set in the minute, bytes.
    pub ws_max_bytes: u64,
    /// Leases the node held.
    pub leases: u32,
}

/// Where the arrivals of an export come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrivals {
    /// The export's own starts.
    Recorded,
    /// The export has no starts; runs are laid out serially in file order.
    Invented,
}

fn fields<'a>(line: &'a str, tag: &str, want: usize) -> Result<Vec<&'a str>, LineError> {
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.first() != Some(&tag) {
        return Err(LineError::UnknownTag);
    }
    if parts.len() != want {
        return Err(LineError::FieldCount {
            want,
            got: parts.len(),
        });
    }
    Ok(parts)
}

fn number<T: std::str::FromStr>(text: &str, field: &'static str) -> Result<T, LineError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(LineError::BadNumber { field });
    }
    text.parse().map_err(|_| LineError::BadNumber { field })
}

fn key_digest(text: &str) -> Result<u64, LineError> {
    let hex = text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !hex {
        return Err(LineError::BadKey);
    }
    u64::from_str_radix(text, 16).map_err(|_| LineError::BadKey)
}

fn platform_token(text: &str) -> Result<String, LineError> {
    let ok = !text.is_empty()
        && text.len() <= PLATFORM_MAX
        && text
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    if ok {
        Ok(text.to_string())
    } else {
        Err(LineError::BadPlatform)
    }
}

fn minutes_needed(wall_ms: u64) -> u64 {
    wall_ms / MINUTE_MS + 1
}

/// The run being read.
struct Open {
    run: Run,
    last_line: usize,
}

impl Open {
    fn close(self, runs: &mut Vec<Run>) -> Result<(), ExportError> {
        let have = u32::try_from(self.run.cores_milli.len()).unwrap_or(u32::MAX);
        let want = u32::try_from(minutes_needed(self.run.wall_ms)).unwrap_or(u32::MAX);
        if have < want {
            return Err(ExportError {
                line: self.last_line,
                kind: LineError::ShortRun { have, want },
            });
        }
        runs.push(self.run);
        Ok(())
    }
}

/// Reads the text of a `key_profile` file.
///
/// # Errors
/// The first malformed line, by number.
pub fn parse_key_profile(text: &str) -> Result<KeyProfile, ExportError> {
    let mut profile = KeyProfile::default();
    let mut index: BTreeMap<u64, KeyIx> = BTreeMap::new();
    let mut open: Option<Open> = None;
    let mut recorded: Option<bool> = None;
    for (n, line) in text.lines().enumerate() {
        let line_no = n + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |kind| ExportError {
            line: line_no,
            kind,
        };
        let f = fields(line, "kp", 8).map_err(at)?;
        let digest = key_digest(f[1]).map_err(at)?;
        let platform = platform_token(f[2]).map_err(at)?;
        let start_ms = if f[3] == "-" {
            None
        } else {
            Some(number::<u64>(f[3], "start_ms").map_err(at)?)
        };
        let wall_ms: u64 = number(f[4], "wall_ms").map_err(at)?;
        let minute: u32 = number(f[5], "minute").map_err(at)?;
        let cores: u32 = number(f[6], "cores_milli").map_err(at)?;
        let ws: u64 = number(f[7], "ws_bytes").map_err(at)?;
        if wall_ms == 0 {
            return Err(at(LineError::ZeroWall));
        }
        let same_run = open.as_ref().is_some_and(|o| {
            minute != 0
                && o.run.start_ms == start_ms
                && o.run.wall_ms == wall_ms
                && o.run.platform == platform
                && profile.keys.get(usize::from(o.run.key.0)) == Some(&digest)
        });
        if minute != 0 && open.is_some() && !same_run {
            return Err(at(LineError::RunChanged));
        }
        match (&mut open, same_run) {
            (Some(o), true) => {
                let want = u32::try_from(o.run.cores_milli.len()).unwrap_or(u32::MAX);
                if minute != want {
                    return Err(at(LineError::MinuteOrder { want, got: minute }));
                }
                let last = u32::try_from(minutes_needed(wall_ms) - 1).unwrap_or(u32::MAX);
                if minute > last {
                    return Err(at(LineError::PastWall { last }));
                }
                o.run.cores_milli.push(cores);
                o.run.ws_bytes.push(ws);
                o.last_line = line_no;
            }
            _ => {
                if minute != 0 {
                    return Err(at(LineError::MinuteOrder {
                        want: 0,
                        got: minute,
                    }));
                }
                if let Some(done) = open.take() {
                    done.close(&mut profile.runs)?;
                }
                if recorded.is_some_and(|r| r != start_ms.is_some()) {
                    return Err(at(LineError::MixedStart));
                }
                recorded = Some(start_ms.is_some());
                let key = match index.get(&digest) {
                    Some(k) => *k,
                    None => {
                        let k = u16::try_from(profile.keys.len())
                            .map_err(|_| at(LineError::TooManyKeys))?;
                        profile.keys.push(digest);
                        index.insert(digest, KeyIx(k));
                        KeyIx(k)
                    }
                };
                open = Some(Open {
                    run: Run {
                        key,
                        platform,
                        start_ms,
                        wall_ms,
                        cores_milli: vec![cores],
                        ws_bytes: vec![ws],
                    },
                    last_line: line_no,
                });
            }
        }
    }
    if let Some(done) = open.take() {
        done.close(&mut profile.runs)?;
    }
    Ok(profile)
}

/// Reads the text of a `node_minute` file.
///
/// # Errors
/// The first malformed line, by number.
pub fn parse_node_minutes(text: &str) -> Result<Vec<NodeMinute>, ExportError> {
    let mut rows: Vec<NodeMinute> = Vec::new();
    let mut last: BTreeMap<u32, u32> = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line_no = n + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |kind| ExportError {
            line: line_no,
            kind,
        };
        let f = fields(line, "nm", 6).map_err(at)?;
        let row = NodeMinute {
            node: number(f[1], "node").map_err(at)?,
            minute: number(f[2], "minute").map_err(at)?,
            mean_cores_milli: number(f[3], "mean_cores_milli").map_err(at)?,
            ws_max_bytes: number(f[4], "ws_max_bytes").map_err(at)?,
            leases: number(f[5], "leases").map_err(at)?,
        };
        if let Some(after) = last.insert(row.node, row.minute)
            && row.minute <= after
        {
            return Err(at(LineError::NodeMinuteOrder { after }));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// The text of `profile`: [`parse_key_profile`] reads it back to an equal value.
pub fn render_key_profile(profile: &KeyProfile) -> String {
    let mut out = String::new();
    for run in &profile.runs {
        let key = profile
            .keys
            .get(usize::from(run.key.0))
            .copied()
            .unwrap_or(0);
        let start = run
            .start_ms
            .map_or_else(|| "-".to_string(), |s| s.to_string());
        for (minute, (cores, ws)) in run.cores_milli.iter().zip(&run.ws_bytes).enumerate() {
            out.push_str(&format!(
                "kp {key:016x} {} {start} {} {minute} {cores} {ws}\n",
                run.platform, run.wall_ms
            ));
        }
    }
    out
}

/// The text of node rows: [`parse_node_minutes`] reads it back to an equal value.
pub fn render_node_minutes(rows: &[NodeMinute]) -> String {
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!(
            "nm {} {} {} {} {}\n",
            r.node, r.minute, r.mean_cores_milli, r.ws_max_bytes, r.leases
        ));
    }
    out
}

/// An export read whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Export {
    /// The runs and their keys.
    pub profile: KeyProfile,
    /// The node rows (none when the directory has no `node_minute.txt`).
    pub nodes: Vec<NodeMinute>,
}

/// Reads the export directory `dir`.
///
/// # Errors
/// A file that cannot be read (a missing `node_minute.txt` is not one) or a malformed
/// line, with its file and number.
pub fn read_dir(dir: &Path) -> Result<Export, ReadError> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name));
    let io = |name: &str, source| ReadError::Io {
        path: dir.join(name),
        source,
    };
    let profile = read(KEY_PROFILE_FILE).map_err(|e| io(KEY_PROFILE_FILE, e))?;
    let profile = parse_key_profile(&profile).map_err(|error| ReadError::Parse {
        file: KEY_PROFILE_FILE,
        error,
    })?;
    let nodes = match read(NODE_MINUTE_FILE) {
        Ok(text) => parse_node_minutes(&text).map_err(|error| ReadError::Parse {
            file: NODE_MINUTE_FILE,
            error,
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(io(NODE_MINUTE_FILE, e)),
    };
    Ok(Export { profile, nodes })
}

/// A run's mean cores times its time in each minute, in ms of CPU.
fn cpu_ms(run: &Run) -> u64 {
    let mut total: u128 = 0;
    for (t, cores) in run.cores_milli.iter().enumerate() {
        let from = (t as u64).saturating_mul(MINUTE_MS);
        let span = run.wall_ms.saturating_sub(from).min(MINUTE_MS);
        total += u128::from(*cores) * u128::from(span) / 1_000;
    }
    u64::try_from(total).unwrap_or(u64::MAX)
}

impl Export {
    /// Whether the starts are the export's or invented.
    pub fn arrivals(&self) -> Arrivals {
        if self.profile.runs.iter().all(|r| r.start_ms.is_some()) {
            Arrivals::Recorded
        } else {
            Arrivals::Invented
        }
    }

    /// The start of each run (file order) on the replay's clock.
    fn starts(&self) -> Vec<u64> {
        let gap = self
            .profile
            .runs
            .iter()
            .map(|r| r.wall_ms)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.profile
            .runs
            .iter()
            .enumerate()
            .map(|(i, r)| r.start_ms.unwrap_or((i as u64).saturating_mul(gap)))
            .collect()
    }

    /// The runs in start order (ties keep file order); lease `i` is `order()[i]`.
    pub fn order(&self) -> Vec<usize> {
        let starts = self.starts();
        let mut order: Vec<usize> = (0..starts.len()).collect();
        order.sort_by_key(|&i| (starts[i], i));
        order
    }

    /// The run behind lease `lease`.
    ///
    /// # Panics
    /// If `lease` is not a run of the export.
    pub fn run(&self, lease: usize) -> &Run {
        &self.profile.runs[self.order()[lease]]
    }

    /// One row per run in start order. Its CPU time is the series' (each minute's mean
    /// cores over the part of the minute the run was alive) and its peak memory the
    /// largest working set, rounded up to whole MiB. Every row is `Outcome::Ok`.
    pub fn rows(&self) -> Vec<Row> {
        let starts = self.starts();
        self.order()
            .into_iter()
            .map(|i| {
                let run = &self.profile.runs[i];
                Row {
                    start_ms: starts[i],
                    key: run.key,
                    wall_ms: run.wall_ms,
                    cpu_ms: cpu_ms(run),
                    peak_mem_mib: run.ws_bytes.iter().max().map_or(0, |b| b.div_ceil(MIB)),
                    outcome: Outcome::Ok,
                }
            })
            .collect()
    }

    /// The belief of each lease (see [`Export::rows`]), built from the earlier runs of the
    /// lease's key only: the learner reads a run once it has ended before the lease
    /// starts, so a key is cold until three such runs exist.
    pub fn beliefs(&self) -> Predictor {
        let rows = self.rows();
        let order = self.order();
        Predictor::build(&rows, |lease, _| {
            let run = &self.profile.runs[order[lease]];
            let cores = run.cores_milli.iter().map(|c| u64::from(*c)).collect();
            let mem = run.ws_bytes.iter().map(|b| b.div_ceil(MIB)).collect();
            Observed::new(cores, mem).expect("a parsed run has minutes and equal series")
        })
    }

    /// The header the runner prints: which inputs are real and which are invented.
    pub fn header(&self, beliefs: &Predictor) -> String {
        let runs = self.profile.runs.len();
        let minutes: usize = self.profile.runs.iter().map(|r| r.cores_milli.len()).sum();
        let cold = (0..beliefs.len())
            .filter(|&i| beliefs.belief(i).is_cold())
            .count();
        let mut out = String::from("x2 inputs (report only; nothing here is asserted)\n");
        out.push_str(&format!(
            "real: per-run minute series (cores, working set): {runs} runs, {} keys, {minutes} run-minutes\n",
            self.profile.keys.len()
        ));
        if self.nodes.is_empty() {
            out.push_str("absent: per-node minute means (no node rows)\n");
        } else {
            let nodes = self
                .nodes
                .iter()
                .map(|n| n.node)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            out.push_str(&format!(
                "real: per-node minute means: {} node-minutes over {nodes} nodes\n",
                self.nodes.len()
            ));
        }
        match self.arrivals() {
            Arrivals::Recorded => out.push_str("real: arrivals (recorded starts)\n"),
            Arrivals::Invented => out.push_str(
                "invented: arrivals (the export carries no starts; runs follow one another in file order)\n",
            ),
        }
        out.push_str("invented: host pool and capacities\n");
        out.push_str("dropped: historical placement, kill metrics, within-minute bursts\n");
        out.push_str(&format!(
            "beliefs: from the key's earlier runs only; cold below {} runs ({cold} of {} leases cold)\n",
            super::predictor::MIN_RING,
            beliefs.len()
        ));
        out
    }
}
