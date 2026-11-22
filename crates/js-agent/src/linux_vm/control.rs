//! The VM owner's control socket: one line in, one line out (`ok ...` or `err ...`). After
//! the answer `ok` to `connect <port>` the connection becomes a byte stream to that port of
//! the guest's virtio socket. A `connect` or `start` that finds the VM stopped starts it, and
//! one that finds it starting or stopping waits, until the guest is ready.
//!
//! The socket is a file in the VM's directory, mode `0600`: only the owner's user (root when the agent runs it)
//! reaches it.

/// The control socket's file name in the VM's directory.
pub const SOCKET: &str = "control.sock";

/// The longest request line the owner reads.
pub const MAX_LINE: usize = 256;

/// One request on the control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Make the connection a stream to the guest's virtio socket port.
    Connect(u32),
    /// Set the memory balloon's target: the memory the guest may use, in MiB.
    Balloon(u64),
    /// The VM's state and the balloon's target.
    State,
    /// Start the VM, if it is not running, and answer once the guest is ready.
    Start,
    /// Ask the guest to shut down (the guest program's poweroff, else the power button), then
    /// stop the VM; the owner stays.
    Stop,
    /// Stop the VM at once, as pulling its power; the owner stays.
    Kill,
    /// Stop the VM as `stop` does, then end the owner.
    Exit,
}

/// The request on `line`.
///
/// # Errors
///
/// The line names no request, or its argument is not a number.
pub fn parse(line: &str) -> Result<Control, String> {
    let mut words = line.split_whitespace();
    let verb = words.next().ok_or("an empty request")?;
    let arg = words.next();
    if words.next().is_some() {
        return Err(format!("too many words in {line:?}"));
    }
    let number = |what: &str| -> Result<u64, String> {
        let text = arg.ok_or_else(|| format!("{verb} takes {what}"))?;
        text.parse::<u64>()
            .map_err(|_| format!("{verb}: {text:?} is not {what}"))
    };
    let none = |control: Control| match arg {
        None => Ok(control),
        Some(_) => Err(format!("{verb} takes no argument")),
    };
    match verb {
        "connect" => {
            let port = number("a port")?;
            u32::try_from(port)
                .map(Control::Connect)
                .map_err(|_| format!("connect: port {port} is too large"))
        }
        "balloon" => number("a size in MiB").map(Control::Balloon),
        "state" => none(Control::State),
        "start" => none(Control::Start),
        "stop" => none(Control::Stop),
        "kill" => none(Control::Kill),
        "exit" => none(Control::Exit),
        other => Err(format!("unknown request {other:?}")),
    }
}

/// `control` as a line, without its newline.
#[must_use]
pub fn render(control: Control) -> String {
    match control {
        Control::Connect(port) => format!("connect {port}"),
        Control::Balloon(mib) => format!("balloon {mib}"),
        Control::State => "state".to_owned(),
        Control::Start => "start".to_owned(),
        Control::Stop => "stop".to_owned(),
        Control::Kill => "kill".to_owned(),
        Control::Exit => "exit".to_owned(),
    }
}

/// The answer line: `Ok(rest)` for `ok rest`, `Err(why)` for `err why` or anything else.
///
/// # Errors
///
/// The owner refused, or the line is not an answer.
pub fn answer(line: &str) -> Result<String, String> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line == "ok" {
        return Ok(String::new());
    }
    if let Some(rest) = line.strip_prefix("ok ") {
        return Ok(rest.to_owned());
    }
    if let Some(why) = line.strip_prefix("err ") {
        return Err(why.to_owned());
    }
    Err(format!("not an answer: {line:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that every request reads back from its own line. Catches a request the client
    /// writes in a form the owner does not read.
    #[test]
    fn every_request_reads_back_from_its_line() {
        for control in [
            Control::Connect(7000),
            Control::Balloon(4096),
            Control::State,
            Control::Start,
            Control::Stop,
            Control::Kill,
            Control::Exit,
        ] {
            assert_eq!(parse(&render(control)), Ok(control));
        }
        assert_eq!(parse("  connect   2049 \n"), Ok(Control::Connect(2049)));
    }

    /// Asserts the refusal of each malformed line, by its exact message. Catches a request
    /// with a missing, extra or out-of-range argument that is carried out anyway.
    #[test]
    fn malformed_requests_are_refused_by_name() {
        let cases = [
            ("", "an empty request"),
            ("connect", "connect takes a port"),
            ("connect x", "connect: \"x\" is not a port"),
            (
                "connect 4294967296",
                "connect: port 4294967296 is too large",
            ),
            ("balloon -1", "balloon: \"-1\" is not a size in MiB"),
            ("state now", "state takes no argument"),
            ("stop 1", "stop takes no argument"),
            ("kill 9", "kill takes no argument"),
            ("start now", "start takes no argument"),
            ("exit 0", "exit takes no argument"),
            ("connect 1 2", "too many words in \"connect 1 2\""),
            ("reboot", "unknown request \"reboot\""),
        ];
        for (line, why) in cases {
            assert_eq!(parse(line), Err(why.to_owned()), "{line:?}");
        }
    }

    /// Asserts how answer lines read: `ok` with and without a rest, `err` with its reason,
    /// and anything else refused. Catches an `err` line read as success.
    #[test]
    fn answers_read_as_ok_or_err() {
        assert_eq!(answer("ok\n"), Ok(String::new()));
        assert_eq!(answer("ok running 8192\r\n"), Ok("running 8192".to_owned()));
        assert_eq!(answer("err no vm\n"), Err("no vm".to_owned()));
        assert_eq!(answer("okay"), Err("not an answer: \"okay\"".to_owned()));
    }
}
