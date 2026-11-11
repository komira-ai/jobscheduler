//! The sets of the agent's flags that one runtime alone takes, each flag with whether it was
//! given: a runtime that takes none of a set refuses every flag of it by name.

use super::Cli;

impl Cli {
    /// The flags of the GPUs a crun job is given, with whether each was given: an agent
    /// that does not run the crun runtime refuses them all.
    #[must_use]
    pub fn gpu_flags(&self) -> [(&'static str, bool); 3] {
        [
            ("--gpu-device", !self.gpu_devices.is_empty()),
            ("--gpu-mount", !self.gpu_mounts.is_empty()),
            ("--gpu-env", !self.gpu_env.is_empty()),
        ]
    }

    /// The flags only the crun runtime takes (the GPU flags and `--host-port`), with whether
    /// each was given: an agent that runs another refuses them all.
    #[must_use]
    pub fn crun_only_flags(&self) -> [(&'static str, bool); 4] {
        let [device, mount, env] = self.gpu_flags();
        [
            device,
            mount,
            env,
            ("--host-port", !self.host_port.is_empty()),
        ]
    }

    /// Every `--native-*` flag with whether it was given, kept beside the fields so that a
    /// runtime that takes none of them refuses them all; a test walks the flags clap knows
    /// and fails when one is missing here.
    #[must_use]
    pub fn native_flags(&self) -> [(&'static str, bool); 13] {
        [
            (
                "--native-helper-socket",
                self.native_helper_socket.is_some(),
            ),
            ("--native-homes", self.native_homes.is_some()),
            ("--native-network", self.native_network.is_some()),
            ("--native-deny-read", !self.native_deny_read.is_empty()),
            ("--native-max-file-mib", self.native_max_file_mib.is_some()),
            (
                "--native-max-processes",
                self.native_max_processes.is_some(),
            ),
            ("--native-tool-root", !self.native_tool_roots.is_empty()),
            ("--native-pf-uid-start", self.native_pf_uid_start.is_some()),
            ("--native-pf-uid-count", self.native_pf_uid_count.is_some()),
            ("--native-pf-endpoint", !self.native_pf_endpoints.is_empty()),
            ("--native-pf-anchor", self.native_pf_anchor.is_some()),
            ("--native-pf-state-dir", self.native_pf_state_dir.is_some()),
            ("--native-pfctl", self.native_pfctl.is_some()),
        ]
    }
}
