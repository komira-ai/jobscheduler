//! The black-box test of the released js-agent and js-server: J1 (the agent's session and a
//! job) and J6 (the server's readiness and a job), and the failures that belong to the same rig
//! (the server restarted, the agent updated, the server unreachable and its name not found, each
//! while a job runs).
//!
//! The released artifacts, taken as the farm takes them and driven only from outside:
//!
//! - the js-agent `.deb` of the release, installed with apt from the suite `testing` of the
//!   project's repository, on a host made ready as a farm host is, with the flags file of the
//!   farm's shape (`farm/js-agent.flags`), and run by the package's own unit,
//!   `js-agent-crun.service`;
//! - the js-server image of the same release (the js-lease-server OCI image of the GitHub
//!   release, checked against its `SHA256SUMS`), loaded into Docker and started with the control
//!   host's command line (`farm/js-server.args`).
//!
//! Their neighbours are fakes, each a process of its own speaking the real protocol: a registry
//! on the loopback serving the job image (the runner's own `sleep`), a mount provider on the
//! socket the farm's flags name for tierstore's (`jobscheduler.provider.v1`), and, for the
//! server's store, the server's own SQLite store in place of Postgres. The test reaches the
//! services through the job service's API, the server's readiness route, the agent's health
//! route, the unit and the jobs it runs. The pass conditions are the constants of the library
//! (`js_blackbox`), measured on the system's monotonic clock with a probe every second.
//!
//! This binary has no libtest harness. Its first argument is a mode:
//!
//! - `rig --version X.Y.Z --fake-provider PATH`: the wrapper (wrapper.rs) installs the release, starts the fakes and
//!   runs `scenarios` with flags that name them; as root on a disposable host only
//!   (.github/workflows/js-blackbox.yml);
//! - `scenarios ...`: the test itself (scenarios.rs), given what was installed and where each
//!   fake listens; it says PASS, FAIL or NOT RUN for each scenario and exits 1 when any did not
//!   pass;
//! - `fake-registry ...`: the fake registry (image.rs). The fake mount provider is a program of
//!   its own, `js-fake-mount-provider` (crates/js-fake-mount-provider), which the wrapper is
//!   given.
//!
//! Run with no mode, or with libtest's arguments (as `cargo test` and cargo-nextest run every
//! test binary of the workspace), it holds no test: it lists none and says so, and exits 0.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
#[path = "released/agent.rs"]
mod agent;
#[cfg(target_os = "linux")]
#[path = "released/host.rs"]
mod host;
#[cfg(target_os = "linux")]
#[path = "released/image.rs"]
mod image;
#[cfg(target_os = "linux")]
#[path = "released/scenarios.rs"]
mod scenarios;
#[cfg(target_os = "linux")]
#[path = "released/server.rs"]
mod server;
#[cfg(target_os = "linux")]
#[path = "released/wrapper.rs"]
mod wrapper;

/// The modes, the first argument of each.
const MODES: [&str; 3] = ["rig", "scenarios", "fake-registry"];

/// What a run without a mode says.
const NO_MODE: &str = "released: no tests here; the black-box test runs as `released rig --version X.Y.Z`, as root on a disposable host (.github/workflows/js-blackbox.yml)";

fn main() -> ExitCode {
    let first = std::env::args().nth(1);
    if !first.as_deref().is_some_and(|m| MODES.contains(&m)) {
        // `--list` (cargo-nextest) wants the names of the tests, one a line: there are none.
        if !std::env::args().any(|a| a == "--list") {
            eprintln!("{NO_MODE}");
        }
        return ExitCode::SUCCESS;
    }
    run_mode()
}

#[cfg(not(target_os = "linux"))]
fn run_mode() -> ExitCode {
    eprintln!("released: the black-box test runs on Linux only");
    ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn run_mode() -> ExitCode {
    use clap::Parser;

    #[derive(Debug, clap::Parser)]
    #[command(name = "released")]
    struct Cli {
        #[command(subcommand)]
        mode: Mode,
    }

    #[derive(Debug, clap::Subcommand)]
    enum Mode {
        /// Install the release, start the fakes, and run the scenarios.
        Rig(wrapper::Flags),
        /// Run the scenarios against what is installed and the fakes named.
        Scenarios(Box<scenarios::Flags>),
        /// Serve the job image on the loopback.
        FakeRegistry(image::Flags),
    }

    match Cli::parse().mode {
        Mode::Rig(flags) => wrapper::run_rig(&flags),
        Mode::Scenarios(flags) => scenarios::run_all(*flags),
        Mode::FakeRegistry(flags) => {
            image::serve(&flags);
            ExitCode::SUCCESS
        }
    }
}

/// 32 random bytes from the kernel, in hex: a token or a password of the test's.
#[cfg(target_os = "linux")]
fn secret() -> String {
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(
        &mut std::fs::File::open("/dev/urandom").expect("open /dev/urandom"),
        &mut bytes,
    )
    .expect("read /dev/urandom");
    hex::encode(bytes)
}

/// A port no one listens on, on the loopback, picked by the kernel.
#[cfg(target_os = "linux")]
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("a free port")
        .port()
}

/// Runs `body` with a watch on the system's monotonic clock, probing every
/// [`js_blackbox::PROBE_EVERY`].
#[cfg(target_os = "linux")]
fn watch<R>(body: impl FnOnce(&mut js_blackbox::Watch<'_>) -> R) -> R {
    let clock = js_system_clock::SystemMonotonic::default();
    let mut pause = std::thread::sleep;
    let mut watch = js_blackbox::Watch::new(&clock, &mut pause, js_blackbox::PROBE_EVERY);
    body(&mut watch)
}
