//! The VM's owner on a Mac: one Linux arm64 VM through Apple's Virtualization framework,
//! booted by its EFI firmware from `disk.raw`, with a NAT network device, a virtio socket
//! device, a traditional memory balloon, an entropy device, the serial console written to
//! `console.log`, and the virtiofs shares and forwarded ports of the command line
//! (`config`).
//!
//! The owner holds the VM's life: stopped, starting, running, stopping. A guest connection
//! (`connect` on the control socket) that finds the VM stopped starts it, and one that finds
//! it starting or stopping waits until the guest is ready; the connection counts as activity
//! for as long as it is open (a container's run holds one until its exit). With
//! `--idle-stop-s` the owner asks the guest to shut down once no connection has been open for
//! that long, so the VM gives its memory back to the Mac, and the next connection starts it
//! again. Each start makes a new VM object from a new configuration.
//!
//! Every call on the VM and its devices runs on the VM's serial dispatch queue (the
//! configuration is built on the booting thread, before a VM exists). The owner prints one JSON
//! object per line on its standard output for each event (`owner`, `starting`, `started`,
//! `ready`, `stopping`, `stopped`, `error`, `owner-exit`), each with `ms`, the milliseconds
//! since the owner started; `started` and `ready` also carry `since_start_ms`, the
//! milliseconds since that boot's start call, and `stopped` carries `stop_ms`, the
//! milliseconds since the stop began.
//!
//! # FFI-BOUNDARY
//! Every Objective-C object is held in a `Retained<T>`, which releases it when its last owner
//! drops it:
//! - the `VZVirtualMachineConfiguration` and its device configurations are built on the
//!   thread that boots and released once the VM is made from them; the VM copies what it
//!   keeps at creation;
//! - the `VZVirtualMachine`, its delegate (`Observer`, which the framework holds weakly) and
//!   the `VZVirtioSocketListener`s (held weakly by the device) are kept in `Running`, one per
//!   boot, and released on the VM's queue when that boot's VM stops (`release_on_queue`);
//! - a `VZVirtioSocketConnection` is retained by the thread that carries its bytes until the
//!   carrying ends; its descriptor belongs to the connection object, which closes it, and the
//!   thread only uses a duplicate of it.
//!
//! # SAFETY
//! The framework requires every call on a VM and its devices on the VM's queue. [`Queued`]
//! holds a value that may only be used there: its `get` is `unsafe`, and each caller says it
//! runs on that queue. [`AnyThread`] holds a value the framework allows on any thread (a
//! retained connection that the holding thread only drops); its constructor is the `unsafe`
//! step.

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write};
use std::net::TcpStream;
use std::os::fd::{BorrowedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Stdio;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use js_mac_session::disclaim::Program;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread as _, DefinedClass, Message, define_class, msg_send};
use objc2_foundation::NSError;
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate, VZVirtioTraditionalMemoryBalloonDevice, VZVirtualMachine,
    VZVirtualMachineDelegate,
};

mod config;

use self::config::{Paths, build, describe};
use super::client;
use super::clock;
use super::control::{self, Control};
use super::flags::RunFlags;
use super::wire;

/// How long the framework has to call a start, stop or connect completion handler.
const COMPLETION: Duration = Duration::from_secs(60);
/// How long a guest asked to shut down has before the VM is stopped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// How long a detached owner has to report its guest ready.
const DETACHED_READY: Duration = Duration::from_secs(300);

/// A value that is only used on one dispatch queue.
struct Queued<T>(T);

// SAFETY: the value inside is reached only through the `unsafe` `get`, whose caller promises
// to run on the value's queue; moving the wrapper between threads never touches the value.
unsafe impl<T> Send for Queued<T> {}

impl<T> Queued<T> {
    /// The value.
    ///
    /// # Safety
    /// The caller runs on the dispatch queue the value belongs to.
    unsafe fn get(&self) -> &T {
        &self.0
    }
}

/// A value the framework allows on any thread.
struct AnyThread<T>(T);

// SAFETY: `AnyThread::new` is `unsafe`; its caller promises the value may be used (or only
// dropped) on any thread. The one use is a retained `VZVirtioSocketConnection` that the
// holding thread only drops: retain and release of an Objective-C object are atomic and may
// happen on any thread, and the thread sends the object no message; the descriptor it uses
// is a duplicate of its own.
unsafe impl<T> Send for AnyThread<T> {}

impl<T> AnyThread<T> {
    /// Wraps `value`.
    ///
    /// # Safety
    /// `value` may be used from any thread, or is only dropped where it ends up.
    unsafe fn new(value: T) -> Self {
        Self(value)
    }
}

/// Prints one event line on standard output.
fn emit(event: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{event}");
    let _ = out.flush();
}

/// The milliseconds since `began`.
fn ms(began: Instant) -> u64 {
    clock::micros_since(began) / 1000
}

/// What the owner's main loop is told.
enum Told {
    /// The VM of boot `boot` stopped: the guest stopped it, the framework stopped it with an
    /// error, or a force stop completed. A notice of an earlier boot than the current one is
    /// stale and ignored.
    Stopped {
        /// The boot the notice is about.
        boot: u32,
        /// The framework's error, when it stopped the VM with one.
        why: Option<String>,
    },
    /// The guest sent its ready line.
    Ready(String),
    /// A control request to stop the VM and end the owner.
    Exit,
}

struct ObserverIvars {
    told: Mutex<Sender<Told>>,
    boot: u32,
    ready_port: u32,
    forwards: Vec<(u32, String)>,
}

define_class!(
    // The delegate of the VM and of its virtio socket listeners; the framework calls it on
    // the VM's queue.
    #[unsafe(super(NSObject))]
    #[name = "JsAgentLinuxVmObserver"]
    #[ivars = ObserverIvars]
    struct Observer;

    // SAFETY: NSObject's protocol, which the superclass NSObject implements.
    unsafe impl NSObjectProtocol for Observer {}

    // SAFETY: each method has the selector and the argument types of the protocol's method;
    // the framework calls them on the VM's queue, and they only send on a channel.
    unsafe impl VZVirtualMachineDelegate for Observer {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            self.tell(Told::Stopped {
                boot: self.ivars().boot,
                why: None,
            });
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            self.tell(Told::Stopped {
                boot: self.ivars().boot,
                why: Some(describe(error)),
            });
        }
    }

    // SAFETY: as for the VM's delegate: the selector and argument types are the protocol's,
    // and the framework calls the method on the VM's queue.
    unsafe impl VZVirtioSocketListenerDelegate for Observer {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> Bool {
            let iv = self.ivars();
            // SAFETY: plain accessors on a connection the framework keeps alive for this
            // call; the descriptor stays open as long as the retained connection lives, which
            // the thread below holds until it is done with the descriptor.
            let (port, fd, keep) = unsafe {
                (
                    connection.destinationPort(),
                    connection.fileDescriptor(),
                    AnyThread::new(connection.retain()),
                )
            };
            if port == iv.ready_port {
                let told = self.sender();
                std::thread::spawn(move || {
                    let line = read_ready_line(fd);
                    drop(keep);
                    if let Some(told) = told {
                        let _ = told.send(Told::Ready(line));
                    }
                });
                return Bool::YES;
            }
            if let Some((_, to)) = iv.forwards.iter().find(|(p, _)| *p == port) {
                let to = to.clone();
                std::thread::spawn(move || {
                    if let Err(why) = forward_to_tcp(fd, &to) {
                        eprintln!("js-agent __linux-vm: port {port} to {to}: {why}");
                    }
                    drop(keep);
                });
                return Bool::YES;
            }
            Bool::NO
        }
    }
);

impl Observer {
    fn new(ivars: ObserverIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: `init` on NSObject's subclass with all its ivars set.
        unsafe { msg_send![super(this), init] }
    }

    fn sender(&self) -> Option<Sender<Told>> {
        self.ivars().told.lock().ok().map(|s| s.clone())
    }

    fn tell(&self, told: Told) {
        if let Some(sender) = self.sender() {
            let _ = sender.send(told);
        }
    }
}

/// A stream on a duplicate of the virtio socket descriptor `fd`, which the caller's retained
/// connection keeps open while this duplicates it.
fn stream_of(fd: RawFd) -> std::io::Result<UnixStream> {
    // SAFETY: `fd` is open for as long as the caller's connection object lives, which is
    // longer than this call; it is only borrowed here to be duplicated, and the duplicate is
    // the caller's own.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    // A virtio socket connection is a stream socket: read, write and shutdown(2) behave on
    // it as on a Unix one.
    Ok(UnixStream::from(borrowed.try_clone_to_owned()?))
}

/// The one line the guest sends on the ready port.
fn read_ready_line(fd: RawFd) -> String {
    let Ok(stream) = stream_of(fd) else {
        return String::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = String::new();
    let _ = BufReader::new(stream.take(4096)).read_line(&mut line);
    line.trim_end().to_owned()
}

/// Carries a guest connection (`fd`) to the TCP address `to` on the Mac.
fn forward_to_tcp(fd: RawFd, to: &str) -> std::io::Result<()> {
    let guest = stream_of(fd)?;
    let tcp = TcpStream::connect(to)?;
    let _ = tcp.set_nodelay(true);
    let (Ok(mut guest_in), Ok(mut tcp_out), Ok(mut tcp_in), Ok(mut guest_out)) = (
        guest.try_clone(),
        tcp.try_clone(),
        tcp.try_clone(),
        guest.try_clone(),
    ) else {
        return Err(std::io::Error::other("cannot duplicate the streams"));
    };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut guest_in, &mut tcp_out);
        let _ = tcp_out.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut tcp_in, &mut guest_out);
    let _ = guest_out.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
    let _ = guest.shutdown(std::net::Shutdown::Both);
    let _ = tcp.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// Copies bytes both ways between two Unix-like streams until either side closes.
fn bridge_unix(a: UnixStream, b: UnixStream) {
    let (Ok(mut a_in), Ok(mut b_out), Ok(mut b_in), Ok(mut a_out)) =
        (a.try_clone(), b.try_clone(), b.try_clone(), a.try_clone())
    else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a_in, &mut b_out);
        let _ = b_out.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut b_in, &mut a_out);
    let _ = a_out.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
    let _ = a.shutdown(std::net::Shutdown::Both);
    let _ = b.shutdown(std::net::Shutdown::Both);
}

/// What lives as long as one boot of the VM; only touched on the VM's queue.
struct Running {
    vm: Retained<VZVirtualMachine>,
    _observer: Retained<Observer>,
    _listeners: Vec<Retained<VZVirtioSocketListener>>,
}

type Holder = Arc<Mutex<Option<Queued<Running>>>>;

/// Runs `f` with the VM on its queue and returns what it returns; `None` when there is no
/// VM.
fn on_queue<R: Send + 'static>(
    queue: &DispatchQueue,
    holder: &Holder,
    f: impl FnOnce(&VZVirtualMachine) -> R + Send + 'static,
) -> Option<R> {
    let (h, (tx, rx)) = (holder.clone(), mpsc::channel());
    queue.exec_sync(move || {
        let Ok(slot) = h.lock() else { return };
        let Some(running) = slot.as_ref() else { return };
        // SAFETY: this closure runs on the VM's queue.
        let vm = unsafe { &running.get().vm };
        let _ = tx.send(f(vm));
    });
    rx.try_recv().ok()
}

/// Releases the VM, its delegate and listeners on the VM's queue.
fn release_on_queue(queue: &DispatchQueue, holder: &Holder) {
    let h = holder.clone();
    queue.exec_sync(move || {
        if let Ok(mut slot) = h.lock() {
            drop(slot.take().map(|q| q.0));
        }
    });
}

/// The traditional balloon device's target in MiB, after setting it to `set` when given.
fn balloon(vm: &VZVirtualMachine, set: Option<u64>) -> Option<u64> {
    // SAFETY: called on the VM's queue (from `on_queue`).
    unsafe {
        let device = vm
            .memoryBalloonDevices()
            .iter()
            .find_map(|d| d.downcast::<VZVirtioTraditionalMemoryBalloonDevice>().ok())?;
        if let Some(mib) = set {
            device.setTargetVirtualMachineMemorySize(mib << 20);
        }
        Some(device.targetVirtualMachineMemorySize() >> 20)
    }
}

/// Connects to the guest's virtio socket `port`; the connection's descriptor and the
/// retained connection that keeps it open.
fn connect_guest(
    queue: &DispatchRetained<DispatchQueue>,
    holder: &Holder,
    port: u32,
) -> Result<(RawFd, AnyThread<Retained<VZVirtioSocketConnection>>), String> {
    let (tx, rx) = mpsc::channel();
    let h = holder.clone();
    queue.exec_async(move || {
        let Ok(slot) = h.lock() else { return };
        let Some(running) = slot.as_ref() else {
            let _ = tx.send(Err("no VM".to_owned()));
            return;
        };
        // SAFETY: on the VM's queue, which `socketDevices` and `connectToPort` require. In
        // the completion handler a non-null `conn` or `err` is live for the call; the
        // connection is retained so its descriptor stays open for the caller.
        unsafe {
            let Some(device) = running
                .get()
                .vm
                .socketDevices()
                .iter()
                .find_map(|d| d.downcast::<VZVirtioSocketDevice>().ok())
            else {
                let _ = tx.send(Err("the VM has no virtio socket device".to_owned()));
                return;
            };
            let block = RcBlock::new(
                move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                    let result = match conn.as_ref() {
                        Some(c) => Ok((c.fileDescriptor(), AnyThread::new(c.retain()))),
                        None => Err(err
                            .as_ref()
                            .map_or_else(|| "no connection and no error".to_owned(), describe)),
                    };
                    let _ = tx.send(result);
                },
            );
            device.connectToPort_completionHandler(port, &block);
        }
    });
    rx.recv_timeout(COMPLETION)
        .map_err(|_| format!("no connect completion for port {port}"))?
}

/// Where the VM is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Stopped => "stopped",
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Stopping => "stopping",
        }
    }
}

/// The owner's state, under `Owner::state`.
struct State {
    phase: Phase,
    /// Guest connections open now (a container's run holds one until its exit).
    active: u32,
    /// When the last guest connection ended, or the guest became ready.
    last_activity: Instant,
    boots: u32,
    start_call: Option<Instant>,
    stop_call: Option<Instant>,
    exiting: bool,
}

/// The VM's owner: it starts the VM when a connection needs it and stops it when asked or
/// idle.
struct Owner {
    flags: RunFlags,
    paths: Paths,
    began: Instant,
    queue: DispatchRetained<DispatchQueue>,
    holder: Holder,
    told: Sender<Told>,
    state: Mutex<State>,
    changed: Condvar,
}

impl Owner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn event(&self, mut event: serde_json::Value) {
        event["ms"] = ms(self.began).into();
        emit(&event);
    }

    /// Boots the VM: a new configuration and VM object, started on the queue. The caller has
    /// set the phase to `Starting`; on an error the phase goes back to `Stopped`.
    fn boot(self: &Arc<Self>) -> Result<(), String> {
        let started = self.boot_inner();
        if started.is_ok() {
            let boot = self.lock().boots;
            self.watch_ready(boot);
        }
        if let Err(why) = &started {
            let mut st = self.lock();
            st.phase = Phase::Stopped;
            self.changed.notify_all();
            self.event(serde_json::json!({"event": "error", "stage": "start", "message": why}));
        }
        started
    }

    fn boot_inner(&self) -> Result<(), String> {
        let config = build(&self.flags, &self.paths)?;
        let boot_no = {
            let mut st = self.lock();
            st.boots += 1;
            st.start_call = Some(clock::now());
            st.boots
        };
        let ivars = ObserverIvars {
            told: Mutex::new(self.told.clone()),
            boot: boot_no,
            ready_port: self.flags.ready_port,
            forwards: self
                .flags
                .forwards
                .iter()
                .map(|f| (f.port, f.to.clone()))
                .collect(),
        };
        let mut ports = vec![self.flags.ready_port];
        ports.extend(self.flags.forwards.iter().map(|f| f.port));
        let (start_tx, start_rx) = mpsc::channel::<Option<String>>();
        let (q, h, cfg) = (self.queue.clone(), self.holder.clone(), Queued(config));
        self.event(serde_json::json!({"event": "starting", "boot": boot_no}));
        self.queue.exec_async(move || {
            // SAFETY: this closure runs on the VM's serial queue, the only place the VM is
            // touched; the observer and listeners are stored in `Running`, which outlives the
            // VM's use of them. In the completion handler a non-null `err` is live for the
            // call.
            unsafe {
                let vm = VZVirtualMachine::initWithConfiguration_queue(
                    VZVirtualMachine::alloc(),
                    cfg.get(),
                    &q,
                );
                let observer = Observer::new(ivars);
                vm.setDelegate(Some(ProtocolObject::from_ref(&*observer)));
                let mut listeners = Vec::new();
                for dev in vm.socketDevices().iter() {
                    let Ok(dev) = dev.downcast::<VZVirtioSocketDevice>() else {
                        continue;
                    };
                    for port in &ports {
                        let l = VZVirtioSocketListener::new();
                        l.setDelegate(Some(ProtocolObject::from_ref(&*observer)));
                        dev.setSocketListener_forPort(&l, *port);
                        listeners.push(l);
                    }
                }
                let block = RcBlock::new(move |err: *mut NSError| {
                    let _ = start_tx.send(err.as_ref().map(describe));
                });
                vm.startWithCompletionHandler(&block);
                if let Ok(mut slot) = h.lock() {
                    *slot = Some(Queued(Running {
                        vm,
                        _observer: observer,
                        _listeners: listeners,
                    }));
                }
            }
        });
        match start_rx.recv_timeout(COMPLETION) {
            Ok(None) => {
                let since = self.lock().start_call.map_or(0, ms);
                self.event(serde_json::json!({"event": "started", "since_start_ms": since}));
                Ok(())
            }
            Ok(Some(why)) => {
                release_on_queue(&self.queue, &self.holder);
                Err(format!("start: {why}"))
            }
            Err(_) => {
                release_on_queue(&self.queue, &self.holder);
                Err("start: no completion".to_owned())
            }
        }
    }

    /// Waits until the VM runs, starting it when it is stopped, and counts a connection
    /// in. A start that arrives while the VM is starting or stopping waits for it.
    fn acquire(self: &Arc<Self>) -> Result<(), String> {
        let deadline = clock::now() + DETACHED_READY;
        let mut st = self.lock();
        loop {
            match st.phase {
                Phase::Running => {
                    st.active += 1;
                    return Ok(());
                }
                Phase::Stopped if st.exiting => return Err("the owner is ending".to_owned()),
                Phase::Stopped => {
                    st.phase = Phase::Starting;
                    self.changed.notify_all();
                    drop(st);
                    self.boot()?;
                    st = self.lock();
                }
                Phase::Starting | Phase::Stopping => {
                    let now = clock::now();
                    if now >= deadline {
                        return Err(format!("the VM did not become ready in {DETACHED_READY:?}"));
                    }
                    st = self
                        .changed
                        .wait_timeout(st, deadline - now)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            }
        }
    }

    /// Counts a connection out.
    fn release(&self) {
        let mut st = self.lock();
        st.active = st.active.saturating_sub(1);
        st.last_activity = clock::now();
    }

    /// Asks the guest program to power the guest off; whether it said it will.
    fn guest_poweroff(&self) -> bool {
        let Ok((fd, keep)) = connect_guest(&self.queue, &self.holder, wire::GUEST_PORT) else {
            return false;
        };
        let Ok(mut stream) = stream_of(fd) else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let Ok(line) = wire::request_line(&wire::Request::Poweroff) else {
            return false;
        };
        let mut reply = String::new();
        let said = writeln!(stream, "{line}").is_ok()
            && BufReader::new(&stream).read_line(&mut reply).is_ok()
            && wire::parse_reply(&reply).is_ok_and(|r| r.ok);
        drop(keep);
        said
    }

    /// Stops the VM: with `graceful`, the guest program is asked to power the guest off over
    /// the virtio socket, then (when it does not answer) the framework's power button is
    /// pressed, and the VM is stopped at once if it has not stopped in [`SHUTDOWN_GRACE`];
    /// otherwise it is stopped at once. With `idle`, only a running VM with no connection open
    /// for that long is stopped, checked under the same hold of the state as the change to
    /// `Stopping`, so a connection that arrives in between keeps the VM. Returns whether a stop
    /// began; the asking and waiting run on a thread of their own.
    fn stop(self: &Arc<Self>, graceful: bool, why: &str, idle: Option<Duration>) -> bool {
        let boot = {
            let mut st = self.lock();
            let stoppable = match idle {
                None => matches!(st.phase, Phase::Running | Phase::Starting),
                Some(idle) => {
                    st.phase == Phase::Running
                        && st.active == 0
                        && clock::now().saturating_duration_since(st.last_activity) >= idle
                }
            };
            if !stoppable {
                return false;
            }
            st.phase = Phase::Stopping;
            st.stop_call = Some(clock::now());
            self.changed.notify_all();
            st.boots
        };
        self.event(serde_json::json!({"event": "stopping", "why": why, "graceful": graceful}));
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let asked = graceful
                && (me.guest_poweroff()
                    || matches!(
                        on_queue(&me.queue, &me.holder, |vm| {
                            // SAFETY: on the VM's queue.
                            unsafe { vm.requestStopWithError() }.map_err(|e| describe(&e))
                        }),
                        Some(Ok(()))
                    ));
            if asked {
                let deadline = clock::now() + SHUTDOWN_GRACE;
                let mut st = me.lock();
                while st.phase == Phase::Stopping && st.boots == boot && clock::now() < deadline {
                    let left = deadline.saturating_duration_since(clock::now());
                    st = me
                        .changed
                        .wait_timeout(st, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
                if st.phase != Phase::Stopping || st.boots != boot {
                    return;
                }
            }
            let forced = force_stop(&me.queue, &me.holder);
            let _ = me.told.send(Told::Stopped {
                boot,
                why: forced.err(),
            });
        });
        true
    }

    /// Stops a boot that never says ready: a guest that boots and whose guest program never
    /// sends its ready line would otherwise stay `Starting`, and every connection would wait.
    fn watch_ready(self: &Arc<Self>, boot: u32) {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let deadline = clock::now() + DETACHED_READY;
            let mut st = me.lock();
            while st.phase == Phase::Starting && st.boots == boot && clock::now() < deadline {
                let left = deadline.saturating_duration_since(clock::now());
                st = me
                    .changed
                    .wait_timeout(st, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            let stuck = st.phase == Phase::Starting && st.boots == boot;
            drop(st);
            if stuck {
                me.event(serde_json::json!({"event": "error", "stage": "ready", "message": "the guest never said ready"}));
                me.stop(false, "never ready", None);
            }
        });
    }
}

/// Stops the VM at once (a force stop; the guest sees its power cut).
fn force_stop(queue: &DispatchRetained<DispatchQueue>, holder: &Holder) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let h = holder.clone();
    queue.exec_async(move || {
        let Ok(slot) = h.lock() else { return };
        let Some(running) = slot.as_ref() else { return };
        let block = RcBlock::new(move |err: *mut NSError| {
            // SAFETY: a non-null `err` is live for the duration of the call.
            let _ = tx.send(unsafe { err.as_ref() }.map(describe));
        });
        // SAFETY: on the VM's queue. A VM that cannot stop is stopped already: the block is
        // dropped unsent.
        unsafe {
            let vm = &running.get().vm;
            if vm.canStop() {
                vm.stopWithCompletionHandler(&block);
            }
        }
    });
    match rx.recv_timeout(COMPLETION) {
        Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
        Ok(Some(why)) => Err(why),
        Err(mpsc::RecvTimeoutError::Timeout) => Err("no stop completion".to_owned()),
    }
}

/// Serves one control connection.
fn serve_control(stream: UnixStream, owner: &Arc<Owner>) {
    let mut line = String::new();
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    if BufReader::new(reader.take(control::MAX_LINE as u64))
        .read_line(&mut line)
        .is_err()
    {
        return;
    }
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut answer = |text: String| {
        let _ = writeln!(out, "{text}");
    };
    match control::parse(&line) {
        Err(why) => answer(format!("err {why}")),
        Ok(Control::Connect(port)) => {
            if let Err(why) = owner.acquire() {
                answer(format!("err {why}"));
                return;
            }
            match connect_guest(&owner.queue, &owner.holder, port)
                .and_then(|(fd, keep)| Ok((stream_of(fd).map_err(|e| e.to_string())?, keep)))
            {
                Err(why) => answer(format!("err {why}")),
                Ok((guest, keep)) => {
                    answer("ok".to_owned());
                    bridge_unix(stream, guest);
                    drop(keep);
                }
            }
            owner.release();
        }
        Ok(Control::Start) => match owner.acquire() {
            Ok(()) => {
                owner.release();
                answer("ok running".to_owned());
            }
            Err(why) => answer(format!("err {why}")),
        },
        Ok(Control::Balloon(mib)) => {
            match on_queue(&owner.queue, &owner.holder, move |vm| {
                balloon(vm, Some(mib))
            }) {
                Some(Some(target)) => answer(format!("ok {target}")),
                _ => answer("err no running VM with a balloon device".to_owned()),
            }
        }
        Ok(Control::State) => {
            let target = on_queue(&owner.queue, &owner.holder, |vm| balloon(vm, None)).flatten();
            let (phase, active, boots) = {
                let st = owner.lock();
                (st.phase.name(), st.active, st.boots)
            };
            answer(format!(
                "ok {phase} active={active} boots={boots} balloon_mib={} owner_pid={}",
                target.map_or_else(|| "-".to_owned(), |t| t.to_string()),
                std::process::id()
            ));
        }
        Ok(Control::Stop) => {
            let began = owner.stop(true, "asked", None);
            answer(
                if began {
                    "ok stopping"
                } else {
                    "ok not running"
                }
                .to_owned(),
            );
        }
        Ok(Control::Kill) => {
            let began = owner.stop(false, "asked", None);
            answer(
                if began {
                    "ok stopping"
                } else {
                    "ok not running"
                }
                .to_owned(),
            );
        }
        Ok(Control::Exit) => {
            answer("ok".to_owned());
            let _ = owner.told.send(Told::Exit);
        }
    }
}

/// `run`: owns the VM until an `exit` request (or, with `--exit-when-ready`, until the
/// guest's first ready line): boots it at once unless `--start-stopped`, starts it when a
/// connection needs it, and stops it after `--idle-stop-s` seconds with no guest
/// connection.
pub fn run(flags: &RunFlags, out: &mut dyn Write) -> Result<(), String> {
    if flags.detach {
        return detach(flags, out);
    }
    let began = clock::now();
    let paths = Paths {
        dir: flags.dir.clone(),
    };
    let socket = paths.file(control::SOCKET);
    if client::ctl(&paths.dir, "state").is_ok() {
        return Err(format!("an owner already answers on {}", socket.display()));
    }
    let _ = fs::remove_file(&socket);
    fs::write(paths.file("owner.pid"), format!("{}\n", std::process::id()))
        .map_err(|why| format!("write owner.pid: {why}"))?;
    let (told_tx, told_rx) = mpsc::channel::<Told>();
    let owner = Arc::new(Owner {
        flags: flags.clone(),
        paths,
        began,
        queue: DispatchQueue::new("com.komira.js-agent.linux-vm", DispatchQueueAttr::SERIAL),
        holder: Arc::new(Mutex::new(None)),
        told: told_tx,
        state: Mutex::new(State {
            phase: Phase::Stopped,
            active: 0,
            last_activity: began,
            boots: 0,
            start_call: None,
            stop_call: None,
            exiting: false,
        }),
        changed: Condvar::new(),
    });
    owner.event(serde_json::json!({"event": "owner", "pid": std::process::id()}));

    let listener =
        UnixListener::bind(&socket).map_err(|why| format!("bind {}: {why}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|why| format!("chmod {}: {why}", socket.display()))?;
    {
        let owner = Arc::clone(&owner);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let owner = Arc::clone(&owner);
                std::thread::spawn(move || serve_control(stream, &owner));
            }
        });
    }
    if flags.idle_stop_s > 0 {
        let owner = Arc::clone(&owner);
        let idle = Duration::from_secs(flags.idle_stop_s);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                owner.stop(true, "idle", Some(idle));
            }
        });
    }
    if !flags.start_stopped {
        let first = Arc::clone(&owner);
        std::thread::spawn(move || {
            if first.acquire().is_ok() {
                first.release();
            }
        });
    }

    while let Ok(told) = told_rx.recv() {
        match told {
            Told::Ready(line) => {
                let _ = fs::write(owner.paths.file("ready"), format!("{line}\n"));
                let since = {
                    let mut st = owner.lock();
                    if st.phase == Phase::Starting {
                        st.phase = Phase::Running;
                    }
                    st.last_activity = clock::now();
                    owner.changed.notify_all();
                    st.start_call.map_or(0, ms)
                };
                owner.event(serde_json::json!({
                    "event": "ready", "since_start_ms": since, "line": line,
                }));
                if flags.exit_when_ready {
                    owner.lock().exiting = true;
                    owner.stop(false, "exit-when-ready", None);
                }
            }
            Told::Stopped { boot, why } => {
                {
                    let st = owner.lock();
                    if st.phase == Phase::Stopped || boot != st.boots {
                        continue;
                    }
                }
                release_on_queue(&owner.queue, &owner.holder);
                let (took, exiting) = {
                    let mut st = owner.lock();
                    st.phase = Phase::Stopped;
                    owner.changed.notify_all();
                    (st.stop_call.take().map(ms), st.exiting)
                };
                owner.event(serde_json::json!({
                    "event": "stopped", "stop_ms": took, "error": why,
                }));
                if exiting {
                    break;
                }
            }
            Told::Exit => {
                owner.lock().exiting = true;
                if !owner.stop(true, "exit", None) && owner.lock().phase == Phase::Stopped {
                    break;
                }
            }
        }
    }
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(owner.paths.file("owner.pid"));
    owner.event(serde_json::json!({"event": "owner-exit"}));
    Ok(())
}

/// `run --detach`: starts this program again, without `--detach`, as the VM's owner in a
/// process group of its own (so that it outlives this process and its launchd job's group),
/// its output appended to `owner.log`, and returns when the guest is ready.
fn detach(flags: &RunFlags, out: &mut dyn Write) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|why| format!("this program's path: {why}"))?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach")
        .map(|a| if a == "adopt" { "run".to_owned() } else { a })
        .collect();
    let ready = flags.dir.join("ready");
    let _ = fs::remove_file(&ready);
    let log_path = flags.dir.join("owner.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|why| format!("open {}: {why}", log_path.display()))?;
    let log2 = log
        .try_clone()
        .map_err(|why| format!("dup {}: {why}", log_path.display()))?;
    let began = clock::now();
    let mut child = Program::new(&exe, &args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2)
        .spawn()
        .map_err(|why| format!("start the owner: {why}"))?;
    let wait_for_ready = !flags.start_stopped;
    loop {
        let up = if wait_for_ready {
            fs::read_to_string(&ready).ok()
        } else {
            client::ctl(&flags.dir, "state").ok()
        };
        if let Some(line) = up {
            let _ = writeln!(
                out,
                "detached owner pid {} up after {} ms: {}",
                child.id(),
                ms(began),
                line.trim_end()
            );
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the owner ended ({status}) before it was up; see {}",
                log_path.display()
            ));
        }
        if u128::from(clock::micros_since(began)) > DETACHED_READY.as_micros() {
            return Err(format!(
                "the owner was not up within {DETACHED_READY:?}; it (pid {}) runs on",
                child.id()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `adopt`: the agent's side of a VM that outlives it. Uses the owner that answers in the
/// directory, or starts one detached; then stays (parked) until its process is ended, as a
/// launchd job is: it does not follow the owner's own end.
pub fn adopt(flags: &RunFlags, out: &mut dyn Write) -> Result<(), String> {
    let began = clock::now();
    match client::ctl(&flags.dir, "state") {
        Ok(state) => {
            let _ = writeln!(
                out,
                "adopted the running owner after {} ms: {state}",
                ms(began)
            );
        }
        Err(_) => detach(flags, out)?,
    }
    let _ = out.flush();
    loop {
        std::thread::park();
    }
}
