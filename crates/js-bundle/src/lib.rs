//! The darwin bundle format: the media types, the config document
//! ([`BundleConfig`]) with its validation, the placeholder grammar ([`Template`]), the
//! closed vocabularies a config may use, and the pure resolution of a validated config
//! into a run's trees, environment, arguments and read grants ([`materialise`]).
//!
//! A bundle is an OCI image manifest of one platform whose layers are sibling trees;
//! its config says which tree is which, how the environment names them and what the
//! tools need from the host. This crate holds the format, its validation, and the layer
//! codec: the packer ([`pack_layer`]) and the extractor ([`unpack_layer`]).
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
//! - [`pack_layer`] turns a directory into a deterministic layer (a sorted tar of fixed
//!   metadata, in one zstd frame); [`unpack_layer`] and [`unpack_tar`] extract a layer onto
//!   a directory descriptor under hostile-input rules (see the `unpack` module's
//!   documentation), within [`Limits`] taken from the config. The extractor is the code the
//!   unprivileged helper runs; it holds no process or privilege logic of its own.
//! - [`check_path`] and [`check_target`] are the naming rules both sides apply.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The lists in `clippy.toml` enforce it. The codec touches the file system
//! (`std::fs` to read a tree, `openat` and friends to write one), which the layer allows.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
// Stubs of the layer codec: the tests land first, red.
#![allow(dead_code, unused)]

mod config;
mod env;
mod error;
mod materialise;
pub mod media;
mod names;
mod pack;
mod stream;
mod template;
mod tree;
mod unpack;
mod validate;
mod version;

pub use config::{
    Built, BundleConfig, Entrypoint, EnvOp, EnvRule, Layer, NetworkLevel, ReadGrantKind, Seatbelt,
    XcodeComponent, XcodeNeed,
};
pub use env::check_env_name;
pub use error::{
    ConfigError, Defect, EnvNameError, ExpandError, MAX_JSON_ERROR_BYTES, MaterialiseError,
    TemplateError,
};
pub use materialise::{Inputs, MAX_ENV_BYTES, Materialised, SYSTEM_PATH, materialise, tree_path};
pub use names::{DiffId, FieldParseError, TreeName};
pub use pack::{PackError, Packed, PackedTar, pack_layer, pack_tar};
pub use stream::{Decoder, MAX_WINDOW_BYTES, StreamError, compress};
pub use template::{Segment, Template};
pub use tree::{
    EntryDefect, LinkDefect, MAX_COMPONENT_BYTES, MAX_DEPTH, MAX_PATH_BYTES, MAX_TARGET_BYTES,
    NameDefect, TreeRefusal, check_path, check_target,
};
pub use unpack::{END_BLOCKS, Limits, UnpackError, Unpacked, unpack_layer, unpack_tar};
pub use validate::{
    MAX_CONFIG_BYTES, MAX_ENTRIES, MAX_ENV_RULES, MAX_LAYERS, MAX_TOTAL_BYTES, MAX_VALUE_BYTES,
    MAX_VALUES_PER_RULE,
};
pub use version::DottedVersion;
