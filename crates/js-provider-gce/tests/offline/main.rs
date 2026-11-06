//! Behaviour that needs no server: the launch records, the cloud's errors and the refusals
//! they make, the label marks, the settings and the `gce` kind, the in-memory project, the
//! identity check, and the provider over them.

mod accelerators;
mod acquire;
mod conformance;
mod fake;
mod grammar;
mod identity;
mod kind;
mod launch;
mod list;
mod observe;
mod payload;
mod refusal;
mod release;
mod rig;
mod verify;
