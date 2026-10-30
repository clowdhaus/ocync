//! The corpus lock: every (source, tag) the benchmark syncs, pinned to the
//! manifest digest it resolved to, so runs on different dates sync the same
//! content and a moved source is refused rather than benchmarked.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use clap::Args;
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::bench::corpus::{self, CorpusPaths, DockerHubAuth, ImageEntry};
use crate::bench::registry::{self, Repository, Tokens};

/// The only digest algorithm a corpus manifest is addressed by.
const DIGEST_PREFIX: &str = "sha256:";

/// Hex characters in a SHA-256 digest.
const SHA256_HEX_LEN: usize = 64;

/// A manifest digest: `sha256:` followed by 64 lowercase hex characters.
///
/// Parsed when read, so a malformed or differently-spelled digest never
/// reaches a comparison.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Digest(String);

impl Digest {
    /// Parse `sha256:` plus 64 lowercase hex characters, refusing anything else.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let hex = raw
            .strip_prefix(DIGEST_PREFIX)
            .ok_or_else(|| format!("digest {raw:?} is not a sha256 digest"))?;
        let well_formed = hex.len() == SHA256_HEX_LEN
            && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if well_formed {
            Ok(Self(raw.to_owned()))
        } else {
            Err(format!(
                "digest {raw:?} is not {SHA256_HEX_LEN} lowercase hex characters after \"{DIGEST_PREFIX}\""
            ))
        }
    }

    /// The digest of `bytes`.
    fn of(bytes: &[u8]) -> Self {
        use std::fmt::Write as _;

        let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
        let mut text = String::from(DIGEST_PREFIX);
        for byte in hash.as_ref() {
            let _ = write!(text, "{byte:02x}");
        }
        Self(text)
    }
}

impl TryFrom<String> for Digest {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, String> {
        Self::parse(&raw)
    }
}

impl From<Digest> for String {
    fn from(digest: Digest) -> String {
        digest.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
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
pub(crate) fn refs_of(entries: &[ImageEntry]) -> BTreeSet<ImageRef> {
    entries
        .iter()
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
    /// Parse the run timestamp format, refusing anything else.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let field = |range: std::ops::Range<usize>| {
            let digits = raw.get(range)?;
            if digits.bytes().all(|b| b.is_ascii_digit()) {
                digits.parse::<u32>().ok()
            } else {
                None
            }
        };
        let dashes = [4, 7, 10]
            .iter()
            .all(|&i| raw.as_bytes().get(i) == Some(&b'-'));
        let in_range = matches!(
            (
                field(0..4),
                field(5..7),
                field(8..10),
                field(11..13),
                field(13..15),
                field(15..17)
            ),
            (
                Some(_),
                Some(1..=12),
                Some(1..=31),
                Some(0..=23),
                Some(0..=59),
                Some(0..=59)
            )
        );
        if raw.len() == 17 && dashes && in_range {
            Ok(Self(raw.to_owned()))
        } else {
            Err(format!(
                "{raw:?} is not a UTC timestamp like 2026-10-30-214340"
            ))
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
}

/// The lock file: when it was written, which partial overrides it was built
/// with, and what each ref resolved to.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lock {
    /// When the lock was written.
    pub(crate) locked_at: LockedAt,
    /// The `--partial-overrides` path, as given, whose tags are in `images`.
    /// A partial scenario reading other overrides syncs tags this lock never saw.
    pub(crate) partial_overrides: String,
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

    /// Refuse partial overrides other than the ones this lock was built from.
    pub(crate) fn ensure_overrides(&self, given: &str) -> Result<(), String> {
        if self.partial_overrides == given {
            return Ok(());
        }
        Err(format!(
            "the corpus lock was built with --partial-overrides {}, not {given}, so the partial \
             scenario would sync tags it never saw; run `cargo xtask bench-lock --partial-overrides {given}`",
            self.partial_overrides
        ))
    }
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
        }
    }
}

/// Every way the freshly resolved digests differ from `lock`. Equal digests
/// mean identical manifest bytes, children included. Lock entries the scenario
/// does not sync are not looked at.
fn compare(lock: &Lock, resolved: &BTreeMap<ImageRef, Digest>) -> Vec<Difference> {
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
    differences
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

/// Judge the refs that resolved against `lock`, and say what could not be
/// resolved. A difference is a move even when other refs failed to resolve.
fn check_unmoved(
    lock: &Lock,
    resolved: &BTreeMap<ImageRef, Digest>,
    failures: &[String],
) -> Result<(), RecheckError> {
    let differences = compare(lock, resolved);
    let unresolved = (!failures.is_empty()).then(|| {
        format!(
            "could not resolve {} source ref(s):\n{}",
            failures.len(),
            failures.join("\n")
        )
    });
    if differences.is_empty() {
        return unresolved.map_or(Ok(()), |text| Err(RecheckError::Lookup(text)));
    }
    let listed: Vec<String> = differences.iter().map(|d| format!("  {d}")).collect();
    let mut text = format!(
        "{} source ref(s) differ from the corpus lock, so this run would not sync the corpus \
         it was locked to:\n{}\nRun `cargo xtask bench-lock` to lock the current content. A new \
         lock starts a new comparison series: do not read its runs against runs under the old one.",
        differences.len(),
        listed.join("\n")
    );
    if let Some(unresolved) = unresolved {
        text.push_str("\nAlso, ");
        text.push_str(&unresolved);
    }
    Err(RecheckError::Moved(text))
}

/// Resolve every ref in `refs` again and refuse if any differs from `lock`.
///
/// Probes with HEAD, which costs no body, and falls back to a GET only for a
/// registry that sends no digest header.
pub(crate) async fn ensure_unmoved(
    lock: &Lock,
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
    tokens: &mut Tokens,
) -> Result<(), RecheckError> {
    eprintln!(
        "bench: checking {} source refs against the lock...",
        refs.len()
    );
    let (resolved, failures) = resolve(refs, auth, tokens, fetch_digest)
        .await
        .map_err(RecheckError::Lookup)?;
    check_unmoved(lock, &resolved, &failures)
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

/// Explain a failed content check by what re-resolving the mismatched sources
/// found: the source moving is not the tool's fault.
pub(crate) fn blame(content_check: &str, recheck: Result<(), RecheckError>) -> String {
    match recheck {
        Err(RecheckError::Moved(text)) => {
            format!("the source moved during the scenario: {text}\n{content_check}")
        }
        Ok(()) => {
            format!("the content synced differs from the unmoved source: {content_check}")
        }
        Err(RecheckError::Lookup(text)) => format!(
            "the content synced differs from the lock, and the source could not be re-checked \
             to tell whether it moved: {text}\n{content_check}"
        ),
    }
}

/// The digest of a manifest body. A registry that also reports a digest must
/// agree with the bytes, or the lock would record something no copy can match.
fn body_digest(header: Option<&str>, body: &[u8]) -> Result<Digest, String> {
    let actual = Digest::of(body);
    if let Some(value) = header {
        let reported = Digest::parse(value.trim())?;
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
        Some(value) => Digest::parse(value.trim()).map(Probe::Header),
        None => Ok(Probe::NeedBody),
    }
}

/// A manifest body, read only for the children an index lists.
#[derive(Deserialize)]
struct ManifestBody {
    /// Child descriptors; absent from a single-image manifest.
    #[serde(default)]
    manifests: Vec<Child>,
}

/// One entry of an index's `manifests` array.
#[derive(Deserialize)]
struct Child {
    /// Digest of the child manifest.
    digest: Digest,
}

/// The child manifest digests of an index body, in order; empty for a single
/// manifest.
fn child_digests(body: &[u8]) -> Result<Vec<Digest>, String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "a public manifest holding no credential"
    )]
    let parsed: ManifestBody =
        serde_json::from_slice(body).map_err(|e| format!("manifest is not valid: {e}"))?;
    Ok(parsed.manifests.into_iter().map(|c| c.digest).collect())
}

/// Resolve every ref with `fetch`. Returns what resolved and a line for each
/// ref that did not, so the caller decides what an incomplete answer means.
async fn resolve<T>(
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
    tokens: &mut Tokens,
    fetch: impl AsyncFn(&reqwest::Client, &Repository<'_>, &str, &str) -> Result<T, String>,
) -> Result<(BTreeMap<ImageRef, T>, Vec<String>), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let mut resolved = BTreeMap::new();
    let mut failures = Vec::new();
    for image in refs {
        let outcome = async {
            let repo = registry::split_source(&image.source)
                .ok_or_else(|| "source names no registry".to_owned())?;
            let token = tokens
                .get(&client, &repo, auth)
                .await
                .as_ref()
                .map_err(String::clone)?;
            fetch(&client, &repo, &image.tag, token).await
        }
        .await;
        match outcome {
            Ok(found) => {
                resolved.insert(image.clone(), found);
            }
            Err(reason) => failures.push(format!("  {image}: {reason}")),
        }
    }
    Ok((resolved, failures))
}

/// Request a manifest, retrying what [`registry::send_retrying`] retries. Any
/// other failing status is reported at once.
async fn request_manifest(
    client: &reqwest::Client,
    method: Method,
    repo: &Repository<'_>,
    tag: &str,
    token: &str,
) -> Result<reqwest::Response, String> {
    let url = format!(
        "https://{}/v2/{}/manifests/{tag}",
        repo.host.api_host(),
        repo.path
    );
    let response = registry::send_retrying(&format!("{repo}:{tag}"), || {
        client
            .request(method.clone(), &url)
            .bearer_auth(token)
            .header("Accept", registry::MANIFEST_ACCEPT)
    })
    .await
    .map_err(|e| format!("manifest request {e}"))?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(format!("manifest request returned {}", response.status()))
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

/// GET a manifest, hash its exact bytes and read them with `read`.
async fn get_manifest<T>(
    client: &reqwest::Client,
    repo: &Repository<'_>,
    tag: &str,
    token: &str,
    read: impl FnOnce(Digest, &[u8]) -> Result<T, String>,
) -> Result<T, String> {
    let response = request_manifest(client, Method::GET, repo, tag, token).await?;
    let header = content_digest(&response);
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("manifest body unreadable: {e}"))?;
    read(body_digest(header.as_deref(), &body)?, &body)
}

/// The digest a tag names: HEAD, then a GET to hash the body when the registry
/// sends no digest header.
async fn fetch_digest(
    client: &reqwest::Client,
    repo: &Repository<'_>,
    tag: &str,
    token: &str,
) -> Result<Digest, String> {
    let head = request_manifest(client, Method::HEAD, repo, tag, token).await?;
    match probe(content_digest(&head).as_deref())? {
        Probe::Header(digest) => Ok(digest),
        Probe::NeedBody => get_manifest(client, repo, tag, token, |digest, _| Ok(digest)).await,
    }
}

/// The digest and children of a tag, from a GET of its exact bytes.
async fn fetch_entry(
    client: &reqwest::Client,
    repo: &Repository<'_>,
    tag: &str,
    token: &str,
) -> Result<Entry, String> {
    get_manifest(client, repo, tag, token, |digest, body| {
        Ok(Entry {
            digest,
            children: child_digests(body)?,
        })
    })
    .await
}

/// Arguments for the `bench-lock` subcommand.
#[derive(Args)]
pub(crate) struct LockArgs {
    #[command(flatten)]
    paths: CorpusPaths,
}

/// The lock to write, or `None` when it would equal the existing one, so
/// re-running over unchanged sources leaves the file, and its `locked_at`
/// series, alone.
fn relock(
    existing: Option<&Lock>,
    partial_overrides: &str,
    images: BTreeMap<ImageRef, Entry>,
    now: LockedAt,
) -> Option<Lock> {
    let unchanged = existing
        .is_some_and(|lock| lock.images == images && lock.partial_overrides == partial_overrides);
    if unchanged {
        return None;
    }
    Some(Lock {
        locked_at: now,
        partial_overrides: partial_overrides.to_owned(),
        images,
    })
}

/// Resolve every ref the corpus and the partial overrides sync, and write the
/// lock beside the corpus. Needs the network and nothing else: no target
/// registry, no cloud write.
pub(crate) async fn run(args: LockArgs) -> Result<(), Box<dyn std::error::Error>> {
    let entries = corpus::load_images(args.paths.corpus())?;
    let overrides = corpus::load_overrides(args.paths.partial_overrides())?;

    let mut refs = refs_of(&entries);
    refs.extend(refs_of(&overrides.overrides));

    let auth = crate::bench::docker_hub_auth();
    eprintln!("bench-lock: resolving {} source refs...", refs.len());
    let (images, failures) =
        resolve(&refs, auth.as_ref(), &mut Tokens::default(), fetch_entry).await?;
    if !failures.is_empty() {
        return Err(format!(
            "could not resolve {} source ref(s):\n{}",
            failures.len(),
            failures.join("\n")
        )
        .into());
    }

    let path = args.paths.lock_path();
    let existing = match Lock::load(&path) {
        Ok(lock) => Some(lock),
        Err(e) => {
            eprintln!("bench-lock: no usable existing lock ({e})");
            None
        }
    };
    match relock(
        existing.as_ref(),
        args.paths.partial_overrides(),
        images,
        LockedAt::now(),
    ) {
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
    use super::{Digest, Entry, Lock, LockedAt};

    /// A digest whose 64 hex characters are all `c`.
    pub(crate) fn digest(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    /// An entry for `digest_fill` listing `children`, each named by its fill character.
    pub(crate) fn entry(digest_fill: char, children: &[char]) -> Entry {
        Entry {
            digest: digest(digest_fill),
            children: children.iter().map(|c| digest(*c)).collect(),
        }
    }

    /// A lock time.
    pub(crate) fn locked_at(raw: &str) -> LockedAt {
        LockedAt::parse(raw).unwrap()
    }

    /// A lock with no images.
    pub(crate) fn lock() -> Lock {
        Lock {
            locked_at: locked_at("2026-10-30-120000"),
            partial_overrides: "overrides.yaml".into(),
            images: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{digest as d, entry, locked_at};
    use super::*;

    const OVERRIDES: &str = "bench/corpus-partial-overrides.yaml";

    fn image(raw: &str) -> ImageRef {
        ImageRef::parse(raw).unwrap()
    }

    fn lock_of(images: &[(&str, Entry)]) -> Lock {
        Lock {
            locked_at: locked_at("2026-10-30-120000"),
            partial_overrides: OVERRIDES.into(),
            images: images.iter().map(|(k, v)| (image(k), v.clone())).collect(),
        }
    }

    fn resolved_of(images: &[(&str, char)]) -> BTreeMap<ImageRef, Digest> {
        images.iter().map(|(k, c)| (image(k), d(*c))).collect()
    }

    #[test]
    fn digest_parse_accepts_only_lowercase_sha256() {
        let valid = format!("sha256:{}", "ab01".repeat(16));
        let cases: [(String, bool); 7] = [
            (valid.clone(), true),
            (valid.to_uppercase().replacen("SHA256", "sha256", 1), false),
            (format!("sha256:{}", "a".repeat(63)), false),
            (format!("sha256:{}", "a".repeat(65)), false),
            (format!("sha512:{}", "a".repeat(64)), false),
            ("a".repeat(64), false),
            (format!("sha256:{}g", "a".repeat(63)), false),
        ];
        for (raw, accepted) in cases {
            assert_eq!(Digest::parse(&raw).is_ok(), accepted, "{raw}");
        }
    }

    const ABC_SHA256: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn digest_of_bytes_is_the_known_sha256() {
        assert_eq!(Digest::of(b"abc").to_string(), ABC_SHA256);
    }

    #[test]
    fn locked_at_accepts_only_the_run_timestamp_format() {
        let cases = [
            ("2026-10-30-214340", true),
            ("2026-10-30-2143", false),
            ("2026-10-30T214340", false),
            ("2026-13-30-214340", false),
            ("2026-10-30-246000", false),
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
        // Catches: hashing a body for every ref (a body per tag on every re-check),
        // and trusting nothing when the registry did name the digest.
        let cases: [(Option<&str>, Option<Probe>); 4] = [
            (
                Some(ABC_SHA256),
                Some(Probe::Header(Digest::parse(ABC_SHA256).unwrap())),
            ),
            (
                Some(" sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n"),
                Some(Probe::Header(Digest::parse(ABC_SHA256).unwrap())),
            ),
            (None, Some(Probe::NeedBody)),
            (Some("sha512:abc"), None),
        ];
        for (header, want) in cases {
            assert_eq!(probe(header).ok(), want, "header {header:?}");
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

    #[test]
    fn child_digests_lists_an_index_in_order_and_is_empty_for_a_manifest() {
        let index = format!(
            r#"{{"manifests":[{{"digest":"sha256:{}"}},{{"digest":"sha256:{}"}}]}}"#,
            "2".repeat(64),
            "1".repeat(64)
        );
        assert_eq!(
            child_digests(index.as_bytes()).unwrap(),
            vec![d('2'), d('1')]
        );
        assert!(
            child_digests(br#"{"layers":[]}"#).unwrap().is_empty(),
            "a single manifest has no children"
        );
        assert!(child_digests(b"<html>").is_err());
    }

    #[test]
    fn lock_round_trips_with_sorted_keys_and_refuses_unknown_fields() {
        let lock = lock_of(&[
            ("quay.io/b:latest", entry('1', &['2', '3'])),
            ("docker.io/a:1", entry('4', &[])),
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
    fn a_partial_scenario_refuses_overrides_the_lock_was_not_built_with() {
        let lock = lock_of(&[]);
        assert!(lock.ensure_overrides(OVERRIDES).is_ok());
        let error = lock
            .ensure_overrides("elsewhere/overrides.yaml")
            .expect_err("other overrides would sync tags the lock never saw");
        assert!(
            error.contains(OVERRIDES)
                && error.contains("bench-lock --partial-overrides elsewhere/overrides.yaml")
        );
    }

    #[test]
    fn compare_reports_a_moved_digest_and_a_missing_ref_and_nothing_else() {
        let lock = lock_of(&[
            ("s/same:1", entry('1', &['2'])),
            ("s/moved:1", entry('3', &[])),
        ]);
        let cases: [(&str, char, Option<Difference>); 3] = [
            ("s/same:1", '1', None),
            (
                "s/moved:1",
                '6',
                Some(Difference::Moved {
                    image: image("s/moved:1"),
                    expected: d('3'),
                    actual: d('6'),
                }),
            ),
            (
                "s/new:1",
                '8',
                Some(Difference::Missing {
                    image: image("s/new:1"),
                }),
            ),
        ];
        for (name, actual, want) in cases {
            let got = compare(&lock, &resolved_of(&[(name, actual)]));
            assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{name}");
        }
        assert!(
            compare(&lock, &BTreeMap::new()).is_empty(),
            "a lock entry the scenario does not resolve is not compared"
        );
    }

    #[test]
    fn a_moved_source_is_refused_and_a_failed_lookup_is_not_called_a_move() {
        let lock = lock_of(&[("s/a:1", entry('1', &[])), ("s/b:1", entry('2', &[]))]);
        let lookup = vec!["  s/b:1: manifest request returned 503 (5 attempts)".to_owned()];

        assert_eq!(
            check_unmoved(&lock, &resolved_of(&[("s/a:1", '1')]), &[]),
            Ok(()),
            "an unmoved source passes"
        );

        let Err(RecheckError::Moved(text)) = check_unmoved(
            &lock,
            &resolved_of(&[("s/a:1", '9'), ("s/c:1", '3')]),
            &lookup,
        ) else {
            panic!("a moved source must not pass, even beside a failed lookup");
        };
        assert!(text.contains(&format!("s/a:1: expected {}, now {}", d('1'), d('9'))));
        assert!(text.contains("s/c:1: not in the lock"));
        assert!(text.contains("cargo xtask bench-lock") && text.contains("new comparison series"));
        assert!(text.contains("s/b:1: manifest request returned 503"));

        let Err(RecheckError::Lookup(text)) =
            check_unmoved(&lock, &resolved_of(&[("s/a:1", '1')]), &lookup)
        else {
            panic!("an incomplete check with no difference is a lookup failure");
        };
        assert!(text.contains("could not resolve 1 source ref(s)"));
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
    fn a_failed_content_check_blames_the_source_only_when_it_moved() {
        // Catches: reporting a source that changed mid-scenario as a tool defect.
        let content = "ocync cold verification failed -- 1 problems";
        let moved = blame(content, Err(RecheckError::Moved("s/a:1 moved".into())));
        assert!(moved.starts_with("the source moved during the scenario"));
        assert!(moved.contains("s/a:1 moved") && moved.contains(content));

        let tool = blame(content, Ok(()));
        assert!(tool.starts_with("the content synced differs from the unmoved source"));
        assert!(!tool.contains("moved during"));

        let unknown = blame(content, Err(RecheckError::Lookup("unreachable".into())));
        assert!(unknown.contains("could not be re-checked") && unknown.contains(content));
    }

    #[test]
    fn relock_leaves_unchanged_content_alone_and_stamps_anything_else() {
        let existing = lock_of(&[("s/a:1", entry('1', &[]))]);
        let now = || locked_at("2026-11-01-000000");
        assert_eq!(
            relock(Some(&existing), OVERRIDES, existing.images.clone(), now()),
            None,
            "unchanged content keeps the file and its locked_at"
        );

        let moved = lock_of(&[("s/a:1", entry('2', &[]))]).images;
        assert_eq!(
            relock(Some(&existing), OVERRIDES, moved.clone(), now()),
            Some(Lock {
                locked_at: now(),
                partial_overrides: OVERRIDES.into(),
                images: moved.clone(),
            })
        );
        assert!(
            relock(
                Some(&existing),
                "other.yaml",
                existing.images.clone(),
                now()
            )
            .is_some(),
            "other overrides are another lock"
        );
        assert!(relock(None, OVERRIDES, moved, now()).is_some());
    }
}
