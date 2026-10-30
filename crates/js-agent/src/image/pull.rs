//! `js-agent image-pull`: pulls one image into the store the crun runtime reads.
//!
//! ```text
//! js-agent image-pull <repository>@sha256:<hex> --image-allow <host[:port]/prefix>...
//!     --scratch-root <dir> --job-user <uid:gid> --cgroup-parent </path> [--crun-bin <path>]
//! ```
//!
//! The verb does what the runtime does for a job's first use of an image, without a job:
//! js-contain's image store fetches the layers with js-oci (the allow-list and the digest
//! of the manifest and of every layer are js-oci's), unpacks them in a helper container
//! that runs as the job user, and renames the finished directory to
//! `<scratch-root>/oci-images/<hex>/rootfs`. No container tool other than `crun` takes
//! part. It runs as root, because the helpers give files to the job user's subordinate
//! ids; every check that needs no privilege is made first.
//!
//! One line on standard output says what happened:
//!
//! ```text
//! image-pull: status=pulled digest=sha256:<hex> root=<dir>
//! image-pull: status=already-published digest=sha256:<hex> root=<dir>
//! ```
//!
//! An image that is already published is not fetched again, and the registry is not asked.
//! Exit status 0 is a pulled or published image, 2 a usage error (nothing was made or
//! asked), 1 a failure of the pull itself (nothing is published; the stage and the
//! downloaded layers are removed).
//!
//! Two pulls into one scratch root do not run together (a lock file in the root), and a
//! pull must not run beside an agent that uses the same root: starting the backend sweeps
//! the stages an earlier run left behind, which would include this one's.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use js_contain::oci::image::{PinnedImage, published_root};
use js_contain::oci::{Oci, OciConfig};
use js_oci::{Allow, Client, Location, Policy};

use super::RegistryImages;
use crate::exit;
use crate::lifecycle::{DirLayout, ScratchRoot};

/// The first argument that selects the verb.
pub const VERB: &str = "image-pull";

/// The lock file in the scratch root.
const LOCK: &str = ".js-image-pull.lock";

/// What the verb takes from the machine it runs on.
pub struct Host<'a> {
    /// The effective user id of the process.
    pub euid: u32,
    /// The registry client for an allow-list.
    pub client: &'a dyn Fn(Vec<Allow>) -> Client,
    /// Adjusts the runtime's configuration.
    pub configure: &'a dyn Fn(&mut OciConfig),
}

fn system_client(allow: Vec<Allow>) -> Client {
    Client::new(Policy::new(allow))
}

fn system_configure(_: &mut OciConfig) {}

impl Host<'static> {
    /// This machine.
    #[must_use]
    pub fn system() -> Self {
        Self {
            euid: rustix::process::geteuid().as_raw(),
            client: &system_client,
            configure: &system_configure,
        }
    }
}

/// The flags of the verb.
#[derive(Debug, Parser)]
#[command(
    name = "js-agent image-pull",
    about = "Pulls an image, pinned by digest, into the image store of the crun runtime",
    disable_version_flag = true
)]
struct Args {
    /// The image: `<repository>@sha256:<64 lowercase hex digits>`. A tag is refused.
    #[arg(value_name = "REFERENCE")]
    reference: String,
    /// A registry and namespace the image may be pulled from, such as `ghcr.io/komira-ai/`;
    /// repeatable. A bare host is refused.
    #[arg(long, value_name = "HOST[:PORT]/PREFIX", required = true)]
    image_allow: Vec<String>,
    /// The directory the image store is made in: owned by the user running the verb, mode
    /// 0711, below directories others can search.
    #[arg(long, value_name = "PATH")]
    scratch_root: PathBuf,
    /// The user the helper containers run as, `UID:GID`; neither is 0.
    #[arg(long, value_name = "UID:GID")]
    job_user: String,
    /// The cgroup the helper containers are made under: an absolute path below the cgroup
    /// mount that the job user may manage.
    #[arg(long, value_name = "/PATH")]
    cgroup_parent: String,
    /// The OCI runtime.
    #[arg(long, value_name = "PATH")]
    crun_bin: Option<PathBuf>,
}

/// Why the verb stopped.
#[derive(Debug)]
enum Failure {
    /// A flag or the reference is wrong, or the user may not run the verb; nothing was
    /// made or asked.
    Usage(String),
    /// The pull itself failed.
    Failed(String),
}

impl Failure {
    fn code(&self) -> u8 {
        match self {
            Self::Usage(_) => exit::USAGE,
            Self::Failed(_) => exit::FAILED,
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Usage(text) | Self::Failed(text) => text,
        }
    }
}

/// Runs the verb on `args` (the program name and the verb first).
///
/// It blocks the calling thread on a runtime of its own, so it is not called from an
/// async task of another runtime without leaving that runtime first.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    match pull(args, host) {
        Ok(text) => {
            let _ = write!(out, "{text}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            let _ = writeln!(err, "js-agent image-pull: {}", failure.text());
            ExitCode::from(failure.code())
        }
    }
}

/// What the verb prints on success: the help text, or the line that says what happened.
fn pull(args: &[OsString], host: &Host<'_>) -> Result<String, Failure> {
    let args = match Args::try_parse_from(args.iter().skip(1)) {
        Ok(args) => args,
        Err(e) if e.use_stderr() => return Err(Failure::Usage(e.to_string())),
        Err(help) => return Ok(help.to_string()),
    };
    let image = pinned(&args.reference)?;
    let user = job_user(&args.job_user)?;
    cgroup_parent(&args.cgroup_parent)?;
    let root = ScratchRoot::open_with(&args.scratch_root, DirLayout::Searchable)
        .map_err(Failure::Usage)?;
    let allow = args
        .image_allow
        .iter()
        .map(|entry| Allow::parse(entry).map_err(|e| Failure::Usage(format!("--image-allow: {e}"))))
        .collect::<Result<Vec<_>, _>>()?;
    let client = (host.client)(allow);
    admitted(&client, &image)?;
    if host.euid != 0 {
        return Err(Failure::Usage(
            "must run as root: the unpack gives files to the job user's ids".to_owned(),
        ));
    }

    let published = published_root(root.path(), &image.digest);
    let done = |status: &str| {
        format!(
            "image-pull: status={status} digest={} root={}\n",
            image.digest,
            published.display()
        )
    };
    if published.is_dir() {
        return Ok(done("already-published"));
    }

    let _lock = lock(root.path())?;
    let mut config = OciConfig {
        job_user: Some(user),
        ..OciConfig::default()
    };
    if let Some(crun) = args.crun_bin {
        config.runtime = crun;
    }
    (host.configure)(&mut config);
    let source = RegistryImages::new(client, std::env::consts::ARCH).map_err(failed)?;
    let oci =
        Oci::new(&config, Arc::new(source), root.path(), &args.cgroup_parent).map_err(failed)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(failed)?;
    runtime.block_on(oci.check_pinned(&image)).map_err(failed)?;
    Ok(done("pulled"))
}

fn failed(why: impl std::fmt::Display) -> Failure {
    Failure::Failed(why.to_string())
}

/// The image `reference` names: `<repository>@sha256:<64 lowercase hex digits>`, a digest
/// and nothing a registry could answer differently tomorrow.
fn pinned(reference: &str) -> Result<PinnedImage, Failure> {
    let usage = |why: &str| Failure::Usage(format!("{reference}: {why}"));
    let (repository, digest) = reference
        .split_once('@')
        .ok_or_else(|| usage("names no digest; give <repository>@sha256:<hex>"))?;
    // The host may carry a port; a tag is a colon in the last path component.
    let name = repository.rsplit('/').next().unwrap_or_default();
    if name.contains(':') {
        return Err(usage("carries a tag; the digest alone names the image"));
    }
    let hex = digest.strip_prefix("sha256:").unwrap_or_default();
    let lowercase = hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if hex.len() != 64 || !lowercase {
        return Err(usage(
            "the digest must be sha256: and 64 lowercase hex digits",
        ));
    }
    Location::parse(repository).map_err(|e| usage(&e.to_string()))?;
    Ok(PinnedImage {
        reference: reference.to_owned(),
        digest: digest.to_owned(),
    })
}

/// Whether the allow-list admits the image's repository. Nothing is asked of a registry.
fn admitted(client: &Client, image: &PinnedImage) -> Result<(), Failure> {
    let refused = |why: &dyn std::fmt::Display| {
        Failure::Usage(format!(
            "{}: not admitted by the --image-allow allow-list ({why})",
            image.reference
        ))
    };
    // `pinned` checked the form of the reference.
    let repository = image
        .reference
        .split_once('@')
        .map_or(image.reference.as_str(), |(repository, _)| repository);
    let location = Location::parse(repository).map_err(|e| refused(&e))?;
    client
        .repository(location)
        .map(drop)
        .map_err(|e| refused(&e))
}

/// `--job-user`: `UID:GID`, neither of them root's.
fn job_user(text: &str) -> Result<(u32, u32), Failure> {
    let usage = || {
        Failure::Usage(format!(
            "--job-user: {text:?} is not UID:GID with neither 0"
        ))
    };
    let (uid, gid) = text.split_once(':').ok_or_else(usage)?;
    let ids: (u32, u32) = (
        uid.parse().map_err(|_| usage())?,
        gid.parse().map_err(|_| usage())?,
    );
    (ids.0 != 0 && ids.1 != 0).then_some(ids).ok_or_else(usage)
}

/// `--cgroup-parent`: an absolute path of plain components.
fn cgroup_parent(path: &str) -> Result<(), Failure> {
    let plain = path
        .strip_prefix('/')
        .is_some_and(|rest| rest.split('/').all(|part| !matches!(part, "" | "." | "..")));
    plain.then_some(()).ok_or_else(|| {
        Failure::Usage(format!(
            "--cgroup-parent: {path:?} is not an absolute path of plain components"
        ))
    })
}

/// Takes the lock of the store, held until the returned file is dropped.
fn lock(root: &Path) -> Result<std::fs::File, Failure> {
    let path = root.join(LOCK);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| failed(format!("{}: {e}", path.display())))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
        |e| {
            failed(format!(
                "{}: {e}: is another image-pull running on this root?",
                path.display()
            ))
        },
    )?;
    Ok(file)
}
