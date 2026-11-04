//! The checks of a start that names a bundle.

use std::fmt;
use std::sync::Arc;

use js_agent_wire::{BUNDLE_PROGRAM_PREFIX, ImagePlatform, JobSpec, check_platform, refusal};
use js_bundle::DottedVersion;
use js_oci::Location;
use js_types::{Arch, BundleRef, Os};

use super::host::{BundleHost, Fetch, Needs};
use super::list::parse_digests;
use super::settings::{Pull, Settings, Trust};

/// A refusal: the wire code and a local explanation.
pub type Refusal = (&'static str, String);

/// Where the name of a pin came from, which decides the registry rule it is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The legacy `bundle` field: held to `--bundle-registry-allow*`.
    Bundle,
    /// The `image` of a start with a `run`: held to `--image-allow`.
    Image,
}

/// The platform of a Mac agent's bundles, the only one `Kind::bundle` accepts and the
/// config check enforces: darwin on arm64.
const MAC: ImagePlatform = ImagePlatform {
    os: Os::Macos,
    arch: Arch::Arm64,
};

/// The rules for a start that names a bundle, over the host they read.
#[derive(Clone)]
pub struct Admission {
    settings: Settings,
    host: Arc<dyn BundleHost>,
}

impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admission")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

fn no(code: &'static str, why: impl Into<String>) -> Refusal {
    (code, why.into())
}

impl Admission {
    /// The rules of `settings`, reading `host`.
    #[must_use]
    pub fn new(settings: Settings, host: Arc<dyn BundleHost>) -> Self {
        Self { settings, host }
    }

    /// Whether the start `spec` may go on, as far as its bundle is concerned. A start that
    /// pins a bundle (`bundle`) or names its image with a `run` (the image is the bundle's
    /// name on a Mac) is checked; any other passes. The checks run in this order, and the
    /// first that fails is the refusal: the name is not a pin; the digest, or the manifest an
    /// index digest resolved to, is on the deny list; it is not on the allow list; the name is outside every allowed registry (a
    /// `bundle` is held to `--bundle-registry-allow*`, an image to `--image-allow`, and
    /// an image outside every entry is refused `image-not-allowed`); the start names
    /// another platform than darwin/arm64; the store does not hold the pin (a manifest
    /// digest, or the digest of an index that resolved to one), and `--bundle-pull never`,
    /// or a fetch cannot begin, or has begun; the bundle needs a newer macOS; the bundle
    /// needs an Xcode the host does not have; the program names an entrypoint the bundle
    /// lacks. A list file that cannot be read refuses, so a broken deny list never lets a
    /// digest through. Nothing here asks a registry anything but the fetch that `missing`
    /// begins, after the name has passed every check.
    ///
    /// # Errors
    ///
    /// The wire code of the refusal and an explanation for the log.
    pub fn admit(&self, spec: &JobSpec) -> Result<(), Refusal> {
        let (name, origin) = match (&spec.bundle, &spec.native, &spec.run) {
            (Some(pin), _, _) => (pin, Origin::Bundle),
            (None, None, Some(_)) => (&spec.image, Origin::Image),
            _ => return Ok(()),
        };
        let pin = BundleRef::parse(name).map_err(|_| match origin {
            Origin::Bundle => no(refusal::INVALID_BUNDLE, "not a pinned bundle"),
            Origin::Image => no(refusal::INVALID_IMAGE, "not a pinned image"),
        })?;
        let digest = pin.digest();
        self.deny(digest)?;
        self.allow(digest)?;
        match origin {
            Origin::Bundle => self.registry(pin.name())?,
            Origin::Image => self.image_allowed(pin.name())?,
        }
        check_platform(spec, &MAC)
            .map_err(|why| no(refusal::PLATFORM_MISMATCH, why.to_string()))?;
        let manifest = self.host.resolve(&self.settings.store, digest);
        if manifest != digest {
            self.deny(&manifest)?;
        }
        let held = self.host.held(&self.settings.store);
        let held = held.map_err(|why| {
            no(
                refusal::BUNDLE_NOT_HELD,
                format!("the store cannot be listed: {why}"),
            )
        })?;
        if !held.iter().any(|have| *have == manifest) {
            return self.missing(pin.name(), digest);
        }
        let needs = self.fits_host(&manifest)?;
        entrypoint(spec, &needs)
    }

    /// Reads the list file `path`; one that cannot be read or parsed refuses with `code`.
    fn digests(
        &self,
        path: &std::path::Path,
        code: &'static str,
        what: &str,
    ) -> Result<std::collections::BTreeSet<String>, Refusal> {
        let text = self
            .host
            .read_file(path)
            .map_err(|why| no(code, format!("the {what} cannot be read: {why}")))?;
        parse_digests(&text).map_err(|why| no(code, format!("the {what} is malformed: {why}")))
    }

    fn deny(&self, digest: &str) -> Result<(), Refusal> {
        let Some(file) = &self.settings.deny_file else {
            return Ok(());
        };
        let denied = self.digests(file, refusal::BUNDLE_DENIED, "deny list")?;
        match denied.contains(digest) {
            true => Err(no(refusal::BUNDLE_DENIED, "the digest is on the deny list")),
            false => Ok(()),
        }
    }

    fn allow(&self, digest: &str) -> Result<(), Refusal> {
        let (Trust::List, Some(file)) = (self.settings.trust, &self.settings.allow_file) else {
            return Ok(());
        };
        let allowed = self.digests(file, refusal::BUNDLE_NOT_ALLOWED, "allow list")?;
        match allowed.contains(digest) {
            true => Ok(()),
            false => Err(no(
                refusal::BUNDLE_NOT_ALLOWED,
                "the digest is not on the allow list",
            )),
        }
    }

    fn registry(&self, name: &str) -> Result<(), Refusal> {
        let registries = &self.settings.registries;
        if registries.is_empty() || registries.iter().any(|allowed| allowed.covers(name)) {
            return Ok(());
        }
        Err(no(
            refusal::BUNDLE_REGISTRY_NOT_ALLOWED,
            "the bundle's name is outside every allowed registry and namespace",
        ))
    }

    /// The image's name is under an `--image-allow` entry, by whole path components. A name
    /// the registry grammar cannot parse is under none.
    fn image_allowed(&self, name: &str) -> Result<(), Refusal> {
        let repository = match name.rsplit_once(':') {
            Some((before, tag)) if !tag.contains('/') => before,
            _ => name,
        };
        let allowed = Location::parse(repository).is_ok_and(|location| {
            self.settings
                .image_allow
                .iter()
                .any(|entry| entry.admits(&location))
        });
        match allowed {
            true => Ok(()),
            false => Err(no(
                refusal::IMAGE_NOT_ALLOWED,
                "the image's name is outside every --image-allow entry",
            )),
        }
    }

    /// The store does not hold the bundle: refuse, or begin a fetch and say so.
    ///
    /// Order requirement for a real fetcher: [`Fetch::Started`] begins a download, and the
    /// lifecycle runs this before its capacity checks, so a start refused afterwards for
    /// capacity would repeat the fetch when the farm requeues it. The fetcher must be
    /// idempotent per digest (a second call joins the first), or the fetch must move after
    /// those checks.
    fn missing(&self, name: &str, digest: &str) -> Result<(), Refusal> {
        if self.settings.pull == Pull::Never {
            return Err(no(
                refusal::BUNDLE_NOT_HELD,
                "the store does not hold the bundle",
            ));
        }
        let (store, low_water) = (&self.settings.store, self.settings.low_water_bytes);
        let used = self.host.used_bytes(store).unwrap_or(u64::MAX);
        let free = self.host.free_bytes(store).unwrap_or(0);
        if used >= self.settings.budget_bytes || free < low_water {
            return Err(no(
                refusal::BUNDLE_NO_SPACE,
                "the store is at its budget or the volume is under its low-water mark",
            ));
        }
        match self.host.fetch(store, name, digest) {
            Fetch::Started => Err(no(refusal::BUNDLE_PENDING, "the bundle is being fetched")),
            Fetch::Index => Err(no(
                refusal::BUNDLE_INDEX,
                "the index's darwin/arm64 entry is itself an index",
            )),
            Fetch::Failed(why) => Err(no(refusal::BUNDLE_FETCH_FAILED, why)),
        }
    }

    /// The held bundle runs on this host: its macOS floor and its Xcode need are met. Gives
    /// what it needs.
    fn fits_host(&self, digest: &str) -> Result<Needs, Refusal> {
        let unread = |why: String| {
            no(
                refusal::BUNDLE_NOT_HELD,
                format!("the bundle's config cannot be read: {why}"),
            )
        };
        let needs = self
            .host
            .needs(&self.settings.store, digest)
            .map_err(unread)?;
        let host_os: DottedVersion = self.host.os_version().map_err(|why| {
            no(
                refusal::BUNDLE_OS_TOO_OLD,
                format!("the host's macOS is unknown: {why}"),
            )
        })?;
        if host_os < needs.os_version {
            return Err(no(
                refusal::BUNDLE_OS_TOO_OLD,
                "the bundle needs a newer macOS",
            ));
        }
        let Some(need) = &needs.xcode else {
            return Ok(needs);
        };
        let met = self
            .host
            .xcodes()
            .iter()
            .any(|have| need.satisfied_by(&have.version, &have.components));
        match met {
            true => Ok(needs),
            false => Err(no(
                refusal::BUNDLE_XCODE_MISMATCH,
                "no Xcode on the host meets the bundle's need",
            )),
        }
    }
}

/// A program of the bundle, `@bundle/<name>`, names an entrypoint the bundle has: asked
/// of the config before anything is created, so a job that names a program the bundle lacks
/// is refused as a request that can never succeed (`invalid-program`) and costs no user.
fn entrypoint(spec: &JobSpec, needs: &Needs) -> Result<(), Refusal> {
    let program = spec
        .native
        .as_ref()
        .map(|native| native.program.as_str())
        .or_else(|| spec.run.as_ref().map(|run| run.program.as_str()));
    match program.and_then(|program| program.strip_prefix(BUNDLE_PROGRAM_PREFIX)) {
        Some(name) if !needs.entrypoints.contains(name) => Err(no(
            refusal::INVALID_PROGRAM,
            format!("the bundle has no entrypoint {name}"),
        )),
        _ => Ok(()),
    }
}
