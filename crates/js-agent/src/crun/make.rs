//! The making of a lease once its sidecar is recorded: the image pinned and made sure of,
//! the container's spec, and the lease's directories, hand-overs, cgroup and bundle.

use js_contain::cgroup::{JOB_OWNED_FILES, LeaseCgroup, LeasePolicy};
use js_contain::container::ContainerSpec;
use js_contain::oci::image::PinnedImage;
use js_contain::oci::{LeaseDirs, Owner};
use js_types::{JobNetwork, Resources};

use super::dirs::{make_dir, owned, project_of, take_dir};
use super::fault::Fault;
use super::fs::Assembled;
use super::spec::{bare, container_spec, host_spec, job_env};
use super::{CrunRuntime, gpu, net};
use crate::image::{ImageConfig, config_path};
use crate::lifecycle::RunSpec;

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
        name: &str,
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
        self.make_resources(spec, &container, &lease, &cgroup)?;
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
    /// A host lease ([`Isolation::Host`](js_contain::Isolation::Host)) is the agent's own:
    /// only its directory is made (no overlay, home or input root), and it, the run's scratch
    /// directory and the cgroup's files are given to the agent itself, not to the job user or
    /// the job's ids; its bundle adds the host-lease marker, so `crun` runs as the agent for
    /// it ([`Oci::lease_user`]). Its bundle is the loopback one: a host job is never wired.
    fn make_resources(
        &self,
        spec: &RunSpec,
        container: &ContainerSpec,
        lease: &LeaseDirs,
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
            let made = match self.leases_beside && dir == lease.dir {
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
        self.rt
            .block_on(net::create(&self.oci, container, network))
            .map_err(|why| Fault::step("write the bundle", &why))
    }
}
