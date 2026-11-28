//! How a host is set aside, as the registry marks it and the scaler reads it.

/// How a host is set aside: for whole-node use, or to be scaled away. The registry sets
/// only `Draining` and `Releasing`; the other two are the vocabulary of the host listing and
/// the scaler's view, which the registry no longer produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMark {
    /// Set aside for a waiting request.
    Reserved,
    /// A whole-node lease is committed on it: kept like a busy host.
    Held,
    /// Marked by the registry's `drain` or by the scaler: the host holds no run and takes no
    /// new job or start of any kind. Only the registry's `undrain` or `release_host` ends the
    /// mark.
    Draining,
    /// Marked by the registry's `release_host` or by the scaler: the host is being given back
    /// and takes no job. Nothing clears the mark; the registry's `remove` drops the host.
    Releasing,
}
