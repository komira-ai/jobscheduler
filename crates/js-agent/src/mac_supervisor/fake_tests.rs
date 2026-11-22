//! The fake launcher: it runs its shell text with the request's environment, directory and
//! streams, in a group of its own, and writes the records the supervisor would.

use std::fs;
use std::io::{PipeWriter, Write as _};
use std::path::Path;
use std::process::Stdio;

use js_testkit::dir::TestDir;

use super::FakeLauncher;
use crate::mac_supervisor::launch::{LaunchRequest, Launcher, Supervised};
use crate::mac_supervisor::record::{EXIT, Ended, ExitRecord, READY, parse_exit, parse_ready};
use crate::mac_supervisor::spec::tests::spec;

/// The exit record, once the run has ended; looks again until it has (the shell ends on its
/// own, so this is a wait, not a timeout).
fn ended(run: &mut dyn Supervised) -> ExitRecord {
    std::iter::repeat_with(|| {
        std::thread::yield_now();
        run.try_exit().unwrap()
    })
    .flatten()
    .next()
    .unwrap()
}

/// A request for `program` with `args` in `dir`, whose records go to `<dir>/records` (made when
/// missing), input is `stdin` and output `<dir>/out`.
fn request<'a>(
    dir: &'a Path,
    env: &'a [(String, String)],
    program: &str,
    args: &[&str],
    stdin: Stdio,
) -> LaunchRequest<'a> {
    let records = dir.join("records");
    fs::create_dir_all(&records).unwrap();
    let mut spec = spec(&records);
    spec.program = program.to_owned();
    spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    LaunchRequest {
        spec,
        env,
        cwd: dir,
        stdio: [
            stdin,
            fs::File::create(dir.join("out")).unwrap().into(),
            Stdio::null(),
        ],
    }
}

/// The shell gets its arguments after its text, the environment given and no other, and the
/// working directory; the ready record names its pid, with no session, and it leads its own
/// process group while it runs; `try_exit` answers `None` until it ends, then its exit record,
/// the same each time and the same as the file. Catches a fake whose records differ from the
/// supervisor's, which the runtime's tests would then not prove anything about.
#[test]
fn the_fake_launcher_runs_its_text_and_writes_the_records() {
    let owner = TestDir::under(std::env::temp_dir(), "mac-supervisor-fake");
    let dir = fs::canonicalize(owner.path()).unwrap();
    let env = [("JS_MARK".to_owned(), "marked".to_owned())];
    let (reader, mut writer): (_, PipeWriter) = std::io::pipe().unwrap();
    let text = r#"read line; printf '%s %s %s\n' "$JS_MARK" "$HOME" "$(pwd)"; exit "$1""#;
    let request = request(&dir, &env, text, &["5"], reader.into());
    let mut run = FakeLauncher.launch(request).unwrap();

    let ready = run.wait_ready().unwrap();
    assert_eq!(u32::try_from(ready.job_pid).unwrap(), run.pid());
    assert_eq!(ready.asid, None);
    let records = dir.join("records");
    let on_disk = fs::read_to_string(records.join(READY)).unwrap();
    assert_eq!(parse_ready(&on_disk), Some(ready));
    let pid = rustix::process::Pid::from_raw(ready.job_pid).unwrap();
    assert_eq!(rustix::process::getpgid(Some(pid)).unwrap(), pid);
    assert_eq!(run.try_exit().unwrap(), None);
    assert_eq!(run.record_dir(), records);

    writer.write_all(b"go\n").unwrap();
    let record = ended(&mut *run);
    let want = ExitRecord {
        ended: Ended::Exited(5),
        job_pid: Some(ready.job_pid),
        asid: None,
    };
    assert_eq!(record, want);
    assert_eq!(run.try_exit().unwrap(), Some(want.clone()));
    let on_disk = fs::read_to_string(records.join(EXIT)).unwrap();
    assert_eq!(parse_exit(&on_disk), Some(want));
    assert_eq!(
        fs::read_to_string(dir.join("out")).unwrap(),
        format!("marked  {}\n", dir.display())
    );
}

/// A shell killed by a signal is recorded so; a record directory that holds records already
/// stops the launch before anything runs. Catches a fake that reads a signal as an exit, or
/// writes over an earlier start's records.
#[test]
fn the_fake_launcher_records_a_signal_and_refuses_old_records() {
    let dir = TestDir::under(std::env::temp_dir(), "mac-supervisor-fake-signal");
    let mut run = FakeLauncher
        .launch(request(dir.path(), &[], "kill -9 $$", &[], Stdio::null()))
        .unwrap();
    assert_eq!(ended(&mut *run).ended, Ended::Signalled(9));

    let again = request(dir.path(), &[], "exit 0", &[], Stdio::null());
    let refused = FakeLauncher.launch(again).err().unwrap();
    assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
}
