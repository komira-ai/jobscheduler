//! What a launched machine reads from its launch payload: the host id it registers under,
//! read from the boot script the server rendered (`ElasticFake::user_data`) exactly as a
//! test of the server reads it, never from a side channel. The machine presents the host
//! certificate of that name: the payload carries no token.
//!
//! The script starts the agent with `--host-id "<name>"`, the name double-quoted for the
//! unit file.

/// What a launched machine takes from its launch payload.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload {
    /// The host id the agent registers under: the machine's name.
    pub host: String,
}

impl Payload {
    /// The host id written in `script`; `None` when it writes none (a machine the server did
    /// not launch has no script).
    #[must_use]
    pub fn read(script: &str) -> Option<Self> {
        let head = "--host-id \"";
        let (_, rest) = script.split_once(head)?;
        let (host, _) = rest.split_once('"')?;
        let host = host.to_owned();
        Some(Self { host })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a reader that takes the host id with its quotes, or from another flag.
    #[test]
    fn the_host_id_is_read_from_the_script() {
        let script = "#!/bin/sh\n\
            ExecStart=/usr/local/bin/js-agent --pool \"burst\" --host-id \"js-1-2\"\n";
        let read = Payload::read(script).expect("a payload");
        assert_eq!(read.host, "js-1-2");
    }

    /// Catches: a reader that makes up a host id for a script that names none, or for one
    /// whose quote never closes.
    #[test]
    fn a_script_without_the_host_id_is_none() {
        assert!(Payload::read("").is_none());
        assert!(Payload::read("--host-id \"h").is_none());
    }
}
