//! `pf_probe <addr> [--timeout-ms N]`: one TCP connect to an IPv4 address, and what became of
//! it, printed as one line. The root test `pf_as_root` starts it as pool uids and as a control
//! uid, so that the connect is made by a process of that uid. Exit status: 0 when connected, 1
//! when not, 2 for a bad command line.

use std::process::ExitCode;

use js_mac_provision::probe::{Probe, parse_args, probe};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((addr, timeout)) = parse_args(&args) else {
        eprintln!("usage: pf_probe <ipv4-addr:port> [--timeout-ms N]");
        return ExitCode::from(2);
    };
    let outcome = probe(addr, timeout);
    println!("{outcome}");
    if outcome == Probe::Connected {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
