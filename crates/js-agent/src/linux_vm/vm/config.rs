//! The VM's configuration: what `run` boots, from the command line and the files of the VM's
//! directory. See the module docs of `vm` for the devices and for who owns each object.

use std::fs;
use std::path::{Path, PathBuf};

use objc2::AnyThread as _;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZDirectorySharingDeviceConfiguration, VZDiskImageStorageDeviceAttachment, VZEFIBootLoader,
    VZEFIVariableStore, VZEFIVariableStoreInitializationOptions, VZEntropyDeviceConfiguration,
    VZFileSerialPortAttachment, VZGenericMachineIdentifier, VZGenericPlatformConfiguration,
    VZMACAddress, VZMemoryBalloonDeviceConfiguration, VZNATNetworkDeviceAttachment,
    VZNetworkDeviceConfiguration, VZSerialPortConfiguration, VZSharedDirectory,
    VZSingleDirectoryShare, VZSocketDeviceConfiguration, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioEntropyDeviceConfiguration, VZVirtioFileSystemDeviceConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtioSocketDeviceConfiguration,
    VZVirtioTraditionalMemoryBalloonDeviceConfiguration, VZVirtualMachineConfiguration,
};

use crate::linux_vm::flags::RunFlags;

/// The files of the VM's directory (see the module docs of `linux_vm`).
pub(super) struct Paths {
    pub(super) dir: PathBuf,
}

impl Paths {
    pub(super) fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// A file URL for `path`.
fn url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// An `NSError` as text.
pub(super) fn describe(error: &NSError) -> String {
    format!(
        "{} ({} {})",
        error.localizedDescription(),
        error.domain(),
        error.code()
    )
}

/// The VM's configuration from `flags`, validated by the framework. The identity, network
/// address and firmware variables are read from the directory, or made and kept there.
pub(super) fn build(
    flags: &RunFlags,
    paths: &Paths,
) -> Result<Retained<VZVirtualMachineConfiguration>, String> {
    // SAFETY: every call below is a message to an object this function owns (`Retained`),
    // with arguments that outlive the call; the framework copies what it keeps.
    unsafe {
        let cfg = VZVirtualMachineConfiguration::new();
        cfg.setCPUCount(flags.cpus as usize);
        cfg.setMemorySize(flags.memory_mib << 20);

        let platform = VZGenericPlatformConfiguration::new();
        let id_file = paths.file("machine-id");
        let id = match fs::read(&id_file) {
            Ok(bytes) => VZGenericMachineIdentifier::initWithDataRepresentation(
                VZGenericMachineIdentifier::alloc(),
                &NSData::with_bytes(&bytes),
            )
            .ok_or_else(|| format!("{} is not a machine identifier", id_file.display()))?,
            Err(_) => {
                let id = VZGenericMachineIdentifier::new();
                fs::write(&id_file, id.dataRepresentation().to_vec())
                    .map_err(|why| format!("write {}: {why}", id_file.display()))?;
                id
            }
        };
        platform.setMachineIdentifier(&id);
        cfg.setPlatform(&platform);

        let vars = paths.file("efi-vars");
        let store = if vars.exists() {
            VZEFIVariableStore::initWithURL(VZEFIVariableStore::alloc(), &url(&vars))
        } else {
            VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
                VZEFIVariableStore::alloc(),
                &url(&vars),
                VZEFIVariableStoreInitializationOptions(0),
            )
            .map_err(|e| format!("make {}: {}", vars.display(), describe(&e)))?
        };
        let boot = VZEFIBootLoader::new();
        boot.setVariableStore(Some(&store));
        cfg.setBootLoader(Some(&boot));

        let mut disks: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::new();
        for (name, read_only, required) in [("disk.raw", false, true), ("seed.iso", true, false)] {
            let path = paths.file(name);
            if !required && !path.exists() {
                continue;
            }
            let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url(&path),
                read_only,
            )
            .map_err(|e| format!("attach {}: {}", path.display(), describe(&e)))?;
            let disk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &attachment,
            );
            disks.push(Retained::into_super(disk));
        }
        cfg.setStorageDevices(&NSArray::from_retained_slice(&disks));

        let mac_file = paths.file("mac");
        let mac = match fs::read_to_string(&mac_file) {
            Ok(text) => VZMACAddress::initWithString(
                VZMACAddress::alloc(),
                &NSString::from_str(text.trim()),
            )
            .ok_or_else(|| format!("{} is not a MAC address", mac_file.display()))?,
            Err(_) => {
                let mac = VZMACAddress::randomLocallyAdministeredAddress();
                fs::write(&mac_file, mac.string().to_string())
                    .map_err(|why| format!("write {}: {why}", mac_file.display()))?;
                mac
            }
        };
        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setMACAddress(&mac);
        net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        let nets: Retained<NSArray<VZNetworkDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(net)]);
        cfg.setNetworkDevices(&nets);

        let entropy: Retained<NSArray<VZEntropyDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioEntropyDeviceConfiguration::new(),
            )]);
        cfg.setEntropyDevices(&entropy);

        let balloon: Retained<NSArray<VZMemoryBalloonDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
            )]);
        cfg.setMemoryBalloonDevices(&balloon);

        let socket: Retained<NSArray<VZSocketDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioSocketDeviceConfiguration::new(),
            )]);
        cfg.setSocketDevices(&socket);

        let console = paths.file("console.log");
        let attachment = VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &url(&console),
            true,
        )
        .map_err(|e| format!("open {}: {}", console.display(), describe(&e)))?;
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(&attachment));
        let serials: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(serial)]);
        cfg.setSerialPorts(&serials);

        let mut shares: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
        for share in &flags.shares {
            let dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str(&share.tag),
            );
            let dir = VZSharedDirectory::initWithURL_readOnly(
                VZSharedDirectory::alloc(),
                &url(&share.path),
                share.read_only,
            );
            let single =
                VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir);
            dev.setShare(Some(&single));
            shares.push(Retained::into_super(dev));
        }
        if !shares.is_empty() {
            cfg.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));
        }

        cfg.validateWithError()
            .map_err(|e| format!("the configuration: {}", describe(&e)))?;
        Ok(cfg)
    }
}
