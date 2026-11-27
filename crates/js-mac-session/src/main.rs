//! `js-mac-session`: see the library's documentation.

use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    // The mode `run` in a GUI session executes this binary in (`macos::gui`), as
    // root under `launchctl asuser`.
    #[cfg(target_os = "macos")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if let Some((mode, rest)) = args.split_first()
            && mode == js_mac_session::macos::gui::AS_USER
        {
            return js_mac_session::macos::gui::as_user(rest);
        }
    }
    js_mac_session::start::main(&js_mac_session::start::Args::parse())
}
