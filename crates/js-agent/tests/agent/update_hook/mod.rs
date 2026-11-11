//! `js-agent update-hook` (host-updates design, 4.2), the verb skeleton: what the command
//! line, the last line and the params file accept and refuse, how an outcome becomes an exit
//! status, and that the verb is answered before the agent installs a signal handler. The steps
//! have their bodies in `step` (tests on the fakes: steps_*.rs). The hook record, its lock, the params and
//! the rules the record is held to are pure of the steps and tested here on their own.

mod args;
mod conformance;
mod conformance_runs;
mod fake_bin;
#[allow(dead_code)]
#[path = "../../update_hook_fakes/mod.rs"]
mod fakes;
mod fakes_tests;
mod harness_tests;
mod hostkeeper_runner;
mod launchd;
mod lease_port;
mod lease_port_transport;
mod lease_stub;
mod machine;
mod machine_flags;
mod machine_root;
mod params;
mod ports;
mod record;
mod record_lock;
mod report;
#[allow(dead_code)]
mod rollback_entries;
#[allow(dead_code)]
mod step_rig;
mod steps_cleanup;
mod steps_cutover;
mod steps_cutover_cordon;
mod steps_cutover_launchd;
mod steps_prepare;
mod steps_prepare_cordon;
mod steps_prepare_supersede;
mod steps_rollback;
mod steps_rollback_first;
mod steps_rollback_guards;
mod steps_unit;
mod steps_verify;
mod unit_conformance;
mod unit_files;
mod validate;
mod verb;
