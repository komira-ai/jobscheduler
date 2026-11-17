//! The making of a lease once its sidecar is recorded: the image pinned and made sure of,
//! the container's spec, and the lease's directories, hand-overs, cgroup and bundle.

use js_contain::cgroup::{JOB_OWNED_FILES, LeaseCgroup, LeasePolicy};
use js_contain::container::ContainerSpec;
use js_contain::oci::image::PinnedImage;
use js_contain::oci::{LeaseDirs, Owner};
use js_types::{JobNetwork, Resources};

use super::dirs::{make_dir, owned, project_of, take_dir};
use super::fault::Fault;
use super::fs;
use super::fs::Assembled;
use super::spec::pinned;
use super::spec::{bare, container_spec, host_spec, job_env};
use super::{CrunRuntime, Run, State, gpu, net, rootfs};
use crate::cli_runtime::container_name;
use crate::image::{ImageConfig, config_path};
use crate::lifecycle::RunSpec;
use crate::lifecycle::{Body, label};
use crate::logs::RunLogs;

impl CrunRuntime {
    /// The pinned image (`given`, parsed as `image`; none for a root of layers alone, whose
    /// digest is empty), the container spec with `args`, what the run's assembly made and
    /// what its GPUs give it, and the resources of the lease `name`, whose sidecar is
    /// recorded.
    pub(super) fn make_recorded(
        &self,
        spec: &RunSpec,
        (given, image): (&str, &PinnedImage),
        (args, assembled): (&[String], Option<&Assembled>),
        (name, held): (&str, bool),
        gpu: gpu::Wiring,
    ) -> Result<(ContainerSpec, LeaseDirs, LeaseCgroup), Fault> {
        let config = match image.digest.is_empty() {
            true => None,
            false => Some(self.image_config(given, image, name, assembled)?),
        };
        let rootfs = js_contain::oci::image::published_root(&self.store, &image.digest);
        gpu.check_targets(&rootfs).map_err(Fault::Gpu)?;
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, name);
        let lease = self.oci.lease_dirs(name);
        let mut container = match spec.isolation {
            js_agent_wire::Isolation::Contained => {
                container_spec(name, spec, image, &lease, cgroup.name())
            }
            js_agent_wire::Isolation::Host => host_spec(name, spec, image, &lease, cgroup.name()),
        };
        if let Some(made) = assembled {
            Self::give(&mut container, made);
        }
        match &config {
            Some(config) => config.apply(args, &mut container)?,
            None => bare(args, &mut container),
        }
        job_env(&mut container, &spec.env);
        gpu.apply(&mut container);
        self.make_resources(spec, &container, (&lease, held), &cgroup)?;
        Ok((container, lease, cgroup))
    }

    /// The configuration of `image` (`given`), pinned for the lease `name` and made sure of.
    /// An image an earlier daemon (or a source that keeps no configs) unpacked has no config
    /// to run it by: it is fetched again, and the source keeps the config this time; but not
    /// under a root the run's assembly mounted, whose layer its tree is.
    fn image_config(
        &self,
        given: &str,
        image: &PinnedImage,
        name: &str,
        assembled: Option<&Assembled>,
    ) -> Result<ImageConfig, Fault> {
        // Before the image is made sure of: from then on no agent on the store removes it
        // until the lease is torn down.
        self.oci.pin_image(image, name)?;
        self.images.lease(name, &image.digest);
        let ensure = || self.rt.block_on(self.images.ensure(image));
        ensure().map_err(Fault::Image)?;
        if !config_path(&self.configs, &image.digest).exists() {
            if assembled.is_some_and(|made| made.root.is_some()) {
                let why = "it has no kept config, and its tree is a layer of the run's root";
                return Err(Fault::step("run the image", &why));
            }
            // The prefetcher is told, as it is for any image taken out. A run of another
            // agent on the image pins it, and the removal is refused.
            self.oci.unpin_image(name)?;
            self.rt.block_on(self.oci.remove_image(image))?;
            self.pulls.forget(given);
            self.oci.pin_image(image, name)?;
            ensure().map_err(Fault::Image)?;
        }
        Ok(ImageConfig::kept(&self.configs, &image.digest)?)
    }

    /// The directories, hand-overs, cgroup and bundle of one lease.
    ///
    /// A `held` lease (made ahead of its job) is made, not taken from the lifecycle, and its
    /// bundle is the held one ([`js_contain::Oci::create_held`]).
    ///
    /// A host lease ([`Isolation::Host`](js_contain::Isolation::Host)) is the agent's own:
    /// only its directory is made (no overlay, home or input root), and it, the run's scratch
    /// directory and the cgroup's files are given to the agent itself, not to the job user or
    /// the job's ids; its bundle adds the host-lease marker, so `crun` runs as the agent for
    /// it ([`Oci::lease_user`]). Its bundle is the loopback one: a host job is never wired.
    fn make_resources(
        &self,
        spec: &RunSpec,
        container: &ContainerSpec,
        (lease, held): (&LeaseDirs, bool),
        cgroup: &LeaseCgroup,
    ) -> Result<(), Fault> {
        let chown = self.hooks.chown;
        let host = container.isolation == js_contain::Isolation::Host;
        let ((job, job_id), dirs, network) = match host {
            true => {
                let own = self.oci.own_ids();
                let dirs = vec![(lease.dir.clone(), Owner::Job)];
                ((own, own), dirs, JobNetwork::None)
            }
            false => {
                let ids = (self.oci.job_ids(), self.oci.container_ids());
                (ids, lease.to_make(), spec.network)
            }
        };
        for (dir, owner) in dirs {
            let (uid, gid) = match owner {
                Owner::Job => job,
                Owner::Container => job_id,
            };
            // A lease directory made beside the run's is the lifecycle's: it must be there,
            // empty and in the run's project, or it would be outside the run's bound.
            // A held lease is the runtime's own, made ahead of any run.
            let made = match self.leases_beside && dir == lease.dir && !held {
                true => take_dir(&dir, &spec.scratch, &project_of),
                false => make_dir(&dir),
            };
            made.and_then(|()| chown(&dir, uid, gid))
                .map_err(|why| Fault::step("make a lease directory", &why))?;
        }
        chown(&spec.scratch, job_id.0, job_id.1)
            .map_err(|why| Fault::step("give the run's scratch directory to the job", &why))?;
        if spec.files.is_some() {
            let files = self.files.as_ref().ok_or(Fault::NoFiles)?;
            files
                .hand_over_with(&spec.run_id, job_id.0, job_id.1, &chown)
                .map_err(|why| Fault::step("give the run's files to the job", &why))?;
        }
        let booking = Resources::new(spec.cpu_millis, spec.memory_bytes, 0, None)?;
        cgroup
            .create_with(&booking, None, LeasePolicy::Hard)
            .map_err(|why| Fault::step("make the lease's cgroup", &why))?;
        for file in JOB_OWNED_FILES {
            chown(&owned(cgroup.dir(), file), job.0, job.1)
                .map_err(|why| Fault::step("give the lease's cgroup to the job user", &why))?;
        }
        if !held {
            return self
                .rt
                .block_on(net::create(&self.oci, container, network))
                .map_err(|why| Fault::step("write the bundle", &why));
        }
        // Loopback only, as a job that asked for no network gets; its pipe is the job's own
        // id's to read in the container.
        self.rt
            .block_on(self.oci.create_held(container))
            .map_err(|why| Fault::step("write the bundle", &why))?;
        chown(&js_contain::oci::job_pipe(&lease.dir), job_id.0, job_id.1)
            .map_err(|why| Fault::step("give the job's pipe to the job", &why))
    }

    /// Makes the container of `spec`, everything but its start. A failure removes what it
    /// made.
    ///
    /// An image in the store with no kept config is removed and fetched again. That deletes
    /// the root such an image gives as the lower layer of a container's mounts, so it relies
    /// on every container of an earlier daemon that still runs on it pinning it (an adopted
    /// run's pin stays until its removal, and a pinned image is not removed), and on the
    /// start-up pass that removes the rest of what an earlier agent left (P12b) having run
    /// before the first run is made. The removal and a fetch of the same image never overlap
    /// (the image store holds one gate for each digest).
    ///
    /// A host job is refused unless the runtime launches them ([`CrunSettings::host_jobs`]),
    /// and when it names a mount, run files or a GPU; it is not wired, since it has the
    /// host's network.
    ///
    /// `assembled` is what the run's assembly made of its layers and mounts (taken from
    /// [`fs::Fs::take`]); a `held` container is one made ahead of its job (`warm`), whose init
    /// waits for the job on its pipe.
    pub(super) fn make(
        &mut self,
        spec: &RunSpec,
        assembled: Option<fs::Assembled>,
        held: bool,
    ) -> Result<String, Fault> {
        let host = spec.isolation == js_agent_wire::Isolation::Host;
        if host {
            if !self.host_jobs {
                return Err(Fault::NotContained);
            }
            if !spec.mounts.is_empty() || spec.files.is_some() {
                return Err(Fault::HostMounts);
            }
            if !spec.gpu_indices.is_empty() {
                return Err(Fault::HostGpus);
            }
        } else {
            net::port_for(self.egress.as_deref(), spec.network).map_err(Fault::Network)?;
        }
        // Before anything is made: a run's nodes are read now, and one that is gone refuses it.
        let gpu = gpu::wiring(self.gpus.as_ref(), &spec.gpu_indices).map_err(Fault::Gpu)?;
        let Body::Container { image: given, args } = &spec.body else {
            return Err(Fault::Native);
        };
        // What the run's assembly made of its layers and mounts (`assembled`): the run's
        // removal unmounts it, or, for a run that is not made, the lifecycle's discard
        // (`Runtime::discard_fs`).
        // A root of layers alone has no image.
        let image = match given.is_empty() {
            true => PinnedImage {
                reference: String::new(),
                digest: String::new(),
            },
            false => pinned(given).map_err(Fault::Image)?,
        };
        let name = container_name(&self.host_id, &spec.run_id)
            .map_err(|_| Fault::Name(spec.run_id.clone()))?;
        if self.runs.contains_key(&name) {
            return Err(Fault::InUse(spec.run_id.clone()));
        }
        let now = (self.hooks.now_ms)();
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), now.to_string());
        // A restart offers the run its host ports again from this.
        if let Some(host_ports) = net::host_ports_label(&spec.host_ports) {
            labels.insert(label::HOST_PORTS.to_owned(), host_ports);
        }
        // Before the first thing the sidecar names is made (see the module `ledger`), the
        // image's pin among them: the next start removes a pin a dead process left, once it
        // has removed the lease's container.
        let entry = self.entry_for(spec, &name, (&labels, &image.digest));
        self.ledger
            .record(&entry)
            .map_err(|why| Fault::step("record the lease's labels", &why))?;
        let (container, lease, cgroup) = match self.make_recorded(
            spec,
            (given, &image),
            (args, assembled.as_ref()),
            (&name, held),
            gpu,
        ) {
            Ok(made) => made,
            Err(fault) => {
                // What the failed step left; none of it is a container. The sidecar goes with
                // it (the pin last), or stays for the next start when some of it cannot be
                // removed.
                let _ = self.ledger.discard(&entry, |entry| self.tear_down(entry));
                return Err(fault);
            }
        };
        self.runs.insert(
            name.clone(),
            Run {
                spec: container,
                labels,
                lease,
                cgroup,
                cap: js_contain::hard_memory_max(spec.memory_bytes),
                backstop: None,
                state: State::Created,
                net: match host {
                    // The host's own network: nothing to wire, nothing to forward.
                    true => net::Net::asked(JobNetwork::None, Vec::new(), Vec::new()),
                    false => {
                        net::Net::asked(spec.network, spec.ports.clone(), spec.host_ports.clone())
                    }
                },
                entry,
                logs: RunLogs::asked(self.logs.as_ref(), spec.log_sink.as_ref()),
                fs: assembled.map(|made| rootfs::RunFs::new(made.sources)),
            },
        );
        Ok(name)
    }
}
