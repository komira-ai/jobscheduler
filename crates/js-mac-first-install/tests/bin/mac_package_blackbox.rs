//! `js-mac-package-blackbox`: the black-box tests of the js-agent macOS package, J2 (its first
//! install) and J4 then J3 (its update from the release before it, with tierstore's mount missing,
//! then served). One program, given the packages as files: it installs them as hostkeeper's
//! daemon does, on flags of the farm's Mac shape, beside a stand-in of what tierstore serves a
//! Mac, joins them to a lease server of this repository in its own process, and drives the agent
//! only from outside. The tests are in `tests/mac_package_blackbox/scenarios.rs`; what each
//! passes on is said there, in numbers.
//!
//! ```text
//! js-mac-package-blackbox first-install --scratch <dir> --pkg <.pkg> --version <X.Y.Z> [--unsigned]
//! js-mac-package-blackbox update --scratch <dir> --pkg <.pkg> --version <X.Y.Z> [--unsigned] \
//!     --previous-pkg <.pkg> --previous-version <X.Y.Z>
//! ```
//!
//! `--unsigned` says the package (not the previous one, which is always a release's) is one a
//! branch's run built: its signature check is answered as the first-install test answers it.
//!
//! It changes the machine it runs on for good (an account, a disk image, packages, launchd jobs,
//! files under `/etc`, `/var` and `/Library`), so it runs only as root on a disposable Mac: a
//! hosted runner of `.github/workflows/js-agent-macos-blackbox.yml`, one Mac for each command. A
//! failed test panics, and the program exits 101 after printing what the Mac showed. The program
//! builds on macOS only; elsewhere it says so and exits 2.

#[cfg(target_os = "macos")]
#[allow(dead_code)] // Shared with tests/mac_first_install.rs, which uses the rest of it.
#[path = "../mac_first_install/host.rs"]
mod host;
#[cfg(target_os = "macos")]
#[allow(dead_code)] // Shared with tests/mac_first_install.rs, which uses the rest of it.
#[path = "../mac_first_install/hostkeeper.rs"]
mod hostkeeper;
#[cfg(target_os = "macos")]
#[allow(dead_code)] // Shared with tests/mac_first_install.rs, which uses the rest of it.
#[path = "../mac_first_install/look.rs"]
mod look;

#[cfg(target_os = "macos")]
#[path = "../mac_package_blackbox/farm.rs"]
mod farm;
#[cfg(target_os = "macos")]
#[path = "../mac_package_blackbox/jobs.rs"]
mod jobs;
#[cfg(target_os = "macos")]
#[path = "../mac_package_blackbox/neighbours.rs"]
mod neighbours;
#[cfg(target_os = "macos")]
#[path = "../mac_package_blackbox/scenarios.rs"]
mod scenarios;
#[cfg(target_os = "macos")]
#[path = "../mac_package_blackbox/watch.rs"]
mod watch;

use std::process::ExitCode;

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    use clap::Parser as _;

    let cli = mac::Cli::parse();
    mac::run(&cli);
    ExitCode::SUCCESS
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("js-mac-package-blackbox: the tests install a macOS package; they run on macOS only");
    ExitCode::from(2)
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;

    use crate::scenarios::{self, Package};

    /// The command line.
    #[derive(clap::Parser)]
    #[command(name = "js-mac-package-blackbox")]
    pub struct Cli {
        #[command(subcommand)]
        test: Test,
    }

    /// The test to run.
    #[derive(clap::Subcommand)]
    enum Test {
        /// J2: the package's first install.
        FirstInstall {
            #[command(flatten)]
            package: PackageArgs,
        },
        /// J4 then J3: the update from the previous package, with tierstore's mount missing,
        /// then served.
        Update {
            #[command(flatten)]
            package: PackageArgs,
            /// The package of the release before it, as the package repository holds it.
            #[arg(long)]
            previous_pkg: PathBuf,
            /// Its version.
            #[arg(long)]
            previous_version: String,
        },
    }

    /// The package under test, and where the test keeps its files.
    #[derive(clap::Args)]
    struct PackageArgs {
        /// A new directory for the test's files (the lease server's, the disk image), root's.
        #[arg(long)]
        scratch: PathBuf,
        /// The package.
        #[arg(long)]
        pkg: PathBuf,
        /// Its version, MAJOR.MINOR.PATCH.
        #[arg(long)]
        version: String,
        /// The package is a branch's unsigned build, not the release's signed one.
        #[arg(long)]
        unsigned: bool,
    }

    impl PackageArgs {
        fn package(&self) -> Package {
            Package {
                pkg: self.pkg.clone(),
                version: self.version.clone(),
                signed: !self.unsigned,
            }
        }
    }

    /// Runs the test `cli` names; a failure panics.
    pub fn run(cli: &Cli) {
        match &cli.test {
            Test::FirstInstall { package } => {
                scenarios::first_install(&package.scratch, &package.package());
            }
            Test::Update {
                package,
                previous_pkg,
                previous_version,
            } => {
                let previous = Package {
                    pkg: previous_pkg.clone(),
                    version: previous_version.clone(),
                    signed: true,
                };
                scenarios::update(&package.scratch, &package.package(), &previous);
            }
        }
        println!("passed");
    }
}
