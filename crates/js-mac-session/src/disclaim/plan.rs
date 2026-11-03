//! What a disclaimed execution is given, made before the fork, and the decisions the
//! child makes with it. Nothing here calls the system: the attempts and the attribute calls
//! are given as seams (`attempt` in [`Plan::run`], [`AttributeCalls`]), so the rules are
//! built and tested on every platform, and macOS supplies the real calls
//! (`super::darwin`).

use std::ffi::{CString, OsStr, OsString, c_char, c_int, c_short};
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;

/// The `PATH` searched when the environment has none (`_PATH_DEFPATH`).
pub(super) const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// The shell that runs an executable file the kernel does not recognise (`_PATH_BSHELL`).
pub(super) const SHELL: &str = "/bin/sh";

/// `POSIX_SPAWN_SETEXEC`: execute in the caller's place. `super::darwin` checks it against
/// the C library's value when it is built.
pub(super) const SETEXEC: c_short = 0x0040;

/// The refusal of a NUL. It names nothing it was given, as an argument or a variable may be
/// a secret.
pub(super) const NUL_REFUSED: &str = "the program, an argument or the environment holds NUL";

/// C strings, and the null-ended vector of pointers to them.
pub(super) struct Strings {
    /// The strings, owned here so that the pointers into them stay valid; read only
    /// through `pointers`.
    _strings: Vec<CString>,
    /// A pointer to each string, in order, then a null pointer.
    pointers: Vec<*const c_char>,
}

// SAFETY: the pointers point into the bytes of `_strings`, which this value owns and
// never changes after it is made; a `CString`'s bytes are on the heap and stay where they
// are when the value moves to another thread.
unsafe impl Send for Strings {}
// SAFETY: as for `Send`: a shared reference only reads the strings and the pointers.
unsafe impl Sync for Strings {}

impl Strings {
    /// `items`, each as a C string.
    ///
    /// # Errors
    /// An item holds NUL ([`NUL_REFUSED`], `InvalidInput`).
    fn new(items: Vec<Vec<u8>>) -> io::Result<Self> {
        let strings = items
            .into_iter()
            .map(CString::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, NUL_REFUSED))?;
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

    /// The first string.
    pub(super) fn first(&self) -> *const c_char {
        self.pointers[0]
    }

    /// The vector: each string, then a null pointer.
    pub(super) fn pointers(&self) -> *const *const c_char {
        self.pointers.as_ptr()
    }

    /// The bytes of each string, read through the pointers up to the null one.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> Vec<Vec<u8>> {
        let mut read = Vec::new();
        let mut at = self.pointers();
        // SAFETY: the vector ends with a null pointer and every pointer before it is a
        // NUL-terminated string this value owns, alive for the loop.
        unsafe {
            while !(*at).is_null() {
                read.push(std::ffi::CStr::from_ptr(*at).to_bytes().to_vec());
                at = at.add(1);
            }
        }
        assert_eq!(read.len(), self._strings.len());
        read
    }
}

/// A file the search tries, and the vector that runs it through [`SHELL`] when the kernel
/// does not recognise it (`sh <file> <arguments>`).
struct Candidate {
    file: Strings,
    shell: Strings,
}

impl Candidate {
    /// The candidate `file`, run with the arguments `tail` when the shell runs it.
    fn new(file: Vec<u8>, tail: &[Vec<u8>]) -> io::Result<Self> {
        let shell = std::iter::once(b"sh".to_vec())
            .chain(std::iter::once(file.clone()))
            .chain(tail.iter().cloned())
            .collect();
        Ok(Self {
            file: Strings::new(vec![file])?,
            shell: Strings::new(shell)?,
        })
    }
}

/// Everything a disclaimed execution of a program uses, made before the fork.
pub(super) struct Plan {
    /// The argument vector: the program as given, then each argument.
    argv: Strings,
    /// The environment vector: `NAME=value` for each variable.
    envp: Strings,
    /// The files to try, in order.
    candidates: Vec<Candidate>,
    /// Whether the files came from a `PATH` search (the program has no `/`).
    searched: bool,
    /// [`SHELL`], as a file to execute.
    shell: Strings,
}

impl Plan {
    /// The execution of `program` with `args` in the environment `env`. A program with a
    /// `/` is the one file to try; otherwise each directory of the environment's `PATH` (or
    /// of [`DEFAULT_PATH`]) gives one, in order, an empty directory meaning the working
    /// directory. Nothing is looked at here: relative files are tried by the child, in the
    /// directory it runs in.
    ///
    /// # Errors
    /// The program, an argument, or a variable's name or value holds NUL
    /// ([`NUL_REFUSED`]); checked before anything else.
    pub(super) fn new(
        program: &OsStr,
        args: &[OsString],
        env: &[(OsString, OsString)],
    ) -> io::Result<Self> {
        let tail: Vec<Vec<u8>> = args.iter().map(|arg| arg.as_bytes().to_vec()).collect();
        let argv = Strings::new(
            std::iter::once(program.as_bytes().to_vec())
                .chain(tail.iter().cloned())
                .collect(),
        )?;
        let envp = Strings::new(
            env.iter()
                .map(|(name, value)| {
                    let mut item = name.clone().into_vec();
                    item.push(b'=');
                    item.extend_from_slice(value.as_bytes());
                    item
                })
                .collect(),
        )?;
        let searched = !program.as_bytes().contains(&b'/');
        let files: Vec<Vec<u8>> = match searched {
            false => vec![program.as_bytes().to_vec()],
            true => {
                let path = env
                    .iter()
                    .find(|(name, _)| name == "PATH")
                    .map_or(OsStr::new(DEFAULT_PATH), |(_, value)| value.as_os_str());
                std::env::split_paths(path)
                    .map(|dir| match dir.as_os_str().is_empty() {
                        true => Path::new(".").join(program),
                        false => dir.join(program),
                    })
                    .map(|file| file.into_os_string().into_vec())
                    .collect()
            }
        };
        let mut candidates = Vec::new();
        for file in files {
            candidates.push(Candidate::new(file, &tail)?);
        }
        Ok(Self {
            argv,
            envp,
            candidates,
            searched,
            shell: Strings::new(vec![SHELL.as_bytes().to_vec()])?,
        })
    }

    /// The environment vector.
    pub(super) fn envp(&self) -> &Strings {
        &self.envp
    }

    /// Executes the plan with `attempt`, which executes one file with one argument vector
    /// and returns an error number (it returns only when it could not). As `execvp` does:
    ///
    /// - a file the kernel does not recognise (`ENOEXEC`) is run by [`SHELL`] as
    ///   `sh <file> <arguments>`, and that answer is final;
    /// - in a `PATH` search, a file that is not there or cannot be reached (`ENOENT`,
    ///   `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`) or may not be executed (`EACCES`) moves on to the
    ///   next; any other error ends the search with it; when every file was passed over, the
    ///   answer is `EACCES` if one was refused so, and `ENOENT` otherwise;
    /// - a program with a `/` is one file, and its error is the answer.
    ///
    /// Allocates nothing.
    pub(super) fn run(&self, attempt: &mut dyn FnMut(&Strings, &Strings) -> c_int) -> c_int {
        let mut denied = false;
        for candidate in &self.candidates {
            match attempt(&candidate.file, &self.argv) {
                libc::ENOEXEC => return attempt(&self.shell, &candidate.shell),
                code if !self.searched => return code,
                libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::ENAMETOOLONG => {}
                libc::EACCES => denied = true,
                code => return code,
            }
        }
        match denied {
            true => libc::EACCES,
            false => libc::ENOENT,
        }
    }
}

/// The calls that make `posix_spawn` attributes, as error numbers (0 for success): the seam
/// through which [`attributes`]'s order and its cleanup are tested.
pub(super) trait AttributeCalls {
    /// `posix_spawnattr_init`.
    fn init(&mut self) -> c_int;
    /// `posix_spawnattr_setflags` with `flags`.
    fn set_flags(&mut self, flags: c_short) -> c_int;
    /// `responsibility_spawnattrs_setdisclaim` with `disclaim`.
    fn set_disclaim(&mut self, disclaim: c_int) -> c_int;
    /// `posix_spawnattr_destroy`.
    fn destroy(&mut self);
}

/// Makes attributes that execute in place ([`SETEXEC`]) with the responsibility disclaimed:
/// init, the flags, the disclaim, in that order. A call that fails ends it with its error,
/// and attributes that were made are destroyed, so a caller that gets an error has nothing
/// to execute with.
///
/// # Errors
/// The error number of the call that failed.
pub(super) fn attributes(calls: &mut dyn AttributeCalls) -> io::Result<()> {
    status(calls.init())?;
    let made = status(calls.set_flags(SETEXEC)).and_then(|()| status(calls.set_disclaim(1)));
    if made.is_err() {
        calls.destroy();
    }
    made
}

/// An error number (0 for none) as a result.
fn status(code: c_int) -> io::Result<()> {
    match code {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

/// `pid` as the system's pid type.
///
/// # Errors
/// It is above the largest pid (`InvalidInput`).
pub(super) fn pid_of(pid: u32) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{pid} is not a pid")))
}

/// The answer of a responsible-pid query: a pid, or a failure (a negative answer), told by
/// `last`, the error the system gave with it, or by [`NO_ANSWER`] when that names none.
///
/// # Errors
/// The query failed.
pub(super) fn answered(answer: libc::pid_t, last: io::Error) -> io::Result<u32> {
    u32::try_from(answer).map_err(|_| match last.raw_os_error() {
        Some(0) | None => io::Error::other(NO_ANSWER),
        Some(_) => last,
    })
}

/// A failed responsible-pid query that set no error number.
pub(super) const NO_ANSWER: &str = "the system named no responsible process";
