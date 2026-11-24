//! The network under a profile: loopback under the off policy, name resolution, and port ranges.

use std::ffi::OsStr;
use std::net::TcpListener;

use super::Scratch;
use super::exec::{Mode, Net, Outcome, describe};
use crate::report::{Probe, Report};

/// A routable address used only to see whether an outbound connect is refused.
const EXTERNAL: &str = "1.1.1.1:443";

pub fn probes(scratch: &Scratch, report: &mut Report) {
    let places = &scratch.places;
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(err) => {
            report.push(Probe::asserted("net/listener", false).fact("error", err));
            return;
        }
    };
    let port = listener.local_addr().map_or(0, |a| a.port());
    let addr = format!("127.0.0.1:{port}");
    let connect = |mode: &Mode<'_>, to: &str| places.child(mode, "connect", OsStr::new(to));

    let off = Net::Off;
    let loopback = Net::Loopback;
    let control = connect(&Mode::Control, &addr);
    report.push(describe(
        Probe::asserted("net/control-loopback-connect", control.ok),
        "",
        &control,
    ));
    let denied = connect(&Mode::Sandboxed(&off, ""), &addr);
    let denied_probe = Probe::asserted("net/off-refuses-loopback", denied.refused());
    report.push(describe(denied_probe, "", &denied));
    let allowed = connect(&Mode::Sandboxed(&loopback, ""), &addr);
    report.push(describe(
        Probe::asserted("net/loopback-allows-loopback", allowed.ok),
        "",
        &allowed,
    ));

    // Reported: an outbound connect, and whether names still resolve under the off policy.
    // Only the observation is recorded; which rule stops a lookup is not established here
    // (the resolver daemon is reached over mach, and nothing below separates the causes).
    let outbound = |mode: &Mode<'_>| connect(mode, EXTERNAL);
    let external_control = outbound(&Mode::Control);
    let external_off = outbound(&Mode::Sandboxed(&off, ""));
    let probe = Probe::info("net/external-connect")
        .fact("target", EXTERNAL)
        .fact("control_connected", external_control.ok)
        .fact("off_connected", external_off.ok);
    report.push(describe(
        describe(probe, "control_", &external_control),
        "off_",
        &external_off,
    ));

    let resolve = |mode: &Mode<'_>, host: &str| places.child(mode, "resolve", OsStr::new(host));
    let mut dns = Probe::info("net/dns-under-off");
    for host in ["example.com", "localhost"] {
        let key = host.replace('.', "_");
        let outcomes: [(&str, Outcome); 3] = [
            ("control", resolve(&Mode::Control, host)),
            ("off", resolve(&Mode::Sandboxed(&off, ""), host)),
            ("loopback", resolve(&Mode::Sandboxed(&loopback, ""), host)),
        ];
        for (level, outcome) in &outcomes {
            dns = dns.fact(&format!("{key}_{level}_resolved"), outcome.ok);
            if !outcome.ok {
                dns = dns.fact(&format!("{key}_{level}_error"), outcome.stderr_tail());
            }
        }
    }
    report.push(dns);

    // Reported: does `localhost:lo-hi` compile, and is it honoured. The listener's port is
    // inside the first range and outside the second.
    let (lo, hi) = (port.saturating_sub(2).max(1), port.saturating_add(2));
    let (out_lo, out_hi) = if port < 65_000 {
        (port + 10, port + 20)
    } else {
        (port - 30, port - 20)
    };
    let inside = Net::Range(lo, hi);
    let outside = Net::Range(out_lo, out_hi);
    let in_range = connect(&Mode::Sandboxed(&inside, ""), &addr);
    let out_of_range = connect(&Mode::Sandboxed(&outside, ""), &addr);
    let probe = Probe::info("net/port-range")
        .fact("listener_port", port)
        .fact("range_with_port", format!("{lo}-{hi}"))
        .fact("range_without_port", format!("{out_lo}-{out_hi}"))
        .fact("connect_inside_range", in_range.ok)
        .fact("connect_outside_range", out_of_range.ok);
    report.push(describe(
        describe(probe, "inside_", &in_range),
        "outside_",
        &out_of_range,
    ));

    // Reported: single ports, the form a per-slot allow list would use instead of a range.
    let with_port = Net::Ports(vec![port]);
    let other_port = Net::Ports(vec![out_lo]);
    let port_allowed = connect(&Mode::Sandboxed(&with_port, ""), &addr);
    let port_other = connect(&Mode::Sandboxed(&other_port, ""), &addr);
    let probe = Probe::info("net/single-port")
        .fact("connect_to_allowed_port", port_allowed.ok)
        .fact("connect_with_only_another_port_allowed", port_other.ok);
    report.push(describe(
        describe(probe, "allowed_", &port_allowed),
        "other_",
        &port_other,
    ));
    drop(listener);
    host_port_probes(scratch, report);
}

/// The rule spellings a deny of one host port could take under `Loopback`, each a pair: the
/// rule for a connect to the port (`{P}`) and the rule for a listen on a free port (`{Q}`).
/// The last one puts the deny before the loopback grant, to show which of two matching
/// rules Seatbelt applies.
const PORT_DENIES: [(&str, &str, &str); 6] = [
    (
        "network_star_ip",
        "(deny network* (remote ip \"localhost:{P}\"))",
        "(deny network* (local ip \"*:{Q}\"))",
    ),
    (
        "outbound_bind_ip",
        "(deny network-outbound (remote ip \"localhost:{P}\"))",
        "(deny network-bind (local ip \"*:{Q}\"))",
    ),
    (
        "outbound_bind_tcp",
        "(deny network-outbound (remote tcp \"localhost:{P}\"))",
        "(deny network-bind (local tcp \"*:{Q}\"))",
    ),
    (
        "any_host",
        "(deny network* (remote ip \"*:{P}\"))",
        "(deny network* (local ip \"localhost:{Q}\"))",
    ),
    (
        "inbound_local",
        "(deny network-outbound (remote ip \"localhost:{P}\"))",
        "(deny network-inbound (local ip \"localhost:{Q}\"))",
    ),
    (
        "deny_first",
        "(deny network* (remote ip \"localhost:{P}\"))",
        "(deny network* (local ip \"*:{Q}\"))",
    ),
];

/// Reported: which spelling of a host-port deny Seatbelt honours under `Loopback`, for a
/// connect to a listener on 127.0.0.1 and on ::1, and a listen on a free port of each, with
/// another listener's port as the control that stays allowed.
fn host_port_probes(scratch: &Scratch, report: &mut Report) {
    let places = &scratch.places;
    let bound = |at: &str| TcpListener::bind(at).ok();
    let (Some(v4), Some(v6), Some(other)) =
        (bound("127.0.0.1:0"), bound("[::1]:0"), bound("127.0.0.1:0"))
    else {
        report.push(Probe::asserted("net/host-port-listeners", false));
        return;
    };
    let port_of = |l: &TcpListener| l.local_addr().map_or(0, |a| a.port());
    let free = bound("127.0.0.1:0").map_or(0, |l| port_of(&l));
    let (p4, p6, po) = (port_of(&v4), port_of(&v6), port_of(&other));
    let child = |mode: &Mode<'_>, name: &str, to: &str| places.child(mode, name, OsStr::new(to));
    let mut probe = Probe::info("net/host-port-deny")
        .fact("port_v4", p4)
        .fact("port_v6", p6)
        .fact("free_port", free);
    for (name, connect_rule, listen_rule) in PORT_DENIES {
        let rules = |port: u16| {
            [
                connect_rule.replace("{P}", &port.to_string()),
                listen_rule.replace("{Q}", &free.to_string()),
            ]
            .join("\n")
        };
        let (net, extra4, extra6) = if name == "deny_first" {
            let grant = "(allow network* (remote ip \"localhost:*\"))";
            (
                Net::Off,
                format!("{}\n{grant}", rules(p4)),
                format!("{}\n{grant}", rules(p6)),
            )
        } else {
            (Net::Loopback, rules(p4), rules(p6))
        };
        let to4 = child(
            &Mode::Sandboxed(&net, &extra4),
            "connect",
            &format!("127.0.0.1:{p4}"),
        );
        let to6 = child(
            &Mode::Sandboxed(&net, &extra6),
            "connect",
            &format!("[::1]:{p6}"),
        );
        let to_other = child(
            &Mode::Sandboxed(&net, &extra4),
            "connect",
            &format!("127.0.0.1:{po}"),
        );
        let listen4 = child(
            &Mode::Sandboxed(&net, &extra4),
            "listen",
            &format!("127.0.0.1:{free}"),
        );
        let listen6 = child(
            &Mode::Sandboxed(&net, &extra4),
            "listen",
            &format!("[::1]:{free}"),
        );
        let listen_any = child(
            &Mode::Sandboxed(&net, &extra4),
            "listen",
            &format!("0.0.0.0:{free}"),
        );
        let listen_zero = child(&Mode::Sandboxed(&net, &extra4), "listen", "127.0.0.1:0");
        for (what, outcome) in [
            ("connect_v4_denied_port", &to4),
            ("connect_v6_denied_port", &to6),
            ("connect_other_port", &to_other),
            ("listen_v4_denied_port", &listen4),
            ("listen_v6_denied_port", &listen6),
            ("listen_any_denied_port", &listen_any),
            ("listen_v4_port_0", &listen_zero),
        ] {
            probe = probe.fact(&format!("{name}_{what}"), outcome.ok);
            if !outcome.ok {
                probe = probe.fact(&format!("{name}_{what}_stderr"), outcome.stderr_tail());
            }
        }
    }
    report.push(probe);
    drop((v4, v6, other));
}
