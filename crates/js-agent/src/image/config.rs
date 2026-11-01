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
    /// A string of the config (an `Env` entry, an `Entrypoint` or `Cmd` word, the
    /// `WorkingDir`) with a NUL byte, which no program argument or path can hold.
    #[error("the image config has a NUL byte in its Env, Entrypoint, Cmd or WorkingDir")]
    Nul,
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
    /// The kept config of the image could not be read.
    #[error("the config kept for the image cannot be read: {0}")]
    Unreadable(String),
    /// Neither the image nor the run gives a program to run.
    #[error("the image has no ENTRYPOINT or CMD and the run gives no command")]
    NoCommand,
}

/// The file in `dir` that keeps the config of the image with `digest` (`sha256:<hex>`):
/// `<hex>.json`.
#[must_use]
pub fn config_path(dir: &std::path::Path, digest: &str) -> std::path::PathBuf {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    dir.join(format!("{hex}.json"))
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
    /// [`ConfigError::Env`] for an `Env` entry without `=`, [`ConfigError::Nul`] for a NUL
    /// byte in a string, and
    /// [`ConfigError::WorkingDir`] for a `WorkingDir` that leaves the image's root.
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        let document: Document =
            serde_json::from_slice(bytes).map_err(|e| ConfigError::Malformed(e.to_string()))?;
        let process = document.config.unwrap_or_default();
        let working_dir = process.working_dir.clone().unwrap_or_default();
        let nul = [&process.entrypoint, &process.cmd, &process.env]
            .into_iter()
            .flatten()
            .flatten()
            .chain([&working_dir])
            .any(|text| text.contains('\0'));
        if nul {
            return Err(ConfigError::Nul);
        }
        let mut env = process
            .env
            .unwrap_or_default()
            .iter()
            .map(|entry| {
                entry
                    .split_once('=')
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .ok_or(ConfigError::Env)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !env.iter().any(|(name, _)| name == "PATH") {
            env.push(("PATH".to_owned(), DEFAULT_PATH.to_owned()));
        }
        Ok(Self {
            entrypoint: process.entrypoint.unwrap_or_default(),
            cmd: process.cmd.unwrap_or_default(),
            env,
            working_directory: absolute(&working_dir)?,
            layers: document.rootfs.diff_ids.len(),
        })
    }

    /// The config of the image with `digest` that a source kept in `dir` (see
    /// [`RegistryImages::keeping_configs`](super::RegistryImages::keeping_configs)).
    ///
    /// # Errors
    /// [`ConfigError::Unreadable`] when there is no such file or it is bigger than
    /// [`CONFIG_LIMIT_BYTES`], and the errors of [`ImageConfig::parse`].
    pub fn kept(dir: &std::path::Path, digest: &str) -> Result<Self, ConfigError> {
        use std::io::Read as _;
        let path = config_path(dir, digest);
        let unreadable =
            |why: std::io::Error| ConfigError::Unreadable(format!("{}: {why}", path.display()));
        let file = std::fs::File::open(&path).map_err(unreadable)?;
        let mut bytes = Vec::new();
        file.take(CONFIG_LIMIT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;
        if u64::try_from(bytes.len()).map_or(true, |len| len > CONFIG_LIMIT_BYTES) {
            return Err(ConfigError::Unreadable(format!(
                "{}: too big",
                path.display()
            )));
        }
        Self::parse(&bytes)
    }

    /// The image's `WORKDIR` as an absolute path in the image (`/` when it sets none),
    /// cleaned of empty and `.` parts.
    ///
    /// It is not part of [`ImageConfig::apply`]: js-contain runs the job in
    /// `{exec_root}/{working_directory}` with the inputs overlaid at `exec_root`, so this
    /// path cannot be written into [`ContainerSpec::working_directory`]. Whoever builds
    /// the container decides where it runs.
    #[must_use]
    pub fn working_dir(&self) -> &str {
        &self.working_directory
    }

    /// Refuses a config that lists other layers than the manifest it came with: the
    /// two would then describe different images.
    ///
    /// # Errors
    /// [`ConfigError::Layers`] when the counts differ.
    pub fn check_layers(&self, manifest: usize) -> Result<(), ConfigError> {
        if self.layers == manifest {
            Ok(())
        } else {
            Err(ConfigError::Layers {
                config: self.layers,
                manifest,
            })
        }
    }

    /// Sets the program and environment of `spec` from the image and the run's `args`,
    /// and leaves every other field as it is, the working directory included (see
    /// [`ImageConfig::working_dir`]).
    ///
    /// The command is the entrypoint followed by `args`; when `args` is empty the image's
    /// `CMD` follows it instead (a run that names no flags runs the image as it was
    /// built), and when the image has no entrypoint `args` is the command. The
    /// environment is the image's, in its order, with a `PATH` added when it has none.
    ///
    /// # Errors
    /// [`ConfigError::NoCommand`] when nothing names a program.
    pub fn apply(&self, args: &[String], spec: &mut ContainerSpec) -> Result<(), ConfigError> {
        let tail = if args.is_empty() {
            self.cmd.as_slice()
        } else {
            args
        };
        let argv: Vec<String> = self.entrypoint.iter().chain(tail).cloned().collect();
        if argv.is_empty() {
            return Err(ConfigError::NoCommand);
        }
        spec.argv = argv;
        spec.env.clone_from(&self.env);
        Ok(())
    }
}

/// `dir`, a directory of the image, as an absolute path with no empty or `.` parts and no
/// `..` (`/` for none).
fn absolute(dir: &str) -> Result<String, ConfigError> {
    let parts: Vec<&str> = dir
        .split('/')
        .filter(|part| !matches!(*part, "" | "."))
        .collect();
    if parts.contains(&"..") {
        return Err(ConfigError::WorkingDir(dir.to_owned()));
    }
    Ok(format!("/{}", parts.join("/")))
}
