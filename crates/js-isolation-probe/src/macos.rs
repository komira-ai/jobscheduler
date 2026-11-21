//! The real host on a Mac: the job's account (its uid, and the `USER` and `HOME` the native
//! runtime sets), the system's places, the tools by their paths, and this program for the
//! background process. Everything it hands over is tested with a fake elsewhere; this is the
//! one place that names the real ones.

use std::ffi::CStr;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::expect::Pins;
use crate::host::{self, Who};
use crate::phase::{self, Host, Phase};
use crate::tools::System;

/// `confstr` name of the user's temporary directory under `/private/var/folders`.
const CS_DARWIN_USER_TEMP_DIR: libc::c_int = 65537;

/// How long one system tool may run before it is killed.
const TOOL_LIMIT: Duration = Duration::from_secs(30);
/// How long one connect may take.
const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// How many small files `probe` makes.
const MANY: u32 = 10_000;

pub(crate) fn run(phase: &Phase) -> ExitCode {
    match prepare().and_then(|(who, tools, pins)| {
        let host = Host {
            tools: &tools,
            who,
            writes: &host::writes,
            reads: host::reads(),
            connect_limit: CONNECT_LIMIT,
            many: MANY,
        };
        let mut out = std::io::stdout().lock();
        let passed = phase::run(phase, &host, &pins, &mut out).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        Ok(passed)
    }) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(why) => {
            eprintln!("js-isolation-probe: {why}");
            ExitCode::from(2)
        }
    }
}

fn prepare() -> Result<(Who, System, Pins), String> {
    let pins = Pins::committed().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().map_err(|e| format!("the working directory: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| format!("this program's path: {e}"))?;
    let uid = rustix::process::getuid().as_raw();
    let who = Who::from_env(uid, |k| std::env::var_os(k), cwd, user_temp())?;
    Ok((
        who,
        System {
            limit: TOOL_LIMIT,
            exe,
        },
        pins,
    ))
}

/// The user's `DARWIN_USER_TEMP_DIR`, when the system names one.
fn user_temp() -> Option<PathBuf> {
    let mut buf = [0u8; 1024];
    // SAFETY: the buffer is 1024 bytes and its length is passed with it; confstr writes at
    // most that many bytes, NUL included.
    let n = unsafe { libc::confstr(CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    let text = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
    Some(PathBuf::from(text.trim_end_matches('/')))
}
