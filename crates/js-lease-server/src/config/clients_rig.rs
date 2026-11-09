//! The rig of the client token tests: a directory of the test's own holding token files, and
//! the configurations the flags and the configuration file give over them. Nothing here
//! binds a socket or starts a server.

use std::cell::Cell;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use crate::clock::SharedClock;
use crate::config::{Cli, Config};
use crate::error::StartupError;
use crate::kinds::{Kinds, OwnerOnlyFiles};

/// The token of `--token-file` (`server.client_token_file`): the client `default`.
pub(crate) const DEFAULT_TOKEN: &str = "default-token-0123456789-abcdefghijklm";

/// The pool's shared join token.
pub(crate) const JOIN: &str = "join-token-0123456789-abcdefghijklmnopqr";

/// The pool that turns agent registration on for the flags, so `--job-listen` is allowed:
/// whole hosts of one shape.
const POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// A joined pool for the file, so `server.job_listen` is allowed, of `x86_64` hosts (the
/// scheduler describes a host by its `arch`).
const JOINED: &str = "[[pool]]\nkind = \"joined\"\nname = \"burst\"\nclass = \"whole-node\"\n\
                      exec = \"none\"\nmax_machines = 6\n\
                      shape = { cpu_millis = 8000, memory_bytes = 17179869184 }\n\
                      capabilities = { arch = \"x86_64\" }\n";

/// A directory of a test's own, removed when the rig is dropped, and the number of token
/// files written in it, which names the next one.
pub(crate) struct ClientsRig {
    dir: TestDir,
    written: Cell<usize>,
}

impl ClientsRig {
    /// A rig over an empty directory named `name`.
    pub(crate) fn new(name: &str) -> ClientsRig {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/client_tokens");
        ClientsRig {
            dir: TestDir::under(root, name),
            written: Cell::new(0),
        }
    }

    /// The path `name` in the directory.
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Writes `text` to the file `name` with `mode`, and answers its path.
    pub(crate) fn write(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, text).expect("the file is written");
        let mode = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(&path, mode).expect("the mode is set");
        path
    }

    /// A new owner-only token file of the client `name`, holding `token`: each call writes a
    /// file of its own, so a case built later never changes the file an earlier case reads.
    pub(crate) fn token_file(&self, name: &str, token: &str) -> PathBuf {
        let number = self.written.get();
        self.written.set(number + 1);
        self.write(&format!("{name}-{number}.token"), token, 0o600)
    }

    /// The flags of a server with agent registration and the job service on, the client
    /// token [`DEFAULT_TOKEN`] and the join token [`JOIN`], followed by `extra`.
    pub(crate) fn flags(&self, extra: &[OsString]) -> Vec<OsString> {
        let pairs: [(&str, OsString); 7] = [
            ("--listen", "127.0.0.1:0".into()),
            ("--job-listen", "127.0.0.1:0".into()),
            ("--state-file", self.path("leases.toml").into()),
            ("--boot", "initialise".into()),
            (
                "--token-file",
                self.token_file("default", DEFAULT_TOKEN).into(),
            ),
            (
                "--agent-token-file",
                self.write("joins", &format!("{JOIN}\n"), 0o600).into(),
            ),
            (
                "--agent-pool-config",
                self.write("pool.toml", POOL, 0o644).into(),
            ),
        ];
        let mut args: Vec<OsString> = vec!["js-lease-server".into()];
        for (flag, value) in pairs {
            args.push(flag.into());
            args.push(value);
        }
        args.extend(extra.iter().cloned());
        args
    }

    /// `--client-token NAME=PATH` for each of `clients` (name and token), each token in a
    /// file of its own.
    pub(crate) fn client_flags(&self, clients: &[(&str, &str)]) -> Vec<OsString> {
        let mut args = Vec::new();
        for (name, token) in clients {
            let path = self.token_file(name, token);
            args.push("--client-token".into());
            args.push(format!("{name}={}", path.display()).into());
        }
        args
    }

    /// The configuration of [`ClientsRig::flags`] with `extra`.
    pub(crate) fn config_of_flags(&self, extra: &[OsString]) -> Result<Config, StartupError> {
        let cli = Cli::try_parse_from(self.flags(extra)).expect("the flags parse");
        Config::from_cli(cli)
    }

    /// A configuration file with the job service on, the client token [`DEFAULT_TOKEN`], a
    /// joined pool whose join token is [`JOIN`], `server` more lines in `[server]` and the
    /// `tables` after it.
    pub(crate) fn file(&self, server: &str, tables: &str) -> String {
        let quoted = |path: PathBuf| format!("{:?}", path.display().to_string());
        format!(
            "[server]\nstate_file = {}\nboot = \"initialise\"\nclient_token_file = {}\n\
             listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n{server}\n\n\
             {tables}\n{JOINED}join_token_file = {}\n",
            quoted(self.path("leases.toml")),
            quoted(self.token_file("default", DEFAULT_TOKEN)),
            quoted(self.write("joins", &format!("{JOIN}\n"), 0o600)),
        )
    }

    /// A `[[client]]` table for each of `clients` (name and token), each token in a file of
    /// its own.
    pub(crate) fn client_tables(&self, clients: &[(&str, &str)]) -> String {
        let mut text = String::new();
        for (name, token) in clients {
            let path = self.token_file(name, token);
            text.push_str(&format!(
                "[[client]]\nname = {name:?}\ntoken_file = {:?}\n\n",
                path.display().to_string()
            ));
        }
        text
    }

    /// The configuration the file `text` gives, its secret files read owner-only.
    pub(crate) fn config_of_file(&self, text: &str) -> Result<Config, StartupError> {
        let path = self.write("js.toml", text, 0o644);
        let clock = SharedClock::new(js_types::ManualClock::default());
        Config::from_config_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock)
    }
}
