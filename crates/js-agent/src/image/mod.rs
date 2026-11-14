//! Container images for the crun runtime: where an image's layers come from
//! ([`RegistryImages`], an [`ImageSource`](js_contain::oci::image::ImageSource) over
//! js-oci's verified pulls) and what the image says about how to run it
//! ([`ImageConfig`]).
//!
//! Both halves read the registry's answer and nothing else of the image:
//! - the manifest is the one the pin names, one image and not an index, for this
//!   machine's platform; every layer file holds exactly the bytes that hash to the digest
//!   the manifest gives, and a layer that fails leaves no file behind;
//! - the config is read under a size limit, and its `rootfs.diff_ids` must be as many as
//!   the manifest's layers;
//! - `ENTRYPOINT`, `CMD` and `ENV` become the command and environment of the container
//!   ([`ImageConfig::apply`]), as a container engine gives them: the run's arguments follow
//!   the entrypoint and replace the image's `CMD`. `WORKDIR` is read
//!   ([`ImageConfig::working_dir`]) but not applied, because js-contain's working directory
//!   is relative to the overlay at its `exec_root`, not to the image's `/`;
//! - the image's `User` is read and handed to the container ([`ImageConfig::apply`]), as
//!   Docker runs it: a job runs as container root, uid and gid 0 of its user namespace, unless
//!   the image names a user, which js-contain resolves in the image's own `/etc/passwd` and
//!   `/etc/group`. The namespace maps every id to the job user's subordinate range, so
//!   either way the job is unprivileged on the host, and an image cannot choose a host id.

mod config;
pub mod pull;
mod source;

pub use config::{CONFIG_LIMIT_BYTES, ConfigError, DEFAULT_PATH, ImageConfig, config_path};
pub use source::RegistryImages;
