//! Starting a program: the one way the helper and the agent do it, so that on macOS no
//! process they start answers to the privacy controls as theirs.
//!
//! macOS's privacy controls (TCC: Full Disk Access, the camera, another app's files)
//! judge a process by its *responsible* process. A process started the usual way (`fork`
//! and `exec`, or `posix_spawn` with no more) answers for whoever started it, so it is
//! allowed whatever its starter was granted. It answers for itself only when its starter
//! disclaims the responsibility as it starts it (`responsibility_spawnattrs_setdisclaim`
//! on the `posix_spawn` attributes), or when launchd starts it as a job of its own. The
//! helper and the agent are launchd daemons and answer for themselves. A job started the
//! standard library's way would answer for the helper that started it, and a probe for
//! the agent. A grant ever given to either would then reach the code of whoever queued a
//! job.
//!
//! [`Program`] is the standard library's [`Command`], with its program and arguments fixed
//! when it is made. Off macOS there is nothing to disclaim, and it is the standard
//! library's [`Command`] unchanged. On macOS, when it is started, it builds in the parent
//! the three things its program is executed with:
//!
//! - the program's file: the program itself when it names a directory, otherwise the first
//!   regular file of that name in the `PATH` of the program's environment (`/usr/bin:/bin`
//!   when it has none), which is the lookup `execvp` makes;
//! - the argument vector: the program, then each argument;
//! - the environment: this process's, or none after [`Program::env_clear`], with the
//!   variables set or removed since.
//!
//! The child that the standard library forks does its usual set-up (standard streams,
//! group and user ids, working directory, process group), then the steps given to
//! [`Program::with_steps`]. Then, where the standard library would call `execvp`, it calls
//! `posix_spawn` with `POSIX_SPAWN_SETEXEC`, which executes the file in the same process
//! with those arguments and that environment and the disclaim attribute set. The program
//! is then its own responsible process: TCC judges it by its own code signature, which no
//! grant names. [`exec`] does the same for a process that executes a program in its own
//! place (the helper's `as-user` mode, the last step of a run in a GUI session), in the
//! environment that process has.
//!
//! Everything the child uses is made before the fork, because between `fork` and `exec`
//! the child may not allocate. That is why the program and its arguments are fixed when
//! the [`Program`] is made, and why a [`Program`] starts its program once: a second
//! [`Program::spawn`] or [`Program::output`] is refused, on every platform.
//!
//! This module is the only code of `js-agent` and `js-mac-session` that starts a program.
//! `cargo test -p xtask --test spawn_sites` fails when their production code, on any
//! platform, does it another way: a `Command::new`, a `pre_exec` or `exec` call, or a C
//! library call that executes a program (`crates/xtask/src/spawn_sites.rs`).
//!
//! `responsibility_spawnattrs_setdisclaim` is not in Apple's public headers. libSystem
//! has exported it since macOS 10.14, and the binaries' deployment target is 11.0. On
//! macOS, [`responsible_pid`] reads a process's responsible process back through the same
//! interface (`responsibility_get_pid_responsible_for_pid`), and the agent's tests use it
//! to check the processes they start.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

#[cfg(not(target_os = "macos"))]
use std::os::unix::process::CommandExt as _;

/// Steps the child runs before the program executes.
type Steps = Box<dyn FnMut() -> io::Result<()> + Send + Sync>;

/// A program to start, with its arguments fixed (see the module documentation).
pub struct Program {
    command: Command,
    /// The steps the child runs before the program, until the program is started.
    steps: Option<Steps>,
    /// Whether the program has been started (it is started once).
    started: bool,
    /// The program, for the file and the vector `posix_spawn` is given.
    #[cfg(target_os = "macos")]
    program: OsString,
    /// Its arguments.
    #[cfg(target_os = "macos")]
    args: Vec<OsString>,
    /// Whether [`Self::env_clear`] was called.
    #[cfg(target_os = "macos")]
    cleared: bool,
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program")
            .field("command", &self.command)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl Program {
    /// `program` with `args`.
    pub fn new<I, S>(program: impl AsRef<OsStr>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        Self::make(program.as_ref(), args, None)
    }

    /// [`Self::new`], with `steps` run in the child after the standard library's own
    /// set-up and before the program executes. A step that fails ends the start with its
    /// error, and the program is not executed.
    ///
    /// # Safety
    /// `steps` runs in the child between `fork` and `exec`, in a copy of a process that
    /// may have other threads: it must call only async-signal-safe functions and must not
    /// allocate.
    pub unsafe fn with_steps<I, S>(
        program: impl AsRef<OsStr>,
        args: I,
        steps: impl FnMut() -> io::Result<()> + Send + Sync + 'static,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        Self::make(program.as_ref(), args, Some(Box::new(steps)))
    }

    /// The command, and what [`Self::arm`] needs to finish it.
    fn make(program: &OsStr, args: Vec<OsString>, steps: Option<Steps>) -> Self {
        let mut command = Command::new(program);
        command.args(&args);
        #[cfg(target_os = "macos")]
        {
            Self {
                command,
                steps,
                started: false,
                program: program.to_owned(),
                args,
                cleared: false,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                command,
                steps,
                started: false,
            }
        }
    }

    /// Gives the command its one closure, once: the steps and, on macOS, the disclaimed
    /// execution after them, with everything it uses made here, before the fork.
    ///
    /// # Errors
    /// The program was started already; on macOS also: the program is not found, it,
    /// an argument or the environment holds NUL, or the attributes could not be made.
    fn arm(&mut self) -> io::Result<()> {
        if self.started {
            return Err(io::Error::other("a Program starts its program once"));
        }
        self.started = true;
        let steps = self.steps.take();
        #[cfg(target_os = "macos")]
        {
            let env = darwin::environment(self.cleared, &self.command);
            let start = darwin::Start::new(&self.program, &self.args, env)?;
            darwin::disclaim(&mut self.command, start, steps);
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(steps) = steps {
            // SAFETY: `with_steps`'s caller promises that `steps` is async-signal-safe and
            // allocates nothing; it is the only closure the command has.
            unsafe {
                self.command.pre_exec(steps);
            }
        }
        Ok(())
    }

    /// Clears the environment the program gets ([`Command::env_clear`]).
    pub fn env_clear(&mut self) -> &mut Self {
        self.command.env_clear();
        #[cfg(target_os = "macos")]
        {
            self.cleared = true;
        }
        self
    }

    /// Sets `key` to `value` in the program's environment ([`Command::env`]).
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.command.env(key, value);
        self
    }

    /// Sets each pair of `vars` in the program's environment ([`Command::envs`]).
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.envs(vars);
        self
    }

    /// Sets the program's working directory ([`Command::current_dir`]).
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(dir);
        self
    }

    /// Sets the user id the program runs as (`CommandExt::uid`).
    pub fn uid(&mut self, id: u32) -> &mut Self {
        std::os::unix::process::CommandExt::uid(&mut self.command, id);
        self
    }

    /// Sets the group id the program runs as (`CommandExt::gid`).
    pub fn gid(&mut self, id: u32) -> &mut Self {
        std::os::unix::process::CommandExt::gid(&mut self.command, id);
        self
    }

    /// Puts the program in the process group `group`; 0 makes it the leader of a new one
    /// (`CommandExt::process_group`).
    pub fn process_group(&mut self, group: i32) -> &mut Self {
        std::os::unix::process::CommandExt::process_group(&mut self.command, group);
        self
    }

    /// Sets the program's standard input ([`Command::stdin`]).
    pub fn stdin(&mut self, stream: impl Into<Stdio>) -> &mut Self {
        self.command.stdin(stream);
        self
    }

    /// Sets the program's standard output ([`Command::stdout`]).
    pub fn stdout(&mut self, stream: impl Into<Stdio>) -> &mut Self {
        self.command.stdout(stream);
        self
    }

    /// Sets the program's standard error ([`Command::stderr`]).
    pub fn stderr(&mut self, stream: impl Into<Stdio>) -> &mut Self {
        self.command.stderr(stream);
        self
    }

    /// Starts the program ([`Command::spawn`]).
    ///
    /// # Errors
    /// It could not be started: the program is missing, a step or the set-up failed, the
    /// program or an argument holds NUL, or it was started already; on macOS also the
    /// environment holds NUL or the attributes that disclaim the responsibility could not
    /// be made.
    pub fn spawn(&mut self) -> io::Result<Child> {
        self.arm()?;
        self.command.spawn()
    }

    /// Starts the program, waits for it and collects what it printed
    /// ([`Command::output`]).
    ///
    /// # Errors
    /// As [`Self::spawn`], or it could not be waited for.
    pub fn output(&mut self) -> io::Result<Output> {
        self.arm()?;
        self.command.output()
    }
}

/// Executes `program` with `args` in this process's place, in its environment, as
/// `execvp` does, with its responsibility disclaimed; returns only when it could not.
#[cfg(target_os = "macos")]
#[must_use]
pub fn exec(program: &OsStr, args: &[OsString]) -> io::Error {
    match darwin::Start::new(program, args, std::env::vars_os().collect()) {
        Ok(start) => darwin::exec(&start),
        Err(why) => why,
    }
}

/// The pid of the process that `pid` answers to for the privacy controls: its own pid
/// when its starter disclaimed the responsibility, or launchd started it.
///
/// # Errors
/// The system could not say (there is no such process).
#[cfg(target_os = "macos")]
pub fn responsible_pid(pid: u32) -> io::Result<u32> {
    darwin::responsible_pid(pid)
}

/// What `posix_spawn` is given, made before the fork: the program's file, and the
/// argument and environment vectors. Built for the tests on every platform, so the
/// construction and the lookup are checked on Linux too.
#[cfg(any(target_os = "macos", test))]
mod vectors {
    use std::ffi::{CString, OsStr, OsString, c_char};
    use std::io;
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    use std::path::PathBuf;

    /// The `PATH` `execvp` searches when the environment has none.
    const DEFAULT_PATH: &str = "/usr/bin:/bin";

    /// C strings, and the null-ended vector of pointers to them.
    pub(super) struct Strings {
        /// The strings, owned here so that the pointers into them stay valid; read only
        /// through `pointers`.
        _strings: Vec<CString>,
        /// A pointer to each string, in order, then a null pointer.
        pointers: Vec<*const c_char>,
    }

    // SAFETY: the pointers point into the bytes of `_strings`, which this value owns and
    // never changes after it is made; a `CString`'s bytes are on the heap and stay where
    // they are when the value moves to another thread.
    unsafe impl Send for Strings {}
    // SAFETY: as for `Send`: a shared reference only reads the strings and the pointers.
    unsafe impl Sync for Strings {}

    impl Strings {
        /// `items`, each as a C string.
        ///
        /// # Errors
        /// An item holds NUL (`InvalidInput`). The message names none, as an argument or
        /// a variable may be a secret.
        fn new(items: Vec<Vec<u8>>) -> io::Result<Self> {
            let strings = items
                .into_iter()
                .map(CString::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the program, an argument or the environment holds NUL",
                    )
                })?;
            let pointers = strings
                .iter()
                .map(|text| text.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            Ok(Self {
                _strings: strings,
                pointers,
            })
        }

        /// The argument vector: `program`, then each of `args`.
        pub(super) fn argv(program: &OsStr, args: &[OsString]) -> io::Result<Self> {
            let items = std::iter::once(program)
                .chain(args.iter().map(OsString::as_os_str))
                .map(|text| text.as_bytes().to_vec())
                .collect();
            Self::new(items)
        }

        /// The environment vector: `NAME=value` for each pair.
        pub(super) fn env(vars: &[(OsString, OsString)]) -> io::Result<Self> {
            let items = vars
                .iter()
                .map(|(name, value)| {
                    let mut item = name.clone().into_vec();
                    item.push(b'=');
                    item.extend_from_slice(value.as_bytes());
                    item
                })
                .collect();
            Self::new(items)
        }

        /// A vector of one string: a file to execute.
        pub(super) fn file(path: &OsStr) -> io::Result<Self> {
            Self::new(vec![path.as_bytes().to_vec()])
        }

        /// The first string.
        pub(super) fn first(&self) -> *const c_char {
            self.pointers[0]
        }

        /// The vector: each string, then a null pointer.
        pub(super) fn pointers(&self) -> *const *const c_char {
            self.pointers.as_ptr()
        }

        /// How many strings it holds.
        #[cfg(test)]
        pub(super) fn len(&self) -> usize {
            self._strings.len()
        }
    }

    /// The file `program` names, as `execvp` finds it: `program` itself when it holds a
    /// `/`, otherwise the first regular file of that name in a directory of `path` (or of
    /// `/usr/bin:/bin` when there is none); none when no directory holds one.
    pub(super) fn locate(program: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
        if program.as_bytes().contains(&b'/') {
            return Some(PathBuf::from(program));
        }
        let path = path.unwrap_or(OsStr::new(DEFAULT_PATH));
        std::env::split_paths(path)
            .map(|dir| dir.join(program))
            .find(|file| file.is_file())
    }

    #[cfg(test)]
    mod tests {
        use std::ffi::CStr;
        use std::path::Path;

        use super::*;

        /// The strings the vector points to, read through its pointers up to the null one.
        fn read(strings: &Strings) -> Vec<String> {
            let mut read = Vec::new();
            let mut at = strings.pointers();
            // SAFETY: the vector ends with a null pointer and every pointer before it is
            // a NUL-terminated string `strings` owns, alive for the loop.
            unsafe {
                while !(*at).is_null() {
                    read.push(CStr::from_ptr(*at).to_str().unwrap().to_owned());
                    at = at.add(1);
                }
            }
            assert_eq!(read.len(), strings.len());
            read
        }

        fn os(words: &[&str]) -> Vec<OsString> {
            words.iter().map(OsString::from).collect()
        }

        /// Catches: a vector that is not null-ended (`posix_spawn` would read past it),
        /// an argument lost or out of order, the program left out, an environment entry
        /// not spelled `NAME=value`, or pointers that dangle once the value has moved (the
        /// child reads them from the closure it was moved into).
        #[test]
        fn the_vectors_are_what_posix_spawn_takes() {
            let argv = Strings::argv(OsStr::new("/bin/sh"), &os(&["-c", "echo a b", ""]));
            let moved = Box::new(argv.unwrap());
            assert_eq!(read(&moved), ["/bin/sh", "-c", "echo a b", ""]);
            // SAFETY: the first string is a NUL-terminated string `moved` owns.
            let first = unsafe { CStr::from_ptr(moved.first()) };
            assert_eq!(first.to_str().unwrap(), "/bin/sh");
            let alone = Strings::argv(OsStr::new("/usr/bin/true"), &[]).unwrap();
            assert_eq!(read(&alone), ["/usr/bin/true"]);
            let vars = [
                (OsString::from("A"), OsString::from("b=c")),
                (OsString::from("EMPTY"), OsString::new()),
            ];
            assert_eq!(read(&Strings::env(&vars).unwrap()), ["A=b=c", "EMPTY="]);
            assert!(read(&Strings::env(&[]).unwrap()).is_empty());
            let file = Strings::file(OsStr::new("/bin/cat")).unwrap();
            assert_eq!(read(&file), ["/bin/cat"]);
        }

        /// Catches: a NUL in the program, an argument or a variable cut short silently (a
        /// different program, argument or value would reach the process), or the refusal
        /// echoing what held it (which may be a secret).
        #[test]
        fn a_nul_anywhere_is_refused_without_naming_it() {
            let secret = OsString::from("token=s3cret\0tail");
            let refusals = [
                Strings::argv(OsStr::new("/bin/e\0cho"), &[]),
                Strings::argv(
                    OsStr::new("/bin/echo"),
                    &[OsString::from("ok"), secret.clone()],
                ),
                Strings::env(&[(OsString::from("TOKEN"), secret.clone())]),
                Strings::env(&[(secret, OsString::from("x"))]),
                Strings::file(OsStr::new("/bin/c\0at")),
            ];
            for refused in refusals {
                let why = refused.err().expect("a NUL was accepted");
                assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
                assert_eq!(
                    why.to_string(),
                    "the program, an argument or the environment holds NUL"
                );
            }
        }

        /// Catches: a lookup that is not `execvp`'s: a program with a `/` looked up (or
        /// refused when missing, which is the start's to say), the `PATH` read out of
        /// order, a directory taken for the program, a program found nowhere taken from
        /// the working directory, or no default when the environment has no `PATH`.
        #[test]
        fn the_program_is_found_as_execvp_finds_it() {
            let bin = Some(OsStr::new("/nonexistent:/bin:/usr/bin"));
            assert_eq!(
                locate(OsStr::new("/no/such/program"), bin),
                Some(PathBuf::from("/no/such/program"))
            );
            assert_eq!(
                locate(OsStr::new("sh"), bin),
                Some(PathBuf::from("/bin/sh"))
            );
            // `/` holds a directory named `tmp`, which is not a program.
            assert_eq!(locate(OsStr::new("tmp"), Some(OsStr::new("/"))), None);
            assert_eq!(locate(OsStr::new("no-such-program-anywhere"), bin), None);
            let default = locate(OsStr::new("sh"), None).unwrap();
            let shells = [Path::new("/usr/bin/sh"), Path::new("/bin/sh")];
            assert!(shells.contains(&default.as_path()), "{default:?}");
        }
    }
}

/// The macOS half: the attributes, the execution in place, and the read-back.
#[cfg(target_os = "macos")]
mod darwin {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString, c_int};
    use std::io;
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    use super::Steps;
    use super::vectors::{Strings, locate};

    // FFI-BOUNDARY: two functions libSystem exports and Apple's public headers do not
    // declare. Neither takes or gives ownership of memory: `setdisclaim` writes into the
    // attribute object its caller owns (made by `posix_spawnattr_init` and freed by
    // `posix_spawnattr_destroy`, both in `Attributes`), and the pid query takes and returns
    // a number.
    unsafe extern "C" {
        fn responsibility_spawnattrs_setdisclaim(
            attr: *mut libc::posix_spawnattr_t,
            disclaim: c_int,
        ) -> c_int;
        fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
    }

    /// `posix_spawn` attributes that execute in place (`POSIX_SPAWN_SETEXEC`) with the
    /// responsibility disclaimed.
    struct Attributes(libc::posix_spawnattr_t);

    // SAFETY: the attribute object is owned by this value alone. `new` writes it before
    // the value can be shared, and after that it is only read (`posix_spawn` copies it);
    // `drop` frees it, once.
    unsafe impl Send for Attributes {}
    // SAFETY: as for `Send`: a shared reference only reads it.
    unsafe impl Sync for Attributes {}

    impl Attributes {
        /// Makes the attributes.
        ///
        /// # Errors
        /// The system refused one of the three calls.
        fn new() -> io::Result<Self> {
            let mut raw: libc::posix_spawnattr_t = std::ptr::null_mut();
            // SAFETY: `raw` is writable; `posix_spawnattr_init` makes an attribute
            // object and stores its address there.
            status(unsafe { libc::posix_spawnattr_init(&raw mut raw) })?;
            // From here on `drop` frees it, whatever fails next.
            let mut attributes = Self(raw);
            let flags = libc::c_short::try_from(libc::POSIX_SPAWN_SETEXEC)
                .map_err(|_| io::Error::other("POSIX_SPAWN_SETEXEC is not a c_short"))?;
            // SAFETY: `attributes.0` is the object `posix_spawnattr_init` made.
            status(unsafe { libc::posix_spawnattr_setflags(&raw mut attributes.0, flags) })?;
            // SAFETY: as above.
            status(unsafe { responsibility_spawnattrs_setdisclaim(&raw mut attributes.0, 1) })?;
            Ok(attributes)
        }
    }

    impl Drop for Attributes {
        fn drop(&mut self) {
            // SAFETY: the object `posix_spawnattr_init` made in `new`, freed only here.
            unsafe { libc::posix_spawnattr_destroy(&raw mut self.0) };
        }
    }

    /// A `posix_spawn` family status (0, or an error number) as a result.
    fn status(code: c_int) -> io::Result<()> {
        match code {
            0 => Ok(()),
            code => Err(io::Error::from_raw_os_error(code)),
        }
    }

    /// Everything one disclaimed execution uses, made before it.
    pub(super) struct Start {
        file: Strings,
        argv: Strings,
        envp: Strings,
        attributes: Attributes,
    }

    impl Start {
        /// The execution of `program` with `args` in the environment `env`.
        ///
        /// # Errors
        /// The program is not found (`ENOENT`), it, an argument or the environment holds
        /// NUL, or the attributes could not be made.
        pub(super) fn new(
            program: &OsStr,
            args: &[OsString],
            env: Vec<(OsString, OsString)>,
        ) -> io::Result<Self> {
            let path = env
                .iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| value.as_os_str());
            let found =
                locate(program, path).ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            Ok(Self {
                file: Strings::file(found.as_os_str())?,
                argv: Strings::argv(program, args)?,
                envp: Strings::env(&env)?,
                attributes: Attributes::new()?,
            })
        }
    }

    /// The environment a command's program gets: this process's, or none when it was
    /// cleared, with the variables the command set or removed since.
    pub(super) fn environment(cleared: bool, command: &Command) -> Vec<(OsString, OsString)> {
        let mut env: BTreeMap<OsString, OsString> = match cleared {
            true => BTreeMap::new(),
            false => std::env::vars_os().collect(),
        };
        for (name, value) in command.get_envs() {
            match value {
                Some(value) => env.insert(name.to_owned(), value.to_owned()),
                None => env.remove(name),
            };
        }
        env.into_iter().collect()
    }

    /// Executes `start` in this process's place; returns only when it could not, with
    /// the reason. Allocates nothing.
    pub(super) fn exec(start: &Start) -> io::Error {
        let mut pid: libc::pid_t = 0;
        // SAFETY: the file is a NUL-terminated string, and the argument and environment
        // vectors are null-ended vectors of NUL-terminated strings, all owned by `start`;
        // the attributes are an object `Attributes::new` made. With `POSIX_SPAWN_SETEXEC`
        // the call returns only on failure.
        let code = unsafe {
            libc::posix_spawn(
                &raw mut pid,
                start.file.first(),
                std::ptr::null(),
                &raw const start.attributes.0,
                start.argv.pointers().cast(),
                start.envp.pointers().cast(),
            )
        };
        io::Error::from_raw_os_error(code)
    }

    /// Makes `command` run `steps`, if any, in the child and then execute `start` with its
    /// responsibility disclaimed.
    pub(super) fn disclaim(command: &mut Command, start: Start, steps: Option<Steps>) {
        let mut steps = steps;
        let last = move || -> io::Result<()> {
            if let Some(steps) = steps.as_mut() {
                steps()?;
            }
            Err(exec(&start))
        };
        // SAFETY: the closure runs in the child between fork and exec. `steps` is
        // async-signal-safe and allocates nothing (the contract of
        // `Program::with_steps`), and `exec` calls only `posix_spawn`, with the strings
        // and the attributes made before the fork. It is the command's only closure, so
        // it runs after the standard library's set-up, and nothing runs after it.
        unsafe {
            command.pre_exec(last);
        }
    }

    /// See [`super::responsible_pid`].
    pub(super) fn responsible_pid(pid: u32) -> io::Result<u32> {
        let pid = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not a pid"))?;
        // SAFETY: a query by number; nothing is passed by address.
        let answer = unsafe { responsibility_get_pid_responsible_for_pid(pid) };
        u32::try_from(answer).map_err(|_| io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    /// What `/bin/sh -c <script>` printed, run through a [`Program`] that `configure`
    /// set up.
    fn printed(script: &str, configure: impl FnOnce(&mut Program)) -> String {
        let mut program = Program::new("/bin/sh", ["-c", script]);
        configure(&mut program);
        let out = program.output().unwrap();
        assert!(out.status.success(), "{script}: {:?}", out);
        String::from_utf8(out.stdout).unwrap()
    }

    /// Catches: arguments lost or reordered on the way to the program, or the environment
    /// calls not reaching it: a variable set and not seen, the cleared environment still
    /// holding the test's own, or one of `envs`' pairs lost.
    #[test]
    fn the_program_gets_its_arguments_and_the_environment_it_is_given() {
        let out = Program::new("/bin/echo", ["one", "two words"])
            .output()
            .unwrap();
        assert_eq!(out.stdout, b"one two words\n");
        let seen = printed(
            r#"echo "$A|$B|$C|${CARGO_MANIFEST_DIR-unset}""#,
            |program| {
                program
                    .env_clear()
                    .env("A", "a")
                    .envs([("B", "b"), ("C", "c")]);
            },
        );
        assert_eq!(seen, "a|b|c|unset\n");
        // Without `env_clear`, the program gets this process's environment.
        let inherited = printed(r#"echo "${CARGO_MANIFEST_DIR-unset}""#, |_| {});
        assert_eq!(inherited.trim_end(), env!("CARGO_MANIFEST_DIR"));
    }

    /// Catches: the working directory or the process group set on the builder and not on
    /// the process, or the ids not passed on (the caller's own are accepted, and the start
    /// fails for one no process may take).
    #[test]
    fn the_program_runs_where_and_as_whom_it_is_told() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let argv = crate::testing::act_argv("pwd ; pgid");
        let (mut out, out_end) = std::io::pipe().unwrap();
        let mut child = Program::new(&argv[0], &argv[1..])
            .current_dir(&dir)
            .uid(uid)
            .gid(gid)
            .process_group(0)
            .stdout(out_end)
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        let mut said = String::new();
        out.read_to_string(&mut said).unwrap();
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines, [dir.to_str().unwrap(), pid.to_string().as_str()]);
        // No one may take the id that means "no id" (-1): the start fails, so the uid
        // reached it.
        let refused = Program::new("/usr/bin/true", [""; 0]).uid(u32::MAX).spawn();
        assert!(refused.is_err(), "the uid did not reach the start");
    }

    /// Catches: a standard stream set on the builder and not given to the process.
    #[test]
    fn the_program_gets_the_streams_it_is_given() {
        let (mut out, out_end) = std::io::pipe().unwrap();
        let (mut err, err_end) = std::io::pipe().unwrap();
        let (in_end, mut feed) = std::io::pipe().unwrap();
        let mut child = Program::new(
            "/bin/sh",
            ["-c", "read line; echo \"out $line\"; echo err >&2"],
        )
        .stdin(in_end)
        .stdout(out_end)
        .stderr(err_end)
        .spawn()
        .unwrap();
        std::io::Write::write_all(&mut feed, b"fed\n").unwrap();
        drop(feed);
        assert!(child.wait().unwrap().success());
        let (mut said, mut warned) = (String::new(), String::new());
        out.read_to_string(&mut said).unwrap();
        err.read_to_string(&mut warned).unwrap();
        assert_eq!((said.as_str(), warned.as_str()), ("out fed\n", "err\n"));
    }

    /// Catches: steps that do not run before the program, or a failing step that does not
    /// end the start with its own error (the program would execute without what the step
    /// should have done).
    #[test]
    fn steps_run_before_the_program_and_a_failing_one_stops_it() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "disclaim-steps");
        let marker = dir.join("ran");
        let script = format!("echo ran > {}", marker.display());
        // SAFETY: the step calls nothing and allocates nothing.
        let mut started = unsafe { Program::with_steps("/bin/sh", ["-c", &script], || Ok(())) };
        let out = started.output().unwrap();
        assert!(out.status.success());
        assert!(marker.exists(), "the program did not run after the step");
        std::fs::remove_file(&marker).unwrap();
        // A step that fails, run here too: in the child it leaves no coverage record.
        fn refuse() -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::EPERM))
        }
        assert_eq!(refuse().unwrap_err().raw_os_error(), Some(libc::EPERM));
        // SAFETY: the step allocates nothing and calls nothing.
        let mut refused = unsafe { Program::with_steps("/bin/sh", ["-c", &script], refuse) };
        let error = refused.spawn().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert!(!marker.exists(), "the program ran after a failing step");
    }

    /// Catches: a NUL in an argument cut short silently (a different argument would reach
    /// the program) instead of refused when the program is started.
    #[test]
    fn a_nul_in_an_argument_is_refused_at_the_start() {
        let makers: [fn() -> Program; 2] = [
            || Program::new("/bin/echo", ["a\0b"]),
            || Program::new("/bin/e\0cho", [""; 0]),
        ];
        for make in makers {
            assert_eq!(
                make().spawn().unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                make().output().unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    /// Catches: a program started twice from one [`Program`] (on macOS the second start
    /// would reuse what the first made), or a second start that is not refused the same
    /// way by `spawn` and `output`.
    #[test]
    fn a_program_starts_once() {
        let mut program = Program::new("/usr/bin/true", [""; 0]);
        assert!(program.output().unwrap().status.success());
        for again in [program.spawn().map(drop), program.output().map(drop)] {
            assert_eq!(
                again.unwrap_err().to_string(),
                "a Program starts its program once"
            );
        }
        let shown = format!("{program:?}");
        assert!(shown.starts_with("Program {"), "{shown}");
        assert!(shown.contains("started: true"), "{shown}");
    }

    /// Catches: a missing program reported as anything but not found.
    #[test]
    fn a_missing_program_is_not_found() {
        let error = Program::new("/no/such/program", [""; 0])
            .spawn()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
