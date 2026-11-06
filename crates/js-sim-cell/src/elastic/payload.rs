//! What a launched machine reads from its launch payload: the join token it says hello
//! with and the host id it registers under, read from the boot script the server rendered
//! (`ElasticFake::user_data`) exactly as a test of the server reads them, never from a
//! side channel.
//!
//! The script writes the token to its file with `printf '%s' '<token>' > <path>`, the token
//! single-quoted for the shell (a quote inside it written `'\''`), and starts the agent
//! with `--host-id "<name>"`, the name double-quoted for the unit file.

/// The file the boot script writes the join token to.
pub const TOKEN_PATH: &str = "/etc/jobscheduler/token";

/// What a launched machine takes from its launch payload. It has no `Debug`: it holds the
/// join token.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload {
    /// The one-time join token: the bearer of the machine's agent.
    pub token: String,
    /// The host id the agent registers under: the machine's name.
    pub host: String,
}

impl Payload {
    /// The token and the host id written in `script`; `None` when it writes either not at
    /// all (a machine the server did not launch has no script).
    #[must_use]
    pub fn read(script: &str) -> Option<Self> {
        let token = script.lines().find_map(token_of)?;
        let head = "--host-id \"";
        let (_, rest) = script.split_once(head)?;
        let (host, _) = rest.split_once('"')?;
        let host = host.to_owned();
        Some(Self { token, host })
    }
}

/// The token a line of the script writes to [`TOKEN_PATH`], unquoted.
fn token_of(line: &str) -> Option<String> {
    let quoted = line.strip_prefix("printf '%s' ")?;
    let quoted = quoted.strip_suffix(&format!(" > {TOKEN_PATH}"))?;
    let inner = quoted.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("'\\''", "'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a reader that keeps the shell's quotes or its escape of a quote, reads the
    /// token from another file's line, or takes the host id with its quotes.
    #[test]
    fn the_token_and_the_host_id_are_read_from_the_script() {
        let script = "#!/bin/sh\n\
            printf '%s' 'other' > /etc/jobscheduler/key\n\
            printf '%s' 'it'\\''s-a-token' > /etc/jobscheduler/token\n\
            chmod 0600 -- /etc/jobscheduler/token\n\
            ExecStart=/usr/local/bin/js-agent --pool \"burst\" --host-id \"js-1-2\" --runtime crun\n";
        let read = Payload::read(script).expect("a payload");
        assert_eq!(read.token, "it's-a-token");
        assert_eq!(read.host, "js-1-2");
    }

    /// Catches: a reader that makes up a token or a host id for a script that writes none,
    /// or that takes a line that only starts like the token's.
    #[test]
    fn a_script_without_the_token_or_the_host_id_is_none() {
        assert!(Payload::read("").is_none());
        let unquoted = "printf '%s' token > /etc/jobscheduler/token\n--host-id \"h\"";
        assert!(Payload::read(unquoted).is_none());
        let half = "printf '%s' 'token > /etc/jobscheduler/token\n--host-id \"h\"";
        assert!(Payload::read(half).is_none());
        let no_host = "printf '%s' 'token' > /etc/jobscheduler/token\n";
        assert!(Payload::read(no_host).is_none());
        let open_host = "printf '%s' 'token' > /etc/jobscheduler/token\n--host-id \"h";
        assert!(Payload::read(open_host).is_none());
        let elsewhere = "printf '%s' 'token' > /etc/jobscheduler/key\n--host-id \"h\"";
        assert!(Payload::read(elsewhere).is_none());
    }
}
