//! `coverage-shard --plan <file> --shard <k> --of <n> -- <program> <args>...`: one coverage shard
//! of `.github/workflows/ci.yml`, the tests the checked-in plan (`.config/coverage-shards.toml`)
//! gives shard `k`, run by the given command.
//!
//! The plan names the longest tests with the seconds each took under coverage, and says how
//! many seconds all the others took together (`light-seconds`). [`assign`] deals out the named
//! tests and `slices` equal slices of the others to the `shards` shards, longest processing time
//! first: the items sorted by weight, heaviest first (a named test weighs its seconds, a slice
//! weighs `light-seconds / slices`; on a tie a named test goes first, named tests in order of
//! binary id and name, slices in order of number), each given to the shard with the least load so
//! far, the lowest shard number on a tie. The weights are compared exactly, as seconds times
//! `slices`.
//!
//! [`runs`] turns a shard's items into the arguments appended to the command, one run each:
//! first, when the shard has named tests, `-E '(binary_id(=B1) & test(=T1)) | ...'`; then, for
//! each slice `j` of the shard in order, `-E 'not (<every named test of the plan>)' --partition
//! slice:j/<slices>` (`-E all()` when the plan names no test). So a named test runs in the one
//! shard that has it, and every other test is in exactly one slice, which exactly one shard has.
//!
//! The verb prints each command before it runs it and starts every run even when an earlier one
//! failed; it fails when any run failed or could not be started, naming each. The program runs as
//! one process per run with its arguments as separate entries (no shell).

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// The arguments of `coverage-shard`.
#[derive(clap::Args)]
pub struct Args {
    /// The plan file (`.config/coverage-shards.toml`).
    #[arg(long)]
    pub plan: PathBuf,
    /// This shard, from 1 to the plan's `shards`.
    #[arg(long)]
    pub shard: u32,
    /// The number of shards; it must equal the plan's `shards`.
    #[arg(long)]
    pub of: u32,
    /// The command each run starts, and its arguments; each run's own arguments are appended.
    #[arg(last = true, required = true)]
    pub command: Vec<OsString>,
}

/// Why the shard could not be run, or a run of it failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The flags do not fit the plan or each other (exit status 2).
    #[error("{0}")]
    Usage(String),
    /// The plan file could not be read.
    #[error("cannot read the plan {}: {source}", .path.display())]
    Read {
        /// The plan file.
        path: PathBuf,
        /// Why it could not be read.
        #[source]
        source: std::io::Error,
    },
    /// The plan file is not TOML of the plan's form (an unknown key is refused).
    #[error("cannot parse the plan {}: {source}", .path.display())]
    Parse {
        /// The plan file.
        path: PathBuf,
        /// What is wrong with it.
        #[source]
        source: toml::de::Error,
    },
    /// The plan parses but its numbers or tests make no plan.
    #[error("the plan {} is invalid: {why}", .path.display())]
    Invalid {
        /// The plan file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// A run ended with a failure status or could not be started.
    #[error("shard {shard} of {of}: {failed} of {total} runs failed: {summary}")]
    Failed {
        /// The shard.
        shard: u32,
        /// The number of shards.
        of: u32,
        /// How many runs failed.
        failed: usize,
        /// How many runs the shard has.
        total: usize,
        /// Each failed run: its command line and how it ended, separated by `; `.
        summary: String,
    },
}

/// A test the plan names, with the seconds it took under coverage.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Heavy {
    /// The test binary, as nextest names it: `<package>::<binary>`.
    pub binary_id: String,
    /// The test's name.
    pub test: String,
    /// The seconds it took under coverage; not zero.
    pub seconds: u64,
}

/// The plan file as it is written.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Raw {
    shards: u32,
    slices: u32,
    light_seconds: u64,
    #[serde(default)]
    heavy: Vec<Heavy>,
}

/// A checked plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The number of shards.
    pub shards: u32,
    /// The number of slices the tests the plan does not name are cut into.
    pub slices: u32,
    /// The seconds all the tests the plan does not name took together.
    pub light_seconds: u64,
    /// The named tests, sorted by binary id and then name.
    pub heavy: Vec<Heavy>,
}

impl Plan {
    /// Reads and checks the plan file `path`.
    ///
    /// # Errors
    /// The file cannot be read or parsed, has an unknown key, or is not a plan (see
    /// [`Plan::parse`]).
    pub fn load(path: &Path) -> Result<Plan, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Plan::parse(path, &text)
    }

    /// Checks the plan `text` of the file `path` (named in the errors).
    ///
    /// # Errors
    /// `text` is not TOML of the plan's form, has an unknown key, or has no shards (`shards = 0`),
    /// no slices (`slices = 0`), fewer slices than shards, a named test with `seconds = 0`, or a
    /// test named twice.
    pub fn parse(path: &Path, text: &str) -> Result<Plan, Error> {
        let raw: Raw = toml::from_str(text).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        let invalid = |why: String| Error::Invalid {
            path: path.to_path_buf(),
            why,
        };
        if raw.shards == 0 {
            return Err(invalid("shards is 0".to_owned()));
        }
        if raw.slices == 0 {
            return Err(invalid("slices is 0".to_owned()));
        }
        if raw.slices < raw.shards {
            return Err(invalid(format!(
                "slices is {}, fewer than its {} shards",
                raw.slices, raw.shards
            )));
        }
        let mut heavy = raw.heavy;
        heavy.sort_by(|a, b| {
            a.binary_id
                .cmp(&b.binary_id)
                .then_with(|| a.test.cmp(&b.test))
        });
        if let Some(zero) = heavy.iter().find(|h| h.seconds == 0) {
            return Err(invalid(format!(
                "{} {} has seconds = 0",
                zero.binary_id, zero.test
            )));
        }
        let same =
            |pair: &[Heavy]| pair[0].binary_id == pair[1].binary_id && pair[0].test == pair[1].test;
        if let Some(pair) = heavy.windows(2).find(|pair| same(pair)) {
            return Err(invalid(format!(
                "{} {} is named twice",
                pair[0].binary_id, pair[0].test
            )));
        }
        Ok(Plan {
            shards: raw.shards,
            slices: raw.slices,
            light_seconds: raw.light_seconds,
            heavy,
        })
    }
}

/// What a shard is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shard {
    /// The named tests it runs, as indexes into [`Plan::heavy`], ascending.
    pub heavy: Vec<usize>,
    /// The light slices it runs, from 1, ascending.
    pub slices: Vec<u32>,
}

/// One thing to deal out. The derived order puts named tests (by index into the sorted list)
/// before slices (by number), which is the order ties are broken in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Item {
    Heavy(usize),
    Light(u32),
}

/// Deals the plan's named tests and light slices out to its shards: one [`Shard`] for each, the
/// first for shard 1.
pub fn assign(plan: &Plan) -> Vec<Shard> {
    let scale = u128::from(plan.slices);
    let mut items: Vec<(u128, Item)> = plan
        .heavy
        .iter()
        .enumerate()
        .map(|(i, h)| (u128::from(h.seconds) * scale, Item::Heavy(i)))
        .chain((1..=plan.slices).map(|j| (u128::from(plan.light_seconds), Item::Light(j))))
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let count = usize::try_from(plan.shards).unwrap_or(usize::MAX);
    let mut loads = vec![0u128; count];
    let mut shards = vec![
        Shard {
            heavy: Vec::new(),
            slices: Vec::new(),
        };
        count
    ];
    for (weight, item) in items {
        let mut best = 0;
        for (k, load) in loads.iter().enumerate() {
            if *load < loads[best] {
                best = k;
            }
        }
        loads[best] += weight;
        match item {
            Item::Heavy(i) => shards[best].heavy.push(i),
            Item::Light(j) => shards[best].slices.push(j),
        }
    }
    for shard in &mut shards {
        shard.heavy.sort_unstable();
        shard.slices.sort_unstable();
    }
    shards
}

/// The filterset that selects exactly `tests`: `(binary_id(=B1) & test(=T1)) | ...`.
fn union<'a>(tests: impl Iterator<Item = &'a Heavy>) -> String {
    tests
        .map(|h| format!("(binary_id(={}) & test(={}))", h.binary_id, h.test))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The arguments appended to the command for each run of `shard`, in the order the runs go.
pub fn runs(plan: &Plan, shard: &Shard) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    if !shard.heavy.is_empty() {
        let own = union(shard.heavy.iter().map(|&i| &plan.heavy[i]));
        out.push(vec!["-E".to_owned(), own]);
    }
    let rest = match plan.heavy.is_empty() {
        true => "all()".to_owned(),
        false => format!("not ({})", union(plan.heavy.iter())),
    };
    for j in &shard.slices {
        out.push(vec![
            "-E".to_owned(),
            rest.clone(),
            "--partition".to_owned(),
            format!("slice:{j}/{}", plan.slices),
        ]);
    }
    out
}

/// `arg` as a shell would need it written to be one word, for the line that shows a command.
fn quoted(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c);
    match !arg.is_empty() && arg.chars().all(plain) {
        true => arg.to_owned(),
        false => format!("'{}'", arg.replace('\'', "'\\''")),
    }
}

/// The command line of a run, as it is printed.
fn line(program: &OsStr, args: &[OsString]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|a| quoted(&a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs shard `args.shard` of the plan: every run of it, in order, even after one failed.
///
/// # Errors
/// [`Error::Usage`] when the command's program is empty, `--of` is not the plan's `shards` or
/// `--shard` is not from 1 to it; the plan's errors; [`Error::Failed`] when a run ended with a
/// failure status or could not be started.
pub fn run(args: &Args) -> Result<String, Error> {
    let (program, fixed) = match args.command.split_first() {
        Some((program, fixed)) if !program.is_empty() => (program, fixed),
        _ => {
            return Err(Error::Usage(
                "coverage-shard needs a program after `--`".to_owned(),
            ));
        }
    };
    let plan = Plan::load(&args.plan)?;
    if args.of != plan.shards {
        return Err(Error::Usage(format!(
            "--of {} differs from the {} shards of the plan {}",
            args.of,
            plan.shards,
            args.plan.display()
        )));
    }
    if args.shard == 0 || args.shard > plan.shards {
        return Err(Error::Usage(format!(
            "--shard {} is not from 1 to the plan's {} shards",
            args.shard, plan.shards
        )));
    }
    let shards = assign(&plan);
    let own = usize::try_from(args.shard - 1).unwrap_or(usize::MAX);
    let all = runs(&plan, &shards[own]);
    let mut failures = Vec::new();
    let mut out = std::io::stdout();
    for appended in &all {
        let argv: Vec<OsString> = fixed
            .iter()
            .cloned()
            .chain(appended.iter().map(OsString::from))
            .collect();
        let shown = line(program, &argv);
        let _ = writeln!(out, "+ {shown}");
        let _ = out.flush();
        match Command::new(program).args(&argv).status() {
            Ok(status) if status.success() => {}
            Ok(status) => failures.push(format!("`{shown}` ended with {status}")),
            Err(e) => failures.push(format!("`{shown}` could not be started: {e}")),
        }
    }
    if failures.is_empty() {
        return Ok(format!(
            "shard {} of {}: {} runs passed",
            args.shard,
            args.of,
            all.len()
        ));
    }
    Err(Error::Failed {
        shard: args.shard,
        of: args.of,
        failed: failures.len(),
        total: all.len(),
        summary: failures.join("; "),
    })
}
