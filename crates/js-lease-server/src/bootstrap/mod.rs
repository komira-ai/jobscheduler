//! The bootstrap: a [`BootPlan`] rendered into the script a machine runs when it boots.
//!
//! One renderer for every provider and mode. A plan holds secrets as named slots, never
//! as text, so the same plan renders two ways:
//!
//! - [`Fill::Mask`] prints every secret as [`MASK`]. This is the text for the status page
//!   and for logs; it holds no secret because it is rendered without one.
//! - [`Fill::Real`] puts the secrets in. [`payload`] makes the [`BootPayload`] a provider
//!   delivers as it is (for Lambda, `user_data`).
//!
//! The two renders differ only at the secret bytes. A provider receives the payload and
//! nothing else: it never sees a plan, a slot or a network mode.
//!
//! What the script does, in order: prepares the image (the provider's `host_prep`),
//! blocks the metadata address (`metadata`: no container and no user but root reaches
//! it, and the agent's unit requires the block), writes the join token and the instance
//! line, runs the network mode's steps, fetches the agent and checks its SHA-256 before it can start, and installs and starts the
//! agent's unit. The first command is a `trap` on `EXIT` that removes the secret files
//! the steps wrote and the traces cloud-init leaves of the script.
//!
//! The unit registers the agent under the machine's name (`--host-id`), which is how the
//! scaler finds the host of a record; pulls an image only when it is missing
//! (`--container-pull missing`: a machine the scaler launches is new and holds no image, so
//! the agent's default of `never` would fail every job on it; a run's image must be pinned
//! by a sha256 digest, so what is pulled is the content the submitter named, and the host
//! reaches the registry that name gives only over the pool's network); and names a hook program the bootstrap writes
//! ([`GPU_TOOLKIT_HOOK_PATH`], mode 0700, which runs `nvidia-container-cli info`, so it
//! fails without a working driver and toolkit). The flag is on every offer: the agent runs
//! the hook only when the host offers a GPU. The unit names the container program, the state root and the scratch root
//! ([`CONTAINER_BIN`], [`STATE_ROOT`], [`SCRATCH_ROOT`] of `js-elastic`), and requires the
//! scratch root's mount (`RequiresMountsFor=`), so a machine whose provider did not mount
//! it does not start the agent. The script makes the two roots' directories; what puts a
//! file system of its own under the scratch root, and what checks the driver and the
//! container program, is the provider's `host_prep`. The unit restarts the agent always
//! and never lists an exit status that stops the restart: the agent exits with a status
//! on purpose and is to be started again.
//!
//! Not rendered here: the host contract check of what the agent finds, which the agent's
//! own report makes at admission.

mod metadata;
mod shell;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use js_elastic::{
    BootPayload, BootPlan, BootStep, CONTAINER_BIN, SCRATCH_ROOT, STATE_ROOT, Secret, SlotName,
};
pub use metadata::{METADATA_ADDRESS, METADATA_RULES_PATH, METADATA_TABLE, METADATA_UNIT, NFT_BIN};
use shell::{hex, single_quoted, unit_quoted, word};

/// What a secret prints as in a masked render.
pub const MASK: &str = "***";

/// The slot of the join token, which every plan has.
pub const JOIN_TOKEN_SLOT: &str = "join_token";

/// The most steps a plan may hold, the provider's and the network mode's together.
pub const MAX_STEPS: usize = 64;

/// The most bytes one secret may have.
pub const MAX_SECRET_BYTES: usize = 4096;

/// Where the join token is written. The agent reads it with `--token-file`.
pub const TOKEN_PATH: &str = "/etc/jobscheduler/token";

/// Where the instance line (`<pool>:<name>`, not secret) is written.
pub const INSTANCE_PATH: &str = "/etc/jobscheduler/instance";

/// Where the agent is installed.
pub const AGENT_PATH: &str = "/usr/local/bin/js-agent";

/// Where the GPU toolkit hook is installed.
pub const GPU_TOOLKIT_HOOK_PATH: &str = "/usr/local/lib/js-agent/gpu-toolkit-hook";

/// What cloud-init leaves of the script and its user data, removed by the `EXIT` trap.
/// Each entry is a fixed, shell-safe word and some are globs, so the list is written
/// unquoted. It is the design's list; until a live run on the stock image confirms it,
/// nothing claims that the traces are gone.
pub const CLOUD_INIT_TRACES: [&str; 4] = [
    "/var/lib/cloud/instance",
    "/var/lib/cloud/instances/*",
    "/var/lib/cloud/data/*",
    "/var/log/cloud-init*.log",
];

/// Why a plan cannot be rendered. No variant holds a secret; a slot is named.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// A step names a slot that no secret fills.
    #[error("no secret for the slot {0}")]
    MissingSlot(String),
    /// A secret fills a slot that no step names.
    #[error("the slot {0} is filled but no step names it")]
    UnusedSlot(String),
    /// Two secrets fill one slot.
    #[error("the slot {0} is filled twice")]
    DuplicateSlot(String),
    /// A secret is empty, has a NUL, or is over [`MAX_SECRET_BYTES`].
    #[error("the secret for the slot {0} must be 1 to {MAX_SECRET_BYTES} bytes with no NUL")]
    BadSecret(String),
    /// The plan has more steps than [`MAX_STEPS`].
    #[error("a plan holds at most {MAX_STEPS} steps, not {0}")]
    TooManySteps(usize),
    /// A `Run` step has no program.
    #[error("a run step needs a program")]
    EmptyRun,
    /// A value of the plan has a control character, which a line of the script cannot hold.
    #[error("the {0} must not hold a control character")]
    BadValue(&'static str),
    /// The rendered payload is over what the provider carries.
    #[error("the boot payload is {size} bytes and the provider carries at most {limit}")]
    PayloadTooLarge {
        /// The payload's size.
        size: usize,
        /// The provider's limit.
        limit: usize,
    },
}

/// The secrets of one launch, by slot.
#[derive(Debug)]
pub struct BootSecrets {
    slots: BTreeMap<SlotName, Secret>,
}

impl BootSecrets {
    /// The join token and the secrets the network mode contributed.
    ///
    /// # Errors
    ///
    /// [`RenderError::DuplicateSlot`] when two secrets, the join token included, fill
    /// one slot.
    pub fn new(
        join_token: Secret,
        contributed: Vec<(SlotName, Secret)>,
    ) -> Result<Self, RenderError> {
        let mut slots = BTreeMap::new();
        slots.insert(SlotName(JOIN_TOKEN_SLOT.to_owned()), join_token);
        for (slot, secret) in contributed {
            if slots.contains_key(&slot) {
                return Err(RenderError::DuplicateSlot(slot.0));
            }
            slots.insert(slot, secret);
        }
        Ok(Self { slots })
    }
}

/// What fills the secret slots of a render.
#[derive(Debug, Clone, Copy)]
pub enum Fill<'a> {
    /// Every secret prints as [`MASK`].
    Mask,
    /// The secrets themselves.
    Real(&'a BootSecrets),
}

/// The script state while it is written.
struct Writer<'a> {
    out: String,
    fill: Fill<'a>,
    used: BTreeSet<&'a SlotName>,
}

impl<'a> Writer<'a> {
    fn line(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// The text that stands for the secret of `slot`, already single-quoted.
    fn secret(&mut self, slot: &'a SlotName) -> Result<String, RenderError> {
        self.used.insert(slot);
        match self.fill {
            Fill::Mask => Ok(single_quoted(MASK)),
            Fill::Real(secrets) => {
                let secret = secrets
                    .slots
                    .get(slot)
                    .ok_or_else(|| RenderError::MissingSlot(slot.0.clone()))?;
                let text = secret.expose();
                if text.is_empty() || text.len() > MAX_SECRET_BYTES || text.contains('\0') {
                    return Err(RenderError::BadSecret(slot.0.clone()));
                }
                Ok(single_quoted(text))
            }
        }
    }

    /// Writes the secret of `slot` to `path`, readable by its owner alone.
    fn secret_file(&mut self, path: &str, slot: &'a SlotName) -> Result<(), RenderError> {
        let secret = self.secret(slot)?;
        let path = word(path);
        self.line(&format!("printf '%s' {secret} > {path}"));
        self.line(&format!("chmod 0600 -- {path}"));
        Ok(())
    }

    /// Downloads `url` to `dest` over https and stops unless it has `sha256`.
    fn fetch(&mut self, url: &str, sha256: &[u8; 32], dest: &str) {
        let (url, dest) = (word(url), word(dest));
        self.line(&format!(
            "curl --fail --silent --show-error --location --proto =https --output {dest} --url {url}"
        ));
        self.line(&format!(
            "printf '%s  %s\\n' {} {dest} | sha256sum --check --status -",
            hex(sha256)
        ));
    }

    fn step(&mut self, step: &'a BootStep) -> Result<(), RenderError> {
        match step {
            BootStep::Run(argv) if argv.is_empty() => Err(RenderError::EmptyRun),
            BootStep::Run(argv) => {
                let words: Vec<String> = argv.iter().map(|arg| word(arg)).collect();
                self.line(&words.join(" "));
                Ok(())
            }
            BootStep::SecretFile { path, slot } => self.secret_file(path, slot),
            BootStep::Fetch { url, sha256, dest } => {
                self.fetch(url, sha256, dest);
                Ok(())
            }
        }
    }
}

/// Refuses a value with a control character: it would end its line.
fn plain_value(field: &'static str, value: &str) -> Result<(), RenderError> {
    match value.chars().any(char::is_control) {
        true => Err(RenderError::BadValue(field)),
        false => Ok(()),
    }
}

/// The script `plan` describes, with its secret slots filled as `fill` says.
///
/// # Errors
///
/// [`RenderError`] when the plan is over a limit or holds a value the script cannot
/// carry; with [`Fill::Real`] also when a slot has no secret, a secret has no slot, or a
/// secret is unusable.
pub fn render(plan: &BootPlan, fill: Fill<'_>) -> Result<String, RenderError> {
    let steps = plan.host_prep.len() + plan.network.len();
    if steps > MAX_STEPS {
        return Err(RenderError::TooManySteps(steps));
    }
    plain_value("scheduler url", &plan.scheduler_url)?;
    plain_value("pool", &plan.pool)?;
    let join_slot = SlotName(JOIN_TOKEN_SLOT.to_owned());
    let mut w = Writer {
        out: String::new(),
        fill,
        used: BTreeSet::new(),
    };

    // The trap comes first, so a failure at any later line still removes the secrets.
    let secret_paths: Vec<String> = plan
        .host_prep
        .iter()
        .chain(&plan.network)
        .filter_map(|step| match step {
            BootStep::SecretFile { path, .. } => Some(word(path)),
            BootStep::Run(_) | BootStep::Fetch { .. } => None,
        })
        .collect();
    w.line("#!/bin/sh");
    w.line("trap cleanup EXIT");
    w.line("cleanup() {");
    let listed: String = secret_paths.iter().map(|path| format!(" {path}")).collect();
    w.line(&format!("  rm -f --{listed}"));
    w.line(&format!("  rm -rf -- {}", CLOUD_INIT_TRACES.join(" ")));
    w.line("}");
    w.line("set -eu");
    w.line("umask 077");

    w.line("# image preparation");
    for step in &plan.host_prep {
        w.step(step)?;
    }
    metadata::write(&mut w);
    w.line("# identity");
    w.line("mkdir -p /etc/jobscheduler");
    w.secret_file(TOKEN_PATH, &join_slot)?;
    w.line(&format!(
        "printf '%s\\n' {} > {INSTANCE_PATH}",
        single_quoted(&plan.instance())
    ));
    w.line("# network");
    for step in &plan.network {
        w.step(step)?;
    }
    w.line("# agent");
    w.line(&format!(
        "install -d -m 0700 -- {STATE_ROOT} {SCRATCH_ROOT}"
    ));
    w.fetch(&plan.agent.url, &plan.agent.sha256, AGENT_PATH);
    w.line(&format!("chmod 0755 {AGENT_PATH}"));
    w.line("# gpu toolkit hook");
    w.line("install -d -m 0755 -- /usr/local/lib/js-agent");
    w.line(&format!("cat > {GPU_TOOLKIT_HOOK_PATH} <<'HOOK'"));
    w.line("#!/bin/sh");
    w.line("exec nvidia-container-cli info");
    w.line("HOOK");
    w.line(&format!("chmod 0700 {GPU_TOOLKIT_HOOK_PATH}"));
    w.line("cat > /etc/systemd/system/js-agent.service <<'UNIT'");
    w.line("[Unit]");
    w.line("Description=jobscheduler agent");
    w.line("After=network-online.target");
    w.line(&format!("Requires={METADATA_UNIT}"));
    w.line(&format!("After={METADATA_UNIT}"));
    w.line("Wants=network-online.target");
    w.line(&format!("RequiresMountsFor={SCRATCH_ROOT}"));
    w.line("");
    w.line("[Service]");
    let _ = writeln!(
        w.out,
        "ExecStart={AGENT_PATH} --server {} --token-file {TOKEN_PATH} --instance-file {INSTANCE_PATH} --pool {} --host-id {} --container-bin {CONTAINER_BIN} --container-pull missing --gpu-toolkit-hook {GPU_TOOLKIT_HOOK_PATH} --state-root {STATE_ROOT} --scratch-root {SCRATCH_ROOT}",
        unit_quoted(&plan.scheduler_url),
        unit_quoted(&plan.pool),
        unit_quoted(plan.host.as_str())
    );
    w.line("Restart=always");
    w.line("RestartSec=5");
    w.line("");
    w.line("[Install]");
    w.line("WantedBy=multi-user.target");
    w.line("UNIT");
    w.line("chmod 0644 /etc/systemd/system/js-agent.service");
    w.line("systemctl daemon-reload");
    w.line("systemctl enable --now js-agent.service");

    let unused = match fill {
        Fill::Mask => None,
        Fill::Real(secrets) => secrets.slots.keys().find(|slot| !w.used.contains(slot)),
    };
    match unused {
        Some(slot) => Err(RenderError::UnusedSlot(slot.0.clone())),
        None => Ok(w.out),
    }
}

/// The boot payload of `plan` with `secrets` in it, for a provider that carries at most
/// `limit` bytes.
///
/// # Errors
///
/// [`RenderError`] as [`render`] with [`Fill::Real`]; also [`RenderError::PayloadTooLarge`]
/// when the script is over `limit`.
pub fn payload(
    plan: &BootPlan,
    secrets: &BootSecrets,
    limit: usize,
) -> Result<BootPayload, RenderError> {
    let text = render(plan, Fill::Real(secrets))?;
    match text.len() > limit {
        true => Err(RenderError::PayloadTooLarge {
            size: text.len(),
            limit,
        }),
        false => Ok(BootPayload::new(text)),
    }
}
