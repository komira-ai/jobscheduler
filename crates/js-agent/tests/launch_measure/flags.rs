//! The flags of `js-launch-measure`, the only configuration it takes (it reads no
//! environment variable for one). Pure: the arguments in, the flags or the flag at fault out.
//!
//! | flag | meaning |
//! |---|---|
//! | `--runs <n>` | measured samples per variant; default 50 |
//! | `--warmup <n>` | samples per variant taken first and discarded; default 5 |
//! | `--summary-file <path>` | a file the Markdown result is appended to (never truncated) |
//! | `--image-name <name>` | required: the runner image, printed with the result |
//! | `--crun <path>` | required, absolute: the `crun` program |
//! | `--busybox <path>` | required, absolute: a static `busybox`, the job's image |
//! | `--job-user <uid>:<gid>` | required: the user every `crun` runs as, not root |
//! | `--work-dir <path>` | required, absolute: where the stores and the runs' directories go; every directory above it must be searchable by the job user |

use std::fmt;
use std::path::PathBuf;

/// The default of `--runs`.
pub const RUNS: u32 = 50;

/// The default of `--warmup`.
pub const WARMUP: u32 = 5;

/// The parsed flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flags {
    /// `--runs`.
    pub runs: u32,
    /// `--warmup`.
    pub warmup: u32,
    /// `--summary-file`.
    pub summary_file: Option<PathBuf>,
    /// `--image-name`.
    pub image_name: String,
    /// `--crun`.
    pub crun: PathBuf,
    /// `--busybox`.
    pub busybox: PathBuf,
    /// `--job-user`.
    pub job_user: (u32, u32),
    /// `--work-dir`.
    pub work_dir: PathBuf,
}

/// The flag at fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagError {
    /// An argument that is no flag of the program.
    Unknown(String),
    /// A flag given last, with no value after it.
    NoValue(&'static str),
    /// A flag given twice.
    Twice(&'static str),
    /// A value the flag does not take.
    Bad {
        /// The flag.
        flag: &'static str,
        /// The value given.
        value: String,
        /// What the flag takes.
        takes: &'static str,
    },
    /// A required flag that is missing.
    Missing(&'static str),
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(arg) => write!(f, "{arg}: not a flag of js-launch-measure"),
            Self::NoValue(flag) => write!(f, "{flag} needs a value"),
            Self::Twice(flag) => write!(f, "{flag} is given twice"),
            Self::Bad { flag, value, takes } => {
                write!(f, "{flag} {value:?}: it takes {takes}")
            }
            Self::Missing(flag) => write!(f, "{flag} is required"),
        }
    }
}

/// The names of the flags.
const NAMES: [&str; 8] = [
    "--runs",
    "--warmup",
    "--summary-file",
    "--image-name",
    "--crun",
    "--busybox",
    "--job-user",
    "--work-dir",
];

/// `value` of `flag` as a whole number.
fn number(flag: &'static str, value: &str) -> Result<u32, FlagError> {
    value.parse().map_err(|_| FlagError::Bad {
        flag,
        value: value.to_owned(),
        takes: "a whole number from 0 to 4294967295",
    })
}

/// `value` of `flag` as an absolute path.
fn absolute(flag: &'static str, value: &str) -> Result<PathBuf, FlagError> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(FlagError::Bad {
            flag,
            value: value.to_owned(),
            takes: "an absolute path",
        })
    }
}

/// `value` of `--job-user`: `<uid>:<gid>`, the uid not 0.
fn job_user(value: &str) -> Result<(u32, u32), FlagError> {
    let bad = || FlagError::Bad {
        flag: "--job-user",
        value: value.to_owned(),
        takes: "<uid>:<gid> of a user that is not root",
    };
    let (uid, gid) = value.split_once(':').ok_or_else(bad)?;
    let ids: (u32, u32) = (
        uid.parse().map_err(|_| bad())?,
        gid.parse().map_err(|_| bad())?,
    );
    match ids.0 {
        0 => Err(bad()),
        _ => Ok(ids),
    }
}

/// `value` of the required flag `NAMES[at]`.
fn required(at: usize, value: Option<&str>) -> Result<&str, FlagError> {
    value.ok_or(FlagError::Missing(NAMES[at]))
}

/// The flags in `args` (the program's name not among them): each flag once, followed by its
/// value.
///
/// # Errors
///
/// The first flag at fault: one that is unknown, has no value, is given twice, has a value it
/// does not take, or is required and missing. A bad value never falls back to the default.
pub fn parse(args: &[String]) -> Result<Flags, FlagError> {
    let mut given: [Option<&str>; NAMES.len()] = [None; NAMES.len()];
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(at) = NAMES.iter().position(|name| *name == arg.as_str()) else {
            return Err(FlagError::Unknown(arg.clone()));
        };
        let value = rest.next().ok_or(FlagError::NoValue(NAMES[at]))?;
        if given[at].replace(value).is_some() {
            return Err(FlagError::Twice(NAMES[at]));
        }
    }
    let [
        runs,
        warmup,
        summary_file,
        image_name,
        crun,
        busybox,
        user,
        work_dir,
    ] = given;
    Ok(Flags {
        runs: runs.map_or(Ok(RUNS), |value| number(NAMES[0], value))?,
        warmup: warmup.map_or(Ok(WARMUP), |value| number(NAMES[1], value))?,
        summary_file: summary_file.map(PathBuf::from),
        image_name: required(3, image_name)?.to_owned(),
        crun: absolute(NAMES[4], required(4, crun)?)?,
        busybox: absolute(NAMES[5], required(5, busybox)?)?,
        job_user: job_user(required(6, user)?)?,
        work_dir: absolute(NAMES[7], required(7, work_dir)?)?,
    })
}
