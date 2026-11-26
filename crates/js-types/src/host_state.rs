//! Where a host's session stands, as the registry judges it and the scaler reads it.

/// Where a host's session stands.
///
/// The scaler acts only on `Live`: a host in either other state covers no request, is never
/// drained and is never released on its silence. The registry keeps the host's booking in
/// both, and so does the scaler. The host covers nothing, but the machine it stands for does
/// while the scaler is new and the host has not said hello since the restart: until then,
/// for `boot_timeout` from the restart, the machine's stored offer covers a request it
/// fits, so the request is not given a second machine. Only a machine in the registered
/// phase is awaited, not a draining one: a draining machine takes no new lease, so it could
/// not serve the request it would hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// The host was put back after a restart of the registry and has not said hello since:
    /// its booking is held and cannot be seen, so the scaler treats it as `Lost`.
    Awaiting,
    /// The host said hello or heartbeat within the grace period.
    Live,
    /// The host has been silent for longer than the grace period. Silence alone releases
    /// nothing.
    Lost,
}
