//! What the configuration file tests share: a provider kind shaped like the cloud one,
//! builders for the file's tables, and the design's examples read out of the docs file.

use std::fs;
use std::path::Path;

use js_elastic::{
    AcquireRequest, Acquired, BootStep, ElasticProvider, GpuClass, Held, HostRef, Offer,
    ProviderError, Released,
};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles, SharedClock,
    StartupError,
};
use js_pool_static::EXAMPLE_CONFIG;
use js_types::ExecMode;

use crate::support::{Scratch, T0, TOKEN};

/// A secret that must never be printed: the content of the key and OAuth files.
pub const API_KEY: &str = "PLANTED-API-KEY-VALUE";
pub const OAUTH_SECRET: &str = "PLANTED-OAUTH-SECRET-VALUE";

/// An elastic provider that launches nothing.
struct NullElastic;

impl ElasticProvider for NullElastic {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        Ok(Vec::new())
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        0
    }

    fn acquire(&self, _request: &AcquireRequest) -> Acquired {
        Acquired::Unknown
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        Ok(Vec::new())
    }

    fn release(&self, _id: &HostRef) -> Released {
        Released::Unknown
    }
}

const KNOWN: &[&str] = &["api_key_file", "regions", "types", "ssh_key", "api_url"];

fn kind_error(field: &str, reason: impl Into<String>) -> KindError {
    KindError {
        field: field.to_owned(),
        reason: reason.into(),
    }
}

fn built(gpus: u64, exec: Vec<ExecMode>) -> Built {
    Built::Elastic(ElasticBuilt {
        provider: Box::new(NullElastic),
        floor: Floor {
            gpus,
            gpu_class: Some(GpuClass::parse("nvidia-a10").unwrap()),
        },
        exec,
    })
}

/// A kind with the table of the cloud provider of the design's examples: it owns its
/// fields, refuses the ones it does not know, reads the key file through the server's
/// reader, and checks the names of the types. It launches nothing, on hosts that run
/// containers, and its floor is one GPU of class `nvidia-a10`.
fn cloud_kind(table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    if let Some(key) = table.keys().find(|key| !KNOWN.contains(&key.as_str())) {
        let reason = format!("unknown field; known: {}", KNOWN.join(", "));
        return Err(kind_error(key, reason));
    }
    let Some(toml::Value::String(file)) = table.get("api_key_file") else {
        return Err(kind_error("api_key_file", "required"));
    };
    let read = ctx.files.read(Path::new(file));
    read.map_err(|reason| kind_error("api_key_file", reason))?;
    if let Some(toml::Value::Array(rows)) = table.get("types") {
        for (index, row) in rows.iter().enumerate() {
            let name = row.get("name").and_then(toml::Value::as_str).unwrap_or("");
            let valid = (1..=64).contains(&name.len())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !valid {
                let field = format!("types[{}].name", index + 1);
                return Err(kind_error(
                    &field,
                    "must be 1 to 64 lower case letters, digits or '_'",
                ));
            }
        }
    }
    Ok(built(1, vec![ExecMode::Container]))
}

/// A kind whose floor says GPUs and no class of them, which no pool can have.
fn broken_floor(_table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    Ok(Built::Elastic(ElasticBuilt {
        provider: Box::new(NullElastic),
        floor: Floor {
            gpus: 1,
            gpu_class: None,
        },
        exec: vec![ExecMode::Container],
    }))
}

/// A kind that launches hosts of either mode, with no GPU.
fn any_mode(_table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    Ok(Built::Elastic(ElasticBuilt {
        provider: Box::new(NullElastic),
        floor: Floor {
            gpus: 0,
            gpu_class: None,
        },
        exec: vec![ExecMode::Container, ExecMode::SandboxExec],
    }))
}

/// The kinds of the tests: `static` as the server ships it, `lambdalabs` as
/// [`cloud_kind`], and two more.
pub fn kinds() -> Kinds {
    Kinds::standard()
        .with("lambdalabs", cloud_kind)
        .with("broken-floor", broken_floor)
        .with("any-mode", any_mode)
}

/// A clock that stands still.
pub fn clock() -> SharedClock {
    SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)))
}

/// Checks `text` as the file `js.toml` of `scratch`.
pub fn load(scratch: &Scratch, text: &str) -> Result<Config, StartupError> {
    let path = scratch.write("js.toml", text, 0o644);
    Config::from_config_file(&path, &kinds(), &OwnerOnlyFiles, clock())
}

/// The message with which `text` is refused.
pub fn refusal(scratch: &Scratch, text: &str) -> String {
    load(scratch, text)
        .err()
        .expect("the file should be refused")
        .to_string()
}

/// `text` as a TOML string.
pub fn quote(text: &str) -> String {
    format!("{text:?}")
}

/// `path` as a TOML string.
pub fn quoted(path: &Path) -> String {
    quote(&path.display().to_string())
}

/// A `[server]` table with the required keys, and `extra` lines after them.
pub fn server(scratch: &Scratch, extra: &str) -> String {
    let token = scratch.write("client-token", TOKEN, 0o600);
    format!(
        "[server]\nstate_file = {}\nboot = \"initialise\"\nclient_token_file = {}\n{extra}\n",
        quoted(&scratch.state()),
        quoted(&token)
    )
}

/// A `[server]` table that lets an elastic pool start: the agent routes on a port of
/// their own.
pub fn server_with_agents(scratch: &Scratch) -> String {
    server(
        scratch,
        "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"",
    )
}

/// One `[[pool]]` table, key by key.
pub struct PoolText(Vec<(String, String)>);

impl PoolText {
    /// A static pool `name` over the shipped two-host file.
    pub fn static_pool(scratch: &Scratch, name: &str) -> Self {
        let hosts = scratch.write(&format!("hosts-{name}.toml"), EXAMPLE_CONFIG, 0o644);
        Self(Vec::new())
            .set("name", &quote(name))
            .set("class", &quote("gpu-host"))
            .set(
                "provider",
                &format!("{{ kind = \"static\", hosts_file = {} }}", quoted(&hosts)),
            )
    }

    /// A pool `name` of hand-joined agents: six machines of 8 cores and 16 GiB, with no
    /// host contract.
    pub fn joined(name: &str) -> Self {
        Self(Vec::new())
            .set("kind", &quote("joined"))
            .set("name", &quote(name))
            .set("class", &quote("whole-node"))
            .set("exec", &quote("none"))
            .set("max_machines", "6")
            .set("shape", "{ cpu_millis = 8000, memory_bytes = 17179869184 }")
    }

    /// An elastic pool `gpu` of the stand-in cloud kind with every required key.
    pub fn elastic(scratch: &Scratch) -> Self {
        let key = scratch.write("api-key", API_KEY, 0o600);
        Self(Vec::new())
            .set("name", &quote("gpu"))
            .set("class", &quote("whole-node"))
            .set("max_instances", "4")
            .set("max_spend_per_hour", &quote("12.00"))
            .set("max_spend_per_day", &quote("96.00"))
            .set("idle", &quote("10m"))
            .set(
                "provider",
                &format!(
                    "{{ kind = \"lambdalabs\", api_key_file = {} }}",
                    quoted(&key)
                ),
            )
    }

    /// The pool with `key` set to the TOML value `value`.
    #[must_use]
    pub fn set(mut self, key: &str, value: &str) -> Self {
        self.0.retain(|(have, _)| have.as_str() != key);
        self.0.push((key.to_owned(), value.to_owned()));
        self
    }

    /// The pool without `key`.
    #[must_use]
    pub fn without(mut self, key: &str) -> Self {
        self.0.retain(|(have, _)| have.as_str() != key);
        self
    }

    /// The table as text.
    pub fn text(&self) -> String {
        let lines: String = self
            .0
            .iter()
            .map(|(key, value)| format!("{key} = {value}\n"))
            .collect();
        format!("[[pool]]\n{lines}")
    }
}

/// A whole file: `server` and then the pools.
pub fn file(server: &str, pools: &[&PoolText]) -> String {
    let pools: String = pools.iter().map(|pool| pool.text()).collect();
    format!("{server}\n{pools}")
}

/// An OAuth client file for the Tailscale mode, owner-only, holding [`OAUTH_SECRET`].
pub fn oauth_file(scratch: &Scratch) -> String {
    let text = format!("client_id = \"id-1\"\nclient_secret = \"{OAUTH_SECRET}\"\n");
    quoted(&scratch.write("oauth", &text, 0o600))
}

/// The first `toml` block after the heading that starts with `heading` in the design
/// document, as it is written there.
pub fn doc_block(heading: &str) -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/pools-providers-network.md");
    let text = fs::read_to_string(path).unwrap();
    let rest = &text[text.find(heading).expect("the heading is in the design")..];
    let fence = "```toml\n";
    let body = &rest[rest.find(fence).expect("the heading has a toml block") + fence.len()..];
    body[..body.find("```").unwrap()].to_owned()
}
