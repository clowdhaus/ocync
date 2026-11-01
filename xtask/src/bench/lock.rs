//! The corpus lock: every (source, tag) the benchmark syncs, pinned to the
//! manifest digest it resolved to, so runs on different dates sync the same
//! content. Tools copy each tag from its locked digest, so a source tag that
//! moves after the lock changes nothing; only a digest the source stops serving
//! stops a run.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::Path;

use clap::Args;
use ocync_distribution::Digest;
use ocync_distribution::sha256::Sha256;
use ocync_sync::engine::ManifestRef;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};

use crate::bench::corpus::{self, CorpusPaths, DockerHubAuth, ImageEntry};
use crate::bench::registry::{self, Access, Repository};

/// The digest of `bytes`.
fn digest_of(bytes: &[u8]) -> Digest {
    Digest::from_sha256(Sha256::digest(bytes))
}

/// Parse a digest a registry sent, ignoring surrounding whitespace.
pub(crate) fn parse_digest(raw: &str) -> Result<Digest, String> {
    raw.trim()
        .parse()
        .map_err(|e: ocync_distribution::Error| e.to_string())
}

/// A source image and one tag of it, as the corpus names them.
///
/// Written in the lock as `source:tag`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct ImageRef {
    /// Source repository, e.g. `quay.io/jupyter/base-notebook`.
    pub(crate) source: String,
    /// Tag of that repository.
    pub(crate) tag: String,
}

impl ImageRef {
    /// Parse `source:tag`, splitting at the last `:`. Refuses an empty part,
    /// and a tag holding `/`, which means the last `:` was a registry port.
    fn parse(raw: &str) -> Result<Self, String> {
        let (source, tag) = raw
            .rsplit_once(':')
            .ok_or_else(|| format!("image ref {raw:?} has no \":<tag>\""))?;
        if source.is_empty() || tag.is_empty() || tag.contains('/') {
            return Err(format!(
                "image ref {raw:?} is not <source>:<tag> with a non-empty source and a tag holding no \"/\""
            ));
        }
        Ok(Self {
            source: source.to_owned(),
            tag: tag.to_owned(),
        })
    }
}

impl TryFrom<String> for ImageRef {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, String> {
        Self::parse(&raw)
    }
}

impl From<ImageRef> for String {
    fn from(image: ImageRef) -> String {
        image.to_string()
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source, self.tag)
    }
}

/// Every (source, tag) in `entries`.
pub(crate) fn refs_of<'a>(entries: impl IntoIterator<Item = &'a ImageEntry>) -> BTreeSet<ImageRef> {
    entries
        .into_iter()
        .flat_map(|entry| {
            entry.tags.iter().map(|tag| ImageRef {
                source: entry.source.clone(),
                tag: tag.clone(),
            })
        })
        .collect()
}

/// When a lock was written: a UTC timestamp in the run timestamp format,
/// `YYYY-MM-DD-HHMMSS`. Runs recorded under different values are different
/// comparison series.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct LockedAt(String);

impl LockedAt {
    /// Accept the run timestamp's shape: 17 characters, digits with dashes
    /// after the year, month and day.
    fn parse(raw: &str) -> Result<Self, String> {
        crate::bench::RunTimestamp::parse(raw).map(|at| Self(at.to_string()))
    }

    /// The current time.
    fn now() -> Self {
        Self(crate::bench::RunTimestamp::now().to_string())
    }
}

impl TryFrom<String> for LockedAt {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, String> {
        Self::parse(&raw)
    }
}

impl From<LockedAt> for String {
    fn from(at: LockedAt) -> String {
        at.0
    }
}

impl fmt::Display for LockedAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a run's tools synced each corpus tag from. Runs that differ here are
/// different comparison series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncFrom {
    /// The digest the lock pins the tag to, whatever the tag names now.
    Digest,
}

impl SyncFrom {
    /// What a run synced from when it synced the corpus locked at `locked_at`;
    /// `None` for a run that synced no locked corpus.
    pub(crate) fn of(locked_at: Option<&LockedAt>) -> Option<Self> {
        locked_at.map(|_| Self::Digest)
    }
}

/// What a tag resolved to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    /// Digest of the manifest the tag names.
    pub(crate) digest: Digest,
    /// The index's child manifest digests in order; empty for a single manifest.
    pub(crate) children: Vec<Digest>,
    /// Config and layer blob digests of the manifest, or of every child of the
    /// index, deduplicated and sorted. Digests are content addresses, so they
    /// stay right for as long as the ref's digest does.
    pub(crate) blobs: Vec<Digest>,
}

/// The lock file: when it was written and what each ref resolved to.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lock {
    /// When the lock was written.
    pub(crate) locked_at: LockedAt,
    /// Resolution of every ref.
    pub(crate) images: BTreeMap<ImageRef, Entry>,
}

impl Lock {
    /// Read and parse the lock at `path`.
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let shown = path.display();
        let raw = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "failed to read corpus lock {shown}: {e}; create it with `cargo xtask bench-lock`"
            )
        })?;
        Self::parse(&raw).map_err(|e| format!("corpus lock {shown}: {e}"))
    }

    /// What the lock records for `tag` of `source`, or the reason it records
    /// nothing.
    pub(crate) fn entry(&self, source: &str, tag: &str) -> Result<&Entry, String> {
        let image = ImageRef {
            source: source.to_owned(),
            tag: tag.to_owned(),
        };
        self.images
            .get(&image)
            .ok_or_else(|| format!("{image}: not in the lock"))
    }

    /// Parse the text of a lock file.
    fn parse(raw: &str) -> Result<Self, String> {
        #[expect(
            clippy::disallowed_methods,
            reason = "a lock file the bench wrote, holding digests and tags"
        )]
        let lock: Self = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        // Every image has a config, so an empty list is a malformed entry and
        // not a ref with nothing to warm.
        if let Some(image) = lock.images.iter().find(|(_, e)| e.blobs.is_empty()) {
            return Err(format!("{} lists no blobs", image.0));
        }
        Ok(lock)
    }

    /// The lock as the text written to disk.
    fn render(&self) -> Result<String, String> {
        let mut text =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize lock: {e}"))?;
        text.push('\n');
        Ok(text)
    }
}

/// Why one ref could not be resolved.
#[derive(Clone)]
struct FetchError {
    /// The registry's answer, when it gave one.
    status: Option<StatusCode>,
    /// What went wrong.
    reason: String,
}

impl From<String> for FetchError {
    fn from(reason: String) -> Self {
        Self {
            status: None,
            reason,
        }
    }
}

/// A line for each ref that could not be resolved.
fn unresolved(failed: &[(ImageRef, FetchError)]) -> String {
    let lines: Vec<String> = failed
        .iter()
        .map(|(image, e)| format!("  {image}: {}", e.reason))
        .collect();
    format!(
        "could not resolve {} source ref(s):\n{}",
        failed.len(),
        lines.join("\n")
    )
}

/// Why the locked digests are not all confirmed served.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NotServed {
    /// The lock lacks a ref, or the source no longer serves a locked digest.
    Gone(String),
    /// Every digest checked is served, but some could not be checked after
    /// retries.
    Incomplete(String),
}

impl fmt::Display for NotServed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone(text) | Self::Incomplete(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for NotServed {}

/// Judge the refs that could not be served. A digest answered 404, 403, or 401
/// after a new token is no longer served, as for a deleted repository; any other
/// failure only leaves the check incomplete. `missing` are the lock's reasons
/// for the refs it lacks.
fn judge_served(missing: Vec<String>, failed: &[(ImageRef, FetchError)]) -> Result<(), NotServed> {
    let (gone, unreachable): (Vec<_>, Vec<_>) = failed.iter().cloned().partition(|(_, e)| {
        matches!(
            e.status,
            Some(StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        )
    });
    let incomplete = (!unreachable.is_empty()).then(|| {
        format!(
            "{}\nUse --skip-registries <host> to leave a registry out, or retry later.",
            unresolved(&unreachable)
        )
    });
    let lines: Vec<String> =
        missing
            .into_iter()
            .chain(gone.iter().map(|(image, e)| {
                format!("{image}: {} (no longer served by the source)", e.reason)
            }))
            .map(|line| format!("  {line}"))
            .collect();
    if lines.is_empty() {
        return incomplete.map_or(Ok(()), |text| Err(NotServed::Incomplete(text)));
    }
    let mut text = format!(
        "{} source ref(s) cannot be copied from the lock:\n{}\nRun `cargo xtask bench-lock` to \
         lock the current content. A new lock starts a new comparison series: do not read its \
         runs against runs under the old one.",
        lines.len(),
        lines.join("\n")
    );
    if let Some(incomplete) = incomplete {
        text.push_str("\nAlso, ");
        text.push_str(&incomplete);
    }
    Err(NotServed::Gone(text))
}

/// The reason a scenario failed, led by a locked digest the source stopped
/// serving when re-checking shows one did: that is not the tool's fault.
pub(crate) fn explain(error: String, recheck: Result<(), NotServed>) -> String {
    match recheck {
        Err(NotServed::Gone(text)) => {
            format!(
                "a locked source digest stopped being served during the scenario: {text}\n{error}"
            )
        }
        _ => error,
    }
}

/// Check that the source still serves the locked manifest digest of every ref
/// in `refs`, each by a HEAD of that digest and never of its tag. A tag that
/// has moved is not a difference: the run copies the locked digest.
pub(crate) async fn ensure_served(
    client: &reqwest::Client,
    lock: &Lock,
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
) -> Result<(), NotServed> {
    let access = Access::new(client, auth);
    check_served(&access, lock, refs, async |repo, digest| {
        access.head_digest(repo, digest).await
    })
    .await
}

/// [`ensure_served`] with the request `head` makes for one locked digest. A ref
/// the lock lacks is refused before any request goes out. Otherwise one request
/// goes out per distinct repository and digest, and a failure is reported for
/// every tag that names it.
async fn check_served(
    access: &Access<'_>,
    lock: &Lock,
    refs: &BTreeSet<ImageRef>,
    head: impl AsyncFn(&Repository<'_>, &Digest) -> Result<(), FetchError>,
) -> Result<(), NotServed> {
    let mut missing = Vec::new();
    let mut sharing: HashMap<(&str, &Digest), Vec<&ImageRef>> = HashMap::new();
    for image in refs {
        match lock.entry(&image.source, &image.tag) {
            Ok(entry) => sharing
                .entry((&image.source, &entry.digest))
                .or_default()
                .push(image),
            Err(reason) => missing.push(reason),
        }
    }
    if !missing.is_empty() {
        return judge_served(missing, &[]);
    }
    let probes: BTreeSet<ImageRef> = sharing.values().map(|tags| tags[0].clone()).collect();
    eprintln!(
        "bench: checking {} locked source digests are still served...",
        probes.len()
    );
    let (_, failed) = access
        .resolve(&probes, async |_, repo, image| {
            let digest = &lock.images[image].digest;
            head(repo, digest).await.map_err(|e| FetchError {
                reason: format!("digest {digest}: {}", e.reason),
                ..e
            })
        })
        .await;
    let failed: Vec<(ImageRef, FetchError)> = failed
        .iter()
        .flat_map(|(probe, e)| {
            sharing[&(probe.source.as_str(), &lock.images[probe].digest)]
                .iter()
                .map(move |image| ((*image).clone(), e.clone()))
        })
        .collect();
    judge_served(Vec::new(), &failed)
}

/// The digest of a manifest body. A registry that also reports a digest must
/// agree with the bytes, or the lock would record something no copy can match.
fn body_digest(header: Option<&str>, body: &[u8]) -> Result<Digest, String> {
    let actual = digest_of(body);
    if let Some(value) = header {
        let reported = parse_digest(value)?;
        if reported != actual {
            return Err(format!(
                "registry reports digest {reported} but the manifest body hashes to {actual}"
            ));
        }
    }
    Ok(actual)
}

/// A descriptor: one entry of an index's `manifests`, or a config or layer.
#[derive(Deserialize)]
struct Descriptor {
    /// Digest of the manifest or blob it names.
    digest: Digest,
}

/// A manifest body, read only for the digests it lists.
#[derive(Deserialize)]
#[serde(untagged)]
enum ManifestBody {
    /// An index: the manifests it lists.
    Index {
        /// Child descriptors, in order.
        manifests: Vec<Descriptor>,
    },
    /// An image: its config and layers.
    Image {
        /// The image config.
        config: Descriptor,
        /// The layers, in order.
        layers: Vec<Descriptor>,
    },
}

impl ManifestBody {
    /// Parse a body. Neither an index nor an image, a digest that is not one,
    /// or an index listing nothing is an error.
    fn parse(body: &[u8]) -> Result<Self, String> {
        #[expect(
            clippy::disallowed_methods,
            reason = "a public manifest holding no credential"
        )]
        let parsed: Self = serde_json::from_slice(body)
            .map_err(|e| format!("manifest is neither a valid index nor a valid image: {e}"))?;
        match parsed {
            Self::Index { manifests } if manifests.is_empty() => {
                Err("index lists no manifests".to_owned())
            }
            parsed => Ok(parsed),
        }
    }

    /// The config and layer digests of an image, config first. An index has
    /// none of its own.
    fn into_blobs(self) -> Result<Vec<Digest>, String> {
        match self {
            Self::Image { config, layers } => Ok(std::iter::once(config)
                .chain(layers)
                .map(|d| d.digest)
                .collect()),
            Self::Index { .. } => Err("is an index, not an image".to_owned()),
        }
    }
}

/// The blobs of the child manifest `listed` by its index, read as `body`. The
/// body must hash to `listed`, or the lock would record another image's blobs;
/// and a child that is itself an index is refused.
fn child_blobs(listed: &Digest, actual: &Digest, body: &[u8]) -> Result<Vec<Digest>, String> {
    if actual != listed {
        return Err(format!("body hashes to {actual}"));
    }
    ManifestBody::parse(body)?.into_blobs()
}

/// The manifest media types a source image can be stored as: OCI image,
/// Docker v2 image, OCI index and Docker manifest list.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json,\
     application/vnd.docker.distribution.manifest.v2+json,\
     application/vnd.oci.image.index.v1+json,\
     application/vnd.docker.distribution.manifest.list.v2+json";

/// How `repo` names `reference` in a request notice: `repo:tag` or
/// `repo@digest`.
fn manifest_label(repo: &Repository<'_>, reference: &ManifestRef) -> String {
    format!("{repo}{}{reference}", reference.separator())
}

/// What the lock reads through [`Access`].
impl Access<'_> {
    /// Request a manifest, retrying what [`Access::send`] retries and renewing
    /// an expired token once. Any other failing status is reported at once,
    /// with the status.
    async fn request_manifest(
        &self,
        method: Method,
        repo: &Repository<'_>,
        reference: &ManifestRef,
    ) -> Result<reqwest::Response, FetchError> {
        let url = repo.url(&format!("manifests/{reference}"));
        let response = self
            .send(repo, &manifest_label(repo, reference), |client| {
                client
                    .request(method.clone(), &url)
                    .header("Accept", MANIFEST_ACCEPT)
            })
            .await
            .map_err(|e| format!("manifest request {e}"))?;
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            Err(FetchError {
                status: Some(status),
                reason: format!("manifest request returned {status}"),
            })
        }
    }

    /// Resolve every ref with `fetch`. Returns what resolved and each ref that
    /// did not, so the caller decides what an incomplete answer means.
    async fn resolve<T>(
        &self,
        refs: &BTreeSet<ImageRef>,
        fetch: impl AsyncFn(&Self, &Repository<'_>, &ImageRef) -> Result<T, FetchError>,
    ) -> (BTreeMap<ImageRef, T>, Vec<(ImageRef, FetchError)>) {
        let mut resolved = BTreeMap::new();
        let mut failed = Vec::new();
        for image in refs {
            let outcome = match registry::split_source(&image.source) {
                Some(repo) => fetch(self, &repo, image).await,
                None => Err("source names no registry".to_owned().into()),
            };
            match outcome {
                Ok(found) => {
                    resolved.insert(image.clone(), found);
                }
                Err(e) => failed.push((image.clone(), e)),
            }
        }
        (resolved, failed)
    }

    /// GET a manifest, hash its exact bytes and read them with `read`.
    async fn get_manifest<T>(
        &self,
        repo: &Repository<'_>,
        reference: &ManifestRef,
        read: impl FnOnce(Digest, &[u8]) -> Result<T, String>,
    ) -> Result<T, FetchError> {
        let response = self.request_manifest(Method::GET, repo, reference).await?;
        let header = content_digest(&response);
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("manifest body unreadable: {e}"))?;
        Ok(read(body_digest(header.as_deref(), &body)?, &body)?)
    }

    /// HEAD the manifest `digest`, which succeeds when the repository serves it.
    async fn head_digest(&self, repo: &Repository<'_>, digest: &Digest) -> Result<(), FetchError> {
        self.request_manifest(Method::HEAD, repo, &ManifestRef::Digest(digest.clone()))
            .await
            .map(drop)
    }

    /// The digest, children and blobs of a tag, from a GET of its exact bytes
    /// and of each child's.
    async fn fetch_entry(&self, repo: &Repository<'_>, tag: &str) -> Result<Entry, FetchError> {
        let (digest, manifest) = self
            .get_manifest(repo, &ManifestRef::Tag(tag.to_owned()), |digest, body| {
                Ok((digest, ManifestBody::parse(body)?))
            })
            .await?;
        let (children, mut blobs) = match manifest {
            ManifestBody::Index { manifests } => {
                let children: Vec<Digest> = manifests.into_iter().map(|d| d.digest).collect();
                let mut blobs = Vec::new();
                for child in &children {
                    let found = self
                        .get_manifest(repo, &ManifestRef::Digest(child.clone()), |actual, body| {
                            child_blobs(child, &actual, body)
                        })
                        .await
                        .map_err(|e| FetchError {
                            reason: format!("child {child}: {}", e.reason),
                            ..e
                        })?;
                    blobs.extend(found);
                }
                (children, blobs)
            }
            image => (Vec::new(), image.into_blobs()?),
        };
        blobs.sort_by_cached_key(ToString::to_string);
        blobs.dedup();
        Ok(Entry {
            digest,
            children,
            blobs,
        })
    }
}

/// The `Docker-Content-Digest` header of a response, if any.
fn content_digest(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Arguments for the `bench-lock` subcommand.
#[derive(Args)]
pub(crate) struct LockArgs {
    #[command(flatten)]
    paths: CorpusPaths,
}

/// The lock to write, or `None` when the images equal the existing lock's, so
/// re-running over unchanged sources leaves the file, and its `locked_at`
/// series, alone.
fn relock(
    existing: Option<&Lock>,
    images: BTreeMap<ImageRef, Entry>,
    now: LockedAt,
) -> Option<Lock> {
    if existing.is_some_and(|lock| lock.images == images) {
        return None;
    }
    Some(Lock {
        locked_at: now,
        images,
    })
}

/// Resolve every ref the corpus and the partial overrides sync, and write the
/// lock beside the corpus. Needs the network and nothing else: no target
/// registry, no cloud write.
pub(crate) async fn run(args: LockArgs) -> Result<(), Box<dyn std::error::Error>> {
    let entries = corpus::load_images(args.paths.corpus())?;
    let overrides = corpus::load_overrides(args.paths.partial_overrides())?;

    // An override for a source outside the corpus is ignored by the run.
    let mut refs = refs_of(&entries);
    refs.extend(refs_of(
        overrides
            .overrides
            .iter()
            .filter(|o| entries.iter().any(|e| e.source == o.source)),
    ));

    let auth = crate::bench::docker_hub_auth();
    eprintln!("bench-lock: resolving {} source refs...", refs.len());
    let client = registry::http_client()?;
    let access = Access::new(&client, auth.as_ref());
    let (images, failed) = access
        .resolve(&refs, async |access, repo, image| {
            access.fetch_entry(repo, &image.tag).await
        })
        .await;
    if !failed.is_empty() {
        return Err(unresolved(&failed).into());
    }

    let path = args.paths.lock_path();
    let existing = match Lock::load(&path) {
        Ok(lock) => Some(lock),
        Err(e) => {
            eprintln!("bench-lock: no usable existing lock ({e})");
            None
        }
    };
    match relock(existing.as_ref(), images, LockedAt::now()) {
        None => eprintln!(
            "bench-lock: {} is already current ({} refs); left unchanged",
            path.display(),
            existing.map_or(0, |lock| lock.images.len())
        ),
        Some(lock) => {
            std::fs::write(&path, lock.render()?)
                .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
            eprintln!(
                "bench-lock: wrote {} refs to {} (locked_at {})",
                lock.images.len(),
                path.display(),
                lock.locked_at
            );
        }
    }
    Ok(())
}

/// Digests, lock entries and timestamps for tests here and in the modules that
/// check against a lock.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::{Digest, Entry, ImageRef, Lock, LockedAt, parse_digest};

    /// A digest whose 64 hex characters are all `c`.
    pub(crate) fn digest(c: char) -> Digest {
        parse_digest(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    /// An entry for `digest_fill` listing `children`, each named by its fill
    /// character, with no blobs.
    pub(crate) fn entry(digest_fill: char, children: &[char]) -> Entry {
        Entry {
            digest: digest(digest_fill),
            children: children.iter().map(|c| digest(*c)).collect(),
            blobs: Vec::new(),
        }
    }

    /// A lock written at a fixed time holding `images`, each named `source:tag`.
    pub(crate) fn lock_of(images: &[(&str, Entry)]) -> Lock {
        Lock {
            locked_at: locked_at("2026-10-30-120000"),
            images: images
                .iter()
                .map(|(k, v)| (ImageRef::parse(k).unwrap(), v.clone()))
                .collect(),
        }
    }

    /// A lock time.
    pub(crate) fn locked_at(raw: &str) -> LockedAt {
        LockedAt::parse(raw).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{digest as d, entry, lock_of, locked_at};
    use super::*;

    fn image(raw: &str) -> ImageRef {
        ImageRef::parse(raw).unwrap()
    }

    const ABC_SHA256: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn locked_at_accepts_only_the_run_timestamp_shape() {
        let cases = [
            ("2026-10-30-214340", true),
            ("2026-10-30-2143", false),
            ("2026-10-30T214340", false),
            ("20261030-214340x", false),
            ("2026-10-3a-214340", false),
        ];
        for (raw, accepted) in cases {
            assert_eq!(LockedAt::parse(raw).is_ok(), accepted, "{raw}");
        }
        assert!(
            LockedAt::parse(&crate::bench::RunTimestamp::now().to_string()).is_ok(),
            "what the run stamps must be what the lock accepts"
        );
    }

    #[test]
    fn body_digest_hashes_the_body_and_refuses_a_header_that_disagrees() {
        let cases: [(Option<&str>, Option<&str>); 4] = [
            (None, Some(ABC_SHA256)),
            (Some(ABC_SHA256), Some(ABC_SHA256)),
            (
                Some(" sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n"),
                Some(ABC_SHA256),
            ),
            (
                Some("sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
                None,
            ),
        ];
        for (header, want) in cases {
            let got = body_digest(header, b"abc").ok().map(|d| d.to_string());
            assert_eq!(got.as_deref(), want, "header {header:?}");
        }
    }

    #[test]
    fn image_ref_splits_at_the_last_colon_and_refuses_what_is_not_a_tag() {
        let cases: [(&str, Option<(&str, &str)>); 6] = [
            (
                "quay.io/jupyter/base-notebook:2026-10-28",
                Some(("quay.io/jupyter/base-notebook", "2026-10-28")),
            ),
            ("localhost:5000/app:v1", Some(("localhost:5000/app", "v1"))),
            ("localhost:5000/app", None),
            ("quay.io/app", None),
            (":latest", None),
            ("quay.io/app:", None),
        ];
        for (raw, want) in cases {
            let got = ImageRef::parse(raw).ok().map(|r| (r.source, r.tag));
            let want = want.map(|(s, t)| (s.to_owned(), t.to_owned()));
            assert_eq!(got, want, "{raw}");
        }
    }

    /// A JSON array of `digests`, each as `{"digest": ...}`.
    fn descriptors(digests: &[char]) -> String {
        let listed: Vec<String> = digests
            .iter()
            .map(|c| format!(r#"{{"digest":"{}"}}"#, d(*c)))
            .collect();
        format!("[{}]", listed.join(","))
    }

    fn image_body(config: char, layers: &[char]) -> String {
        format!(
            r#"{{"config":{{"digest":"{}"}},"layers":{}}}"#,
            d(config),
            descriptors(layers)
        )
    }

    fn index_body(children: &[char]) -> String {
        format!(r#"{{"manifests":{}}}"#, descriptors(children))
    }

    #[test]
    fn a_manifest_body_is_an_index_or_an_image_and_nothing_else() {
        // (name, body, the kind and digests it reads as; None for an error)
        let no_config = format!(r#"{{"layers":{}}}"#, descriptors(&['1']));
        let no_layers = format!(r#"{{"config":{{"digest":"{}"}}}}"#, d('c'));
        let no_digest = format!(r#"{{"config":{{}},"layers":{}}}"#, descriptors(&['1']));
        let cases = [
            (
                "an image lists its config first",
                image_body('c', &['1', '2']),
                Some(("image", vec![d('c'), d('1'), d('2')])),
            ),
            (
                "an index lists its children in order",
                index_body(&['2', '1']),
                Some(("index", vec![d('2'), d('1')])),
            ),
            ("an index with no children", index_body(&[]), None),
            ("an image with no config", no_config, None),
            ("an image with no layers", no_layers, None),
            ("a config with no digest", no_digest, None),
            (
                "a malformed digest",
                r#"{"config":{"digest":"sha256:xyz"},"layers":[]}"#.to_owned(),
                None,
            ),
            ("not JSON", "<html>".to_owned(), None),
        ];
        for (name, body, expected) in cases {
            let got = ManifestBody::parse(body.as_bytes()).ok().map(|m| match &m {
                ManifestBody::Index { manifests } => (
                    "index",
                    manifests.iter().map(|x| x.digest.clone()).collect(),
                ),
                ManifestBody::Image { .. } => ("image", m.into_blobs().unwrap()),
            });
            assert_eq!(got, expected, "{name}");
        }
    }

    #[test]
    fn a_child_must_hash_to_its_listed_digest_and_be_an_image() {
        // Catches: recording the blobs of a different image than the lock's
        // digest names, or of an index nested under an index.
        let image = image_body('c', &['1']);
        let nested = index_body(&['a']);
        let cases = [
            (
                "matches",
                d('5'),
                d('5'),
                image.as_str(),
                Some(vec![d('c'), d('1')]),
            ),
            (
                "hashes to another digest",
                d('5'),
                d('6'),
                image.as_str(),
                None,
            ),
            ("is an index", d('5'), d('5'), nested.as_str(), None),
        ];
        for (name, listed, actual, body, expected) in cases {
            let got = child_blobs(&listed, &actual, body.as_bytes()).ok();
            assert_eq!(got, expected, "{name}");
        }
    }

    #[test]
    fn lock_round_trips_with_sorted_keys_and_refuses_unknown_fields() {
        let with_blobs = |entry: Entry| Entry {
            blobs: vec![d('5')],
            ..entry
        };
        let lock = lock_of(&[
            ("quay.io/b:latest", with_blobs(entry('1', &['2', '3']))),
            ("docker.io/a:1", with_blobs(entry('4', &[]))),
        ]);
        let text = lock.render().unwrap();
        assert!(
            text.find("docker.io/a:1").unwrap() < text.find("quay.io/b:latest").unwrap(),
            "keys are sorted"
        );
        assert_eq!(Lock::parse(&text).unwrap(), lock);

        let extra_top = text.replacen("\"images\"", "\"extra\": 1, \"images\"", 1);
        assert!(Lock::parse(&extra_top).is_err(), "unknown top-level field");
        let extra_entry = text.replacen("\"children\"", "\"extra\": 1, \"children\"", 1);
        assert!(Lock::parse(&extra_entry).is_err(), "unknown entry field");
        let bad_key = text.replacen("docker.io/a:1", "docker.io/a", 1);
        assert!(Lock::parse(&bad_key).is_err(), "key with no tag");
        let bad_time = text.replacen("2026-10-30-120000", "yesterday", 1);
        assert!(Lock::parse(&bad_time).is_err(), "malformed locked_at");
        // Catches: an entry with no blobs loading, warming nothing and being
        // reported warm.
        let no_blobs = lock_of(&[("quay.io/b:latest", entry('1', &[]))]);
        let error = Lock::parse(&no_blobs.render().unwrap()).unwrap_err();
        assert!(error.contains("quay.io/b:latest lists no blobs"), "{error}");
    }

    /// Run [`check_served`] with a source that serves exactly the digests in
    /// `served` and answers `status` for any other, with a bare reason. Returns
    /// the verdict and the `repo@digest` the source was asked for, in order.
    fn check(
        lock: &Lock,
        refs: &[&str],
        served: &[char],
        status: StatusCode,
    ) -> (Result<(), NotServed>, Vec<String>) {
        let served: Vec<Digest> = served.iter().map(|c| d(*c)).collect();
        let asked = std::cell::RefCell::new(Vec::new());
        let client = registry::http_client().unwrap();
        let access = Access::new(&client, None);
        let refs: BTreeSet<ImageRef> = refs.iter().map(|r| image(r)).collect();
        let verdict = futures_util::FutureExt::now_or_never(check_served(
            &access,
            lock,
            &refs,
            async |repo, digest| {
                asked.borrow_mut().push(format!("{repo}@{digest}"));
                if served.contains(digest) {
                    Ok(())
                } else {
                    Err(FetchError {
                        status: Some(status),
                        reason: format!("manifest request returned {status}"),
                    })
                }
            },
        ))
        .expect("nothing here waits");
        (verdict, asked.into_inner())
    }

    #[test]
    fn the_run_needs_each_locked_digest_served_and_nothing_else() {
        // The source is asked for digests only, so a tag that has since moved is
        // not a difference. Catches: refusing a run, or syncing other content,
        // because a floating tag moved after the lock.
        let lock = lock_of(&[("s/a:1", entry('1', &[])), ("s/b:1", entry('2', &[]))]);
        let both = ["s/a:1", "s/b:1"];
        let (verdict, asked) = check(&lock, &both, &['1', '2'], StatusCode::NOT_FOUND);
        assert_eq!(verdict, Ok(()));
        assert_eq!(
            asked,
            [format!("s/a@{}", d('1')), format!("s/b@{}", d('2'))]
        );

        // Catches: a digest the source dropped being waved through because its
        // tag still resolves, which would fail later in a tool, mid-run; and
        // the digest being left out of the reason.
        let (verdict, _) = check(&lock, &both, &['1'], StatusCode::NOT_FOUND);
        let Err(NotServed::Gone(text)) = verdict else {
            panic!("a dropped digest must stop the run");
        };
        assert!(
            text.contains(&format!(
                "s/b:1: digest {}: manifest request returned 404",
                d('2')
            )),
            "{text}"
        );
        assert!(!text.contains("s/a:1"), "{text}");
        assert!(text.contains("1 source ref(s) cannot be copied from the lock"));
        assert!(text.contains("cargo xtask bench-lock") && text.contains("new comparison series"));
    }

    #[test]
    fn one_head_goes_out_per_repository_and_digest_however_many_tags_name_it() {
        // Catches: a HEAD per tag when alpine's `3`, `3.24` and `latest` are
        // one manifest, and a digest shared by two repositories checked once.
        let lock = lock_of(&[
            ("s/a:1", entry('1', &[])),
            ("s/a:2", entry('1', &[])),
            ("s/b:1", entry('1', &[])),
            ("s/b:2", entry('2', &[])),
        ]);
        let refs = ["s/a:1", "s/a:2", "s/b:1", "s/b:2"];
        let (verdict, asked) = check(&lock, &refs, &['1', '2'], StatusCode::NOT_FOUND);
        assert_eq!(verdict, Ok(()));
        assert_eq!(
            asked,
            [
                format!("s/a@{}", d('1')),
                format!("s/b@{}", d('1')),
                format!("s/b@{}", d('2')),
            ]
        );

        // Catches: a shared digest the source dropped reported for one of its
        // tags, so the count and the list understate what cannot be copied.
        let lock = lock_of(&[
            ("s/a:1", entry('1', &[])),
            ("s/a:2", entry('1', &[])),
            ("s/a:3", entry('1', &[])),
            ("s/b:1", entry('2', &[])),
        ]);
        let refs = ["s/a:1", "s/a:2", "s/a:3", "s/b:1"];
        let (verdict, asked) = check(&lock, &refs, &['2'], StatusCode::NOT_FOUND);
        let Err(NotServed::Gone(text)) = verdict else {
            panic!("a dropped shared digest must stop the run");
        };
        assert!(
            text.contains("3 source ref(s) cannot be copied from the lock"),
            "{text}"
        );
        for tag in ["s/a:1", "s/a:2", "s/a:3"] {
            assert!(
                text.contains(&format!("{tag}: digest {}", d('1'))),
                "{tag}: {text}"
            );
        }
        assert!(!text.contains("s/b:1"), "{text}");
        assert_eq!(asked.len(), 2, "{asked:?}");
    }

    #[test]
    fn a_ref_the_lock_lacks_stops_the_run_before_any_request() {
        // Catches: a ref with no locked digest being run from its tag, and
        // requests sent for a run that is refused anyway.
        let lock = lock_of(&[("s/a:1", entry('1', &[]))]);
        let (verdict, asked) = check(&lock, &["s/a:1", "s/c:1"], &['1'], StatusCode::NOT_FOUND);
        let Err(NotServed::Gone(text)) = verdict else {
            panic!("a ref the lock lacks must stop the run");
        };
        assert!(text.contains("s/c:1: not in the lock"), "{text}");
        assert!(asked.is_empty(), "{asked:?}");
    }

    #[test]
    fn a_digest_answered_as_a_deleted_one_stops_and_an_outage_only_leaves_the_check_incomplete() {
        // Catches: a deleted repository (Docker Hub and quay answer 401) being
        // waved through as an outage, and an outage called a missing digest.
        let lock = lock_of(&[("s/a:1", entry('1', &[]))]);
        for status in [404, 403, 401] {
            let status = StatusCode::from_u16(status).unwrap();
            let (verdict, _) = check(&lock, &["s/a:1"], &[], status);
            let Err(NotServed::Gone(text)) = verdict else {
                panic!("{status} must stop the run");
            };
            assert!(text.contains("no longer served"), "{status}: {text}");
            assert!(!text.contains("--skip-registries"), "{status}: {text}");
        }
        for status in [503, 429] {
            let status = StatusCode::from_u16(status).unwrap();
            let (verdict, _) = check(&lock, &["s/a:1"], &[], status);
            let Err(NotServed::Incomplete(text)) = verdict else {
                panic!("{status} must only leave the check incomplete");
            };
            assert!(text.contains("could not resolve 1 source ref(s)"), "{text}");
            assert!(text.contains("--skip-registries"), "{text}");
            assert!(!text.contains("no longer served"), "{text}");
        }
    }

    #[test]
    fn a_failed_scenario_is_blamed_on_the_source_only_when_it_stopped_serving_a_digest() {
        // Catches: reporting a digest the source dropped mid-run as a tool defect.
        let error = "ocync cold verification failed".to_owned();
        let gone = explain(error.clone(), Err(NotServed::Gone("s/a:1 gone".into())));
        assert!(gone.starts_with("a locked source digest stopped being served"));
        assert!(gone.contains("s/a:1 gone") && gone.ends_with(&error));
        assert_eq!(explain(error.clone(), Ok(())), error);
        assert_eq!(
            explain(error.clone(), Err(NotServed::Incomplete("down".into()))),
            error
        );
    }

    #[test]
    fn requests_for_a_digest_are_labelled_with_an_at_sign_and_for_a_tag_with_a_colon() {
        // Catches: `repo:sha256:...` in retry notices, which reads as a tag.
        let repo = registry::split_source("quay.io/jupyter/base").unwrap();
        assert_eq!(
            manifest_label(&repo, &ManifestRef::Tag("2026-10-28".to_owned())),
            "quay.io/jupyter/base:2026-10-28"
        );
        assert_eq!(
            manifest_label(&repo, &ManifestRef::Digest(d('1'))),
            format!("quay.io/jupyter/base@{}", d('1'))
        );
    }

    #[test]
    fn relock_leaves_unchanged_content_alone_and_stamps_anything_else() {
        let existing = lock_of(&[("s/a:1", entry('1', &[]))]);
        let now = || locked_at("2026-11-01-000000");
        assert_eq!(
            relock(Some(&existing), existing.images.clone(), now()),
            None,
            "unchanged content keeps the file and its locked_at"
        );

        let moved = lock_of(&[("s/a:1", entry('2', &[]))]).images;
        assert_eq!(
            relock(Some(&existing), moved.clone(), now()),
            Some(Lock {
                locked_at: now(),
                images: moved.clone(),
            })
        );
        assert!(relock(None, moved, now()).is_some());
    }
}
