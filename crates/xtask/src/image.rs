//! `image-recipe --out <dir> [--root <dir>] [--pins <file>]`: the jobscheduler machine images of
//! `linux-x86_64` and `linux-arm64`, as data (docs/runbooks/machine-images.md).
//!
//! For each platform it writes four files under `<out>/<platform>/`:
//!
//! - `recipe.json`, the image's build description and the one data file of the platform: the
//!   base image and the archives baked in (crun, hostkeeper, js-agent), each by its HTTPS URL and
//!   SHA-256 from the pins file ([`PINS`]); every file the build installs, with its path, mode,
//!   text and SHA-256 ([`BAKED`]); the crun host setup of `docs/runbooks/crun-host-setup.md`, as
//!   its steps ([`CRUN_STEPS`]) and the one command of `crun_host_setup.sh` that makes them; the
//!   units enabled at boot ([`ENABLED_UNITS`]); the paths the provider's user data fills
//!   ([`USER_DATA_PATHS`]); and the build's commands, in order.
//! - `user-data` and `meta-data`, the NoCloud seed that turns the base image into the image: the
//!   recipe's files as cloud-init `write_files` (with the recipe itself, at [`RECIPE_PATH`], and
//!   the SHA-256 list of the archives), its commands as `runcmd` after `set -eu`, so the first
//!   command that fails ends the build, and a power-off. The last command prints
//!   `js-image-build-done` on the console.
//! - `base.sha256`, the base image's SHA-256 as `sha256sum --check` reads it, for `base.img`.
//!
//! It makes no network call and reads no clock: the same pins and repository files give the same
//! bytes. Before it writes anything it refuses, naming the file and where in it:
//!
//! - a key that looks like secret material ([`secret_looking`]): a key of the pins file, or a
//!   key in a baked unit, drop-in or tmpfiles file (a `NAME=` assignment or a `--name` flag, on
//!   any line, comments included);
//! - a pin URL that is not HTTPS, or that carries a user, a query or a fragment;
//! - a pins or steps file that does not parse, has a key it does not know, or lacks one.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::fetch::{NotASha256, Sha256};

/// The pins file, relative to the repository root.
pub const PINS: &str = "packaging/machine-image/pins.toml";

/// The steps of the crun host setup, the list the runbook's table follows.
pub const CRUN_STEPS: &str = "packaging/js-agent/crun_host_steps.toml";

/// The runbook of the crun host setup.
pub const CRUN_RUNBOOK: &str = "docs/runbooks/crun-host-setup.md";

/// Where the build puts what it downloads; removed at the end of the build.
pub const STAGE: &str = "/var/tmp/js-image";

/// Where the image carries its own recipe.
pub const RECIPE_PATH: &str = "/usr/share/js-machine-image/recipe.json";

/// The server written into the unit at build time, in place of the unit's placeholder: a name
/// under `.invalid`, which never resolves. The user data's drop-in sets the real one.
pub const SERVER_PLACEHOLDER: &str = "https://set-by-user-data.invalid";

/// The pool written into the unit at build time; the user data's drop-in sets the real one.
pub const POOL_PLACEHOLDER: &str = "set-by-user-data";

/// The file the scratch file system lives in, and its size (sparse).
pub const SCRATCH_FILE: &str = "/var/lib/js-agent-scratch.xfs";

/// The size of [`SCRATCH_FILE`], as `truncate --size` reads it.
pub const SCRATCH_SIZE: &str = "16G";

/// The scratch mount unit: `crun_host_setup.sh`'s default `--scratch-mount-unit`.
pub const SCRATCH_UNIT: &str = "var-lib-js\\x2dagent-scratch.mount";

/// The user-data reader.
pub const READER_UNIT: &str = "js-agent-user-data.service";

/// The setup script, in the image.
pub const SETUP_SCRIPT: &str = "/usr/share/js-agent/crun_host_setup.sh";

/// The crun unit the setup script installs, in the image.
pub const CRUN_UNIT: &str = "/usr/share/js-agent/js-agent-crun.service";

/// The tmpfiles file the setup script installs, in the image.
pub const CRUN_TMPFILES: &str = "/usr/share/js-agent/tmpfiles-crun.conf";

/// The drop-in of the agent's unit that the user data writes.
pub const USER_DATA_DROP_IN: &str = "/etc/systemd/system/js-agent-crun.service.d/50-user-data.conf";

/// The join token the user data writes.
pub const USER_DATA_TOKEN: &str = "/etc/js-agent/token";

/// hostkeeper's config, which the user data may write.
pub const USER_DATA_HOSTKEEPER: &str = "/etc/hostkeeper/hostkeeper.toml";

/// The units the image enables, sorted.
pub const ENABLED_UNITS: [&str; 4] = [
    "hostkeeper.service",
    "js-agent-crun.service",
    READER_UNIT,
    SCRATCH_UNIT,
];

/// The words a key that holds secret material has, compared with the key lower-cased and with
/// `-` read as `_`. A key that ends in `_file` or `_path` names a file, not its content, and is
/// not refused (`--token-file`).
pub const SECRET_WORDS: [&str; 14] = [
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "apikey",
    "api_key",
    "private_key",
    "privatekey",
    "access_key",
    "accesskey",
    "bearer",
    "authorization",
];

/// The platforms an image is made for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    /// `linux-x86_64`, the config key `images.x86_64`.
    X86_64,
    /// `linux-arm64`, the config key `images.arm64`.
    Arm64,
}

/// Every platform, in the order the recipes are written.
pub const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Arm64];

impl Arch {
    /// The platform's name: the table of the pins file and the directory of its files.
    #[must_use]
    pub fn platform(self) -> &'static str {
        match self {
            Arch::X86_64 => "linux-x86_64",
            Arch::Arm64 => "linux-arm64",
        }
    }

    /// The key of the provider's `images` table this image's id goes under.
    #[must_use]
    pub fn images_key(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Arm64 => "arm64",
        }
    }
}

/// A file the image build installs: where it is in the repository and in the image.
#[derive(Clone, Copy, Debug)]
pub struct Baked {
    /// The file in the repository, relative to the root.
    pub source: &'static str,
    /// Its path in the image.
    pub path: &'static str,
    /// Its mode in the image (octal).
    pub mode: &'static str,
    /// Whether it is configuration (a unit, a drop-in, a tmpfiles file), whose keys are refused
    /// when they look like secret material. The setup script is a program: its variables are
    /// not settings (`passwd=` holds a line of `/etc/passwd`), and its SHA-256 pins it.
    pub config: bool,
}

/// Every file the build installs from the repository, in the order the recipe lists them.
pub const BAKED: [Baked; 6] = [
    Baked {
        source: "packaging/js-agent/crun_host_setup.sh",
        path: SETUP_SCRIPT,
        mode: "0755",
        config: false,
    },
    Baked {
        source: "packaging/js-agent/js-agent-crun.service",
        path: CRUN_UNIT,
        mode: "0644",
        config: true,
    },
    Baked {
        source: "packaging/js-agent/tmpfiles-crun.conf",
        path: CRUN_TMPFILES,
        mode: "0644",
        config: true,
    },
    Baked {
        source: "packaging/machine-image/js-agent-user-data.service",
        path: "/etc/systemd/system/js-agent-user-data.service",
        mode: "0644",
        config: true,
    },
    Baked {
        source: "packaging/machine-image/js-agent-crun-image.conf",
        path: "/etc/systemd/system/js-agent-crun.service.d/10-image.conf",
        mode: "0644",
        config: true,
    },
    Baked {
        source: "packaging/machine-image/scratch.mount",
        path: "/etc/systemd/system/var-lib-js\\x2dagent-scratch.mount",
        mode: "0644",
        config: true,
    },
];

/// A path the provider's user data fills at boot.
#[derive(Clone, Copy, Debug)]
pub struct UserDataPath {
    /// The path.
    pub path: &'static str,
    /// The mode the user data gives it.
    pub mode: &'static str,
    /// Whether the agent starts without it (the reader asserts the required ones).
    pub required: bool,
    /// What it holds.
    pub what: &'static str,
}

/// The paths the provider's user data fills, each with `write_files` of a cloud-config.
pub const USER_DATA_PATHS: [UserDataPath; 3] = [
    UserDataPath {
        path: USER_DATA_DROP_IN,
        mode: "0644",
        required: true,
        what: "the agent's command line: `[Service]`, `ExecStart=` cleared, and `ExecStart=` set \
               again with this host's --server and --pool",
    },
    UserDataPath {
        path: USER_DATA_TOKEN,
        mode: "0600",
        required: true,
        what: "the join token the agent reads with --token-file",
    },
    UserDataPath {
        path: USER_DATA_HOSTKEEPER,
        mode: "0600",
        required: false,
        what: "hostkeeper's config, filled from its template; without it hostkeeper does not start",
    },
];

/// The arguments of `image-recipe`.
#[derive(clap::Args)]
pub struct Args {
    /// The directory the files are written under, one directory per platform; made when missing.
    #[arg(long)]
    pub out: PathBuf,
    /// The repository root, where the pins, the crun steps and the baked files are read.
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// The pins file, relative to `--root`.
    #[arg(long, default_value = PINS)]
    pub pins: PathBuf,
}

/// Why `image-recipe` wrote nothing, or stopped writing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An input could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// An output could not be written.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The file or directory.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// A TOML input does not parse.
    #[error("{}: {why}", path.display())]
    Toml {
        /// The file.
        path: PathBuf,
        /// The parser's message.
        why: String,
    },
    /// A field of a TOML input is missing, unknown or of the wrong form.
    #[error("{}: {at}: {why}", path.display())]
    Field {
        /// The file.
        path: PathBuf,
        /// The field, dotted (`linux-x86_64.crun.url`).
        at: String,
        /// What is wrong with it.
        why: String,
    },
    /// An input holds a key that looks like secret material.
    #[error(
        "{}: {at}: the key {key:?} looks like secret material, and a machine image carries none \
         (its configuration comes from the provider's user data at boot)",
        path.display()
    )]
    Secret {
        /// The file.
        path: PathBuf,
        /// Where: the dotted field of a pins key, or `line <n>` of a baked file.
        at: String,
        /// The key, as written.
        key: String,
    },
}

/// A file pinned by its URL and SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    /// Where it is downloaded from.
    pub url: String,
    /// Its SHA-256.
    pub sha256: Sha256,
}

/// The pins of one platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchPins {
    /// The base image.
    pub base: Pin,
    /// The distribution's crun package.
    pub crun: Pin,
    /// hostkeeper's deb.
    pub hostkeeper: Pin,
    /// js-agent's release archive.
    pub js_agent: Pin,
}

/// The pins of every platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pins {
    /// `linux-x86_64`.
    pub x86_64: ArchPins,
    /// `linux-arm64`.
    pub arm64: ArchPins,
}

impl Pins {
    /// The pins of `arch`.
    #[must_use]
    pub fn of(&self, arch: Arch) -> &ArchPins {
        match arch {
            Arch::X86_64 => &self.x86_64,
            Arch::Arm64 => &self.arm64,
        }
    }
}

/// A step of the crun host setup, as the steps file and the runbook's table name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrunStep {
    /// The table's `step` column.
    pub name: String,
    /// The table's `what` column.
    pub what: String,
    /// The script's function for the step.
    pub function: String,
}

/// A baked file, read.
#[derive(Clone, Debug)]
pub struct File {
    /// What it is and where it goes.
    pub baked: Baked,
    /// Its text.
    pub text: String,
}

/// What a recipe is made from besides the pins: the baked files and the crun steps.
#[derive(Clone, Debug)]
pub struct Inputs {
    /// The baked files, in the order of [`BAKED`].
    files: Vec<File>,
    /// The crun host setup's steps.
    crun_steps: Vec<CrunStep>,
}

impl Inputs {
    /// The SHA-256 of the baked file installed at `path`, in hex. Every path of [`BAKED`] is
    /// read once, so there is exactly one.
    fn sha256_of(&self, path: &str) -> String {
        self.files
            .iter()
            .filter(|f| f.baked.path == path)
            .map(|f| Sha256::of(f.text.as_bytes()).to_string())
            .collect()
    }
}

/// The files of one platform: name in its directory, and text.
pub type Platform = [(&'static str, String); 4];

/// Runs the verb: reads and checks every input, then writes both platforms.
///
/// # Errors
///
/// [`Error`]: an input that cannot be read, does not parse or holds secret material (nothing
/// is written), or an output that cannot be written.
pub fn run(args: &Args) -> Result<String, Error> {
    let pins_path = args.root.join(&args.pins);
    let pins = parse_pins(&pins_path, &read(&pins_path)?)?;
    let inputs = read_inputs(&args.root)?;
    let mut digests = Vec::new();
    for arch in ARCHES {
        let files = platform(arch, &pins, &inputs);
        digests.push(format!(
            "{} (recipe sha256 {})",
            arch.platform(),
            Sha256::of(files[0].1.as_bytes())
        ));
        write_platform(&args.out, arch, files)?;
    }
    Ok(format!(
        "image-recipe: wrote {} under {}",
        digests.join(" and "),
        args.out.display()
    ))
}

/// The four files of `arch`: `recipe.json`, `user-data`, `meta-data` and `base.sha256`.
#[must_use]
pub fn platform(arch: Arch, pins: &Pins, inputs: &Inputs) -> Platform {
    let recipe = format!("{:#}\n", recipe(arch, pins, inputs));
    let seed = user_data(arch, pins, inputs, &recipe);
    let meta = format!(
        "instance-id: js-image-build-{0}\nlocal-hostname: js-image-build-{0}\n",
        arch.platform()
    );
    let base = format!("{}  base.img\n", pins.of(arch).base.sha256);
    [
        ("recipe.json", recipe),
        ("user-data", seed),
        ("meta-data", meta),
        ("base.sha256", base),
    ]
}

/// The recipe of `arch`.
#[must_use]
pub fn recipe(arch: Arch, pins: &Pins, inputs: &Inputs) -> Value {
    let p = pins.of(arch);
    let files: Vec<Value> = inputs
        .files
        .iter()
        .map(|f| {
            json!({
                "source": f.baked.source,
                "path": f.baked.path,
                "mode": f.baked.mode,
                "sha256": Sha256::of(f.text.as_bytes()).to_string(),
                "text": f.text,
            })
        })
        .collect();
    let steps: Vec<Value> = inputs
        .crun_steps
        .iter()
        .map(|s| json!({"name": s.name, "what": s.what, "function": s.function}))
        .collect();
    let user_data: Vec<Value> = USER_DATA_PATHS
        .iter()
        .map(|u| json!({"path": u.path, "mode": u.mode, "required": u.required, "what": u.what}))
        .collect();
    json!({
        "format": 1,
        "platform": arch.platform(),
        "images_key": arch.images_key(),
        "base": download("base", "base.img", &p.base),
        "archives": archives(p).iter().map(|(name, file, pin)| download(name, file, pin)).collect::<Vec<_>>(),
        "files": files,
        "crun_host_setup": {
            "runbook": CRUN_RUNBOOK,
            "steps_source": CRUN_STEPS,
            "steps": steps,
            "command": setup_command(inputs),
        },
        "enabled_units": ENABLED_UNITS,
        "user_data": {
            "reader": READER_UNIT,
            "format": "a #cloud-config whose write_files writes the paths below",
            "paths": user_data,
        },
        "build": build_commands(p, inputs),
    })
}

/// The archives baked in: name, file name in [`STAGE`], and pin.
fn archives(p: &ArchPins) -> [(&'static str, &'static str, &Pin); 3] {
    [
        ("crun", "crun.deb", &p.crun),
        ("hostkeeper", "hostkeeper.deb", &p.hostkeeper),
        ("js-agent", "js-agent.tar.gz", &p.js_agent),
    ]
}

/// A download, as the recipe lists it.
fn download(name: &str, file: &str, pin: &Pin) -> Value {
    json!({"name": name, "file": file, "url": pin.url, "sha256": pin.sha256.to_string()})
}

/// The path of `file` in [`STAGE`].
fn staged(file: &str) -> String {
    format!("{STAGE}/{file}")
}

/// The command of `crun_host_setup.sh` that makes the steps: an apply, with the unit and the
/// tmpfiles file by path and SHA-256, and the placeholders in place of the server and the pool.
#[must_use]
pub fn setup_command(inputs: &Inputs) -> Vec<String> {
    [
        SETUP_SCRIPT,
        "--apply",
        "--server",
        SERVER_PLACEHOLDER,
        "--pool",
        POOL_PLACEHOLDER,
        "--unit",
        CRUN_UNIT,
        "--unit-sha256",
        &inputs.sha256_of(CRUN_UNIT),
        "--tmpfiles",
        CRUN_TMPFILES,
        "--tmpfiles-sha256",
        &inputs.sha256_of(CRUN_TMPFILES),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The build's commands, in order, each one program and its arguments.
#[must_use]
pub fn build_commands(p: &ArchPins, inputs: &Inputs) -> Vec<Vec<String>> {
    let words = |list: &[&str]| list.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    let agent = staged("js-agent");
    let mut commands = vec![words(&["mkdir", "--parents", &agent])];
    for (_, file, pin) in archives(p) {
        commands.push(words(&[
            "curl",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--output",
            &staged(file),
            &pin.url,
        ]));
    }
    commands.extend([
        words(&["sha256sum", "--check", "--strict", &staged("SHA256SUMS")]),
        words(&["apt-get", "update"]),
        words(&[
            "apt-get",
            "install",
            "--yes",
            "--no-install-recommends",
            &staged("crun.deb"),
            &staged("hostkeeper.deb"),
            "uidmap",
            "xfsprogs",
        ]),
        words(&[
            "tar",
            "--extract",
            "--gzip",
            "--file",
            &staged("js-agent.tar.gz"),
            "--directory",
            &agent,
            "--strip-components",
            "1",
        ]),
        words(&[
            "install",
            "--mode",
            "0755",
            &format!("{agent}/js-agent"),
            "/usr/bin/js-agent",
        ]),
        words(&[
            "install",
            "-D",
            "--mode",
            "0644",
            &format!("{agent}/updater.d/js-agent.toml"),
            "/usr/share/js-agent/updater.d/js-agent.toml",
        ]),
        words(&[
            "install",
            "-D",
            "--mode",
            "0644",
            &format!("{agent}/LICENSE"),
            "/usr/share/doc/js-agent/copyright",
        ]),
        words(&["truncate", "--size", SCRATCH_SIZE, SCRATCH_FILE]),
        words(&["mkfs.xfs", "-q", SCRATCH_FILE]),
        words(&["systemctl", "daemon-reload"]),
        words(&["systemctl", "enable", "--now", SCRATCH_UNIT]),
        setup_command(inputs),
        words(&["systemctl", "daemon-reload"]),
    ]);
    let mut enable = words(&["systemctl", "enable"]);
    enable.extend(words(&ENABLED_UNITS));
    commands.extend([
        enable,
        words(&["rm", "--recursive", "--one-file-system", STAGE]),
        words(&["cloud-init", "clean", "--logs", "--machine-id"]),
        words(&["printf", "js-image-build-%s\\n", "done"]),
    ]);
    commands
}

/// The cloud-init user data that builds the image of `arch` from its base image: a
/// `#cloud-config` whose body is JSON (which YAML reads as it is).
#[must_use]
pub fn user_data(arch: Arch, pins: &Pins, inputs: &Inputs, recipe: &str) -> String {
    let p = pins.of(arch);
    let file = |path: &str, mode: &str, content: &str| json!({"path": path, "owner": "root:root", "permissions": mode, "content": content});
    let sums: String = archives(p)
        .iter()
        .map(|(_, name, pin)| format!("{}  {}\n", pin.sha256, staged(name)))
        .collect();
    let mut files: Vec<Value> = inputs
        .files
        .iter()
        .map(|f| file(f.baked.path, f.baked.mode, &f.text))
        .collect();
    files.push(file(&staged("SHA256SUMS"), "0644", &sums));
    files.push(file(RECIPE_PATH, "0644", recipe));
    let mut runcmd = vec![json!("set -eu")];
    runcmd.extend(build_commands(p, inputs).into_iter().map(Value::from));
    let body = json!({
        "write_files": files,
        "runcmd": runcmd,
        "power_state": {"mode": "poweroff", "condition": true},
    });
    format!("#cloud-config\n{body:#}\n")
}

/// Writes the files of `arch` into `<out>/<platform>/`.
fn write_platform(out: &Path, arch: Arch, files: Platform) -> Result<(), Error> {
    let dir = out.join(arch.platform());
    std::fs::create_dir_all(&dir).map_err(|source| Error::Write {
        path: dir.clone(),
        source,
    })?;
    for (name, text) in files {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(|source| Error::Write { path, source })?;
    }
    Ok(())
}

/// The text of `path`.
fn read(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })
}

/// Reads the baked files and the crun steps under `root`, refusing a baked configuration file
/// that holds a secret-looking key.
///
/// # Errors
///
/// [`Error::Read`], [`Error::Secret`], or the steps file's [`Error::Toml`] and [`Error::Field`].
pub fn read_inputs(root: &Path) -> Result<Inputs, Error> {
    let mut files = Vec::new();
    for baked in BAKED {
        let path = root.join(baked.source);
        let text = read(&path)?;
        if baked.config {
            refuse_secret_text(&path, &text)?;
        }
        files.push(File { baked, text });
    }
    let steps = root.join(CRUN_STEPS);
    let crun_steps = parse_crun_steps(&steps, &read(&steps)?)?;
    Ok(Inputs { files, crun_steps })
}

/// Whether `key` looks like a key that holds secret material (see [`SECRET_WORDS`]).
#[must_use]
pub fn secret_looking(key: &str) -> bool {
    let key = key
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('-', "_");
    let names_a_file = key.ends_with("_file") || key.ends_with("_path");
    !names_a_file && SECRET_WORDS.iter().any(|word| key.contains(word))
}

/// The keys of `line`: each word (letters, digits, `_` and `-`) that is followed by `=` or that
/// starts with `--` (a flag), without its leading dashes.
#[must_use]
pub fn keys_of(line: &str) -> Vec<&str> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut keys = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find(is_word) {
        let tail = &rest[start..];
        let end = tail.find(|c: char| !is_word(c)).unwrap_or(tail.len());
        let (word, after) = tail.split_at(end);
        if word.starts_with("--") || after.starts_with('=') {
            keys.push(word.trim_start_matches('-'));
        }
        rest = after;
    }
    keys
}

/// Refuses the first line of `text` with a secret-looking key.
fn refuse_secret_text(path: &Path, text: &str) -> Result<(), Error> {
    for (n, line) in text.lines().enumerate() {
        if let Some(key) = keys_of(line).into_iter().find(|k| secret_looking(k)) {
            return Err(Error::Secret {
                path: path.to_owned(),
                at: format!("line {}", n + 1),
                key: key.to_owned(),
            });
        }
    }
    Ok(())
}

/// `key` under the dotted field `at`.
fn join(at: &str, key: &str) -> String {
    if at.is_empty() {
        key.to_owned()
    } else {
        format!("{at}.{key}")
    }
}

/// A [`Error::Field`].
fn bad(path: &Path, at: &str, why: impl Into<String>) -> Error {
    Error::Field {
        path: path.to_owned(),
        at: at.to_owned(),
        why: why.into(),
    }
}

/// `text` as a TOML table.
fn parse_toml(path: &Path, text: &str) -> Result<toml::Table, Error> {
    toml::from_str(text).map_err(|e| Error::Toml {
        path: path.to_owned(),
        why: e.to_string().trim_end().to_owned(),
    })
}

/// Refuses a key of `table` (at `at`) that is not one of `known`.
fn only(path: &Path, at: &str, table: &toml::Table, known: &[&str]) -> Result<(), Error> {
    match table.keys().find(|k| !known.contains(&k.as_str())) {
        Some(key) => Err(bad(path, &join(at, key), "is not a known key")),
        None => Ok(()),
    }
}

/// The value of `key` in `table` (at `at`).
fn field<'a>(
    path: &Path,
    at: &str,
    table: &'a toml::Table,
    key: &str,
) -> Result<&'a toml::Value, Error> {
    table
        .get(key)
        .ok_or_else(|| bad(path, &join(at, key), "is missing"))
}

/// `value` (the field `at`) as a table.
fn as_table<'a>(path: &Path, at: &str, value: &'a toml::Value) -> Result<&'a toml::Table, Error> {
    value
        .as_table()
        .ok_or_else(|| bad(path, at, "must be a table"))
}

/// The string `key` of `table` (at `at`).
fn string(path: &Path, at: &str, table: &toml::Table, key: &str) -> Result<String, Error> {
    field(path, at, table, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| bad(path, &join(at, key), "must be a string"))
}

/// Refuses a secret-looking key anywhere in `table` (at `at`), tables in arrays included.
fn refuse_secret_keys(path: &Path, at: &str, table: &toml::Table) -> Result<(), Error> {
    for (key, value) in table {
        let here = join(at, key);
        if secret_looking(key) {
            return Err(Error::Secret {
                path: path.to_owned(),
                at: here,
                key: key.clone(),
            });
        }
        refuse_secret_value(path, &here, value)?;
    }
    Ok(())
}

/// [`refuse_secret_keys`] of a table, or of each item of an array; nothing for a scalar.
fn refuse_secret_value(path: &Path, at: &str, value: &toml::Value) -> Result<(), Error> {
    match value {
        toml::Value::Table(table) => refuse_secret_keys(path, at, table),
        toml::Value::Array(items) => items
            .iter()
            .try_for_each(|item| refuse_secret_value(path, at, item)),
        _ => Ok(()),
    }
}

/// Why `url` is not a pin URL: HTTPS, printable ASCII, a host and a path, and no user, query or
/// fragment.
///
/// # Errors
///
/// The reason, for a URL that is refused.
pub fn check_url(url: &str) -> Result<(), &'static str> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err("must be an https:// URL");
    };
    if !url.chars().all(|c| c.is_ascii_graphic()) {
        return Err("must be printable ASCII with no space");
    }
    if url.contains(['?', '#']) {
        return Err("must not carry a query or a fragment (a signed URL holds a credential)");
    }
    let (host, file) = rest.split_once('/').unwrap_or((rest, ""));
    if host.contains('@') {
        return Err("must not name a user (a credential has no place in a pin)");
    }
    if host.is_empty() || file.is_empty() {
        return Err("must name a host and a file");
    }
    Ok(())
}

/// The pin `name` of the platform table `table` (at `at`).
fn pin(path: &Path, at: &str, table: &toml::Table, name: &str) -> Result<Pin, Error> {
    let here = join(at, name);
    let pin = as_table(path, &here, field(path, at, table, name)?)?;
    only(path, &here, pin, &["sha256", "url"])?;
    let url = string(path, &here, pin, "url")?;
    check_url(&url).map_err(|why| bad(path, &join(&here, "url"), why))?;
    let sha256 = string(path, &here, pin, "sha256")?;
    let sha256 = sha256
        .parse()
        .map_err(|e: NotASha256| bad(path, &join(&here, "sha256"), e.to_string()))?;
    Ok(Pin { url, sha256 })
}

/// The pins of the platform `arch` in `table`.
fn arch_pins(path: &Path, table: &toml::Table, arch: Arch) -> Result<ArchPins, Error> {
    let at = arch.platform();
    let pins = as_table(path, at, field(path, "", table, at)?)?;
    only(path, at, pins, &["base", "crun", "hostkeeper", "js-agent"])?;
    Ok(ArchPins {
        base: pin(path, at, pins, "base")?,
        crun: pin(path, at, pins, "crun")?,
        hostkeeper: pin(path, at, pins, "hostkeeper")?,
        js_agent: pin(path, at, pins, "js-agent")?,
    })
}

/// The pins file `text` (read from `path`).
///
/// # Errors
///
/// [`Error::Toml`], [`Error::Secret`] (checked before the fields), or [`Error::Field`].
pub fn parse_pins(path: &Path, text: &str) -> Result<Pins, Error> {
    let table = parse_toml(path, text)?;
    refuse_secret_keys(path, "", &table)?;
    let platforms = ARCHES.map(Arch::platform);
    only(path, "", &table, &platforms)?;
    Ok(Pins {
        x86_64: arch_pins(path, &table, Arch::X86_64)?,
        arm64: arch_pins(path, &table, Arch::Arm64)?,
    })
}

/// The crun steps file `text` (read from `path`): its `[[step]]` tables, in order.
///
/// # Errors
///
/// [`Error::Toml`], or [`Error::Field`] for a missing, unknown or ill-formed key, or no step.
pub fn parse_crun_steps(path: &Path, text: &str) -> Result<Vec<CrunStep>, Error> {
    let table = parse_toml(path, text)?;
    only(path, "", &table, &["step"])?;
    let steps = field(path, "", &table, "step")?
        .as_array()
        .filter(|steps| !steps.is_empty())
        .ok_or_else(|| bad(path, "step", "must be an array of one or more tables"))?;
    steps
        .iter()
        .enumerate()
        .map(|(n, step)| {
            let at = format!("step[{n}]");
            let step = as_table(path, &at, step)?;
            only(path, &at, step, &["function", "name", "what"])?;
            Ok(CrunStep {
                name: string(path, &at, step, "name")?,
                what: string(path, &at, step, "what")?,
                function: string(path, &at, step, "function")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
