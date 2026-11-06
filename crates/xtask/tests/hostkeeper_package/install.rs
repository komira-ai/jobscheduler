//! A built package placed on a host the way hostkeeper places it, over hostkeeper's own fakes:
//! the package is unpacked with the daemon's unpacker, its manifest loaded, and the placer's
//! steps run (claims and lint, begin, link, switch of `current`, reload, commit) on a scratch
//! tree that stands for `/`, with a service manager that records its calls. Nothing is started.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use hostkeeper_config::manifest::Manifest;
use hostkeeper_placed::lint::LintError;
use hostkeeper_placed::testing::{Event, FakeUnitControl, FaultFs, FsWrite, Log, UnitCall};
use hostkeeper_placed::{PathState, PlacedError, Placer, Roots};
use xtask::release::hostkeeper_package::Platform;

use crate::support::{
    CURRENT_BINARY, Scratch, TOKEN, VERSION, expected, package, running, unpack, version_dir,
    write_tree,
};

/// What the operator keeps in the join token file, which no step may read, write or record.
const SECRET: &str = "operator-secret-material-0123456789";

/// A host tree below a scratch directory: the unpacked package, the operator's token and flags
/// files, and the fakes the placer works over.
struct Host {
    scratch: Scratch,
    log: Log,
    fs: FaultFs,
    units: FakeUnitControl,
    manifest: Manifest,
}

impl Host {
    fn path(&self, host_path: &str) -> PathBuf {
        self.scratch.path().join(host_path.trim_start_matches('/'))
    }

    fn placer(&self, roots: Roots) -> Placer<'_> {
        Placer::new(roots, &self.fs, &self.units)
    }
}

/// A host that holds the unpacked package of `platform` and the operator's token and flags
/// files.
fn host(name: &str, platform: Platform) -> Host {
    let scratch = Scratch::new(name);
    let state = scratch.path().join("var/lib/hostkeeper");
    std::fs::create_dir_all(&state).expect("the state directory");
    let unpacked = unpack(&state, &package(platform), platform).expect("unpacked");
    // The operator's files, as the runbook places them.
    let etc = scratch.path().join("etc/js-agent");
    std::fs::create_dir_all(&etc).expect("etc");
    std::fs::write(etc.join("token"), format!("{SECRET}\n")).expect("the token");
    std::fs::set_permissions(etc.join("token"), std::fs::Permissions::from_mode(0o600))
        .expect("its mode");
    std::fs::write(
        etc.join("flags"),
        "--server=https://js.example.com:8443\n--pool=farm\n",
    )
    .expect("the flags");
    let log = Log::new();
    let fs = FaultFs::new(scratch.path(), log.clone());
    let units = FakeUnitControl::new(log.clone());
    Host {
        scratch,
        log,
        fs,
        units,
        manifest: unpacked.manifest,
    }
}

/// The paths of the events that wrote something, as the host names them.
fn written(log: &Log) -> Vec<PathBuf> {
    log.events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Fs(
                FsWrite::Write(path)
                | FsWrite::SyncFile(path)
                | FsWrite::SyncDir(path)
                | FsWrite::Remove(path)
                | FsWrite::RemoveDir(path)
                | FsWrite::CreateDirs(path),
            ) => Some(path),
            Event::Fs(FsWrite::Rename { to, .. }) => Some(to),
            Event::Fs(FsWrite::Symlink { link, .. }) => Some(link),
            Event::Unit(_) | Event::Mark(_) => None,
        })
        .collect()
}

/// The target of the link at the host path `path`.
fn target(host: &Host, path: &str) -> PathBuf {
    std::fs::read_link(host.path(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The Linux package placed step by step: both units are linked in the unit directory and the
/// tmpfiles file where the manifest says, into `current`; `current` is switched; the service
/// manager is reloaded after the links and not before; every path is recorded placed. Catches a
/// package whose units the placer's lint refuses, a path that is claimed by a second package
/// of the host, a reload that comes before the links it is for, and a switch that does not move
/// `current` to the version.
#[test]
fn the_linux_package_is_linked_into_the_unit_directory_and_current_is_switched() {
    for platform in [Platform::LinuxX86_64, Platform::LinuxArm64] {
        let host = host("hostkeeper-install-linux", platform);
        let placer = host.placer(Roots::linux());
        let claims = placer
            .claims(&host.manifest)
            .expect("the units pass the lint");
        let wanted: BTreeSet<PathBuf> = [
            "/etc/systemd/system/js-agent-crun-gpu.service",
            "/etc/systemd/system/js-agent-crun.service",
            "/etc/tmpfiles.d/js-agent-crun.conf",
        ]
        .map(PathBuf::from)
        .into();
        assert_eq!(claims.paths(), wanted);
        let linking = placer.begin(&claims).expect("begin");
        assert!(placer.link(&linking).expect("link"));
        assert!(
            placer
                .switch_current(&host.manifest.name, &host.manifest.version)
                .expect("switch")
        );
        placer.reload().expect("reload");
        assert!(placer.commit(linking).expect("commit"));

        let current = "/var/lib/hostkeeper/packages/js-agent/current";
        assert_eq!(
            target(&host, "/etc/systemd/system/js-agent-crun.service"),
            Path::new(&format!("{current}/units/js-agent-crun.service"))
        );
        assert_eq!(
            target(&host, "/etc/systemd/system/js-agent-crun-gpu.service"),
            Path::new(&format!("{current}/units/js-agent-crun-gpu.service"))
        );
        assert_eq!(
            target(&host, "/etc/tmpfiles.d/js-agent-crun.conf"),
            Path::new(&format!("{current}/files/tmpfiles.conf"))
        );
        assert_eq!(target(&host, current), Path::new(VERSION));
        assert_eq!(
            placer.current("js-agent").expect("current"),
            Some(VERSION.to_owned())
        );
        // The links resolve to the package's files: the unit the host reads is the package's.
        let linked = std::fs::read_to_string(
            version_dir(host.scratch.path()).join("units/js-agent-crun.service"),
        )
        .expect("unit");
        assert!(linked.contains(CURRENT_BINARY));

        let record = placer.store().load().expect("placed.json");
        assert_eq!(
            record.paths.keys().cloned().collect::<BTreeSet<_>>(),
            wanted
        );
        for (path, entry) in &record.paths {
            assert_eq!(
                (entry.service.as_str(), entry.version.as_str()),
                ("js-agent", VERSION),
                "{path:?}"
            );
            assert_eq!(entry.state, PathState::Placed, "{path:?}");
        }
        assert!(!record.reload_pending);

        let events = host.log.events();
        let reloads: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| **e == Event::Unit(UnitCall::DaemonReload))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(reloads.len(), 1, "one daemon-reload: {events:?}");
        let last_link = events.iter().rposition(|event| matches!(event, Event::Fs(FsWrite::Symlink { link, .. }) if link.starts_with("/etc/"))).expect("a unit link was made");
        let switched = events.iter().position(|event| matches!(event, Event::Fs(FsWrite::Rename { to, .. }) if to == Path::new(current))).expect("current was switched");
        assert!(
            last_link < reloads[0] && switched < reloads[0],
            "the reload follows the links and the switch: {events:?}"
        );
    }
}

/// The package places nothing of the operator's: the join token and the flags file are not
/// claimed, linked, written or recorded, the token's contents are in no record of a write or of
/// a call to the service manager, and the token file keeps its mode and bytes. The only mention
/// of the token's path in the unit is the value of `--token-file`. Catches a package that carries
/// a token, a `[[files]]` entry at the token's path, and a step that touches `/etc/js-agent`.
#[test]
fn the_package_places_nothing_at_the_operators_token_or_flags_file() {
    let host = host("hostkeeper-install-token", Platform::LinuxX86_64);
    let placer = host.placer(Roots::linux());
    let claims = placer.claims(&host.manifest).expect("claims");
    let linking = placer.begin(&claims).expect("begin");
    placer.link(&linking).expect("link");
    placer
        .switch_current(&host.manifest.name, &host.manifest.version)
        .expect("switch");
    placer.reload().expect("reload");
    placer.commit(linking).expect("commit");

    let operator = Path::new("/etc/js-agent");
    assert!(
        claims
            .paths()
            .iter()
            .all(|path| !path.starts_with(operator))
    );
    assert!(
        host.manifest
            .files
            .iter()
            .all(|file| !Path::new(&file.path).starts_with(operator))
    );
    let events = host.log.events();
    assert!(!events.is_empty());
    for path in written(&host.log) {
        assert!(!path.starts_with(operator), "a write at {path:?}");
    }
    assert!(!format!("{events:?}").contains(SECRET));
    assert!(!format!("{events:?}").contains(TOKEN));
    let token = host.path(TOKEN);
    assert_eq!(
        std::fs::read_to_string(&token).expect("the token"),
        format!("{SECRET}\n")
    );
    assert_eq!(
        std::fs::metadata(&token)
            .expect("its metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    let names: BTreeSet<String> = std::fs::read_dir(host.path("/etc/js-agent"))
        .expect("the directory")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["flags".to_owned(), "token".to_owned()])
    );

    // In the unit the token's path is the value of `--token-file`, once and nowhere else.
    let unit = std::fs::read_to_string(
        version_dir(host.scratch.path()).join("units/js-agent-crun.service"),
    )
    .expect("the unit");
    let settings = crate::linux::code(&unit).join("\n");
    assert_eq!(settings.matches(TOKEN).count(), 1, "{settings}");
    assert!(settings.contains(&format!("--token-file {TOKEN} ")));
    assert!(!unit.contains(SECRET));
}

/// Hostkeeper's own relink, which also links the `WantedBy=` of the run unit: the unit is
/// enabled by a link in `multi-user.target.wants`, a second relink changes nothing, and a unit
/// that names a package file through a version directory instead of `current` is refused before
/// anything is written. Catches a run unit hostkeeper cannot enable, a relink that is not
/// idempotent and a lint that does not read the package's unit.
#[test]
fn a_relink_enables_the_run_unit_and_a_unit_pinned_to_a_version_is_refused() {
    let host = host("hostkeeper-install-relink", Platform::LinuxX86_64);
    let placer = host.placer(Roots::linux());
    placer
        .switch_current(&host.manifest.name, &host.manifest.version)
        .expect("switch");
    assert!(placer.relink_to(&host.manifest).expect("relink"));
    assert_eq!(
        target(
            &host,
            "/etc/systemd/system/multi-user.target.wants/js-agent-crun.service"
        ),
        Path::new("/etc/systemd/system/js-agent-crun.service")
    );
    assert_eq!(host.units.active(), BTreeSet::new(), "nothing is started");
    assert!(!placer.relink_to(&host.manifest).expect("a second relink"));

    let pinned = Scratch::new("hostkeeper-install-pinned");
    let version = version_dir(pinned.path());
    write_tree(&version, &package(Platform::LinuxX86_64), |name, bytes| {
        if name == "units/js-agent-crun.service" {
            String::from_utf8(bytes)
                .expect("UTF-8")
                .replace("/current/bin/", &format!("/{VERSION}/bin/"))
                .into_bytes()
        } else {
            bytes
        }
    });
    let manifest =
        Manifest::load(&version, &expected(Platform::LinuxX86_64), &running()).expect("loads");
    let log = Log::new();
    let fs = FaultFs::new(pinned.path(), log.clone());
    let units = FakeUnitControl::new(log.clone());
    let placer = Placer::new(Roots::linux(), &fs, &units);
    let refused = placer.claims(&manifest).unwrap_err();
    assert!(
        matches!(&refused, PlacedError::Lint(LintError::PackagePath { key, .. }) if key == "ExecStart="),
        "{refused:?}"
    );
    assert!(log.events().is_empty());
}

/// The Mac package placed the same way, with the launchd directory as the unit directory: the
/// job is linked as `js-agent.plist`, into `current`, and the placer's lint reads the plist's
/// program through `current`. Nothing is written for the token.
#[test]
fn the_macos_package_is_linked_as_one_plist_in_the_launchd_directory() {
    let host = host("hostkeeper-install-macos", Platform::MacosArm64);
    let roots = Roots::new(
        Path::new("/var/lib/hostkeeper"),
        Path::new("/Library/LaunchDaemons"),
        Path::new("/var/lib/hostkeeper/packages"),
    );
    let placer = host.placer(roots);
    let claims = placer
        .claims(&host.manifest)
        .expect("the plist passes the lint");
    assert_eq!(
        claims.paths(),
        BTreeSet::from([PathBuf::from("/Library/LaunchDaemons/js-agent.plist")])
    );
    let linking = placer.begin(&claims).expect("begin");
    assert!(placer.link(&linking).expect("link"));
    placer
        .switch_current(&host.manifest.name, &host.manifest.version)
        .expect("switch");
    placer.reload().expect("reload");
    assert!(placer.commit(linking).expect("commit"));
    assert_eq!(
        target(&host, "/Library/LaunchDaemons/js-agent.plist"),
        Path::new("/var/lib/hostkeeper/packages/js-agent/current/units/js-agent.plist")
    );
    assert_eq!(
        target(&host, "/var/lib/hostkeeper/packages/js-agent/current"),
        Path::new(VERSION)
    );
    for path in written(&host.log) {
        assert!(!path.starts_with("/etc/js-agent"), "a write at {path:?}");
    }
    assert!(!format!("{:?}", host.log.events()).contains(SECRET));
}
