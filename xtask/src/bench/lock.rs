//! The corpus lock: every (source, tag) the benchmark syncs, pinned to the
//! manifest digest it resolved to, so runs on different dates sync the same
//! content and a moved source is refused rather than benchmarked.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::time::Duration;

use clap::Args;
use serde::{Deserialize, Serialize};

use crate::bench::corpus::{self, DockerHubAuth, ImageEntry};
use crate::bench::registry;

/// Where the lock lives, relative to the repository root.
pub(crate) const LOCK_PATH: &str = "bench/corpus.lock.json";

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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ImageRef {
    /// Source repository, e.g. `quay.io/jupyter/base-notebook`.
    pub(crate) source: String,
    /// Tag of that repository.
    pub(crate) tag: String,
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
    /// Resolution of every ref, keyed by `source:tag`.
    pub(crate) images: BTreeMap<String, LockEntry>,
}

impl Lock {
    /// Read and parse the lock at `path`.
    pub(crate) fn load(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "failed to read corpus lock {path}: {e}; create it with `cargo xtask bench-lock`"
            )
        })?;
        Self::parse(&raw).map_err(|e| format!("corpus lock {path}: {e}"))
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
    Missing { image: String },
    /// The tag now names a different manifest.
    Moved {
        image: String,
        expected: Digest,
        actual: Digest,
    },
    /// The tag names the same digest but its index lists other children.
    ChildrenChanged {
        image: String,
        expected: Vec<Digest>,
        actual: Vec<Digest>,
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
            Self::ChildrenChanged {
                image,
                expected,
                actual,
            } => write!(
                f,
                "{image}: children expected [{}], now [{}]",
                join(expected),
                join(actual)
            ),
        }
    }
}

/// `digests` as a comma-separated list.
fn join(digests: &[Digest]) -> String {
    digests
        .iter()
        .map(Digest::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every way the freshly resolved refs differ from `lock`. Lock entries the
/// scenario does not sync are not looked at.
fn compare(lock: &Lock, resolved: &BTreeMap<String, LockEntry>) -> Vec<Difference> {
    let mut differences = Vec::new();
    for (image, actual) in resolved {
        let Some(expected) = lock.images.get(image) else {
            differences.push(Difference::Missing {
                image: image.clone(),
            });
            continue;
        };
        if expected.digest != actual.digest {
            differences.push(Difference::Moved {
                image: image.clone(),
                expected: expected.digest.clone(),
                actual: actual.digest.clone(),
            });
        } else if expected.children != actual.children {
            differences.push(Difference::ChildrenChanged {
                image: image.clone(),
                expected: expected.children.clone(),
                actual: actual.children.clone(),
            });
        }
    }
    differences
}

/// `Err` listing every ref that differs from `lock`, `Ok` when none does.
fn check_unmoved(lock: &Lock, resolved: &BTreeMap<String, LockEntry>) -> Result<(), String> {
    let differences = compare(lock, resolved);
    if differences.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = differences.iter().map(|d| format!("  {d}")).collect();
    Err(format!(
        "{} source ref(s) differ from {LOCK_PATH}, so this run would not sync the corpus it \
         was locked to:\n{}\nRun `cargo xtask bench-lock` to lock the current content. A new \
         lock starts a new comparison series: do not read its runs against runs under the old one.",
        differences.len(),
        listed.join("\n")
    ))
}

/// Resolve every ref in `refs` again and refuse if any differs from `lock`.
pub(crate) async fn ensure_unmoved(
    lock: &Lock,
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
) -> Result<(), String> {
    eprintln!(
        "bench: checking {} source refs against the lock...",
        refs.len()
    );
    let resolved = resolve(refs, auth).await?;
    check_unmoved(lock, &resolved)
}

/// The digest of a manifest: the registry's `Docker-Content-Digest` when it
/// sent one, else the SHA-256 of the body.
fn manifest_digest(header: Option<&str>, body: &[u8]) -> Result<Digest, String> {
    match header {
        Some(value) => Digest::parse(value.trim()),
        None => Ok(Digest::of(body)),
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
    digest: String,
}

/// The child manifest digests of an index body, in order; empty for a single
/// manifest.
fn child_digests(body: &[u8]) -> Result<Vec<Digest>, String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "a public manifest holding no credential"
    )]
    let parsed: ManifestBody =
        serde_json::from_slice(body).map_err(|e| format!("manifest is not JSON: {e}"))?;
    parsed
        .manifests
        .iter()
        .map(|child| Digest::parse(&child.digest))
        .collect()
}

/// Resolve every ref to its manifest digest and children.
///
/// Fetches the manifest rather than probing it, because the children are in the
/// body. A ref that cannot be resolved fails the whole call, naming each one.
async fn resolve(
    refs: &BTreeSet<ImageRef>,
    auth: Option<&DockerHubAuth>,
) -> Result<BTreeMap<String, LockEntry>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let mut tokens: HashMap<(String, String), Result<String, String>> = HashMap::new();
    let mut resolved = BTreeMap::new();
    let mut failures = Vec::new();

    for image in refs {
        let outcome = async {
            let (host, repo) = registry::split_source(&image.source)
                .ok_or_else(|| "source names no registry".to_owned())?;
            let key = (host.to_owned(), repo.to_owned());
            if !tokens.contains_key(&key) {
                let token = registry::oci_token(&client, host, repo, auth).await;
                tokens.insert(key.clone(), token);
            }
            let token = tokens[&key].as_ref().map_err(String::clone)?;
            resolve_one(&client, host, repo, &image.tag, token).await
        }
        .await;
        match outcome {
            Ok(entry) => {
                resolved.insert(image.to_string(), entry);
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

/// Attempts at one manifest before a 429 is reported.
const MAX_ATTEMPTS: u32 = 5;

/// Longest a single wait for a 429 may last.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long to wait after a 429: the registry's `Retry-After` seconds when it
/// sent them, else doubling from 2 s, never more than [`MAX_BACKOFF`].
fn backoff(attempt: u32, retry_after: Option<&str>) -> Duration {
    let secs = retry_after
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| 2u64.saturating_pow(attempt.saturating_add(1)));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// GET one manifest and read its digest and children, waiting out a 429.
async fn resolve_one(
    client: &reqwest::Client,
    host: &str,
    repo: &str,
    tag: &str,
    token: &str,
) -> Result<LockEntry, String> {
    let mut attempt = 0;
    let response = loop {
        let response = client
            .get(format!("https://{host}/v2/{repo}/manifests/{tag}"))
            .bearer_auth(token)
            .header("Accept", registry::MANIFEST_ACCEPT)
            .send()
            .await
            .map_err(|e| format!("manifest request failed: {e}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt + 1 < MAX_ATTEMPTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok());
            let wait = backoff(attempt, retry_after);
            eprintln!("  {host}/{repo}:{tag}: 429, waiting {}s", wait.as_secs());
            tokio::time::sleep(wait).await;
            attempt += 1;
            continue;
        }
        if !status.is_success() {
            return Err(format!("manifest request returned {status}"));
        }
        break response;
    };
    let header = response
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("manifest body unreadable: {e}"))?;
    Ok(LockEntry {
        digest: manifest_digest(header.as_deref(), &body)?,
        children: child_digests(&body)?,
    })
}

/// Arguments for the `bench-lock` subcommand.
#[derive(Args)]
pub(crate) struct LockArgs {
    /// Path to corpus YAML file.
    #[arg(long, default_value = "bench/corpus.yaml")]
    pub(crate) corpus: String,

    /// Path to partial sync overrides YAML; its tags are locked too, because
    /// the partial scenario syncs them.
    #[arg(long, default_value = "bench/corpus-partial-overrides.yaml")]
    pub(crate) partial_overrides: String,
}

/// Resolve every ref the corpus and the partial overrides sync, and write the
/// lock. Needs the network and nothing else: no target registry, no cloud write.
pub(crate) async fn run(args: LockArgs) -> Result<(), Box<dyn std::error::Error>> {
    let entries = corpus::load_images(&args.corpus)?;
    let overrides = corpus::load_overrides(&args.partial_overrides)?;

    let mut refs = refs_of(&entries);
    refs.extend(refs_of(&overrides.overrides));

    let auth = crate::bench::dockerhub_auth();
    eprintln!("bench-lock: resolving {} source refs...", refs.len());
    let images = resolve(&refs, auth.as_ref()).await?;

    let lock = Lock {
        locked_at: crate::bench::utc_timestamp(),
        images,
    };
    std::fs::write(LOCK_PATH, lock.render()?)
        .map_err(|e| format!("failed to write {LOCK_PATH}: {e}"))?;
    eprintln!(
        "bench-lock: wrote {} refs to {LOCK_PATH} (locked_at {})",
        lock.images.len(),
        lock.locked_at
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A digest whose 64 hex characters are all `c`.
    fn d(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    fn entry(digest: char, children: &[char]) -> LockEntry {
        LockEntry {
            digest: d(digest),
            children: children.iter().map(|c| d(*c)).collect(),
        }
    }

    fn lock_of(images: &[(&str, LockEntry)]) -> Lock {
        Lock {
            locked_at: "2026-10-30-120000".into(),
            images: images
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        }
    }

    fn resolved_of(images: &[(&str, LockEntry)]) -> BTreeMap<String, LockEntry> {
        lock_of(images).images
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

    #[test]
    fn digest_of_bytes_is_the_known_sha256() {
        assert_eq!(
            Digest::of(b"abc").to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn manifest_digest_prefers_the_header_and_hashes_the_body_without_one() {
        let header = format!("sha256:{}", "e".repeat(64));
        assert_eq!(
            manifest_digest(Some(&header), b"abc").unwrap().to_string(),
            header
        );
        assert_eq!(
            manifest_digest(None, b"abc").unwrap().to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(manifest_digest(Some("sha512:abc"), b"abc").is_err());
    }

    #[test]
    fn backoff_follows_retry_after_doubles_without_it_and_is_capped() {
        let cases: [(u32, Option<&str>, u64); 5] = [
            (0, Some("7"), 7),
            (0, None, 2),
            (2, None, 8),
            (0, Some("3600"), 60),
            (1, Some("soon"), 4),
        ];
        for (attempt, header, secs) in cases {
            assert_eq!(
                backoff(attempt, header),
                Duration::from_secs(secs),
                "attempt {attempt}, Retry-After {header:?}"
            );
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
        let bad_digest = text.replacen("sha256:4", "sha256:X", 1);
        assert!(Lock::parse(&bad_digest).is_err(), "malformed digest");
    }

    #[test]
    fn compare_reports_each_way_a_ref_differs() {
        let lock = lock_of(&[
            ("s/same:1", entry('1', &['2'])),
            ("s/moved:1", entry('3', &[])),
            ("s/kids:1", entry('4', &['5'])),
        ]);
        let cases: [(&str, LockEntry, Option<Difference>); 4] = [
            ("s/same:1", entry('1', &['2']), None),
            (
                "s/moved:1",
                entry('6', &[]),
                Some(Difference::Moved {
                    image: "s/moved:1".into(),
                    expected: d('3'),
                    actual: d('6'),
                }),
            ),
            (
                "s/kids:1",
                entry('4', &['7']),
                Some(Difference::ChildrenChanged {
                    image: "s/kids:1".into(),
                    expected: vec![d('5')],
                    actual: vec![d('7')],
                }),
            ),
            (
                "s/new:1",
                entry('8', &[]),
                Some(Difference::Missing {
                    image: "s/new:1".into(),
                }),
            ),
        ];
        for (image, actual, want) in cases {
            let got = compare(&lock, &resolved_of(&[(image, actual)]));
            assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{image}");
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
            check_unmoved(&lock, &resolved_of(&[("s/a:1", entry('1', &[]))])).is_ok(),
            "an unmoved source passes"
        );

        let error = check_unmoved(
            &lock,
            &resolved_of(&[
                ("s/a:1", entry('9', &[])),
                ("s/b:1", entry('2', &[])),
                ("s/c:1", entry('3', &[])),
            ]),
        )
        .expect_err("a moved source must not pass");
        assert!(error.contains(&format!("s/a:1: expected {}, now {}", d('1'), d('9'))));
        assert!(error.contains("s/c:1: not in the lock"));
        assert!(!error.contains("s/b:1"), "an unmoved ref is not listed");
        assert!(error.contains("cargo xtask bench-lock"));
        assert!(error.contains("new comparison series"));
    }
}
