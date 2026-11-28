//! `js-fake-mount-provider`: see the library docs.

fn main() -> std::process::ExitCode {
    js_fake_mount_provider::main(std::env::args_os().skip(1))
}
