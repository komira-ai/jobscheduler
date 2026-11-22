//! `js-agent __mac-job-start --start <file>`: the start of a native job, in a process of its own.
//!
//! The agent cannot apply a Seatbelt profile between `fork` and `exec`: the library allocates,
//! and an agent with an account pool (OpenDirectory runs threads of its own), a tree client or a
//! session has threads, one of which may hold the allocator's lock in the child. So the agent
//! starts its own program in this mode instead, with the job's environment, working directory
//! and streams, and the mode, a new process with one thread, does what the child would have:
//!
//! 1. it reads the start file ([`StartFile`], written by the agent, root's and readable by root
//!    alone) and removes it;
//! 2. with `new_session`, it opens a new audit session and writes its id to `asid_file`;
//! 3. it drops to the job's user (its primary group as its only group) for good, when it runs as
//!    root (a test runs it as itself);
//! 4. it applies the profile with `sandbox_init_with_parameters`, through the agent's one call
//!    of it (`crate::job_start`), on the C strings made of the file's text and parameters;
//! 5. it executes the job's program in its own place, with the agent's responsibility kept (never
//!    disclaimed), so the program answers to the privacy controls as the agent that started this
//!    process.
//!
//! The pid the agent started is the job's program's from then on. A step that fails writes why to
//! standard error (the job's) and exits [`START_FAILED`] without executing anything.

use serde::{Deserialize, Serialize};

/// The mode word.
pub const VERB: &str = "__mac-job-start";

/// The status of a start that failed before the program was executed.
pub const START_FAILED: u8 = 125;

/// What the start of one job needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartFile {
    /// The user the job runs as.
    pub uid: u32,
    /// Its primary group, its only group.
    pub gid: u32,
    /// Whether to open a new audit session.
    pub new_session: bool,
    /// Where the new session's id is written, as decimal text.
    pub asid_file: String,
    /// The profile text.
    pub profile: String,
    /// The profile's parameters, by name.
    pub params: Vec<(String, String)>,
    /// The program, an absolute path.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

impl StartFile {
    /// Decodes a start file. Every string must be free of NUL, which no C call can take.
    ///
    /// # Errors
    /// The text is not a start file, or a string holds a NUL.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: Self =
            serde_json::from_str(text).map_err(|why| format!("the start file: {why}"))?;
        let strings = [&file.asid_file, &file.profile, &file.program]
            .into_iter()
            .chain(file.args.iter())
            .chain(file.params.iter().flat_map(|(k, v)| [k, v]));
        if strings.into_iter().any(|s| s.contains('\0')) {
            return Err("the start file holds a NUL".to_owned());
        }
        Ok(file)
    }
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
