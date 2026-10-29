//! What the end-to-end tests of the Lambda path share: a server whose elastic pool is the
//! Lambda provider over an account in memory, behind the server's own Tailscale mode, aimed
//! at an HTTP server on 127.0.0.1 that plays the Tailscale API. Nothing here reaches the
//! live Lambda or Tailscale APIs, time is the test's clock, and the ticks are stepped.

use std::sync::Arc;

use js_lease_server::RecordsFile;
use js_provider::{Provider, StartAck};
use js_provider_lambdalabs::{FakeLambda, TerminateEffect};
use js_scaler::{Phase, State};
use serde_json::json;

use crate::agents_support::{Session, hosts};
use crate::config_support::{API_KEY, OAUTH_SECRET, oauth_file};
use crate::scaler_support::{
    account, beat, hello_body_of, records_path, spec, start, text_with, token_in,
};
use crate::support::{Harness, Reply, Scratch, TOKEN, request};
use crate::tailnet::{AUTH_KEY, Fake, Reply as Canned, TOKEN as TAILNET_TOKEN};

/// The route the access token is asked at.
pub const OAUTH: &str = "POST /api/v2/oauth/token";
/// The route an auth key is minted at.
pub const KEYS: &str = "POST /api/v2/tailnet/-/keys";
/// The route the devices are listed at.
pub const DEVICES: &str = "GET /api/v2/tailnet/-/devices";
/// The route the device of the launched host is deleted at.
pub const DELETE_DEVICE: &str = "DELETE /api/v2/device/d1";

/// The request id of the one request the consumer in these tests makes.
const REQUEST: &str = "r1";

/// A server with a launching pool `burst` of the fake cloud, the account behind it and the
/// fake Tailscale API.
pub struct Rig {
    pub h: Harness,
    pub account: Arc<FakeLambda>,
    pub tailnet: Fake,
    /// Whether the pool joins hosts to the tailnet; a pool in `default` mode does not.
    pub tailscale: bool,
}

/// The `network` table of a pool that joins its hosts to the tailnet at `url`.
fn network(scratch: &Scratch, url: &str) -> String {
    format!(
        "{{ kind = \"tailscale\", oauth_client_file = {}, tag = \"tag:js-agent\", \
         api_url = \"{url}\", binary = {{ url = \"https://pkgs.example.org/ts.tgz\", \
         sha256 = \"{}\" }} }}",
        oauth_file(scratch),
        "ab".repeat(32)
    )
}

/// The text of the file of a live pool: in Tailscale mode against `url`, or in `default`
/// mode when there is none.
fn pool_text(scratch: &Scratch, url: Option<&str>, boot: &str) -> String {
    match url {
        Some(url) => {
            let table = network(scratch, url);
            let more = [("network", table.as_str()), ("exec", "\"container\"")];
            text_with(scratch, "live", boot, &more)
        }
        None => text_with(scratch, "live", boot, &[("exec", "\"container\"")]),
    }
}

/// A fake Tailscale API whose replies work, and whose device list holds no host yet.
pub fn tailnet() -> Fake {
    Fake::tailscale("not-launched-yet")
}

/// The device list of a tailnet that holds the device of `host`.
pub fn devices_of(host: &str) -> Canned {
    let one = format!(r#"{{"id":"d1","hostname":"{host}","tags":["tag:js-agent"]}}"#);
    let other = r#"{"id":"d9","hostname":"other","tags":["tag:js-agent"]}"#;
    Canned::json(200, &format!(r#"{{"devices":[{one},{other}]}}"#))
}

/// The reply of an OAuth exchange that works.
pub fn token_reply() -> Canned {
    let body = format!(
        r#"{{"access_token":"{TAILNET_TOKEN}","token_type":"Bearer","expires_in":3600,"scope":"auth_keys"}}"#
    );
    Canned::json(200, &body)
}

/// Every secret the tests plant, which no reply of a status route may hold: the key and
/// OAuth files, the client token and the Tailscale auth key.
pub const PLANTED: [&str; 4] = [API_KEY, OAUTH_SECRET, TOKEN, AUTH_KEY];

impl Rig {
    /// A server whose pool joins hosts to the tailnet of a fake that works.
    pub async fn start(name: &str) -> Self {
        Self::over(name, tailnet(), true).await
    }

    /// A server whose pool is in `default` mode: no network step, no Tailscale call.
    pub async fn start_default(name: &str) -> Self {
        Self::over(name, tailnet(), false).await
    }

    /// A server whose pool is in Tailscale mode, or in `default` mode, against `tailnet`;
    /// the account is empty of instances and offers `gpu_1x_a100`, and terminating removes
    /// the instance at once.
    pub async fn over(name: &str, tailnet: Fake, tailscale: bool) -> Self {
        let scratch = Scratch::new(name);
        let account = account(&scratch);
        account.set_terminate_effect(TerminateEffect::Removed);
        let url = tailnet.url();
        let source = pool_text(&scratch, tailscale.then_some(url.as_str()), "initialise");
        let h = start(scratch, &source).await;
        Self {
            h,
            account,
            tailnet,
            tailscale,
        }
    }

    /// Stops the server and starts another over the same files with `--boot resume`, the
    /// same account and the same fake Tailscale API: a restart of the program.
    pub async fn restart(self) -> Self {
        let Self {
            h,
            account,
            tailnet,
            tailscale,
        } = self;
        let Harness {
            running, scratch, ..
        } = h;
        running.shutdown().await.unwrap();
        let url = tailnet.url();
        let source = pool_text(&scratch, tailscale.then_some(url.as_str()), "resume");
        let h = start(scratch, &source).await;
        Self {
            h,
            account,
            tailnet,
            tailscale,
        }
    }

    /// The registry the agent routes and the scaler share.
    pub fn registry(&self) -> js_registry::Registry {
        self.h.running.registry().unwrap()
    }

    /// What the scaler decided at its last tick.
    pub fn state(&self) -> State {
        self.h.running.scaler().unwrap().state()
    }

    /// The phase of the first record the scaler holds.
    pub fn phase(&self) -> Phase {
        self.state().records.instances[0].phase
    }

    /// The name of the machine of the first record.
    pub fn name(&self) -> String {
        self.state().records.instances[0].name.clone()
    }

    /// The records as the file holds them now.
    pub fn file(&self) -> js_scaler::Records {
        RecordsFile::new(records_path(&self.h.scratch))
            .load()
            .unwrap()
    }

    /// One tick of the server, the scaler's included.
    pub async fn tick(&self) {
        self.h.ticks.step().await;
    }

    /// The clock moved forward by `seconds`.
    pub fn advance(&self, seconds: u64) {
        self.h
            .clock
            .advance(std::time::Duration::from_secs(seconds));
    }

    /// The consumer presents its request: the answer the registry gives it.
    pub fn present(&self) -> StartAck {
        self.registry().provider().start(spec(1, REQUEST))
    }

    /// The first half of the sequence: the scaler is told what the pool offers, a request
    /// waits, the next tick launches a machine and the one after sees it booting. Returns
    /// the machine's name and the join token in its user data, and has the fake Tailscale
    /// API list the machine's device.
    pub async fn launch(&self) -> (String, String) {
        self.tick().await;
        self.wait();
        self.tick().await;
        assert_eq!(self.account.launched(), 1);
        let name = self.name();
        self.show_device(&name);
        self.tick().await;
        assert_eq!(self.phase(), Phase::Booting);
        (name, self.token(0))
    }

    /// Presents the request and asserts that it found no host and waits.
    pub fn wait(&self) {
        assert!(matches!(self.present(), StartAck::Unknown));
        assert_eq!(self.registry().waiting().len(), 1);
    }

    /// The consumer gives its request up: the registry drops it from the wait list.
    pub fn withdraw(&self) {
        let id = js_types::RequestId::new(REQUEST).unwrap();
        assert!(self.registry().cancel_request(&id));
    }

    /// The user data of the `n`th launch the account saw.
    pub fn script(&self, n: usize) -> String {
        self.account.launches()[n].user_data().unwrap().to_owned()
    }

    /// The join token in the user data of the `n`th launch.
    pub fn token(&self, n: usize) -> String {
        token_in(&self.script(n))
    }

    /// Makes the fake Tailscale API list the device of `host`.
    pub fn show_device(&self, host: &str) {
        self.tailnet.set(DEVICES, devices_of(host));
    }

    /// A hello of `host` on `instance` with `token`, from an agent that keeps a host
    /// credential, on the agent port.
    pub async fn hello(&self, token: &str, host: &str, instance: &str) -> Reply {
        let mut body = hello_body_of(host);
        body["instance"] = json!({ "provider": "burst", "instance_id": instance });
        body["accepts_host_credential"] = json!(true);
        let bytes = body.to_string().into_bytes();
        let addr = self.h.running.agent_addr().unwrap();
        request(addr, "POST", "/v1/agent/hello", Some(token), Some(&bytes)).await
    }

    /// The hello the agent the `n`th launch booted would send, derived from the rendered
    /// unit and nothing else: the host id is the one the unit's `ExecStart` gives with
    /// `--host-id`, the capabilities are the ones the agent reports for the flags it is
    /// given (a runtime and a mode with `--container-bin`, the GPU toolkit only with
    /// `--gpu-toolkit-hook`).
    pub fn agent_hello(&self, n: usize, name: &str) -> serde_json::Value {
        let script = self.script(n);
        let line = script
            .lines()
            .find_map(|line| line.trim().strip_prefix("ExecStart="))
            .expect("the unit has an ExecStart");
        let words: Vec<&str> = line.split_whitespace().collect();
        let flag = |wanted: &str| {
            let at = words.iter().position(|word| *word == wanted);
            at.map(|at| words.get(at + 1).copied().unwrap_or_default())
        };
        // Without `--host-id` the agent would register under a name of its own; that is
        // asserted by `the_unit_names_the_host_the_scaler_looks_for`, and the other tests
        // go on under the record's name.
        let host = flag("--host-id").map_or(name, |host| host.trim_matches('"'));
        let mut body = hello_body_of(host);
        body["instance"] = json!({ "provider": "burst", "instance_id": instance });
        body["accepts_host_credential"] = json!(true);
        let bytes = body.to_string().into_bytes();
        let addr = self.h.running.agent_addr().unwrap();
        request(addr, "POST", "/v1/agent/hello", Some(token), Some(&bytes)).await
    }

    /// The hello the agent the `n`th launch booted would send, derived from the rendered
    /// unit and nothing else: the host id is the one the unit's `ExecStart` gives with
    /// `--host-id`, the capabilities are the ones the agent reports for the flags it is
    /// given (a runtime and a mode with `--container-bin`, the GPU toolkit only with
    /// `--gpu-toolkit-hook`).
    pub fn agent_hello(&self, n: usize, name: &str) -> serde_json::Value {
        let script = self.script(n);
        let line = script
            .lines()
            .find_map(|line| line.trim().strip_prefix("ExecStart="))
            .expect("the unit has an ExecStart");
        let words: Vec<&str> = line.split_whitespace().collect();
        let flag = |wanted: &str| {
            let at = words.iter().position(|word| *word == wanted);
            at.map(|at| words.get(at + 1).copied().unwrap_or_default())
        };
        let host = flag("--host-id")
            .unwrap_or_else(|| panic!("the unit passes no --host-id, so the agent registers under a name of its own, not {name}: {line}"));
        let mut body = hello_body_of(host);
        body["instance"] = json!({ "provider": "burst", "instance_id": name });
        body["accepts_host_credential"] = json!(true);
        if flag("--container-bin").is_some() {
            body["capabilities"]["runtime"] = json!("container");
            body["capabilities"]["exec"] = json!("container");
        }
        if flag("--gpu-toolkit-hook").is_some() {
            body["capabilities"]["gpu-toolkit"] = json!("true");
        }
        body
    }

    /// The `--host-id` the `n`th launch's unit gives the agent, if it gives one.
    pub fn unit_host_id(&self, n: usize) -> Option<String> {
        let script = self.script(n);
        let line = script
            .lines()
            .find_map(|l| l.trim().strip_prefix("ExecStart="))?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let at = words.iter().position(|word| *word == "--host-id")?;
        words
            .get(at + 1)
            .map(|host| host.trim_matches('"').to_owned())
    }

    /// A hello of the first launch's agent with `token`.
    pub async fn agent_says_hello(&self, token: &str, name: &str) -> Reply {
        let bytes = self.agent_hello(0, name).to_string().into_bytes();
        let addr = self.h.running.agent_addr().unwrap();
        request(addr, "POST", "/v1/agent/hello", Some(token), Some(&bytes)).await
    }

    /// A hello that must be admitted: the session it opened and the host credential it
    /// carried.
    pub async fn join(&self, token: &str, host: &str) -> (Session, String) {
        let reply = self.agent_says_hello(token, host).await;
        let session = crate::agents_support::session(&reply, host);
        let credential = reply.json()["host_credential"]
            .as_str()
            .expect("the welcome carries a host credential")
            .to_owned();
        assert_eq!(credential.len(), 64, "{credential}");
        (session, credential)
    }

    /// A heartbeat of `me` with `bearer`.
    pub async fn beat(&self, bearer: &str, me: &Session) -> Reply {
        beat(&self.h, bearer, me).await
    }

    /// What an agent does about the exclusive lease the registry committed to its host:
    /// it hears the lease in a heartbeat reply and reports itself ready for it. Returns
    /// the lease it heard.
    pub async fn ready(&self, credential: &str, me: &Session) -> String {
        let heard = self.beat(credential, me).await;
        assert_eq!(heard.status, 200, "{}", heard.body);
        let lease = heard.json()["exclusive"]
            .as_str()
            .expect("the reply names the lease the host is committed to")
            .to_owned();
        let mut message = me.beat();
        message["exclusive_ready"] = json!(lease);
        let bytes = message.to_string().into_bytes();
        let addr = self.h.running.agent_addr().unwrap();
        let path = "/v1/agent/heartbeat";
        let sent = request(addr, "POST", path, Some(credential), Some(&bytes)).await;
        assert_eq!(sent.status, 200, "{}", sent.body);
        lease
    }

    /// The `mark` the registry lists for `host`.
    pub async fn mark(&self, host: &str) -> serde_json::Value {
        let listing = hosts(&self.h).await;
        crate::agents_support::entry(&listing, host)["mark"].clone()
    }

    /// Asserts that neither status route shows one of `secrets` nor any planted secret.
    pub async fn assert_status_is_clean(&self, secrets: &[&str]) {
        for path in ["/v1/pools", "/status"] {
            let reply = request(self.h.running.addr(), "GET", path, None, None).await;
            assert_eq!(reply.status, 200, "{path}: {}", reply.body);
            let shown = format!("{}\n{}", reply.head, reply.body);
            for secret in PLANTED.iter().chain(secrets) {
                assert!(!shown.contains(secret), "{path} shows a secret");
            }
        }
    }

    /// How many requests the fake Tailscale API has had at `route`.
    pub fn asked(&self, route: &str) -> usize {
        self.tailnet.count(route)
    }
}

/// The number of join tokens the file of `h` holds.
pub fn joins_held(h: &Harness) -> usize {
    let mut joins = h.scratch.state().into_os_string();
    joins.push(".joins");
    std::fs::read_to_string(joins).map_or(0, |text| text.matches("[[join]]").count())
}
