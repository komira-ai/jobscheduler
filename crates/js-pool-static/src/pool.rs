use std::path::Path;

use js_provider::HostBook;
use js_types::{HostCapacity, MemoryHeadroom, MemoryPercent, ProviderId, ResourceClass};

use crate::config::{self, ConfigError};

/// A fixed list of hosts read from an operator's file. See the crate docs for what the
/// pool keeps of them.
#[derive(Debug)]
pub struct StaticPool {
    id: ProviderId,
    classes: Vec<ResourceClass>,
    book: HostBook,
}

impl StaticPool {
    /// A pool over configuration text, booking the share of each host's memory that the
    /// text's `memory_schedulable_percent` says (all of it when the key is left out).
    pub fn from_toml(id: ProviderId, source: &str) -> Result<Self, ConfigError> {
        Self::from_toml_with_percent(id, source, None)
    }

    /// A pool over configuration text, booking `percent` of each host's memory when it
    /// is given, and else the share the text says. The percent is applied once, here, to
    /// the memory the file lists: each host is booked as the shape with its memory cut,
    /// and a host's own claim has no part in it (a static host makes none).
    pub fn from_toml_with_percent(
        id: ProviderId,
        source: &str,
        percent: Option<MemoryPercent>,
    ) -> Result<Self, ConfigError> {
        let parsed = config::parse(source)?;
        let percent = percent.unwrap_or(parsed.memory_percent);
        let mut book = HostBook::new();
        for (host, shape) in parsed.hosts {
            let memory = MemoryHeadroom::new(shape.resources().memory_bytes(), percent);
            book.insert_host_with(host, shape.with_memory_percent(percent), memory);
        }
        Ok(Self {
            id,
            classes: vec![ResourceClass::gpu_host()],
            book,
        })
    }

    /// A pool over the configuration file at `path`.
    pub fn from_config_file(id: ProviderId, path: &Path) -> Result<Self, ConfigError> {
        Self::from_config_file_with_percent(id, path, None)
    }

    /// A pool over the configuration file at `path`, as
    /// [`from_toml_with_percent`](StaticPool::from_toml_with_percent) reads text.
    pub fn from_config_file_with_percent(
        id: ProviderId,
        path: &Path,
        percent: Option<MemoryPercent>,
    ) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_with_percent(id, &source, percent)
    }

    /// The same pool serving `classes` instead of the shipped one.
    pub fn with_classes(mut self, classes: Vec<ResourceClass>) -> Self {
        self.classes = classes;
        self
    }

    /// The id the pool was made with.
    pub fn id(&self) -> ProviderId {
        self.id.clone()
    }

    /// The resource classes the pool serves: `gpu-host` unless
    /// [`with_classes`](StaticPool::with_classes) named others.
    pub fn classes(&self) -> Vec<ResourceClass> {
        self.classes.clone()
    }

    /// Every host the file lists, in id order, with its shape (the memory cut to the
    /// schedulable percent) and the memory it was listed with. The server shows these
    /// beside the hosts that register themselves.
    pub fn hosts(&self) -> Vec<HostCapacity> {
        self.book.hosts()
    }
}
