//! The pure part of the inbound test: what a probe came to, what the three cases add up to,
//! the lines that are printed and the parser of what the listener prints. Nothing here
//! touches the machine, so ordinary tests run it on every platform.

use std::fmt;
use std::io;

/// What one TCP connect to the listener came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The connection was made.
    Connected,
    /// No answer within the connect bound (pf drops a blocked SYN silently).
    TimedOut,
    /// A reset came back (a host or a `block return` answered).
    Refused,
    /// Any other error; it is not shown to be pf's.
    Failed(String),
}

impl Probe {
    /// The outcome of a connect that returned `result`. A timeout and a would-block are
    /// both "no answer".
    pub fn from_connect(result: &io::Result<()>) -> Self {
        match result {
            Ok(()) => Self::Connected,
            Err(error) => match error.kind() {
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::TimedOut,
                io::ErrorKind::ConnectionRefused => Self::Refused,
                _ => Self::Failed(error.to_string()),
            },
        }
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => f.write_str("connected"),
            Self::TimedOut => f.write_str("timed out"),
            Self::Refused => f.write_str("refused"),
            Self::Failed(why) => write!(f, "failed: {why}"),
        }
    }
}

/// How far the blocking rule's packet counter moved around case a.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rise {
    /// The counter after the probe minus the counter before.
    Rose(u64),
    /// The counters could not be read, before or after.
    Unread(String),
}

/// Case a: the connect to the primary address with the anchor in force, and the counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseA {
    /// What the connect came to.
    pub probe: Probe,
    /// The blocking rule's counter around it.
    pub rise: Rise,
}

/// What the three cases add up to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Case a was blocked, proven by the counter, and b and c connected.
    Filtered,
    /// Case a connected: the anchor does not stop inbound traffic to the listener.
    NotFiltered,
    /// The cases do not settle it; each reason says which and why.
    CouldNotTell(Vec<String>),
}

/// The verdict of the three cases: a with the anchor in force to the primary address, b
/// with the anchor in force to 127.0.0.1, c with the anchor removed to the primary address.
pub fn verdict(a: &CaseA, b: &Probe, c: &Probe) -> Verdict {
    if a.probe == Probe::Connected {
        return Verdict::NotFiltered;
    }
    let mut why = Vec::new();
    if let Probe::Failed(error) = &a.probe {
        why.push(format!(
            "case a: failed with an error that is not a block by pf ({error})"
        ));
    }
    match &a.rise {
        Rise::Rose(0) => why.push(
            "case a: not connected, but the blocking rule's counter did not rise, so pf is not shown to have blocked it"
                .to_owned(),
        ),
        Rise::Rose(_) => {}
        Rise::Unread(error) => {
            why.push(format!("case a: the counters could not be read ({error})"));
        }
    }
    if *b != Probe::Connected {
        why.push(format!(
            "case b: the control to 127.0.0.1 did not connect ({b}), so the listener is not shown to be up"
        ));
    }
    if *c != Probe::Connected {
        why.push(format!(
            "case c: with the anchor removed the primary address did not connect ({c}), so the listener is not shown to be reachable there"
        ));
    }
    if why.is_empty() {
        Verdict::Filtered
    } else {
        Verdict::CouldNotTell(why)
    }
}

/// The one line printed for case a.
pub fn case_a_line(a: &CaseA) -> String {
    let counter = match &a.rise {
        Rise::Rose(n) => format!("+{n}"),
        Rise::Unread(why) => format!("unread ({why})"),
    };
    format!("case a: {}, counter {counter}", a.probe)
}

/// The one line printed for a control case (`b` or `c`).
pub fn control_line(case: char, probe: &Probe) -> String {
    format!("case {case}: {probe}")
}

/// The one final line of the test.
pub fn final_line(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Filtered => "inbound-by-uid: filtered".to_owned(),
        Verdict::NotFiltered => "inbound-by-uid: NOT filtered".to_owned(),
        Verdict::CouldNotTell(why) => format!("inbound-by-uid: could not tell: {}", why.join("; ")),
    }
}

/// The prefix of the lines the listener prints.
const PREFIX: &str = "INBOUND";

/// The line the listener prints for its own uid.
pub fn uid_line(uid: u32) -> String {
    format!("{PREFIX} uid={uid}")
}

/// The line the listener prints for the port it bound.
pub fn port_line(port: u16) -> String {
    format!("{PREFIX} port={port}")
}

/// What the listener reported once it was up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    /// The uid the listener runs as.
    pub uid: u32,
    /// The port it listens on.
    pub port: u16,
}

/// Reads the listener's output so far. Only lines that end in a newline are read, so a line
/// still being written is not mistaken for a bad one. `Ok(None)` when the uid line or the
/// port line is not there yet; an error when one is there with a value that is not a number.
pub fn parse_ready(text: &str) -> Result<Option<Ready>, String> {
    let mut uid = None;
    let mut port = None;
    for line in text.split_inclusive('\n').filter(|l| l.ends_with('\n')) {
        let line = line.trim_end();
        if let Some(value) = line.strip_prefix("INBOUND uid=") {
            uid = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| format!("not a uid: {line:?}"))?,
            );
        } else if let Some(value) = line.strip_prefix("INBOUND port=") {
            port = Some(
                value
                    .parse::<u16>()
                    .map_err(|_| format!("not a port: {line:?}"))?,
            );
        }
    }
    Ok(uid.zip(port).map(|(uid, port)| Ready { uid, port }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(probe: Probe, rise: Rise) -> CaseA {
        CaseA { probe, rise }
    }

    #[test]
    fn a_blocked_primary_address_with_the_controls_connected_is_filtered() {
        for blocked in [Probe::TimedOut, Probe::Refused] {
            assert_eq!(
                verdict(
                    &a(blocked, Rise::Rose(3)),
                    &Probe::Connected,
                    &Probe::Connected
                ),
                Verdict::Filtered
            );
        }
    }

    #[test]
    fn a_connection_to_the_primary_address_under_the_anchor_is_not_filtered() {
        assert_eq!(
            verdict(
                &a(Probe::Connected, Rise::Rose(0)),
                &Probe::Connected,
                &Probe::Connected
            ),
            Verdict::NotFiltered
        );
        // Whatever the controls came to: the listener was reached through the anchor.
        assert_eq!(
            verdict(
                &a(Probe::Connected, Rise::Unread("x".to_owned())),
                &Probe::TimedOut,
                &Probe::TimedOut
            ),
            Verdict::NotFiltered
        );
    }

    #[test]
    fn a_block_with_no_counter_rise_is_unproven() {
        assert_eq!(
            verdict(
                &a(Probe::TimedOut, Rise::Rose(0)),
                &Probe::Connected,
                &Probe::Connected
            ),
            Verdict::CouldNotTell(vec![
                "case a: not connected, but the blocking rule's counter did not rise, so pf is not shown to have blocked it"
                    .to_owned()
            ])
        );
    }

    #[test]
    fn counters_that_could_not_be_read_are_unproven() {
        assert_eq!(
            verdict(
                &a(Probe::TimedOut, Rise::Unread("pfctl failed".to_owned())),
                &Probe::Connected,
                &Probe::Connected
            ),
            Verdict::CouldNotTell(vec![
                "case a: the counters could not be read (pfctl failed)".to_owned()
            ])
        );
    }

    #[test]
    fn an_error_that_is_not_a_timeout_or_a_reset_is_not_a_block() {
        assert_eq!(
            verdict(
                &a(Probe::Failed("no route".to_owned()), Rise::Rose(2)),
                &Probe::Connected,
                &Probe::Connected
            ),
            Verdict::CouldNotTell(vec![
                "case a: failed with an error that is not a block by pf (no route)".to_owned()
            ])
        );
    }

    #[test]
    fn a_control_that_did_not_connect_leaves_the_block_unexplained() {
        assert_eq!(
            verdict(
                &a(Probe::TimedOut, Rise::Rose(1)),
                &Probe::Refused,
                &Probe::TimedOut
            ),
            Verdict::CouldNotTell(vec![
                "case b: the control to 127.0.0.1 did not connect (refused), so the listener is not shown to be up"
                    .to_owned(),
                "case c: with the anchor removed the primary address did not connect (timed out), so the listener is not shown to be reachable there"
                    .to_owned(),
            ])
        );
    }

    #[test]
    fn the_final_line_is_one_of_three() {
        assert_eq!(final_line(&Verdict::Filtered), "inbound-by-uid: filtered");
        assert_eq!(
            final_line(&Verdict::NotFiltered),
            "inbound-by-uid: NOT filtered"
        );
        assert_eq!(
            final_line(&Verdict::CouldNotTell(vec![
                "one".to_owned(),
                "two".to_owned()
            ])),
            "inbound-by-uid: could not tell: one; two"
        );
    }

    #[test]
    fn the_case_lines_name_the_outcome_and_the_counter() {
        assert_eq!(
            case_a_line(&a(Probe::TimedOut, Rise::Rose(3))),
            "case a: timed out, counter +3"
        );
        assert_eq!(
            case_a_line(&a(Probe::Connected, Rise::Rose(0))),
            "case a: connected, counter +0"
        );
        assert_eq!(
            case_a_line(&a(Probe::Refused, Rise::Unread("no pfctl".to_owned()))),
            "case a: refused, counter unread (no pfctl)"
        );
        assert_eq!(control_line('b', &Probe::Connected), "case b: connected");
        assert_eq!(
            control_line('c', &Probe::Failed("no route".to_owned())),
            "case c: failed: no route"
        );
    }

    #[test]
    fn a_connect_result_becomes_a_probe_outcome() {
        let err = |kind| Probe::from_connect(&Err(io::Error::from(kind)));
        assert_eq!(Probe::from_connect(&Ok(())), Probe::Connected);
        assert_eq!(err(io::ErrorKind::TimedOut), Probe::TimedOut);
        assert_eq!(err(io::ErrorKind::WouldBlock), Probe::TimedOut);
        assert_eq!(err(io::ErrorKind::ConnectionRefused), Probe::Refused);
        assert_eq!(
            Probe::from_connect(&Err(io::Error::other("no route"))),
            Probe::Failed("no route".to_owned())
        );
    }

    #[test]
    fn the_listener_is_ready_when_both_lines_are_whole() {
        let text = format!("running 1 test\n{}\n{}\n", uid_line(501), port_line(40000));
        let ready = parse_ready(&text).unwrap().unwrap();
        assert_eq!((ready.uid, ready.port), (501, 40000));
    }

    #[test]
    fn the_listener_is_not_ready_before_both_lines_or_with_a_line_half_written() {
        assert_eq!(parse_ready("").unwrap(), None);
        assert_eq!(parse_ready("INBOUND uid=501\n").unwrap(), None);
        assert_eq!(
            parse_ready("INBOUND uid=501\nINBOUND port=400").unwrap(),
            None
        );
    }

    #[test]
    fn a_line_with_a_value_that_is_not_a_number_is_an_error() {
        assert_eq!(
            parse_ready("INBOUND uid=abc\n").unwrap_err(),
            "not a uid: \"INBOUND uid=abc\""
        );
        assert_eq!(
            parse_ready("INBOUND port=70000\n").unwrap_err(),
            "not a port: \"INBOUND port=70000\""
        );
    }
}
