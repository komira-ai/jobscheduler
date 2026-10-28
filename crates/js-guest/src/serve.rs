//! `kbf-guest serve`: binds the host's socket and serves the one command of the boot.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

use js_guest::server::{Agent, Config, HELLO_TIMEOUT};
use js_guest::session;
use js_guest::wire::TOKEN_LEN;

/// The exit status of `serve`: 0 once the host's command has run, 1 with the reason on
/// stderr otherwise.
pub(crate) fn serve_exit(served: Result<(), String>) -> ExitCode {
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-guest: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn serve(
    socket: &std::path::Path,
    token_file: &std::path::Path,
    inputs: PathBuf,
    outputs: PathBuf,
) -> Result<(), String> {
    let text = std::fs::read_to_string(token_file)
        .map_err(|e| format!("reading {}: {e}", token_file.display()))?;
    let token: [u8; TOKEN_LEN] = hex::decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| {
            format!(
                "{} does not hold {} hex digits",
                token_file.display(),
                TOKEN_LEN * 2
            )
        })?;
    serve_on(&bind(socket)?, token, inputs, outputs)
}

/// The listener on `socket`.
fn bind(socket: &std::path::Path) -> Result<UnixListener, String> {
    UnixListener::bind(socket).map_err(|e| format!("binding {}: {e}", socket.display()))
}

/// Serves the host's one command on `listener` until it has run.
fn serve_on(
    listener: &UnixListener,
    token: [u8; TOKEN_LEN],
    inputs: PathBuf,
    outputs: PathBuf,
) -> Result<(), String> {
    let mut agent = Agent::new(Config {
        token,
        inputs,
        outputs,
        session: session::manager_name(),
        hello_timeout: HELLO_TIMEOUT,
    });
    let served = agent.serve(listener);
    served.map_err(|e| format!("accepting: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches an agent that failed to serve exiting 0 (launchd and the host both read the
    /// status), and one that served the command exiting non-zero.
    #[test]
    fn serve_exits_by_whether_the_command_was_served() {
        assert_eq!(serve_exit(Ok(())), ExitCode::SUCCESS);
        assert_eq!(serve_exit(Err("no".to_owned())), ExitCode::FAILURE);
    }

    /// Catches a listener that cannot be bound being reported as success or a panic: the
    /// host would then wait on a socket nothing serves.
    #[test]
    fn bind_reports_a_socket_it_cannot_bind() {
        let dir = crate::test_dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-bind");
        let err = bind(&dir.join("no-such-dir").join("s.sock")).expect_err("cannot bind");
        assert!(err.starts_with("binding "), "{err}");
    }

    /// Catches a failed `accept` ending the agent as success: the host would be told the
    /// agent ran its command when it never listened. A listener that would block fails
    /// its `accept` at once.
    #[test]
    fn serve_reports_a_listener_that_cannot_accept() {
        let dir = crate::test_dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-accept");
        let listener = UnixListener::bind(dir.join("s.sock")).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let err = serve_on(
            &listener,
            [0; TOKEN_LEN],
            dir.to_path_buf(),
            dir.to_path_buf(),
        )
        .expect_err("accept fails");
        assert!(err.starts_with("accepting: "), "{err}");
    }
}
