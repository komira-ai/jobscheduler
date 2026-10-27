//! The command line: flags only, never environment variables.

use std::path::PathBuf;

use clap::Parser;

/// The flags of `js-agent`.
#[derive(Debug, Parser)]
#[command(
    name = "js-agent",
    version,
    about = "Registers this machine with a lease server and keeps it registered"
)]
pub struct Cli {
    /// The lease server, such as `https://leases.example:7300`. Plain `http://` is
    /// accepted for a loopback host only: the join token never crosses a network in
    /// clear.
    #[arg(long, value_name = "URL")]
    pub server: String,
    /// A file holding the pool's join token: a regular file owned by the user running the
    /// agent, mode 0600 or 0400. The token is sent only in the `Authorization` header.
    #[arg(long, value_name = "PATH")]
    pub token_file: PathBuf,
    /// A PEM file with the certificate to trust for `https://` (the server's, or the
    /// authority that issued it). Required for `https://`; the platform's roots are not
    /// read.
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<PathBuf>,
    /// The pool this host asks to join.
    #[arg(long, value_name = "NAME")]
    pub pool: String,
    /// The id this host registers under. By default a salted hash of the machine id
    /// (`/etc/machine-id`, or the platform UUID on a Mac), never the machine id itself.
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
    /// Accept a start whose job is not confined to a container (`isolation = host`). Not
    /// available yet: the scheduler does not record a host job's taint, and the agent
    /// refuses this flag at start. Without it such a start creates nothing and is
    /// refused.
    #[arg(long)]
    pub allow_host_jobs: bool,
    /// A directory jobs may mount from, as `name=path`; the name is what a mount in a
    /// start refers to. May be given more than once. The path must be an existing
    /// directory; it is canonicalised when the agent starts. No root, no mount.
    #[arg(long = "mount-root", value_name = "NAME=PATH", value_parser = named_path)]
    pub mount_roots: Vec<(String, PathBuf)>,
    /// The directory under which every run gets a private scratch directory (`0700`). It
    /// must be owned by the user running the agent, with no permission for group or
    /// others.
    #[arg(long, value_name = "PATH")]
    pub scratch_root: Option<PathBuf>,
    /// The directory the agent keeps its taint marker in: it must be owned by the user
    /// running the agent, with no permission for group or others. Required with
    /// `--container-bin`. A host that a post-job check found unclean is tainted for good:
    /// the marker survives a restart, and no code of the agent removes it.
    #[arg(long, value_name = "PATH")]
    pub state_root: Option<PathBuf>,
    /// A directory whose listing (names, sizes, modification times and modes of its
    /// entries, not its subdirectories' contents) the post-job checks compare with the
    /// listing taken when the host was last known clean; a change taints the host. May be
    /// given more than once. An absolute path to an existing directory that holds no mount
    /// root, scratch root or state root and lies in none. A directory with more than 4096
    /// entries cannot be checked and taints the host. Needs `--container-bin`.
    #[arg(long = "watch-dir", value_name = "PATH")]
    pub watch_dirs: Vec<PathBuf>,
    /// The absolute path of the `docker` or `podman` program that runs container jobs.
    /// It is started by exactly this path, never looked up on the `PATH`, with an
    /// environment of its own (a fixed `PATH` and `--container-host`, nothing the agent
    /// inherited); without it the agent runs no container. A job that names a GPU needs
    /// the NVIDIA container toolkit with Docker, and with Podman a CDI specification
    /// (`nvidia.com/gpu=<index>`) the operator has generated. Rootless engines are not
    /// supported: the engine gets none of `XDG_RUNTIME_DIR`, `HOME` or the D-Bus session
    /// that a rootless Podman needs (it may then run containers without their limits), so
    /// use a rootful engine.
    #[arg(long, value_name = "PATH")]
    pub container_bin: Option<PathBuf>,
    /// The network containers join: the name of a user-defined network. By default
    /// `none`, no network at all. `host` is refused. Any named network gives the job
    /// egress to whatever that network reaches, the host's metadata endpoint included;
    /// the agent warns at start. Needs `--container-bin`.
    #[arg(long, value_name = "NAME")]
    pub container_network: Option<String>,
    /// The `<uid>:<gid>` containers run as, both above zero. By default `65534:65534`.
    /// The run's scratch directory is private to the agent: an agent that runs as root
    /// gives it to this user, any other agent refuses to start unless this is its own
    /// `<uid>:<gid>`. Needs `--container-bin`.
    #[arg(long, value_name = "UID:GID")]
    pub container_user: Option<String>,
    /// The engine to talk to, as `unix:///<path>`, `tcp://<address>` or `ssh://<address>`;
    /// given to the program as `DOCKER_HOST` and `CONTAINER_HOST` for its calls only. By
    /// default none is given and the engine uses its own default. An `ssh://` host is
    /// accepted but not supported: the engine gets no `SSH_AUTH_SOCK` and no `HOME`, so
    /// it cannot authenticate through an SSH agent or the user's keys. Needs
    /// `--container-bin`.
    #[arg(long, value_name = "URI")]
    pub container_host: Option<String>,
    /// When a job's image is pulled: `never` (the default: the image must be on the host,
    /// so a job cannot make the host reach a registry of its choosing) or `missing`.
    /// With `never`, the operator puts an image on the host (`docker pull`, or `docker
    /// load` from an archive) before a job names it; a job whose image is not there ends
    /// as `failed_to_start`. Needs `--container-bin`.
    #[arg(long, value_name = "never|missing")]
    pub container_pull: Option<String>,
    /// The size of each container's `/tmp`, such as `64m` (the default) or `1g`. Needs
    /// `--container-bin`.
    #[arg(long, value_name = "SIZE")]
    pub container_tmp_size: Option<String>,
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
