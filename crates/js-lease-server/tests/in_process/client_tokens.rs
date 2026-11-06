//! The client token files of the job service read again at the in-process tick: the line
//! the tick logs when a named client's token changed, and the warning when its file went
//! bad, both exactly once per tick and neither holding a token.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use js_lease_server::Cli;
use js_lease_server::Config;
use js_lease_server::in_process::InProcess;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Metadata, Subscriber};

use crate::support::{Scratch, Stepping, args, clock};

/// The first token of the client `ci`.
const FIRST: &str = "ci-first-0123456789-abcdefghijklmnopqrst";

/// The token `ci`'s file is rewritten with.
const SECOND: &str = "ci-second-0123456789-abcdefghijklmnopqrs";

/// An event this crate logged at info or above: its level and its fields as text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Logged {
    level: Level,
    fields: BTreeMap<String, String>,
}

/// A subscriber that keeps the events of `js_lease_server` at info or above, for the
/// thread it is the default of.
#[derive(Clone, Default)]
struct Kept(Arc<Mutex<Vec<Logged>>>);

impl Kept {
    /// A new collector, the default of this thread while the guard lives. A global
    /// subscriber that discards everything is installed first, once, so a callsite another
    /// test reached with no subscriber in scope is never cached as off.
    fn scoped() -> (Kept, tracing::subscriber::DefaultGuard) {
        static GLOBAL: Once = Once::new();
        GLOBAL.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_max_level(Level::TRACE)
                .with_writer(std::io::sink)
                .try_init();
        });
        let kept = Kept::default();
        let guard = tracing::subscriber::set_default(kept.clone());
        (kept, guard)
    }

    /// The events about the client tokens kept since the last call, which forgets every
    /// event.
    fn about_client_tokens(&self) -> Vec<Logged> {
        let taken = std::mem::take(&mut *self.0.lock().unwrap());
        taken
            .into_iter()
            .filter(|event| event.fields["message"].contains("client token"))
            .collect()
    }
}

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl Subscriber for Kept {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let metadata = event.metadata();
        if !metadata.target().starts_with("js_lease_server") || *metadata.level() > Level::INFO {
            return;
        }
        let mut fields = BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        self.0.lock().unwrap().push(Logged {
            level: *metadata.level(),
            fields,
        });
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// `(name, value)` pairs as the fields of an event.
fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// Asserts that, in a server started in process with the job service and the client `ci`, a
/// tick that finds `ci`'s token file unchanged logs nothing about the client tokens, a tick
/// after the file is rewritten with a new token logs exactly one info line saying the new
/// tokens are in force, and a tick after the file is emptied logs exactly one warning whose
/// fields are the fixed message and the reason naming the entry and the file; no line holds
/// either token. Catches a reload that never runs at the tick, one that reports every tick as
/// a change, a bad file that goes unsaid or is said twice, and a token in a log line.
#[tokio::test]
async fn the_tick_says_when_a_client_token_changed_and_when_its_file_went_bad() {
    let (kept, _scope) = Kept::scoped();
    let scratch = Scratch::new("client_tokens_tick");
    let file = scratch.write("ci-token", FIRST, 0o600);
    let mut flags: Vec<OsString> = args(&scratch, "initialise", true);
    flags.extend(
        [
            "--job-listen".to_owned(),
            "127.0.0.1:0".to_owned(),
            "--client-token".to_owned(),
            format!("ci={}", file.display()),
        ]
        .map(OsString::from),
    );
    let config = Config::from_cli(Cli::try_parse_from(flags).unwrap()).unwrap();
    let monotonic = Stepping::of(Duration::from_millis(1));
    let config = config.with_monotonic(Arc::new(monotonic));
    let (_clock, shared) = clock();
    let server = InProcess::start(config, shared).unwrap();

    server.tick().await;
    assert_eq!(kept.about_client_tokens(), Vec::new());

    scratch.write("ci-token", SECOND, 0o600);
    server.tick().await;
    let changed = Logged {
        level: Level::INFO,
        fields: fields(&[(
            "message",
            "the client tokens changed: the new tokens are in force",
        )]),
    };
    assert_eq!(kept.about_client_tokens(), vec![changed]);
    server.tick().await;
    assert_eq!(kept.about_client_tokens(), Vec::new());

    scratch.write("ci-token", "", 0o600);
    server.tick().await;
    let reason = format!("--client-token ci: {} is empty", file.display());
    let refused = Logged {
        level: Level::WARN,
        fields: fields(&[
            (
                "message",
                "cannot read the client token files again; the client tokens in force stay",
            ),
            ("error", &reason),
        ]),
    };
    let said = kept.about_client_tokens();
    assert_eq!(said, vec![refused]);
    for event in &said {
        for value in event.fields.values() {
            assert!(!value.contains(FIRST) && !value.contains(SECOND), "{value}");
        }
    }
    server.shutdown();
}
