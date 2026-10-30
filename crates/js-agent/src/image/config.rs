//! The image config: the part of it a job runs by.

use js_contain::container::ContainerSpec;
use serde::Deserialize;

/// The most bytes of a config blob the client reads: a config that declares more is
/// refused before it is fetched. An image config is a few KiB (its history grows by a
/// line per layer, at most 128 of them).
pub const CONFIG_LIMIT_BYTES: u64 = 256 * 1024;

/// The `PATH` a container engine gives a container whose image sets none.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Why a config cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The bytes are not an image config.
    #[error("the image config is not a config: {0}")]
    Malformed(String),
    /// An `Env` entry that is not `NAME=value`.
    #[error("the image config has an Env entry that is not NAME=value")]
    Env,
    /// A `WorkingDir` with a `..` part.
    #[error("the image config has a WorkingDir with a '..' part: {0:?}")]
    WorkingDir(String),
    /// The config lists a different number of layers than the manifest.
    #[error("the image config lists {config} layers, the manifest {manifest}")]
    Layers {
        /// `rootfs.diff_ids` of the config.
        config: usize,
        /// The layers of the manifest.
        manifest: usize,
    },
    /// Neither the image nor the run gives a program to run.
    #[error("the image has no ENTRYPOINT or CMD and the run gives no command")]
    NoCommand,
}

/// What the image config says about running the image, as a job uses it.
///
/// The image's `USER` is deliberately absent: it is never read, so no config can name a
/// user the job would run as (see the [module](super) documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageConfig {
    entrypoint: Vec<String>,
    cmd: Vec<String>,
    env: Vec<(String, String)>,
    working_directory: String,
    layers: usize,
}

/// The JSON of a config, as far as it is read. Unknown fields (`User` among them) are
/// not read.
#[derive(Deserialize)]
struct Document {
    config: Option<Process>,
    rootfs: Rootfs,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Process {
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    env: Option<Vec<String>>,
    working_dir: Option<String>,
}

#[derive(Deserialize)]
struct Rootfs {
    diff_ids: Vec<String>,
}

impl ImageConfig {
    /// The config in `bytes`: the blob the registry client verified against the manifest.
    ///
    /// # Errors
    /// [`ConfigError::Malformed`] when it is not a config with a `rootfs`,
    /// [`ConfigError::Env`] for an `Env` entry without `=`, and
    /// [`ConfigError::WorkingDir`] for a `WorkingDir` that leaves the image's root.
    pub fn parse(_bytes: &[u8]) -> Result<Self, ConfigError> {
        Err(ConfigError::Malformed("not implemented".to_owned()))
    }

    /// Refuses a config that lists other layers than the manifest it came with: the
    /// two would then describe different images.
    ///
    /// # Errors
    /// [`ConfigError::Layers`] when the counts differ.
    pub fn check_layers(&self, _manifest: usize) -> Result<(), ConfigError> {
        Ok(())
    }

    /// Sets the program, environment and working directory of `spec` from the image and
    /// the run's `args`, and leaves every other field as it is.
    ///
    /// The command is the entrypoint followed by `args`; when `args` is empty the image's
    /// `CMD` follows it instead (a run that names no flags runs the image as it was
    /// built), and when the image has no entrypoint `args` is the command. The
    /// environment is the image's, in its order, with a `PATH` added when it has none. The
    /// working directory is the image's `WORKDIR`, written relative to the image's root
    /// (`/work` is `work`; none, or `/`, is empty), which is how
    /// [`ContainerSpec::working_directory`] is read.
    ///
    /// # Errors
    /// [`ConfigError::NoCommand`] when nothing names a program.
    pub fn apply(&self, _args: &[String], _spec: &mut ContainerSpec) -> Result<(), ConfigError> {
        Err(ConfigError::NoCommand)
    }
}

/// `dir`, an absolute directory of the image, as a path relative to its root: no empty or
/// `.` parts, and no `..`.
fn relative(dir: &str) -> Result<String, ConfigError> {
    let _ = dir;
    let parts: Vec<&str> = dir
        .split('/')
        .filter(|part| !matches!(*part, "" | "."))
        .collect();
    if parts.contains(&"..") {
        return Err(ConfigError::WorkingDir(dir.to_owned()));
    }
    Ok(parts.join("/"))
}
