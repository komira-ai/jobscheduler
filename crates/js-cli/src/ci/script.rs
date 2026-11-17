//! The program a CI step's job runs: a POSIX shell script, the job's `args` after `-c`, that
//! fetches the step's source, runs the step, and writes its declared outputs to stdout as
//! one framed tar archive (`super::frame` reads it back on the runner).
//!
//! The script takes its values as positional parameters, never inside its text: `$1` the
//! step's commands, `$2` the repository URL, `$3` the commit, `$4` the outputs marker (`-`
//! when no output is declared) and the declared outputs after them. So a value can hold any
//! byte and is never parsed as shell.
//!
//! What the job's host must give it: `/bin/sh`, `mktemp`, `git`, `bash` and (with outputs)
//! `tar` and `wc`, and a network level that reaches the repository. On Linux the image holds
//! them; on macOS the system does.

/// The name the script runs under (`$0`), which its messages begin with.
pub(crate) const NAME: &str = "js-ci";

/// The name of the job file that holds the token the fetch authenticates with. The job reads
/// it from `$JS_FILES_DIR` (macOS) or `/run/js-files` (Linux), and the service returns it in
/// no read.
pub(crate) const TOKEN_FILE: &str = "source-token";

/// The marker in the place of `$4` when no output is declared.
pub(crate) const NO_OUTPUTS: &str = "-";

/// The script.
///
/// 1. A fresh directory from `mktemp -d` holds the checkout (`src`) and the archive.
/// 2. `git init`, then `git fetch --depth 1` of the one commit and a detached checkout of it.
///    With a token file, git asks a credential helper, configured for the repository's URL
///    alone, that prints the user `x-access-token` and the file's content as the password:
///    the token is in no argument, no environment variable and no file of the checkout, and
///    no other host is given it. The helper is removed once the commit is checked out, so the
///    step's own git commands are not given the token.
/// 3. The step runs as GitHub runs a `run:` step, `bash --noprofile --norc -e -o pipefail
///    -c`, in the checkout, and its status is kept. A failed fetch ends the job before it, with
///    git's status.
/// 4. With outputs declared (whatever the step's status), each declared path that exists is
///    archived with `tar -cf` (on macOS with `COPYFILE_DISABLE=1`, so no `._` entries of
///    extended attributes are added); a path that does not exist is said on stderr. Then
///    stdout gets a line break, the marker, a space, the archive's length in bytes and a line
///    break, and the archive itself (none when no path existed, with the length 0). An archive
///    that cannot be made is said on stderr and sends no frame. Nothing of this step changes
///    the job's status.
/// 5. The job exits with the step's status.
pub(crate) const SCRIPT: &str = r#"set -eu
run=$1 url=$2 commit=$3 marker=$4
shift 4
work=$(mktemp -d)
mkdir "$work/src"
cd "$work/src"
git init -q
token="${JS_FILES_DIR:-/run/js-files}/source-token"
if [ -r "$token" ]; then
  export JS_CI_TOKEN_FILE="$token"
  git config "credential.$url.helper" '!f() { test "$1" = get || exit 0; echo username=x-access-token; printf "password=%s\n" "$(cat "$JS_CI_TOKEN_FILE")"; }; f'
fi
git fetch -q --depth 1 --no-tags "$url" "$commit"
git checkout -q --detach FETCH_HEAD
if [ -r "$token" ]; then
  git config --unset "credential.$url.helper"
  unset JS_CI_TOKEN_FILE
fi
set +e
bash --noprofile --norc -e -o pipefail -c "$run"
status=$?
if [ "$marker" != - ]; then
  count=$#
  while [ "$count" -gt 0 ]; do
    path=$1
    shift
    if [ -e "$path" ] || [ -L "$path" ]; then
      set -- "$@" "$path"
    else
      echo "js-ci: output $path was not made" >&2
    fi
    count=$((count - 1))
  done
  if [ "$#" -eq 0 ]; then
    printf '\n%s 0\n' "$marker"
  elif COPYFILE_DISABLE=1 tar -cf "$work/outputs.tar" "$@"; then
    size=$(($(wc -c < "$work/outputs.tar")))
    printf '\n%s %s\n' "$marker" "$size"
    cat "$work/outputs.tar"
  else
    echo "js-ci: the outputs could not be archived" >&2
  fi
fi
exit "$status"
"#;

/// The job's arguments after its command, `/bin/sh`: `-c`, [`SCRIPT`], [`NAME`] (`$0`), then
/// the script's parameters.
pub(crate) fn args(
    run: &str,
    url: &str,
    commit: &str,
    marker: Option<&str>,
    outputs: &[String],
) -> Vec<String> {
    let fixed = ["-c", SCRIPT, NAME, run, url, commit];
    let marker = marker.unwrap_or(NO_OUTPUTS);
    let fixed = fixed.into_iter().chain([marker]).map(str::to_owned);
    fixed.chain(outputs.iter().cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the order of the job's arguments: `-c`, the script, its `$0`, the step, the
    /// URL, the commit, the marker (or `-`), then each output. It catches two values
    /// swapped, which the script would read as the wrong thing.
    #[test]
    fn the_values_follow_the_script_in_the_order_it_reads_them() {
        let outputs = ["a/b".to_owned(), "c".to_owned()];
        let got = args("make", "https://x/r", "f00", Some("m-1"), &outputs);
        let want = [
            "-c",
            SCRIPT,
            "js-ci",
            "make",
            "https://x/r",
            "f00",
            "m-1",
            "a/b",
            "c",
        ];
        assert_eq!(got, want);
        let none = args("make", "u", "c", None, &[]);
        assert_eq!(none, ["-c", SCRIPT, "js-ci", "make", "u", "c", "-"]);
    }

    /// Asserts that the script names the token file by [`TOKEN_FILE`] and the no-outputs
    /// marker by [`NO_OUTPUTS`], the names the command sends. It catches one side renamed.
    #[test]
    fn the_script_reads_the_names_the_command_sends() {
        assert!(SCRIPT.contains(&format!("/run/js-files}}/{TOKEN_FILE}\"")));
        assert!(SCRIPT.contains(&format!("if [ \"$marker\" != {NO_OUTPUTS} ]")));
    }

    /// The real script, run by `/bin/sh` against a repository on the local disk (a `file://`
    /// URL, so no network): the host's `sh`, `git`, `bash` and `tar`, as a job's host gives
    /// them. Each run is in a scratch directory of its own, with `HOME`, `TMPDIR` and the
    /// job-files directory inside it and git's system configuration off.
    mod run {
        use std::path::{Path, PathBuf};
        use std::process::{Command, Output};

        use js_testkit::dir::TestDir;

        use super::super::{SCRIPT, TOKEN_FILE, args};
        use crate::ci::frame::{Demux, Frame};
        use crate::ci::outputs::unpack;

        const MARKER: &str = "js-ci-outputs-0123abcd";

        /// A scratch with a repository of one commit holding `tracked.txt`.
        struct Repo {
            dir: TestDir,
            commit: String,
        }

        impl Repo {
            fn new(name: &str) -> Self {
                let dir = TestDir::under(std::env::temp_dir(), name);
                for sub in ["repo", "home", "tmp", "files"] {
                    std::fs::create_dir(dir.path().join(sub)).unwrap();
                }
                let repo = Self {
                    dir,
                    commit: String::new(),
                };
                repo.git(&["init", "-q"]);
                std::fs::write(repo.at("repo/tracked.txt"), "from the commit\n").unwrap();
                repo.git(&["add", "tracked.txt"]);
                let who = ["-c", "user.name=ci", "-c", "user.email=ci@example.invalid"];
                repo.git(&[&who[..], &["commit", "-q", "-m", "one"]].concat());
                let head = repo.git(&["rev-parse", "HEAD"]);
                let commit = String::from_utf8(head.stdout).unwrap().trim().to_owned();
                Self { commit, ..repo }
            }

            fn at(&self, path: &str) -> PathBuf {
                self.dir.path().join(path)
            }

            fn url(&self) -> String {
                format!("file://{}", self.at("repo").display())
            }

            /// A command with the scratch's environment.
            fn command(&self, program: &str) -> Command {
                let mut command = Command::new(program);
                command
                    .env_clear()
                    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                    .env("HOME", self.at("home"))
                    .env("TMPDIR", self.at("tmp"))
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("JS_FILES_DIR", self.at("files"));
                command
            }

            fn git(&self, words: &[&str]) -> Output {
                let output = self
                    .command("git")
                    .args(words)
                    .current_dir(self.at("repo"))
                    .output();
                let output = output.unwrap();
                assert!(output.status.success(), "{output:?}");
                output
            }

            /// Runs the script for `run` at `commit` with `marker` and `outputs`.
            fn script(
                &self,
                run: &str,
                commit: &str,
                marker: Option<&str>,
                outputs: &[&str],
            ) -> Output {
                let outputs: Vec<String> = outputs.iter().map(|p| (*p).to_owned()).collect();
                let words = args(run, &self.url(), commit, marker, &outputs);
                let output = self
                    .command("/bin/sh")
                    .args(words)
                    .current_dir(self.dir.path())
                    .output();
                output.unwrap()
            }
        }

        /// The stdout of `output` through a demultiplexer of [`MARKER`]: what goes out and
        /// the frame.
        fn split(output: &Output) -> (String, Frame) {
            let mut out = Vec::new();
            let mut demux = Demux::new(&mut out, Some(MARKER));
            std::io::Write::write_all(&mut demux, &output.stdout).unwrap();
            let frame = demux.finish().unwrap();
            (String::from_utf8(out).unwrap(), frame)
        }

        /// The archive of `frame`, if it holds one.
        fn archive(frame: Frame) -> Option<Vec<u8>> {
            match frame {
                Frame::Read(archive) => Some(archive),
                _ => None,
            }
        }

        /// Asserts that only a read frame holds an archive. It keeps [`archive`] from taking
        /// another frame for one.
        #[test]
        fn only_a_read_frame_holds_an_archive() {
            assert_eq!(archive(Frame::Read(vec![1])), Some(vec![1]));
            assert_eq!(archive(Frame::Absent), None);
        }

        /// The files under `dir`, as relative paths with their contents, sorted.
        fn files(dir: &Path) -> Vec<(String, String)> {
            let mut found = Vec::new();
            let mut stack = vec![dir.to_path_buf()];
            while let Some(at) = stack.pop() {
                for entry in std::fs::read_dir(at).unwrap() {
                    let path = entry.unwrap().path();
                    match path.is_dir() {
                        true => stack.push(path),
                        false => {
                            let name = path.strip_prefix(dir).unwrap().display().to_string();
                            found.push((name, std::fs::read_to_string(&path).unwrap()));
                        }
                    }
                }
            }
            found.sort();
            found
        }

        /// Asserts that the script checks out the commit, runs the step there, archives the
        /// declared outputs that exist (a directory with its files, a file) and says the one
        /// that does not, and that the archive unpacks to those files with the step's own
        /// stdout before it intact. It catches a step run outside the checkout, an output
        /// missed, and a frame the demultiplexer cannot read.
        #[test]
        fn the_step_runs_in_the_checkout_and_its_outputs_come_back() {
            let repo = Repo::new("ci-script-outputs");
            let run = "mkdir -p out/deep && cp tracked.txt out/deep/copy.txt && echo made > one.txt && echo step-says";
            let outputs = ["out", "one.txt", "missing.txt"];
            let got = repo.script(run, &repo.commit, Some(MARKER), &outputs);
            assert_eq!(got.status.code(), Some(0), "{got:?}");
            assert_eq!(
                String::from_utf8(got.stderr.clone()).unwrap(),
                "js-ci: output missing.txt was not made\n"
            );
            let (out, frame) = split(&got);
            assert_eq!(out, "step-says\n");
            let archive = archive(frame).unwrap();
            let into = repo.at("unpacked");
            unpack(&archive, &into).unwrap();
            assert_eq!(
                files(&into),
                [
                    ("one.txt".to_owned(), "made\n".to_owned()),
                    (
                        "out/deep/copy.txt".to_owned(),
                        "from the commit\n".to_owned()
                    ),
                ]
            );
        }

        /// Asserts that a step that fails still sends its outputs and the job exits with the
        /// step's own code, that a step whose outputs were none made sends an empty archive,
        /// and that a pipeline that fails in its middle fails the step (pipefail). It
        /// catches outputs sent only on success, a status lost to the archive's commands,
        /// and a step run without GitHub's shell options.
        #[test]
        fn a_failed_step_keeps_its_code_and_sends_its_outputs() {
            let repo = Repo::new("ci-script-failed");
            let got = repo.script(
                "echo part > p.txt; exit 3",
                &repo.commit,
                Some(MARKER),
                &["p.txt"],
            );
            assert_eq!(got.status.code(), Some(3));
            let archive = archive(split(&got).1).unwrap();
            let into = repo.at("failed");
            unpack(&archive, &into).unwrap();
            assert_eq!(files(&into), [("p.txt".to_owned(), "part\n".to_owned())]);
            let none = repo.script("true", &repo.commit, Some(MARKER), &["gone"]);
            assert_eq!(
                (none.status.code(), split(&none)),
                (Some(0), (String::new(), Frame::Read(Vec::new())))
            );
            let piped = repo.script("false | cat; echo not-reached", &repo.commit, None, &[]);
            assert_eq!((piped.status.code(), piped.stdout), (Some(1), Vec::new()));
        }

        /// Asserts that when the outputs cannot be archived (a `tar` that fails, first on the
        /// `PATH`) the job still exits with the step's own status, says so on stderr, and
        /// sends no frame. It catches the archive's failure taken for the step's.
        #[test]
        fn an_archive_that_fails_keeps_the_steps_status() {
            let repo = Repo::new("ci-script-tar-fails");
            std::fs::create_dir(repo.at("bin")).unwrap();
            let tar = repo.at("bin/tar");
            std::fs::write(&tar, "#!/bin/sh\nexit 2\n").unwrap();
            let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
            std::fs::set_permissions(&tar, mode).unwrap();
            let path = std::env::var_os("PATH").unwrap_or_default();
            let path = std::env::join_paths(
                std::iter::once(repo.at("bin")).chain(std::env::split_paths(&path)),
            )
            .unwrap();
            for (run, status) in [("echo made > o.txt", 0), ("echo made > o.txt; exit 4", 4)] {
                let outputs = vec!["o.txt".to_owned()];
                let words = args(run, &repo.url(), &repo.commit, Some(MARKER), &outputs);
                let mut command = repo.command("/bin/sh");
                command.env("PATH", &path);
                let got = command
                    .args(words)
                    .current_dir(repo.dir.path())
                    .output()
                    .unwrap();
                assert_eq!(got.status.code(), Some(status), "{got:?}");
                assert_eq!(
                    String::from_utf8(got.stderr.clone()).unwrap(),
                    "js-ci: the outputs could not be archived\n"
                );
                assert_eq!(split(&got), (String::new(), Frame::Absent));
            }
        }

        /// Asserts that with no outputs declared the stdout is the step's alone and holds no
        /// frame, and that the checkout is the commit asked for. It catches a frame sent with
        /// no outputs and a checkout of another commit.
        #[test]
        fn without_outputs_the_stdout_is_the_steps_alone() {
            let repo = Repo::new("ci-script-plain");
            let got = repo.script("git rev-parse HEAD", &repo.commit, None, &[]);
            assert_eq!(got.status.code(), Some(0), "{got:?}");
            assert_eq!(
                String::from_utf8(got.stdout).unwrap(),
                format!("{}\n", repo.commit)
            );
        }

        /// Asserts that a commit the repository does not hold ends the job with git's failure
        /// before the step runs, and sends no frame. It catches a step run in an empty
        /// checkout.
        #[test]
        fn a_commit_that_cannot_be_fetched_ends_the_job_before_the_step() {
            let repo = Repo::new("ci-script-nofetch");
            let absent = "0".repeat(40);
            let got = repo.script("echo ran", &absent, Some(MARKER), &["x"]);
            assert_ne!(got.status.code(), Some(0));
            assert_eq!(split(&got), (String::new(), Frame::Absent));
        }

        /// Asserts that with a token in the job's files the credential helper, while the
        /// commit is fetched, answers for the repository's URL with the user x-access-token and
        /// the token, and gives another host nothing; that the helper's text, in the
        /// checkout's configuration, does not hold the token; and that the step itself finds no
        /// helper, with or without a token. The helper is probed by the script with one line
        /// added after the checkout's fetch. It catches a token written into the checkout or
        /// an argument, one handed to any host, and one left for the step's own git commands.
        #[test]
        fn a_token_file_is_handed_to_git_for_the_fetch_alone() {
            let repo = Repo::new("ci-script-token");
            let look = "git config --get-regexp '^credential' || echo none";
            let none = repo.script(look, &repo.commit, None, &[]);
            assert_eq!(String::from_utf8(none.stdout).unwrap(), "none\n");
            std::fs::write(repo.at(&format!("files/{TOKEN_FILE}")), "s3cret-token\n").unwrap();
            let got = repo.script(look, &repo.commit, None, &[]);
            assert_eq!(
                String::from_utf8(got.stdout.clone()).unwrap(),
                "none\n",
                "{got:?}"
            );
            let fetched = "git fetch -q --depth 1 --no-tags \"$url\" \"$commit\"\n";
            let probe = "git config --get-regexp '^credential' >&2\nprintf 'url=%s\\n\\n' \"$url\" | git credential fill >&2\nprintf 'protocol=https\\nhost=other.invalid\\n\\n' | git credential fill >&2 || echo no-answer >&2\n";
            assert!(SCRIPT.contains(fetched));
            let probing = SCRIPT.replacen(fetched, &format!("{fetched}{probe}"), 1);
            let mut words = args("true", &repo.url(), &repo.commit, None, &[]);
            words[1] = probing;
            let mut command = repo.command("/bin/sh");
            command
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "false");
            let out = command
                .args(words)
                .current_dir(repo.dir.path())
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(0), "{out:?}");
            let text = String::from_utf8(out.stderr).unwrap();
            let (helper, rest) = text.split_once('\n').unwrap();
            let url = repo.url();
            assert!(
                helper.starts_with(&format!("credential.{url}.helper !f() {{")),
                "{helper}"
            );
            assert!(!helper.contains("s3cret"), "{helper}");
            let answered = rest
                .lines()
                .filter(|line| line.starts_with("password="))
                .count();
            assert_eq!(answered, 1, "{rest}");
            assert!(
                rest.contains("username=x-access-token\npassword=s3cret-token\n"),
                "{rest}"
            );
            assert!(rest.contains("no-answer"), "{rest}");
        }
    }
}
