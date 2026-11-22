//! The launchd domains of a uid, judged by their outcome: a `launchctl bootout` is asked for
//! and its failure only logged; `launchctl print` is read as a domain that is there or not,
//! and no exit status of it is matched. A domain is gone when the print exits non-zero, or
//! exits 0 and prints a domain that holds nothing: `launchctl print user/<uid>` exits 0 for
//! any uid (the print makes the user domain), so for a user domain "gone" is an empty one,
//! with no service and no unmanaged process ([`holds_nothing`]).

use std::io;

use super::host::{AccountHost, PrintAnswer};

/// The launchd domains of `uid` that are booted out and judged, in order: its login
/// session's and its user domain. A uid that never logged in has no `gui` domain, and that
/// reads as gone.
pub(super) fn domains(uid: u32) -> [String; 2] {
    [format!("gui/{uid}"), format!("user/{uid}")]
}

/// Whether the text of a successful `launchctl print <domain>` is that of a domain that
/// holds nothing: its own `services = {` block and its own `unmanaged processes = {` block,
/// the domain's top-level properties (one tab deep, not an endpoint's or a nested block's),
/// are both empty. A block with anything in it, or one that cannot be found, is a domain
/// that stays.
fn holds_nothing(stdout: &str) -> bool {
    let lines: Vec<&str> = stdout.lines().map(str::trim_end).collect();
    ["\tservices = {", "\tunmanaged processes = {"]
        .iter()
        .all(|header| {
            lines
                .iter()
                .position(|line| line == header)
                .is_some_and(|at| lines.get(at + 1) == Some(&"\t}"))
        })
}

/// Whether `launchctl print` printed a domain that stays (`answer`): a non-zero exit status
/// is a domain launchd does not print (no exit status is matched as "not found", because
/// launchd's answer for a domain it does not have varies: 112, 113 and 125 have each been
/// seen for the `gui/<uid>` of a user that never logged in). Exit 0 is a domain launchd
/// has, unless what it printed holds nothing ([`holds_nothing`]).
///
/// # Errors
/// A signal ended `launchctl`, so it gave no answer.
fn printed(answer: &PrintAnswer) -> io::Result<bool> {
    match answer.code {
        Some(code) => Ok(code == 0 && !holds_nothing(&answer.stdout)),
        None => Err(io::Error::other(format!(
            "launchctl print was ended by a signal: {}",
            answer.stderr.trim()
        ))),
    }
}

/// `launchctl bootout` of each of `domains`. A bootout's own exit status is no verdict (it
/// fails for a domain that is not there, and can fail after it ended the domain): each
/// failure is logged and returned with its domain, for the report of a domain that still
/// prints.
pub(super) fn boot_out(host: &dyn AccountHost, domains: &[String]) -> Vec<(String, String)> {
    let mut refused = Vec::new();
    for domain in domains {
        if let Err(why) = host.bootout(domain) {
            tracing::info!("launchctl bootout {domain} (the outcome decides): {why}");
            refused.push((domain.clone(), why.to_string()));
        }
    }
    refused
}

/// Those of `domains` that are not known to be gone: each that `launchctl print` printed
/// (with why its bootout failed, from `refused`, if it did), or that it gave no answer for
/// (with why).
pub(super) fn still_printing(
    host: &dyn AccountHost,
    domains: &[String],
    refused: &[(String, String)],
) -> Vec<String> {
    let mut left = Vec::new();
    for domain in domains {
        match host
            .launchctl_print(domain)
            .and_then(|answer| printed(&answer))
        {
            Ok(false) => {}
            Ok(true) => left.push(match refused.iter().find(|(d, _)| d == domain) {
                Some((_, why)) => format!("{domain} (bootout: {why})"),
                None => domain.clone(),
            }),
            Err(why) => left.push(format!("{domain} (launchctl print: {why})")),
        }
    }
    left
}

#[cfg(test)]
mod tests;
