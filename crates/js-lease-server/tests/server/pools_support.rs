//! What the pool-check and status tests share: a cloud and a network mode that are
//! fakes, a server started from a configuration file over them with the pool checks
//! stepped by the test, and the grep for secrets.
//!
//! Nothing here calls a real cloud or a real Tailscale: the provider is [`FakeCloud`],
//! the network mode is [`FakeNet`] or one of the server's own modes that makes no call
//! (`default`, or `tailscale` that is built and never asked), and time is a manual clock.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_elastic::{
    AcquireRequest, Acquired, Artifact, BootStep, ElasticProvider, GpuClass, Held, HostRef, Offer,
    PriceUnit, ProviderError, Released, Secret, SlotName,
};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::network::{Contribution, LaunchCtx, NetworkError, NetworkMode};
use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, LaunchInputs, OwnerOnlyFiles,
    Running, SharedClock, TickHandle, channel_ticker, start_with_checks,
};
use js_types::ExecMode;
use serde_json::Value;
use tokio_rustls::TlsConnector;

use crate::agents_support::JOIN;
use crate::config_support::{
    API_KEY, OAUTH_SECRET, PoolText, clock, file, oauth_file, quote, quoted, server,
};
use crate::support::{LogCapture, Reply, Scratch, T0, TOKEN, quiet_logs, request};

/// The secret the fake network mode would mint for a launch. Nothing mints it: a plan is
/// shown and nothing is launched.
pub const NET_SECRET: &str = "PLANTED-NET-AUTHKEY-VALUE";

/// The slot the fake network mode's key fills.
pub const NET_SLOT: &str = "network.fake.key";

/// The address the servers advertise.
pub const ADVERTISE: &str = "https://js.example:7301";

/// Every secret the tests plant: the key file, the OAuth client, the client token, the join
/// token and the fake network key.
pub const SECRETS: [&str; 5] = [API_KEY, OAUTH_SECRET, TOKEN, JOIN, NET_SECRET];

/// The secrets of [`SECRETS`] that `text` holds.
pub fn leaked(text: &str) -> Vec<&'static str> {
    SECRETS
        .into_iter()
        .filter(|secret| text.contains(secret))
        .collect()
}

/// An offer of `gpus` GPUs of `class`.
pub fn offer(id: &str, gpus: u32, class: Option<&str>, price: Option<u64>, ok: bool) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: Some("us-west-1".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 512 << 30,
        gpus,
        gpu_class: class.map(|class| GpuClass::parse(class).unwrap()),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: price,
        price_unit: PriceUnit::UsCents,
        available: ok,
    }
}

/// An offer of `gpus` GPUs of class `nvidia-a10` at `price` cents an hour, with capacity.
pub fn a10(id: &str, gpus: u32, price: u64) -> Offer {
    offer(id, gpus, Some("nvidia-a10"), Some(price), true)
}

/// A provider that answers what the test says and counts what it is asked.
pub struct FakeCloud {
    catalog: Mutex<Result<Vec<Offer>, String>>,
    prep: Mutex<Vec<BootStep>>,
    panics: AtomicBool,
    catalogs: AtomicUsize,
    others: AtomicUsize,
}

impl FakeCloud {
    /// A cloud whose catalog is `offers`.
    pub fn new(offers: Vec<Offer>) -> Arc<Self> {
        Arc::new(Self {
            catalog: Mutex::new(Ok(offers)),
            prep: Mutex::new(vec![BootStep::Run(vec![
                "apt-get".to_owned(),
                "install".to_owned(),
                "fake-driver".to_owned(),
            ])]),
            panics: AtomicBool::new(false),
            catalogs: AtomicUsize::new(0),
            others: AtomicUsize::new(0),
        })
    }

    /// The catalog from now on.
    pub fn offers(&self, offers: Vec<Offer>) {
        *self.catalog.lock().unwrap() = Ok(offers);
    }

    /// Makes the catalog call fail with `message`.
    pub fn fail(&self, message: &str) {
        *self.catalog.lock().unwrap() = Err(message.to_owned());
    }

    /// The host preparation steps from now on.
    pub fn prep(&self, steps: Vec<BootStep>) {
        *self.prep.lock().unwrap() = steps;
    }

    /// Makes the catalog call panic, or not.
    pub fn panic(&self, panics: bool) {
        self.panics.store(panics, Ordering::SeqCst);
    }

    /// How many catalog calls were made.
    pub fn catalogs(&self) -> usize {
        self.catalogs.load(Ordering::SeqCst)
    }

    /// How many calls other than the catalog and the host preparation (acquire, list,
    /// release) were made: a pool in `plan` makes none.
    pub fn others(&self) -> usize {
        self.others.load(Ordering::SeqCst)
    }
}

struct Handle(Arc<FakeCloud>);

impl ElasticProvider for Handle {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        self.0.catalogs.fetch_add(1, Ordering::SeqCst);
        if self.0.panics.load(Ordering::SeqCst) {
            panic!("the fake cloud was told to panic");
        }
        let answer = self.0.catalog.lock().unwrap().clone();
        answer.map_err(ProviderError::new)
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        self.0.prep.lock().unwrap().clone()
    }

    fn payload_limit(&self) -> usize {
        16 * 1024
    }

    fn acquire(&self, _request: &AcquireRequest) -> Acquired {
        self.0.others.fetch_add(1, Ordering::SeqCst);
        Acquired::Unknown
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        self.0.others.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    fn release(&self, _id: &HostRef) -> Released {
        self.0.others.fetch_add(1, Ordering::SeqCst);
        Released::Unknown
    }
}

thread_local! {
    /// The cloud the next `fake-cloud` pool is built over. A kind is a plain function, so
    /// the test leaves its cloud here, on the thread that reads the file.
    static NEXT: RefCell<Option<Arc<FakeCloud>>> = const { RefCell::new(None) };
}

/// The `fake-cloud` kind: reads its key file as a real kind does, and builds a pool over
/// the cloud the test left, with one GPU of class `nvidia-a10` as its floor.
fn fake_cloud(table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let Some(toml::Value::String(path)) = table.get("api_key_file") else {
        return Err(KindError {
            field: "api_key_file".to_owned(),
            reason: "required".to_owned(),
        });
    };
    ctx.files
        .read(std::path::Path::new(path))
        .map_err(|reason| KindError {
            field: "api_key_file".to_owned(),
            reason,
        })?;
    let cloud = NEXT.with(|next| next.borrow_mut().take()).unwrap();
    Ok(Built::Elastic(ElasticBuilt {
        provider: Box::new(Handle(cloud)),
        floor: Floor {
            gpus: 1,
            gpu_class: Some(GpuClass::parse("nvidia-a10").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// The kinds of these tests: the server's own and `fake-cloud`.
pub fn kinds() -> Kinds {
    Kinds::standard().with("fake-cloud", fake_cloud)
}

/// A network mode that answers what the test says and counts what it is asked. Its
/// `prepare` would mint [`NET_SECRET`]; the tests assert it is never called.
pub struct FakeNet {
    check: Mutex<Result<(), NetworkError>>,
    panics: AtomicBool,
    checks: AtomicUsize,
    prepares: AtomicUsize,
}

impl FakeNet {
    /// A mode whose check passes.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            check: Mutex::new(Ok(())),
            panics: AtomicBool::new(false),
            checks: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
        })
    }

    /// Makes the check fail with `error`.
    pub fn fail(&self, error: NetworkError) {
        *self.check.lock().unwrap() = Err(error);
    }

    /// Makes the check pass.
    pub fn pass(&self) {
        *self.check.lock().unwrap() = Ok(());
    }

    /// Makes the check panic, or not.
    pub fn panic(&self, panics: bool) {
        self.panics.store(panics, Ordering::SeqCst);
    }

    /// How many checks were made.
    pub fn checks(&self) -> usize {
        self.checks.load(Ordering::SeqCst)
    }

    /// How many launches were prepared (a secret minted): a plan mints none.
    pub fn prepares(&self) -> usize {
        self.prepares.load(Ordering::SeqCst)
    }
}

/// The steps the fake mode contributes: one program and one secret file.
pub fn net_steps() -> Vec<BootStep> {
    vec![
        BootStep::Run(vec!["fake-net-up".to_owned()]),
        BootStep::SecretFile {
            path: "/etc/jobscheduler/fake-net.key".to_owned(),
            slot: SlotName(NET_SLOT.to_owned()),
        },
    ]
}

impl NetworkMode for FakeNet {
    fn prepare(&self, _launch: &LaunchCtx<'_>) -> Result<Contribution, NetworkError> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        Ok(Contribution {
            steps: net_steps(),
            secrets: vec![(
                SlotName(NET_SLOT.to_owned()),
                Secret::new(NET_SECRET.to_owned()),
            )],
        })
    }

    fn describe(&self, _launch: &LaunchCtx<'_>) -> Vec<BootStep> {
        net_steps()
    }

    fn check(&self) -> Result<(), NetworkError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.panics.load(Ordering::SeqCst) {
            panic!("the fake network was told to panic");
        }
        self.check.lock().unwrap().clone()
    }

    fn forget(&self, _host: &js_elastic::HostName) -> Result<(), NetworkError> {
        Ok(())
    }
}

/// The network of a pool.
#[derive(Clone, Copy)]
pub enum Net {
    /// [`FakeNet`], standing in for the mode the file names.
    Fake,
    /// The server's `default` mode, with no table.
    Default,
    /// The server's `tailscale` mode; `pinned` gives it a build to fetch, without which
    /// it cannot be made. It is never asked: a step of the checks would call Tailscale.
    Tailscale { pinned: bool },
}

/// How a test server is wired.
#[derive(Clone, Copy)]
pub struct Options {
    /// `server.advertise_url`.
    pub advertise: Option<&'static str>,
    /// Whether the agent binary's digest is known.
    pub digest: bool,
    /// The network mode.
    pub net: Net,
    /// Listen on every interface with TLS, as a VPC does.
    pub wildcard: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            advertise: Some(ADVERTISE),
            digest: true,
            net: Net::Fake,
            wildcard: false,
        }
    }
}

/// A server from a configuration file over [`FakeCloud`], with both tickers stepped by
/// the test and everything it was shown kept for the grep for secrets.
pub struct Rig {
    pub running: Running,
    pub clock: ManualClock,
    pub ticks: TickHandle,
    pub checks: TickHandle,
    pub scratch: Scratch,
    pub cloud: Arc<FakeCloud>,
    pub net: Arc<FakeNet>,
    pub logs: LogCapture,
    tls: Option<TlsConnector>,
    seen: RefCell<Vec<String>>,
}

impl Rig {
    /// Starts a server named `name` over a cloud with `offers`.
    pub async fn start(name: &str, offers: Vec<Offer>, options: Options) -> Self {
        let logs = LogCapture::start();
        quiet_logs();
        let scratch = Scratch::new(name);
        let cloud = FakeCloud::new(offers);
        let net = FakeNet::new();
        let (extra, tls) = Self::listeners(&scratch, options);
        let advertise = options
            .advertise
            .map(|url| format!("advertise_url = {}\n", quote(url)))
            .unwrap_or_default();
        let server = server(&scratch, &format!("{extra}{advertise}"));
        let key = scratch.path("api-key");
        let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
        let provider = format!(
            "{{ kind = \"fake-cloud\", api_key_file = {} }}",
            quoted(&key)
        );
        let pool = PoolText::elastic(&scratch)
            .set("exec", &quote("none"))
            .set("provider", &provider)
            .set("join_token_file", &quoted(&joins));
        let pool = match options.net {
            Net::Tailscale { .. } => pool.set(
                "network",
                &format!(
                    "{{ kind = \"tailscale\", oauth_client_file = {}, tag = \"tag:js-agent\" }}",
                    oauth_file(&scratch)
                ),
            ),
            Net::Fake | Net::Default => pool,
        };
        let path = scratch.write("js.toml", &file(&server, &[&pool]), 0o644);
        NEXT.with(|next| *next.borrow_mut() = Some(cloud.clone()));
        let config = Config::from_config_file(&path, &kinds(), &OwnerOnlyFiles, clock()).unwrap();
        let mut inputs = LaunchInputs::default();
        if options.digest {
            inputs.agent_sha256.insert("x86_64".to_owned(), [0xab; 32]);
        }
        match options.net {
            Net::Fake => inputs.network = Some(net.clone()),
            Net::Tailscale { pinned: true } => {
                inputs.tailscale = Some(Artifact {
                    url: "https://pkgs.example.org/tailscale.tgz".to_owned(),
                    sha256: [0x5a; 32],
                });
            }
            Net::Default | Net::Tailscale { pinned: false } => {}
        }
        let shown = format!("{:?}", config.elastic_pool());
        let config = config.with_launch_inputs(inputs);
        let manual = ManualClock::new(Timestamp::from_millis(T0));
        let (ticks, ticker) = channel_ticker();
        let (checks, check_ticker) = channel_ticker();
        let shared = SharedClock::new(manual.clone());
        let running = start_with_checks(config, shared, ticker, check_ticker)
            .await
            .unwrap();
        Self {
            running,
            clock: manual,
            ticks,
            checks,
            scratch,
            cloud,
            net,
            logs,
            tls,
            seen: RefCell::new(vec![shown]),
        }
    }

    /// The `[server]` lines for the listeners, and the TLS client when they speak TLS.
    fn listeners(scratch: &Scratch, options: Options) -> (String, Option<TlsConnector>) {
        if !options.wildcard {
            let lines = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n";
            return (lines.to_owned(), None);
        }
        let (cert, key) = crate::tls::test_identity();
        let cert_file = scratch.write("test-only-cert.pem", &cert, 0o644);
        let key_file = scratch.write("test-only-key.pem", &key, 0o600);
        let lines = format!(
            "listen = \"0.0.0.0:0\"\nagent_listen = \"127.0.0.2:0\"\ntls_cert = {}\ntls_key = {}\n",
            quoted(&cert_file),
            quoted(&key_file)
        );
        (lines, Some(crate::tls::connector(&cert)))
    }

    /// One step of the pool checks: a pass if one is due, and the step returns when the
    /// pass is done.
    pub async fn step(&self) {
        self.checks.step().await;
    }

    /// The clock moved forward by `seconds`.
    pub fn advance(&self, seconds: u64) {
        self.clock.advance(std::time::Duration::from_secs(seconds));
    }

    /// `GET path` on the listen port with no token, kept for the grep for secrets.
    pub async fn get(&self, path: &str) -> Reply {
        let reply = match &self.tls {
            Some(connector) => {
                let port = self.running.addr().port();
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                crate::tls::tls_request(addr, connector, "GET", path, None, None).await
            }
            None => request(self.running.addr(), "GET", path, None, None).await,
        };
        self.seen
            .borrow_mut()
            .push(format!("{}\n{}", reply.head, reply.body));
        reply
    }

    /// `GET /v1/pools`, which must be 200.
    pub async fn pools(&self) -> Value {
        let reply = self.get("/v1/pools").await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.json()
    }

    /// The one pool of `GET /v1/pools`.
    pub async fn pool(&self) -> Value {
        let mut pools = self.pools().await;
        assert_eq!(pools["pools"].as_array().unwrap().len(), 1);
        pools["pools"][0].take()
    }

    /// Asserts that no secret is in any reply this rig fetched, in a log line it captured,
    /// or in the `Debug` of the pool's plan, and that nothing was launched or minted.
    pub fn assert_clean(&self) {
        let everything = format!("{}\n{}", self.seen.borrow().join("\n"), self.logs.text());
        assert_eq!(leaked(&everything), Vec::<&str>::new(), "{everything}");
        assert_eq!(
            self.cloud.others(),
            0,
            "a pool in plan mode launches nothing"
        );
        assert_eq!(self.net.prepares(), 0, "a plan mints no secret");
    }
}
