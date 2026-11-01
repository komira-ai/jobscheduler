//! What the tests of `enroll` share: a host whose files are in a directory of the test, and a
//! stand-in for the packaged setup script that records how it was called.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;

use js_agent::enroll::{Host, run};
use js_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

/// The provider and instance of the host being enrolled.
pub const PROVIDER: &str = "burst";
pub const INSTANCE: &str = "i-0abc";
/// The instance file the host gets.
pub const LINE: &str = "burst:i-0abc\n";

/// A join token as the server mints it: 64 lower-case hex digits.
pub const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// Another one.
pub const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// The sha256 of `text` as 64 lower-case hex digits, computed here and not by the verb.
pub fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The debug text of the exit status `n`.
pub fn status(n: u8) -> String {
    format!("{:?}", ExitCode::from(n))
}

/// What a run printed and ended with.
#[derive(Debug)]
pub struct Ran {
    pub status: String,
    pub out: String,
    pub err: String,
}

/// A host for one test: `etc/` holds the instance file and the token file, and `setup.sh`
/// is the stand-in script. The directory is removed when the rig drops.
pub struct Rig {
    pub dir: TestDir,
    pub host: Host,
}

impl Rig {
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-agent"),
            name,
        );
        let etc = dir.path().join("etc");
        fs::create_dir(&etc).unwrap();
        // The verb refuses a directory that group or others can write; the umask of the
        // machine running the test must not decide that.
        for path in [dir.path(), etc.as_path()] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let host = Host {
            euid: 0,
            owner: rustix::process::geteuid().as_raw(),
            stdin_is_terminal: false,
            instance_file: etc.join("instance"),
            token_file: etc.join("token"),
            setup_script: dir.path().join("setup.sh"),
            shell: PathBuf::from("/usr/bin/bash"),
        };
        let rig = Self { dir, host };
        rig.script(0);
        rig
    }

    /// The directory of the instance file and the token file.
    pub fn etc(&self) -> PathBuf {
        self.dir.path().join("etc")
    }

    /// Installs the stand-in setup script. It records its arguments and the names in `etc/`
    /// at the time it ran, prints a line to each stream, and exits with `status`.
    pub fn script(&self, status: i32) {
        self.script_with(&format!("exit {status}"));
    }

    /// The stand-in script with `last` as its last command.
    pub fn script_with(&self, last: &str) {
        // The verb runs the script as `/proc/self/fd/N`, so `$0` names nothing: the paths
        // the script writes to are in its text.
        let me = self.host.setup_script.display();
        let dir = self.dir.path().display();
        let text = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{me}.args'\nls '{dir}/etc' > '{me}.seen'\n\
             echo 'stand-in setup output'\necho 'stand-in setup complaint' >&2\n{last}\n"
        );
        let path = &self.host.setup_script;
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The path the stand-in script can write the file `suffix` to, beside itself.
    pub fn beside_script(&self, suffix: &str) -> String {
        format!("{}.{suffix}", self.host.setup_script.display())
    }

    /// The arguments the stand-in script was last called with; `None` if it never ran.
    pub fn setup_args(&self) -> Option<Vec<String>> {
        let path = format!("{}.args", self.host.setup_script.display());
        let text = fs::read_to_string(path).ok()?;
        Some(
            text.lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    /// The names that were in `etc/` when the stand-in script last ran.
    pub fn seen_by_setup(&self) -> Vec<String> {
        let path = format!("{}.seen", self.host.setup_script.display());
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The names in `etc/`, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.etc())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Writes `etc/<name>` with `text` and `mode`.
    pub fn put(&self, name: &str, text: &str, mode: u32) {
        let path = self.etc().join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The text of `etc/<name>`.
    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.etc().join(name)).unwrap()
    }

    /// The permission bits of `etc/<name>`.
    pub fn mode(&self, name: &str) -> u32 {
        fs::metadata(self.etc().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    /// Runs the verb for the host `PROVIDER:INSTANCE` with `extra` flags and `stdin` as
    /// its standard input. A join token must be in neither of the streams it printed.
    pub fn go(&self, extra: &[&str], stdin: &str) -> Ran {
        let mut words = vec!["--provider", PROVIDER, "--instance", INSTANCE];
        words.extend(extra);
        let ran = self.run(&words, stdin);
        for secret in [TOKEN, OTHER] {
            assert!(
                !ran.out.contains(secret) && !ran.err.contains(secret),
                "{ran:?}"
            );
        }
        ran
    }

    /// Runs the verb with exactly these words.
    pub fn run(&self, words: &[&str], stdin: &str) -> Ran {
        let mut args: Vec<std::ffi::OsString> = vec!["js-agent".into(), "enroll".into()];
        args.extend(words.iter().map(std::ffi::OsString::from));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = stdin.as_bytes();
        let code = run(&args, &mut input, &mut out, &mut err, &self.host);
        Ran {
            status: format!("{code:?}"),
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }
}

impl Rig {
    /// The standard output of a run that got as far as the last line: the status of the
    /// setup, the instance file and the token file, then the status of the host. `token` is
    /// the token whose sha256 the line names.
    pub fn report(&self, steps: [&str; 3], status: &str, token: &str) -> String {
        let [setup, instance, file] = steps;
        format!(
            "enroll: setup status={setup}\n\
             enroll: instance status={instance} path={}\n\
             enroll: token status={file} path={} sha256={}\n\
             enroll: status={status} provider={PROVIDER} instance={INSTANCE}\n",
            self.host.instance_file.display(),
            self.host.token_file.display(),
            sha(token),
        )
    }
}
