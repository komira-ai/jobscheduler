//! The darwin bundle format: the media types, the config document
//! ([`BundleConfig`]) with its validation, the placeholder grammar ([`Template`]), the
//! closed vocabularies a config may use, and the pure resolution of a validated config
//! into a run's trees, environment, arguments and read grants ([`materialise`]).
//!
//! A bundle is an OCI image manifest of one platform whose layers are sibling trees;
//! its config says which tree is which, how the environment names them and what the
//! tools need from the host. This crate holds the format and its validation only. It
//! reads no bundle bytes (packing and unpacking are other crates' work) and touches no
//! file: every function is a function of its arguments.
//!
//! - [`media`] holds the media types.
//! - [`BundleConfig::parse`] reads a config under the size limit, refusing unknown
//!   fields, and validates it; [`BundleConfig::validate`] checks a config built in
//!   memory; [`BundleConfig::to_canonical_json`] writes the one byte form of it.
//! - [`Template`] is a string with `${tree:NAME}` and `${developer_dir}` placeholders;
//!   the same grammar and normalisation rules apply to config values, a job's
//!   environment values and a job's arguments.
//! - [`check_env_name`] decides which environment names a bundle or a job may set.
//! - [`materialise`] resolves all of it for one run.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod config;
mod env;
mod error;
mod materialise;
pub mod media;
mod names;
mod template;
mod validate;
mod version;

pub use config::{
    Built, BundleConfig, Entrypoint, EnvOp, EnvRule, Layer, NetworkLevel, ReadGrantKind, Seatbelt,
    XcodeComponent, XcodeNeed,
};
pub use env::{RESERVED_ENV_PREFIX, check_env_name};
pub use error::{ConfigError, Defect, EnvNameError, ExpandError, MaterialiseError, TemplateError};
pub use materialise::{Inputs, Materialised, SYSTEM_PATH, materialise, tree_path};
pub use names::{DiffId, ParseError, TreeName};
pub use template::{Segment, Template};
pub use validate::{
    MAX_CONFIG_BYTES, MAX_ENV_RULES, MAX_LAYERS, MAX_VALUE_BYTES, MAX_VALUES_PER_RULE,
};
pub use version::DottedVersion;
