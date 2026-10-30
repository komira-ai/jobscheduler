//! `js-mac-provision`: see the library's documentation.

fn main() -> std::process::ExitCode {
    js_mac_provision::cli::main_with(std::env::args_os())
}
