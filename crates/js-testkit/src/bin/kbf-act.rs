//! `kbf-act`: the act verbs and the argv0 fakes (`js_testkit::act`), for this crate's
//! own tests. It answers through [`js_testkit::act::hook`] first, as a test binary
//! that expands `act_test_hook!` does, so this crate's tests cover the hook's answer
//! (a process the hook ends before `main` writes no coverage data); run under another
//! name, it answers through `act::main`, as another crate's bin does.

fn main() -> std::process::ExitCode {
    js_testkit::act::hook();
    js_testkit::act::main()
}
