//! The probes, in the order the design lists them. Each is asserted (it has an expected
//! outcome and fails the run when that is wrong) or reported (its answer is unknown and
//! the facts are the result).

use std::io;
use std::path::{Path, PathBuf};

use crate::report::{Probe, Report};

mod clone;
mod exec;
mod net;
mod sandbox;
mod sys;
mod system;
mod toolchain;
mod usage;

use exec::Places;

/// Every probe, against scratch directories under `work`.
pub fn collect(work: &Path, home: &Path) -> Report {
    let mut report = Report::default();
    report.push(system::probe());
    match Scratch::new(work, home) {
        Ok(scratch) => {
            sandbox::probes(&scratch, &mut report);
            net::probes(&scratch, &mut report);
            clone::probes(&scratch, &mut report);
            usage::probes(&mut report);
            toolchain::probes(&scratch, &mut report);
            scratch.remove();
        }
        Err(err) => report.push(Probe::asserted("setup", false).fact("error", err)),
    }
    report
}

/// The directories one run uses, all canonical.
struct Scratch {
    places: Places,
    base: PathBuf,
    work: PathBuf,
    home: PathBuf,
    /// Outside the run directory: the control's counterpart of `places.run`.
    control: PathBuf,
}

impl Scratch {
    fn new(work: &Path, home: &Path) -> io::Result<Self> {
        let work = work.canonicalize()?;
        let home = home.canonicalize()?;
        let base = work.join(format!("js-mac-probe-{}", std::process::id()));
        let run = base.join("run");
        let input = run.join("in");
        let control = base.join("control");
        for dir in [&input, &control] {
            std::fs::create_dir_all(dir)?;
        }
        let temp = sys::confstr_path(sys::CS_USER_TEMP_DIR)?.canonicalize()?;
        let uid_entry = temp
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("no uid entry"))?;
        let exe = std::env::current_exe()?.canonicalize()?;
        let places = Places {
            exe,
            run: run.canonicalize()?,
            input: input.canonicalize()?,
            uid_entry,
        };
        Ok(Self {
            places,
            base,
            work,
            home,
            control: control.canonicalize()?,
        })
    }

    fn remove(&self) {
        // Best effort: the runner is discarded anyway.
        let _ = std::fs::remove_dir_all(&self.base);
    }
}
