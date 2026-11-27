//! `js-agent config-hook`: the hook of a js-agent config package. The command line, the params,
//! the steps over fakes of the unit manager, the lease server and the clock, the steps on a
//! machine with a real lease server, and the verb as the program answers it.

mod args;
mod machine;
mod params;
mod rig;
mod steps_cleanup;
mod steps_cutover;
mod steps_kept;
mod steps_kept_descriptor;
mod steps_rollback;
mod steps_verify;
mod verb;
