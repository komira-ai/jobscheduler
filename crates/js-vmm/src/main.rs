//! `js-vmm`: see the library's documentation. On a platform other than macOS every
//! command except `--help` and `--version` refuses.

use std::process::ExitCode;

use clap::Parser;
use js_vmm::app;
use js_vmm::cli::Cli;
use js_vmm::probe::Timing;

fn main() -> ExitCode {
    let cli = Cli::parse();
    #[cfg(target_os = "macos")]
    let real = js_vmm::real::RealSystem::new();
    #[cfg(target_os = "macos")]
    let sys: Option<&dyn js_vmm::system::System> = Some(&real);
    #[cfg(not(target_os = "macos"))]
    let sys: Option<&dyn js_vmm::system::System> = None;
    app::run(
        cli,
        sys,
        Timing::real(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}
