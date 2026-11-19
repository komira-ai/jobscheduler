//! A lease server's own-host routes in memory, a clock the test moves by hand, and a
//! standard output the test can read while the window runs.

use std::cell::{Cell, RefCell};
use std::io::{self, Write};
use std::rc::Rc;
use std::time::Duration;

use js_agent::Wake;
use js_agent::cordon::Pace;
use js_agent::own_host::{
    CordonView, Cordoned, HeldLease, HostReading, Lease, LeaseError, MAX_CORDON_SECS, Uncordoned,
};
use js_types::CordonHolder;

/// The host the fake serves.
pub const HOST: &str = "h-1";

/// Which route a call is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cordon,
    Uncordon,
    Host,
}

/// A call the fake took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Cordon(u64),
    Uncordon,
    Host,
}

/// What a call does instead of answering as the fake's state says.
pub enum Effect {
    /// Fails with this error.
    Fail(LeaseError),
    /// Answers, naming this host in the reply.
    Name(&'static str),
}

/// What the fake server knows.
pub struct State {
    /// The host the replies name.
    pub id: String,
    /// A cordon is in force (whoever placed it).
    pub cordoned: bool,
    /// The holder of the cordon in force, when it names one.
    pub holder: Option<String>,
    /// The holder and take-over flag each cordon request carried, in order.
    pub cordon_asks: Vec<(String, bool)>,
    /// The holder each uncordon carried, in order.
    pub uncordon_asks: Vec<String>,
    /// The lease the host holds.
    pub lease: Option<String>,
    /// The `leases` count the host reads.
    pub leases: u64,
    /// The host holds no lease but has runs to finish.
    pub releasing: bool,
    /// The host's mark.
    pub mark: Option<String>,
    /// The `lease_secs` of the last cordon request.
    pub lease_secs: u64,
    /// The calls taken, in order.
    pub calls: Vec<Call>,
    overrides: Vec<(Kind, usize, Effect)>,
    /// The clock each call takes `latency` from.
    latency: Option<(Rc<Cell<Duration>>, Duration)>,
}

/// The lease server's own-host routes in memory: a cordon, a host reading, a log of calls.
#[derive(Clone)]
pub struct FakeLease(Rc<RefCell<State>>);

impl FakeLease {
    /// A host that is idle and not cordoned.
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(State {
            id: HOST.to_owned(),
            cordoned: false,
            holder: None,
            cordon_asks: Vec::new(),
            uncordon_asks: Vec::new(),
            lease: None,
            leases: 0,
            releasing: false,
            mark: None,
            lease_secs: 0,
            calls: Vec::new(),
            overrides: Vec::new(),
            latency: None,
        })))
    }

    /// Changes what the server knows.
    pub fn set(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.0.borrow_mut());
    }

    /// The calls taken so far.
    pub fn calls(&self) -> Vec<Call> {
        self.0.borrow().calls.clone()
    }

    /// Makes the `nth` call (from 0) of `kind` do `effect`.
    pub fn on(&self, kind: Kind, nth: usize, effect: Effect) {
        self.0.borrow_mut().overrides.push((kind, nth, effect));
    }

    /// Makes every call take `each` of `clock`, as a slow server does.
    pub fn slow(&self, clock: &Rc<Cell<Duration>>, each: Duration) {
        self.0.borrow_mut().latency = Some((Rc::clone(clock), each));
    }

    /// The holder and take-over flag each cordon request carried.
    pub fn cordon_asks(&self) -> Vec<(String, bool)> {
        self.0.borrow().cordon_asks.clone()
    }

    /// The holder each uncordon carried.
    pub fn uncordon_asks(&self) -> Vec<String> {
        self.0.borrow().uncordon_asks.clone()
    }

    /// The holder of the cordon in force.
    pub fn holder(&self) -> Option<String> {
        self.0.borrow().holder.clone()
    }

    /// Whether a cordon is in force.
    pub fn cordoned(&self) -> bool {
        self.0.borrow().cordoned
    }

    /// Takes the call: logs it, spends its time, and returns what it was told to do instead
    /// of the ordinary answer.
    fn enter(&self, kind: Kind, call: Call) -> Option<Effect> {
        let mut state = self.0.borrow_mut();
        if let Some((clock, each)) = &state.latency {
            clock.set(clock.get() + *each);
        }
        let nth = state
            .calls
            .iter()
            .filter(|seen| Kind::of(seen) == kind)
            .count();
        state.calls.push(call);
        let at = state
            .overrides
            .iter()
            .position(|(k, n, _)| (*k, *n) == (kind, nth));
        at.map(|at| state.overrides.remove(at).2)
    }

    /// The host as the server reads it, naming `named` when the call said to.
    fn reading(&self, named: Option<&str>) -> HostReading {
        let state = self.0.borrow();
        let id = named.unwrap_or(&state.id).to_owned();
        let cordon = state.cordoned.then(|| CordonView {
            expires_at_ms: 1_000_000 + state.lease_secs * 1000,
            expires_in_ms: Some(state.lease_secs * 1000),
            holder: state.holder.clone(),
        });
        HostReading {
            id,
            state: "live".to_owned(),
            epoch: 1,
            session_started_ms: Some(1),
            leases: state.leases,
            lease: state.lease.clone(),
            releasing: state.releasing,
            mark: state.mark.clone(),
            cordon,
            taint: None,
            agent_version: None,
            runtime: None,
        }
    }

    /// The error or the name a call was told to give.
    fn effect(&self, kind: Kind, call: Call) -> Result<Option<&'static str>, LeaseError> {
        match self.enter(kind, call) {
            Some(Effect::Fail(why)) => Err(why),
            Some(Effect::Name(id)) => Ok(Some(id)),
            None => Ok(None),
        }
    }
}

impl Kind {
    fn of(call: &Call) -> Kind {
        match call {
            Call::Cordon(_) => Kind::Cordon,
            Call::Uncordon => Kind::Uncordon,
            Call::Host => Kind::Host,
        }
    }
}

impl Lease for FakeLease {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        let named = self.effect(Kind::Cordon, Call::Cordon(lease_secs))?;
        let already_cordoned = self.cordoned();
        self.set(|state| {
            state.cordoned = true;
            state.lease_secs = lease_secs.min(MAX_CORDON_SECS);
        });
        Ok(Cordoned {
            already_cordoned,
            held_by_other: false,
            host: self.reading(named),
        })
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        let named = self.effect(Kind::Uncordon, Call::Uncordon)?;
        let lifted = self.cordoned();
        self.set(|state| state.cordoned = false);
        Ok(Uncordoned {
            lifted,
            held_by_other: false,
            host: self.reading(named),
        })
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        let named = self.effect(Kind::Host, Call::Host)?;
        Ok(self.reading(named))
    }
}

impl HeldLease for FakeLease {
    /// The rule of the server: with no cordon in force the request places one for its holder;
    /// the holder renews its own; another holder's is left as it is (`held_by_other`, and no
    /// change); a take-over makes the cordon in force the asker's.
    fn cordon_held(
        &self,
        lease_secs: u64,
        holder: &CordonHolder,
        take_over: bool,
    ) -> Result<Cordoned, LeaseError> {
        let named = self.effect(Kind::Cordon, Call::Cordon(lease_secs))?;
        let name = holder.as_str().to_owned();
        let (already_cordoned, other) = {
            let state = self.0.borrow();
            let other = state.cordoned && !take_over && state.holder.as_ref() != Some(&name);
            (state.cordoned, other)
        };
        self.set(|state| {
            state.cordon_asks.push((name.clone(), take_over));
            if !other {
                state.cordoned = true;
                state.holder = Some(name);
                state.lease_secs = lease_secs.min(MAX_CORDON_SECS);
            }
        });
        Ok(Cordoned {
            already_cordoned,
            held_by_other: other,
            host: self.reading(named),
        })
    }

    fn uncordon_held(&self, holder: &CordonHolder) -> Result<Uncordoned, LeaseError> {
        let named = self.effect(Kind::Uncordon, Call::Uncordon)?;
        let name = holder.as_str().to_owned();
        let (cordoned, ours) = {
            let state = self.0.borrow();
            (state.cordoned, state.holder.as_ref() == Some(&name))
        };
        self.set(|state| {
            state.uncordon_asks.push(name);
            if ours {
                state.cordoned = false;
                state.holder = None;
            }
        });
        Ok(Uncordoned {
            lifted: cordoned && ours,
            held_by_other: cordoned && !ours,
            host: self.reading(named),
        })
    }
}

/// A clock moved by hand, and the stop request.
pub struct FakePace<'a> {
    now: Rc<Cell<Duration>>,
    /// The span of every wait, in order.
    pub waits: Vec<Duration>,
    stop_at: Option<usize>,
    on_wait: Box<dyn FnMut(usize, Duration) + 'a>,
}

impl<'a> FakePace<'a> {
    /// A clock at zero, no stop request, nothing done at a wait.
    pub fn new() -> Self {
        Self {
            now: Rc::new(Cell::new(Duration::ZERO)),
            waits: Vec::new(),
            stop_at: None,
            on_wait: Box::new(|_, _| {}),
        }
    }

    /// The clock, for a fake that spends time of its own.
    pub fn clock(&self) -> Rc<Cell<Duration>> {
        Rc::clone(&self.now)
    }

    /// A stop request arrives during the `nth` wait (from 0): that wait ends at once and says
    /// `Stop`, and no time passes.
    pub fn stop_at(mut self, nth: usize) -> Self {
        self.stop_at = Some(nth);
        self
    }

    /// Calls `act` with the number of waits taken so far and the span of the one that has just
    /// passed, before the window goes on: a test changes what the server reads there.
    pub fn on_wait(mut self, act: impl FnMut(usize, Duration) + 'a) -> Self {
        self.on_wait = Box::new(act);
        self
    }

    /// The time passed.
    pub fn elapsed(&self) -> Duration {
        self.now.get()
    }
}

impl Pace for FakePace<'_> {
    fn now(&self) -> Duration {
        self.now.get()
    }

    fn wait(&mut self, span: Duration) -> Wake {
        let nth = self.waits.len();
        self.waits.push(span);
        if self.stop_at == Some(nth) {
            return Wake::Stop;
        }
        self.now.set(self.now.get() + span);
        (self.on_wait)(self.waits.len(), span);
        Wake::Elapsed
    }
}

/// A standard output the test reads while the window runs.
#[derive(Clone, Default)]
pub struct Lines(Rc<RefCell<Vec<u8>>>);

impl Lines {
    /// The lines printed so far.
    pub fn all(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
