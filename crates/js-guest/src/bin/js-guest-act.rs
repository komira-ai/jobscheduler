//! `js-guest-act`: the act verbs of `js_testkit::act`, the program this crate's
//! tests give the agent to run in place of `sh -c`. Test-only: not part of what ships.

fn main() -> std::process::ExitCode {
    js_testkit::act::main()
}
