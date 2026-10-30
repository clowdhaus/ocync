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

/// What a tag resolved to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LockEntry {
    /// Digest of the manifest the tag names.
    pub(crate) digest: Digest,
    /// The index's child manifest digests in order; empty for a single manifest.
    pub(crate) children: Vec<Digest>,
}

/// The lock file: when it was written and what each ref resolved to.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lock {
    /// UTC time the lock was written, in the run timestamp format. Runs recorded
    /// under different values are different comparison series.
    pub(crate) locked_at: String,
    /// Resolution of every ref.
    pub(crate) images: BTreeMap<ImageRef, LockEntry>,
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

/// `Err` listing every ref that differs from `lock`, `Ok` when none does.
fn check_unmoved(lock: &Lock, resolved: &BTreeMap<ImageRef, Digest>) -> Result<(), String> {
    let differences = compare(lock, resolved);
    if differences.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = differences.iter().map(|d| format!("  {d}")).collect();
    Err(format!(
        "{} source ref(s) differ from the corpus lock, so this run would not sync the corpus \
         it was locked to:\n{}\nRun `cargo xtask bench-lock` to lock the current content. A new \
         lock starts a new comparison series: do not read its runs against runs under the old one.",
        differences.len(),
        listed.join("\n")
    ))
}

/// Resolve every ref in `refs` again and refuse if any differs from `lock`.
///
/// Probes with HEAD, which every registry answers with the digest and which
/// costs no body, and falls back to a GET only where a registry sends none.
pub(crate) async fn ensure_unmoved(
    lock: &Lock,
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
) -> Result<(), String> {
    eprintln!(
        "bench: checking {} source refs against the lock...",
        refs.len()
    );
    let resolved = resolve(refs, auth, Mode::Recheck).await?;
    let digests = resolved
        .into_iter()
        .map(|(image, fetched)| (image, fetched.digest))
        .collect();
    check_unmoved(lock, &digests)
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

/// What a resolution is for.
#[derive(Clone, Copy)]
enum Mode {
    /// Writing the lock: fetch every manifest and hash the exact bytes.
    Lock,
    /// Checking the lock: the digest alone.
    Recheck,
}

/// What a tag resolved to.
struct Fetched {
    /// Digest of the manifest the tag names.
    digest: Digest,
    /// The manifest bytes, when they were fetched.
    body: Option<Vec<u8>>,
}

/// Resolve every ref, naming each one that cannot be resolved.
async fn resolve(
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
    mode: Mode,
) -> Result<BTreeMap<ImageRef, Fetched>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let mut tokens = Tokens::default();
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
            fetch_one(&client, &repo, &image.tag, token, mode).await
        }
        .await;
        match outcome {
            Ok(fetched) => {
                resolved.insert(image.clone(), fetched);
            }
            Err(reason) => failures.push(format!("  {image}: {reason}")),
        }
    }

    if failures.is_empty() {
        Ok(resolved)
    } else {
        Err(format!(
            "could not resolve {} source ref(s):\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
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

/// GET a manifest and hash its exact bytes.
async fn fetch_body(
    client: &reqwest::Client,
    repo: &Repository<'_>,
    tag: &str,
    token: &str,
) -> Result<Fetched, String> {
    let response = request_manifest(client, Method::GET, repo, tag, token).await?;
    let header = content_digest(&response);
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("manifest body unreadable: {e}"))?;
    Ok(Fetched {
        digest: body_digest(header.as_deref(), &body)?,
        body: Some(body.to_vec()),
    })
}

/// The `Docker-Content-Digest` header of a response, if any.
fn content_digest(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Resolve one tag the way `mode` asks.
async fn fetch_one(
    client: &reqwest::Client,
    repo: &Repository<'_>,
    tag: &str,
    token: &str,
    mode: Mode,
) -> Result<Fetched, String> {
    if let Mode::Recheck = mode {
        let response = request_manifest(client, Method::HEAD, repo, tag, token).await?;
        if let Some(header) = content_digest(&response) {
            return Ok(Fetched {
                digest: Digest::parse(header.trim())?,
                body: None,
            });
        }
    }
    fetch_body(client, repo, tag, token).await
}

/// Arguments for the `bench-lock` subcommand.
#[derive(Args)]
pub(crate) struct LockArgs {
    #[command(flatten)]
    paths: CorpusPaths,
}

/// The lock to write, or `None` when `images` equal the existing lock's, so
/// re-running over unchanged sources leaves the file, and its `locked_at`
/// series, alone.
fn relock(
    existing: Option<&Lock>,
    images: BTreeMap<ImageRef, LockEntry>,
    now: String,
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

    let mut refs = refs_of(&entries);
    refs.extend(refs_of(&overrides.overrides));

    let auth = crate::bench::docker_hub_auth();
    eprintln!("bench-lock: resolving {} source refs...", refs.len());
    let mut images = BTreeMap::new();
    for (image, fetched) in resolve(&refs, auth.as_ref(), Mode::Lock).await? {
        let body = fetched
            .body
            .ok_or_else(|| format!("{image}: manifest was not fetched"))?;
        let children = child_digests(&body).map_err(|e| format!("{image}: {e}"))?;
        images.insert(
            image,
            LockEntry {
                digest: fetched.digest,
                children,
            },
        );
    }

    let path = args.paths.lock_path();
    let existing = match Lock::load(&path) {
        Ok(lock) => Some(lock),
        Err(e) => {
            eprintln!("bench-lock: no usable existing lock ({e})");
            None
        }
    };
    match relock(existing.as_ref(), images, crate::bench::utc_timestamp()) {
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

/// Digests and lock entries for tests here and in the modules that check
/// against a lock.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::{Digest, LockEntry};

    /// A digest whose 64 hex characters are all `c`.
    pub(crate) fn digest(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    /// A lock entry for `digest` listing `children`, each named by its fill character.
    pub(crate) fn entry(digest_fill: char, children: &[char]) -> LockEntry {
        LockEntry {
            digest: digest(digest_fill),
            children: children.iter().map(|c| digest(*c)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{digest as d, entry};
    use super::*;

    fn image(raw: &str) -> ImageRef {
        ImageRef::parse(raw).unwrap()
    }

    fn lock_of(images: &[(&str, LockEntry)]) -> Lock {
        Lock {
            locked_at: "2026-10-30-120000".into(),
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
        assert!(child_digests(br#"{"manifests":[{"digest":"nope"}]}"#).is_err());
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
    fn a_moved_source_is_refused_naming_every_difference_and_the_remedy() {
        let lock = lock_of(&[("s/a:1", entry('1', &[])), ("s/b:1", entry('2', &[]))]);

        assert!(
            check_unmoved(&lock, &resolved_of(&[("s/a:1", '1')])).is_ok(),
            "an unmoved source passes"
        );

        let error = check_unmoved(
            &lock,
            &resolved_of(&[("s/a:1", '9'), ("s/b:1", '2'), ("s/c:1", '3')]),
        )
        .expect_err("a moved source must not pass");
        assert!(error.contains(&format!("s/a:1: expected {}, now {}", d('1'), d('9'))));
        assert!(error.contains("s/c:1: not in the lock"));
        assert!(!error.contains("s/b:1"), "an unmoved ref is not listed");
        assert!(error.contains("cargo xtask bench-lock"));
        assert!(error.contains("new comparison series"));
    }

    #[test]
    fn relock_leaves_unchanged_content_alone_and_stamps_anything_else() {
        let existing = lock_of(&[("s/a:1", entry('1', &[]))]);
        let same = existing.images.clone();
        assert_eq!(
            relock(Some(&existing), same, "2026-11-01-000000".into()),
            None,
            "unchanged content keeps the file and its locked_at"
        );

        let moved = lock_of(&[("s/a:1", entry('2', &[]))]).images;
        let relocked = relock(Some(&existing), moved.clone(), "2026-11-01-000000".into());
        assert_eq!(
            relocked,
            Some(Lock {
                locked_at: "2026-11-01-000000".into(),
                images: moved.clone(),
            })
        );
        assert!(relock(None, moved, "2026-11-01-000000".into()).is_some());
    }
}
