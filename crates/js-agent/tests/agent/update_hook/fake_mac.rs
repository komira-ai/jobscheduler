//! A Mac's local directory and disks for the tests of the host setup: answers `dscl`,
//! `dseditgroup` and `diskutil` as a Mac does, from a state its answers change, and looks at a
//! path as the setup does. Everything is on device 1 except the scratch volume while it is
//! mounted on the scratch root, which is device 2; a volume that ignores ownership shows its
//! root owned by uid 99 until `diskutil enableOwnership`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use js_agent::update_hook::Params;
use js_agent::update_hook::step::{
    DATA_VOLUME, DISKUTIL, DSCL, DSEDITGROUP, PathFacts, SCRATCH_VOLUME,
};

use super::step_rig::{Rig, Spy};

/// The helper's socket group the Mac's params name.
pub const GROUP: &str = "_jsagent";

/// A Mac whose params name the helper's socket group and a scratch root: a first install
/// (`previous` false) or an update from 1.3.0, with the state root and the scratch root
/// `db/js-agent/{state,scratch}` of the rig's tree, neither there yet. The params are read
/// from the text a params file holds, with `socket_group` and `scratch_root` as its keys.
pub fn mac_rig(name: &str, previous: bool) -> Rig {
    let mut rig = if previous {
        Rig::mac_with_helper(name)
    } else {
        Rig::mac_first_install_with_helper(name)
    };
    let db = rig.tmp.path().join("db/js-agent");
    let state_root = format!(
        "state_root = \"{}/world/state\"\n",
        rig.tmp.path().display()
    );
    assert!(rig.params_text.contains(&state_root), "{}", rig.params_text);
    rig.params_text = rig.params_text.replace(
        &state_root,
        &format!(
            "state_root = \"{db}/state\"\nscratch_root = \"{db}/scratch\"\n\
             socket_group = \"{GROUP}\"\n",
            db = db.display()
        ),
    );
    rig.params = Params::parse(&rig.params_text).unwrap();
    assert_eq!(rig.params.state_root, db.join("state"));
    assert_eq!(rig.params.scratch_root, Some(db.join("scratch")));
    assert_eq!(rig.params.settings.socket_group.as_deref(), Some(GROUP));
    rig
}

/// The scratch root of a [`mac_rig`].
pub fn scratch_of(rig: &Rig) -> PathBuf {
    rig.params.scratch_root.clone().unwrap()
}

/// A spy over `rig` whose host setup goes to `mac`.
pub fn spy_on<'a>(rig: &'a Rig, mac: &Rc<FakeMac>) -> Spy<'a> {
    let spy = Spy::new(rig);
    spy.on_mac(mac);
    spy
}

/// The APFS container of the Data volume.
pub const CONTAINER: &str = "disk3";
/// The container's free space, in bytes.
pub const FREE: u64 = 3_944_388_337_664;
/// The quota the setup asks for: nine tenths of [`FREE`], in whole gigabytes.
pub const QUOTA: &str = "3549G";
/// The UUID the scratch volume is made with.
pub const UUID: &str = "0E3D7A4C-5B21-4F0A-9C38-2D6E1F7B8A90";
/// The device the scratch volume is made as.
pub const DEVICE: &str = "disk3s7";
/// The gid `dseditgroup` gives the first group it makes.
pub const FIRST_GID: u32 = 501;
/// The owner a volume that ignores ownership shows.
pub const UNKNOWN_UID: u32 = 99;
/// The error number of a call the system refuses (`EPERM`).
pub const EPERM: i32 = 1;

/// The scratch volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Where it is mounted, if it is.
    pub mount: Option<PathBuf>,
    /// Whether ownership is enabled on it.
    pub owners: bool,
}

/// The Mac.
pub struct FakeMac {
    /// The local directory's groups and their gids.
    pub groups: RefCell<BTreeMap<String, u32>>,
    /// The scratch volume, once it is made.
    pub volume: RefCell<Option<Volume>>,
    /// Answers given in place of the Mac's own, by the command line (`program arg arg...`).
    pub answers: RefCell<BTreeMap<String, Result<String, String>>>,
    /// The commands that changed the Mac, in order.
    pub changes: RefCell<Vec<String>>,
    /// Whether a mount puts the volume where it is asked to; when not, the mount says it did and
    /// the volume stays where it was.
    pub mount_takes: Cell<bool>,
    /// The paths a look at fails, as a file system that cannot be read.
    pub look_fails: RefCell<BTreeSet<PathBuf>>,
    /// The paths a look at fails once a mount has run.
    pub look_fails_after_mount: RefCell<BTreeSet<PathBuf>>,
    /// The paths a look at is refused by the system (`EPERM`), as macOS refuses a protected
    /// file to a process without the grant it needs.
    pub look_refused: RefCell<BTreeSet<PathBuf>>,
}

impl FakeMac {
    /// A Mac with the groups every Mac has (`wheel`, `staff`, `admin`) and no scratch volume.
    pub fn new() -> Rc<FakeMac> {
        let groups = [("admin", 80), ("staff", 20), ("wheel", 0)]
            .into_iter()
            .map(|(name, gid)| (name.to_owned(), gid))
            .collect();
        Rc::new(FakeMac {
            groups: RefCell::new(groups),
            volume: RefCell::new(None),
            answers: RefCell::new(BTreeMap::new()),
            changes: RefCell::new(Vec::new()),
            mount_takes: Cell::new(true),
            look_fails: RefCell::new(BTreeSet::new()),
            look_fails_after_mount: RefCell::new(BTreeSet::new()),
            look_refused: RefCell::new(BTreeSet::new()),
        })
    }

    /// The Mac answers `line` (`program arg arg...`) with `answer` from now on.
    pub fn answer(&self, line: &str, answer: Result<&str, &str>) {
        self.answers.borrow_mut().insert(
            line.to_owned(),
            answer.map(str::to_owned).map_err(str::to_owned),
        );
    }

    /// The group `name` is there with `gid`.
    pub fn with_group(&self, name: &str, gid: u32) {
        self.groups.borrow_mut().insert(name.to_owned(), gid);
    }

    /// The scratch volume is there, as `volume`.
    pub fn with_volume(&self, volume: Volume) {
        *self.volume.borrow_mut() = Some(volume);
    }

    /// The commands that changed the Mac so far.
    pub fn changes(&self) -> Vec<String> {
        self.changes.borrow().clone()
    }

    fn change(&self, line: &str) {
        self.changes.borrow_mut().push(line.to_owned());
    }

    /// What the Mac prints for `program` with `args`; `None` for a program that is not one of
    /// the setup's, which the spy answers itself.
    pub fn run(&self, program: &str, args: &[&str]) -> Option<Result<String, String>> {
        let line = format!("{program} {}", args.join(" "));
        if let Some(answer) = self.answers.borrow().get(&line) {
            if answer.is_ok() && program != DSCL && !line.contains("-plist") {
                self.change(&line);
            }
            return Some(answer.clone());
        }
        let answer = match (program, args) {
            (DSCL, [".", "-list", "/Groups", "PrimaryGroupID"]) => Ok(self
                .groups
                .borrow()
                .iter()
                .map(|(name, gid)| format!("{name:<32} {gid}\n"))
                .collect()),
            (DSEDITGROUP, ["-o", "create", name]) => {
                self.change(&line);
                let mut groups = self.groups.borrow_mut();
                let gid = groups
                    .values()
                    .copied()
                    .filter(|gid| *gid >= FIRST_GID)
                    .max();
                let gid = gid.map_or(FIRST_GID, |gid| gid + 1);
                groups.insert((*name).to_owned(), gid);
                Ok(String::new())
            }
            (DISKUTIL, ["info", "-plist", DATA_VOLUME]) => Ok(dictionary(&format!(
                "<key>APFSContainerReference</key><string>{CONTAINER}</string>\
                 <key>MountPoint</key><string>{DATA_VOLUME}</string>"
            ))),
            (DISKUTIL, ["apfs", "list", "-plist", CONTAINER]) => Ok(self.list()),
            (
                DISKUTIL,
                [
                    "apfs",
                    "addVolume",
                    CONTAINER,
                    "APFS",
                    SCRATCH_VOLUME,
                    "-quota",
                    _,
                    "-nomount",
                ],
            ) => {
                self.change(&line);
                self.with_volume(Volume {
                    mount: None,
                    owners: false,
                });
                Ok(String::new())
            }
            (DISKUTIL, ["info", "-plist", DEVICE]) => {
                let mount = self.volume.borrow().as_ref().and_then(|v| v.mount.clone());
                let mount = mount.map_or(String::new(), |path| path.display().to_string());
                Ok(dictionary(&format!(
                    "<key>DeviceIdentifier</key><string>{DEVICE}</string>\
                     <key>MountPoint</key><string>{mount}</string>"
                )))
            }
            (DISKUTIL, ["unmount", DEVICE]) => {
                self.change(&line);
                self.alter(|volume| volume.mount = None);
                Ok(String::new())
            }
            (DISKUTIL, ["mount", "nobrowse", "-mountPoint", point, DEVICE]) => {
                self.change(&line);
                let after = std::mem::take(&mut *self.look_fails_after_mount.borrow_mut());
                self.look_fails.borrow_mut().extend(after);
                if self.mount_takes.get() {
                    let point = PathBuf::from(point);
                    self.alter(|volume| volume.mount = Some(point));
                }
                Ok(String::new())
            }
            (DISKUTIL, ["enableOwnership", _]) => {
                self.change(&line);
                self.alter(|volume| volume.owners = true);
                Ok(String::new())
            }
            (DSCL | DSEDITGROUP | DISKUTIL, _) => {
                Err(format!("the fake Mac has no answer to {line}"))
            }
            _ => return None,
        };
        Some(answer)
    }

    fn alter(&self, change: impl FnOnce(&mut Volume)) {
        if let Some(volume) = self.volume.borrow_mut().as_mut() {
            change(volume);
        }
    }

    /// `diskutil apfs list -plist disk3`: the container, with the scratch volume if it is made.
    fn list(&self) -> String {
        let scratch = match &*self.volume.borrow() {
            Some(_) => format!(
                "<dict><key>APFSVolumeUUID</key><string>{UUID}</string>\
                 <key>DeviceIdentifier</key><string>{DEVICE}</string>\
                 <key>Name</key><string>{SCRATCH_VOLUME}</string></dict>"
            ),
            None => String::new(),
        };
        container(&format!(
            "<dict><key>APFSVolumeUUID</key><string>AD2CBB63-38BA-425F-B3BC-966DC17D727F</string>\
             <key>DeviceIdentifier</key><string>disk3s5</string>\
             <key>Name</key><string>Macintosh HD - Data</string></dict>{scratch}"
        ))
    }

    /// The device and the owner of `path`, as the setup looks at them.
    pub fn look(&self, path: &Path) -> io::Result<PathFacts> {
        if self.look_fails.borrow().contains(path) {
            return Err(io::Error::other("the disk does not answer"));
        }
        if self.look_refused.borrow().contains(path) {
            return Err(io::Error::from_raw_os_error(EPERM));
        }
        let meta = fs::metadata(path)?;
        let volume = self
            .volume
            .borrow()
            .clone()
            .filter(|volume| volume.mount.as_deref() == Some(path));
        Ok(match volume {
            Some(volume) => PathFacts {
                device: 2,
                uid: if volume.owners {
                    meta.uid()
                } else {
                    UNKNOWN_UID
                },
            },
            None => PathFacts {
                device: 1,
                uid: meta.uid(),
            },
        })
    }
}

/// A property list of one dictionary with `body`.
pub fn dictionary(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>{body}</dict></plist>\n"
    )
}

/// `diskutil apfs list -plist` of the one container [`CONTAINER`] holding `volumes`.
pub fn container(volumes: &str) -> String {
    dictionary(&format!(
        "<key>Containers</key><array><dict>\
         <key>CapacityFree</key><integer>{FREE}</integer>\
         <key>ContainerReference</key><string>{CONTAINER}</string>\
         <key>Volumes</key><array>{volumes}</array></dict></array>"
    ))
}
