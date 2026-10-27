//! The generic scheduling vocabulary: QoS levels, lease identifiers and kinds, node
//! phases, farm time, the vocabulary of scheduled work (operations, workers, request
//! vectors, control-log records), the `StateMachine` trait and `Effect` enum, and the
//! version a binary reports and the cap two versions negotiate.
//!
//! Nothing here is specific to building: no REAPI, no content `Digest`. These types live
//! under `js_types::sched` and are not re-exported at the crate root: the root's
//! [`Resources`](crate::Resources) and [`LeaseId`](crate::LeaseId) are the provider
//! slice's own types, and `sched::Resources` and `sched::LeaseId` are different ones.
//!
//! The work itself is named by a [`WorkKey`], a class and a dedup text, and a completed
//! attempt's stored result by a [`ResultRef`], an identity and a size; a build's own key
//! and digest convert to and from them at the edges. The test at the foot of this file
//! refuses a build name in the code of any file of the module.

mod lease;
mod phase;
mod qos;
mod state;
mod time;
mod version;
mod work;

pub use lease::{LeaseId, LeaseKind};
pub use phase::NodePhase;
pub use qos::{CustomQos, Qos, QosError};
pub use state::{Effect, StateMachine};
pub use time::FarmTime;
pub use version::{Supported, TooNew, VERSION_CAP, Version, VersionFault, understood};
pub use work::{
    Answer, ControlRecord, Failure, FarmFault, FarmRun, FencePolicy, LeaseGrant, LostWhy,
    MemoryKill, MemoryRun, OperationId, Outcome, Refusal, RefusalRecord, Resources, ResultRecord,
    ResultRef, StartLease, WaiterId, Waiting, WorkKey, WorkerId,
};

#[cfg(test)]
mod tests {
    /// The names this module must not use.
    const BUILD_NAMES: [&str; 3] = ["Digest", "ActionKey", "REAPI"];

    /// The non-comment lines before the unit tests of one source file.
    fn code_lines(source: &str) -> Vec<&str> {
        source
            .lines()
            .take_while(|line| *line != "#[cfg(test)]")
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .collect()
    }

    fn offenders(source: &str) -> Vec<&str> {
        code_lines(source)
            .into_iter()
            .filter(|line| BUILD_NAMES.iter().any(|name| line.contains(name)))
            .collect()
    }

    #[test]
    fn the_generic_files_name_no_build_type() {
        let clean = [
            ("lease.rs", include_str!("sched/lease.rs")),
            ("phase.rs", include_str!("sched/phase.rs")),
            ("qos.rs", include_str!("sched/qos.rs")),
            ("state.rs", include_str!("sched/state.rs")),
            ("time.rs", include_str!("sched/time.rs")),
            ("version.rs", include_str!("sched/version.rs")),
            ("work.rs", include_str!("sched/work.rs")),
        ];
        for (file, source) in clean {
            assert_eq!(offenders(source), Vec::<&str>::new(), "{file}");
        }
    }

    #[test]
    fn the_scan_sees_a_build_name_in_code_and_skips_comments() {
        let source = "// the REAPI name\nlet d: Digest;\n#[cfg(test)]\nlet e: Digest;\n";
        assert_eq!(offenders(source), vec!["let d: Digest;"]);
    }

    /// The module's `LeaseId` is not the provider slice's: the two are different types
    /// and neither converts to the other, so a mix-up is a compile error.
    #[test]
    fn the_module_and_the_root_each_have_their_own_lease_id() {
        let here: super::LeaseId = super::LeaseId::new(1, 7);
        assert_eq!(here.to_string(), "1.7");
        assert_ne!(
            std::any::TypeId::of::<super::LeaseId>(),
            std::any::TypeId::of::<crate::LeaseId>()
        );
    }
}
