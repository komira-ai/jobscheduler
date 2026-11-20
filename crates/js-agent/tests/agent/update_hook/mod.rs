//! `js-agent update-hook` (host-updates design, 4.2), the verb skeleton: what the command
//! line, the last line and the params file accept and refuse, how an outcome becomes an exit
//! status, and that the verb is answered before the agent installs a signal handler. The steps
//! have their bodies in `step` (tests on the fakes: steps_*.rs; `drain` and `resume` in
//! steps_drain.rs and steps_resume.rs over maintain_rig.rs). The hook record, its lock, the
//! params and the rules the record is held to are pure of the steps and tested here on their own.

mod args;
mod config_hook;
mod conformance;
mod conformance_runs;
mod drain_params;
mod fake_bin;
#[allow(dead_code)]
#[path = "../../update_hook_fakes/mod.rs"]
mod fakes;
mod fakes_tests;
mod harness_tests;
mod hostkeeper_runner;
mod launchd;
mod maintain_args;
mod maintain_conformance;
mod maintain_machine;
// The rig of the drain and resume tests: each test file uses part of it.
mod lease_port;
mod lease_port_transport;
mod lease_stub;
mod machine;
mod machine_flags;
mod machine_host_certificate;
mod machine_root;
#[allow(dead_code)]
mod maintain_rig;
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
mod steps_drain;
mod steps_helper;
mod steps_host_files;
mod steps_pf;
mod steps_prepare;
mod steps_prepare_cordon;
mod steps_prepare_first_dirs;
mod steps_prepare_supersede;
mod steps_resume;
mod steps_rollback;
mod steps_rollback_first;
mod steps_rollback_guards;
mod steps_taint_clearable;
mod steps_unit;
mod steps_verify;
mod unit_conformance;
mod unit_files;
mod validate;
mod verb;
