//! The corpus lock: every (source, tag) the benchmark syncs, pinned to the
//! manifest digest it resolved to, so runs on different dates sync the same
//! content and a moved source is refused rather than benchmarked.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use clap::Args;
use ocync_distribution::Digest;
use ocync_distribution::sha256::Sha256;
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
        let shaped = raw.len() == 17
            && raw.bytes().enumerate().all(|(i, b)| match i {
                4 | 7 | 10 => b == b'-',
                _ => b.is_ascii_digit(),
            });
        if shaped {
            Ok(Self(raw.to_owned()))
        } else {
            Err(format!("{raw:?} is not a timestamp like 2026-10-30-214340"))
        }
    }

    /// The current time.
    fn now() -> Self {
        Self(crate::bench::utc_timestamp())
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
        Self::parse(&raw).map_err(|e| {
            format!("corpus lock {shown}: {e}; re-create it with `cargo xtask bench-lock`")
        })
    }

    /// Parse the text of a lock file.
    fn parse(raw: &str) -> Result<Self, String> {
        #[expect(
            clippy::disallowed_methods,
            reason = "a lock file the bench wrote, holding digests and tags"
        )]
        serde_json::from_str(raw).map_err(|e| e.to_string())
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

/// A way a resolved ref differs from the lock.
#[derive(Debug, PartialEq, Eq)]
enum Difference {
    /// The ref is not in the lock.
    Missing { image: ImageRef },
    /// The tag now names a different manifest.
    Moved {
        image: ImageRef,
        expected: Digest,
        actual: Digest,
    },
    /// The registry answers 404, 403, or 401 after a new token, as it does for a
    /// deleted repository.
    Gone { image: ImageRef },
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { image } => write!(f, "{image}: not in the lock"),
            Self::Moved {
                image,
                expected,
                actual,
            } => write!(f, "{image}: expected {expected}, now {actual}"),
            Self::Gone { image } => write!(
                f,
                "{image}: no longer at the source (404, 403, or 401 after a new token)"
            ),
        }
    }
}

/// Why a check against the lock did not pass.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecheckError {
    /// A source differs from the lock.
    Moved(String),
    /// Nothing differs among the refs that resolved, but some could not be
    /// resolved after retries, so the check is incomplete.
    Lookup(String),
}

impl fmt::Display for RecheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Moved(text) | Self::Lookup(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for RecheckError {}

/// Judge the refs that resolved against `lock`. Equal digests mean identical
/// manifest bytes, children included. A tag answered 404, 403, or 401 after a new
/// token, has moved; any other failure only leaves the check incomplete.
fn check_unmoved(
    lock: &Lock,
    resolved: &BTreeMap<ImageRef, Digest>,
    failed: &[(ImageRef, FetchError)],
) -> Result<(), RecheckError> {
    let mut differences = Vec::new();
    for (image, actual) in resolved {
        match lock.images.get(image) {
            None => differences.push(Difference::Missing {
                image: image.clone(),
            }),
            Some(expected) if expected.digest != *actual => differences.push(Difference::Moved {
                image: image.clone(),
                expected: expected.digest.clone(),
                actual: actual.clone(),
            }),
            Some(_) => {}
        }
    }
    let (gone, unreachable): (Vec<_>, Vec<_>) = failed.iter().cloned().partition(|(_, e)| {
        matches!(
            e.status,
            Some(StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        )
    });
    differences.extend(
        gone.into_iter()
            .map(|(image, _)| Difference::Gone { image }),
    );
    let incomplete = (!unreachable.is_empty()).then(|| unresolved(&unreachable));
    if differences.is_empty() {
        return incomplete.map_or(Ok(()), |text| Err(RecheckError::Lookup(text)));
    }
    let listed: Vec<String> = differences.iter().map(|d| format!("  {d}")).collect();
    let mut text = format!(
        "{} source ref(s) differ from the corpus lock, so this run would not sync the corpus \
         it was locked to:\n{}\nRun `cargo xtask bench-lock` to lock the current content. A new \
         lock starts a new comparison series: do not read its runs against runs under the old one.",
        differences.len(),
        listed.join("\n")
    );
    if let Some(incomplete) = incomplete {
        text.push_str("\nAlso, ");
        text.push_str(&incomplete);
    }
    Err(RecheckError::Moved(text))
}

/// The client every lock check in a run shares.
pub(crate) fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))
}

/// Resolve every ref in `refs` again and refuse if any differs from `lock`.
///
/// Probes with HEAD, which costs no body, and falls back to a GET only for a
/// registry that sends no digest header.
pub(crate) async fn ensure_unmoved(
    client: &reqwest::Client,
    lock: &Lock,
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
) -> Result<(), RecheckError> {
    eprintln!(
        "bench: checking {} source refs against the lock...",
        refs.len()
    );
    let access = Access::new(client, auth);
    let (resolved, failed) = access.resolve(refs, Access::fetch_digest).await;
    check_unmoved(lock, &resolved, &failed)
}

/// What a run does after the check between scenarios.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Run the next scenario; the check could not finish, which is warned of.
    Proceed { warning: Option<String> },
    /// Do not run the remaining scenarios, for this reason.
    Stop(String),
}

/// Decide from the between-scenarios check. Only an actual difference stops
/// the run: a lookup that failed after retries leaves the post-pass digest
/// check to guard the content.
pub(crate) fn between_scenarios(check: Result<(), RecheckError>) -> Next {
    match check {
        Ok(()) => Next::Proceed { warning: None },
        Err(RecheckError::Lookup(text)) => Next::Proceed {
            warning: Some(format!(
                "could not re-check the sources before the next scenario, continuing; \
                 the post-pass digest check still guards the content:\n{text}"
            )),
        },
        Err(RecheckError::Moved(text)) => Next::Stop(text),
    }
}

/// The reason a scenario failed, led by the sources having moved when
/// re-resolving its refs shows they did: that is not the tool's fault.
pub(crate) fn explain(error: String, recheck: Result<(), RecheckError>) -> String {
    match recheck {
        Err(RecheckError::Moved(text)) => {
            format!("the source moved during the scenario: {text}\n{error}")
        }
        _ => error,
    }
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

/// What a HEAD answer settles.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// The registry named the digest.
    Header(Digest),
    /// It sent none, so the body must be fetched and hashed.
    NeedBody,
}

/// Read the `Docker-Content-Digest` of a HEAD answer.
fn probe(header: Option<&str>) -> Result<Probe, String> {
    match header {
        Some(value) => parse_digest(value).map(Probe::Header),
        None => Ok(Probe::NeedBody),
    }
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

/// What the lock reads through [`Access`].
impl Access<'_> {
    /// Request a manifest, retrying what [`Access::send`] retries and renewing
    /// an expired token once. Any other failing status is reported at once,
    /// with the status.
    async fn request_manifest(
        &self,
        method: Method,
        repo: &Repository<'_>,
        reference: &str,
    ) -> Result<reqwest::Response, FetchError> {
        let url = format!(
            "https://{}/v2/{}/manifests/{reference}",
            repo.host.api_host(),
            repo.path
        );
        let response = self
            .send(repo, Some(&format!("{repo}:{reference}")), |token| {
                self.request(method.clone(), &url)
                    .bearer_auth(token)
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
        fetch: impl AsyncFn(&Self, &Repository<'_>, &str) -> Result<T, FetchError>,
    ) -> (BTreeMap<ImageRef, T>, Vec<(ImageRef, FetchError)>) {
        let mut resolved = BTreeMap::new();
        let mut failed = Vec::new();
        for image in refs {
            let outcome = match registry::split_source(&image.source) {
                Some(repo) => fetch(self, &repo, &image.tag).await,
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
        tag: &str,
        read: impl FnOnce(Digest, &[u8]) -> Result<T, String>,
    ) -> Result<T, FetchError> {
        let response = self.request_manifest(Method::GET, repo, tag).await?;
        let header = content_digest(&response);
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("manifest body unreadable: {e}"))?;
        Ok(read(body_digest(header.as_deref(), &body)?, &body)?)
    }

    /// The digest a tag names: HEAD, then a GET to hash the body when the
    /// registry sends no digest header.
    async fn fetch_digest(&self, repo: &Repository<'_>, tag: &str) -> Result<Digest, FetchError> {
        let head = self.request_manifest(Method::HEAD, repo, tag).await?;
        match probe(content_digest(&head).as_deref())? {
            Probe::Header(digest) => Ok(digest),
            Probe::NeedBody => self.get_manifest(repo, tag, |digest, _| Ok(digest)).await,
        }
    }

    /// The digest, children and blobs of a tag, from a GET of its exact bytes
    /// and of each child's.
    async fn fetch_entry(&self, repo: &Repository<'_>, tag: &str) -> Result<Entry, FetchError> {
        let (digest, manifest) = self
            .get_manifest(repo, tag, |digest, body| {
                Ok((digest, ManifestBody::parse(body)?))
            })
            .await?;
        let (children, mut blobs) = match manifest {
            ManifestBody::Index { manifests } => {
                let children: Vec<Digest> = manifests.into_iter().map(|d| d.digest).collect();
                let mut blobs = Vec::new();
                for child in &children {
                    let found = self
                        .get_manifest(repo, &child.to_string(), |actual, body| {
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
    let client = http_client()?;
    let access = Access::new(&client, auth.as_ref());
    let (images, failed) = access.resolve(&refs, Access::fetch_entry).await;
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

    fn resolved_of(images: &[(&str, char)]) -> BTreeMap<ImageRef, Digest> {
        images.iter().map(|(k, c)| (image(k), d(*c))).collect()
    }

    fn failure(raw: &str, status: Option<u16>) -> (ImageRef, FetchError) {
        let status = status.map(|s| StatusCode::from_u16(s).unwrap());
        (
            image(raw),
            FetchError {
                status,
                reason: format!("manifest request status {status:?}"),
            },
        )
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
            LockedAt::parse(&crate::bench::utc_timestamp()).is_ok(),
            "what the run stamps must be what the lock accepts"
        );
    }

    #[test]
    fn body_digest_hashes_the_body_and_refuses_a_header_that_disagrees() {
        let cases: [(Option<&str>, Option<&str>); 3] = [
            (None, Some(ABC_SHA256)),
            (Some(ABC_SHA256), Some(ABC_SHA256)),
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
    fn a_head_answer_with_a_digest_is_used_and_one_without_sends_for_the_body() {
        // Catches: fetching a body for every ref on every re-check, and
        // trusting nothing when the registry did name the digest.
        let abc = parse_digest(ABC_SHA256).unwrap();
        let cases: [(Option<&str>, Option<Probe>); 3] = [
            (Some(ABC_SHA256), Some(Probe::Header(abc.clone()))),
            (
                Some(" sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n"),
                Some(Probe::Header(abc)),
            ),
            (None, Some(Probe::NeedBody)),
        ];
        for (header, want) in cases {
            assert_eq!(probe(header).ok(), want, "header {header:?}");
        }
        assert!(probe(Some("not a digest")).is_err());
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
            (
                "a body that is neither",
                r#"{"layers":[]}"#.to_owned(),
                None,
            ),
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
            let got = ManifestBody::parse(body.as_bytes()).ok().map(|m| match m {
                ManifestBody::Image { config, layers } => (
                    "image",
                    std::iter::once(config)
                        .chain(layers)
                        .map(|x| x.digest)
                        .collect(),
                ),
                ManifestBody::Index { manifests } => {
                    ("index", manifests.into_iter().map(|x| x.digest).collect())
                }
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
            ("is not a manifest", d('5'), d('5'), "<html>", None),
        ];
        for (name, listed, actual, body, expected) in cases {
            let got = child_blobs(&listed, &actual, body.as_bytes()).ok();
            assert_eq!(got, expected, "{name}");
        }
    }

    #[test]
    fn a_lock_without_blobs_is_refused_with_the_command_that_writes_one() {
        // Catches: a lock from before blobs were recorded failing with a bare
        // parse error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.lock.json");
        let old = format!(
            r#"{{"locked_at":"2026-10-30-120000","images":{{"s/a:1":{{"digest":"{}","children":[]}}}}}}"#,
            d('1')
        );
        std::fs::write(&path, old).unwrap();
        let error = Lock::load(&path).unwrap_err();
        assert!(
            error.contains("blobs") && error.contains("cargo xtask bench-lock"),
            "{error}"
        );
    }

    #[test]
    fn lock_round_trips_with_sorted_keys_and_refuses_unknown_fields() {
        let lock = lock_of(&[
            ("quay.io/b:latest", entry('1', &['2', '3'])),
            (
                "docker.io/a:1",
                Entry {
                    blobs: vec![d('5')],
                    ..entry('4', &[])
                },
            ),
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
    }

    #[test]
    fn a_moved_or_gone_source_is_refused_and_a_failed_lookup_is_not_called_a_move() {
        let lock = lock_of(&[
            ("s/a:1", entry('1', &[])),
            ("s/b:1", entry('2', &[])),
            ("s/gone:1", entry('3', &[])),
        ]);
        let outage = failure("s/b:1", Some(503));

        assert_eq!(
            check_unmoved(&lock, &resolved_of(&[("s/a:1", '1')]), &[]),
            Ok(()),
            "an unmoved source passes"
        );

        let Err(RecheckError::Moved(text)) = check_unmoved(
            &lock,
            &resolved_of(&[("s/a:1", '9'), ("s/c:1", '3')]),
            &[outage],
        ) else {
            panic!("a moved source must not pass, even beside a failed lookup");
        };
        assert!(text.contains(&format!("s/a:1: expected {}, now {}", d('1'), d('9'))));
        assert!(text.contains("s/c:1: not in the lock"));
        assert!(text.contains("cargo xtask bench-lock") && text.contains("new comparison series"));
        assert!(text.contains("could not resolve 1 source ref(s)"));

        // Catches: a tag or repository deleted at the source (Docker Hub and
        // quay answer 401) being waved through as an outage.
        for status in [404, 403, 401] {
            let Err(RecheckError::Moved(text)) = check_unmoved(
                &lock,
                &resolved_of(&[("s/a:1", '1')]),
                &[failure("s/gone:1", Some(status))],
            ) else {
                panic!("a {status} for a locked ref is a moved source");
            };
            assert!(text.contains("s/gone:1: no longer at the source"));
        }

        for status in [Some(503), Some(429), None] {
            let check = check_unmoved(&lock, &resolved_of(&[]), &[failure("s/b:1", status)]);
            assert!(
                matches!(check, Err(RecheckError::Lookup(_))),
                "{status:?} leaves the check incomplete, not failed"
            );
        }
    }

    #[test]
    fn between_scenarios_only_a_difference_stops_the_run() {
        // Catches: stopping a long run, and discarding its finished scenarios,
        // because a registry was briefly unreachable.
        let moved = RecheckError::Moved("s/a:1 moved".into());
        let lookup = RecheckError::Lookup("s/a:1 unreachable".into());
        assert_eq!(between_scenarios(Ok(())), Next::Proceed { warning: None });
        assert_eq!(
            between_scenarios(Err(moved)),
            Next::Stop("s/a:1 moved".into())
        );
        let Next::Proceed { warning: Some(w) } = between_scenarios(Err(lookup)) else {
            panic!("a failed lookup proceeds with a warning");
        };
        assert!(w.contains("s/a:1 unreachable") && w.contains("post-pass digest check"));
    }

    #[test]
    fn a_failed_scenario_is_blamed_on_the_source_only_when_it_moved() {
        // Catches: reporting a source that changed mid-scenario as a tool defect.
        let error = "ocync cold verification failed".to_owned();
        let moved = explain(
            error.clone(),
            Err(RecheckError::Moved("s/a:1 moved".into())),
        );
        assert!(moved.starts_with("the source moved during the scenario"));
        assert!(moved.contains("s/a:1 moved") && moved.contains(&error));
        assert_eq!(explain(error.clone(), Ok(())), error);
        assert_eq!(
            explain(error.clone(), Err(RecheckError::Lookup("down".into()))),
            error
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
