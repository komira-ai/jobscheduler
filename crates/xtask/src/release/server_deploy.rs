//! `release server-deploy --repo <owner/repo> --version <X.Y.Z or ""> --after-run <run id or "">
//! --dir <dir> [--output <file>] [--gh <program>]`: what
//! `.github/workflows/js-server-deploy.yml` deploys as the service `js-server`, or why it
//! deploys nothing. It runs before any step of that workflow can hold a token that writes, so a
//! refusal here means the deploy service is never called.
//!
//! Which release is chosen as [`super::desired`] chooses it (the same code):
//!
//! - `--version X.Y.Z` (a run by hand): the release `vX.Y.Z`, whichever it is, an older one
//!   included;
//! - `--after-run <id>` (the completion of a run of `release.yml`): when the job `publish` of
//!   that run succeeded, the newest published release of `<owner/repo>`; when it did not,
//!   nothing is deployed: the output is `deploy=false` and the verb succeeds.
//!
//! Exactly one of the two is given; an empty one counts as not given.
//!
//! The release must be a published GitHub release (not a draft, not a pre-release), and then:
//!
//! - it has the asset [`tar_name`] (the amd64 image, [`server_image`]) and the asset
//!   `<that>.sigstore.json` (the attestation bundle of `artifacts.yml`);
//! - the tar, the only asset downloaded into `<dir>`, has an `index.json` with exactly one
//!   manifest, whose platform is `linux/amd64` and whose digest is `sha256:` and 64 lowercase
//!   hex digits. That digest is what the image is deployed by.
//!
//! Every problem found is reported, one line each. When there is none, `--output` (on Actions,
//! `GITHUB_OUTPUT`) gets `deploy=true`, `version=<X.Y.Z>`, `image-digest=<sha256:...>`,
//! `image-url=<https URL of the tar>` and `bundle-url=<https URL of the bundle>`; both URLs
//! are release downloads of `<owner/repo>`.

use std::io::Read;
use std::path::PathBuf;

use serde_json::Value;

use super::desired::{Error, PUBLISH_JOB, append_output, array, assets, capture, chosen, strings};
use super::server_image::{Target, file_name};
use super::{Version, io_error};

#[cfg(test)]
mod tests;

/// The most of `index.json` that is read; the image's own is a few hundred bytes.
const INDEX_LIMIT: u64 = 1 << 20;

/// The arguments of `release server-deploy`.
#[derive(clap::Args)]
pub struct ServerDeployArgs {
    /// `owner/name` of the repository whose release is deployed.
    #[arg(long)]
    pub repo: String,
    /// A released version, `X.Y.Z`; empty when `--after-run` is given.
    #[arg(long)]
    pub version: String,
    /// The id of a run of `release.yml`; empty when `--version` is given.
    #[arg(long)]
    pub after_run: String,
    /// Where the image tar is downloaded.
    #[arg(long)]
    pub dir: PathBuf,
    /// The file the outputs are appended to (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The GitHub CLI.
    #[arg(long, default_value = "gh")]
    pub gh: String,
}

/// The name of the amd64 image asset of `version`.
#[must_use]
pub fn tar_name(version: Version) -> String {
    file_name(version, Target::X86_64)
}

/// The name of the attestation bundle of the asset `asset`.
fn bundle_name(asset: &str) -> String {
    format!("{asset}.sigstore.json")
}

/// The bytes of `index.json` in the tar `file` (`None` when it has none).
fn read_index(file: std::fs::File) -> std::io::Result<Option<Vec<u8>>> {
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == std::path::Path::new("index.json") {
            let mut bytes = Vec::new();
            entry.by_ref().take(INDEX_LIMIT).read_to_end(&mut bytes)?;
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

/// Whether `digest` is `sha256:` and 64 lowercase hex digits.
fn is_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The digest of the one manifest of the `index.json` text `bytes` of the tar `name`, or the
/// problems found.
fn manifest_digest(name: &str, bytes: &[u8]) -> Result<String, Vec<String>> {
    let index: Value = serde_json::from_slice(bytes)
        .map_err(|e| vec![format!("index.json of {name} is not JSON: {e}")])?;
    let manifests = array(&index["manifests"]);
    let [manifest] = manifests else {
        return Err(vec![format!(
            "index.json of {name} lists {} manifests, not exactly one",
            manifests.len()
        )]);
    };
    let mut problems = Vec::new();
    let platform = |key: &str| manifest["platform"][key].as_str().unwrap_or("(none)");
    if platform("os") != "linux" || platform("architecture") != "amd64" {
        problems.push(format!(
            "the manifest of {name} has platform {}/{}, not linux/amd64",
            platform("os"),
            platform("architecture")
        ));
    }
    let digest = manifest["digest"].as_str().unwrap_or("");
    if !is_digest(digest) {
        problems.push(format!(
            "the manifest of {name} has digest {digest:?}, not sha256: and 64 lowercase hex digits"
        ));
    }
    if problems.is_empty() {
        Ok(digest.to_owned())
    } else {
        Err(problems)
    }
}

/// Runs `release server-deploy`: the lines to print, joined by newlines.
///
/// # Errors
/// [`Error::Usage`] for neither or both of `--version` and `--after-run`, a `--version` that is
/// not `<major>.<minor>.<patch>` or an `--after-run` that is not digits; [`Error::Failed`] for a
/// run GitHub lists in more than one page, a repository with no published release or a release
/// that is a draft or a pre-release; [`Error::Refused`], with a line per problem, for a release
/// without the image tar or its bundle, a file that is not a tar with an `index.json`, or an
/// `index.json` that does not name exactly one linux/amd64 manifest by a sha256 digest;
/// [`Error::Release`] for a program that does not run or fails and a file that cannot be read or
/// written.
pub fn run(args: &ServerDeployArgs) -> Result<String, Error> {
    let Some(version) = chosen(&args.gh, &args.repo, &args.version, &args.after_run)? else {
        let lines = [
            format!(
                "run {} of {} did not publish a release ({PUBLISH_JOB} did not succeed): nothing is deployed",
                args.after_run, args.repo
            ),
            "deploy=false".to_owned(),
        ];
        if let Some(output) = &args.output {
            append_output(output, &lines[1..])?;
        }
        return Ok(lines.join("\n"));
    };
    let tag = version.tag();
    let tar = tar_name(version);
    let bundle = bundle_name(&tar);
    let listed = assets(&args.gh, &args.repo, &tag)?;
    let problems: Vec<String> = [&tar, &bundle]
        .into_iter()
        .filter(|name| !listed.contains(name))
        .map(|name| format!("release {tag} has no {name}"))
        .collect();
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    std::fs::create_dir_all(&args.dir).map_err(|e| io_error(&args.dir, &e))?;
    let mut download = strings(&["release", "download", &tag, "--repo", &args.repo, "--dir"]);
    download.extend([
        args.dir.display().to_string(),
        "--pattern".to_owned(),
        tar.clone(),
    ]);
    capture(&args.gh, &download)?;
    let path = args.dir.join(&tar);
    let file = std::fs::File::open(&path).map_err(|e| io_error(&path, &e))?;
    let bytes = match read_index(file) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(Error::Refused(vec![format!("{tar} has no index.json")])),
        Err(e) => {
            return Err(Error::Refused(vec![format!(
                "{tar} cannot be read as a tar holding index.json: {e}"
            )]));
        }
    };
    let digest = manifest_digest(&tar, &bytes).map_err(Error::Refused)?;
    let base = format!("https://github.com/{}/releases/download/{tag}", args.repo);
    let lines = [
        "deploy=true".to_owned(),
        format!("version={version}"),
        format!("image-digest={digest}"),
        format!("image-url={base}/{tar}"),
        format!("bundle-url={base}/{bundle}"),
    ];
    if let Some(output) = &args.output {
        append_output(output, &lines)?;
    }
    let mut printed = vec![format!(
        "release {tag}: {tar} holds one linux/amd64 image, {digest}, and {bundle} is attached"
    )];
    printed.extend(lines);
    Ok(printed.join("\n"))
}
