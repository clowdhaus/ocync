//! YAML corpus config parsing with environment variable expansion.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};

use clap::Args;
use serde::{Deserialize, Serialize};

/// The main corpus file. Its records carry no corpus name.
const DEFAULT_CORPUS: &str = "bench/corpus.yaml";

/// The file stem of a corpus other than the main one, which names its lock,
/// baseline and run records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct CorpusName(String);

impl CorpusName {
    /// The name `stem` gives a corpus.
    pub(crate) fn new(stem: &str) -> Self {
        Self(stem.to_owned())
    }
}

impl std::fmt::Display for CorpusName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which corpus a run measures. The serialized form is the optional stem: absent
/// for the main corpus, which keeps run records written before corpora were
/// named readable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Option<CorpusName>", into = "Option<CorpusName>")]
pub(crate) enum CorpusId {
    /// The main corpus, `bench/corpus.yaml`.
    #[default]
    Main,
    /// Any other corpus file.
    Named(CorpusName),
}

impl From<Option<CorpusName>> for CorpusId {
    fn from(name: Option<CorpusName>) -> Self {
        name.map_or(Self::Main, Self::Named)
    }
}

impl From<CorpusId> for Option<CorpusName> {
    fn from(id: CorpusId) -> Self {
        match id {
            CorpusId::Main => None,
            CorpusId::Named(name) => Some(name),
        }
    }
}

impl CorpusId {
    /// Whether this is the main corpus.
    pub(crate) fn is_main(&self) -> bool {
        matches!(self, Self::Main)
    }

    /// The corpus as a person reads it: its name, or the main corpus.
    pub(crate) fn label(&self) -> Cow<'static, str> {
        match self {
            Self::Main => Cow::Borrowed("the main corpus"),
            Self::Named(name) => Cow::Owned(name.to_string()),
        }
    }
}

/// Where the corpus and its partial-sync overrides are read from, shared by
/// `bench` and `bench-lock`.
#[derive(Args)]
pub(crate) struct CorpusPaths {
    /// Path to corpus YAML file. Its lock is the `.lock.json` beside it.
    #[arg(long, default_value = DEFAULT_CORPUS)]
    corpus: String,

    /// Path to partial sync overrides YAML, applied on top of the corpus for
    /// the partial scenario. Its tags are locked too.
    #[arg(long, default_value = "bench/corpus-partial-overrides.yaml")]
    partial_overrides: String,
}

impl CorpusPaths {
    /// The corpus YAML.
    pub(crate) fn corpus(&self) -> &str {
        &self.corpus
    }

    /// The partial sync overrides YAML.
    pub(crate) fn partial_overrides(&self) -> &str {
        &self.partial_overrides
    }

    /// The lock beside the corpus: `bench/corpus.yaml` is locked by
    /// `bench/corpus.lock.json`.
    pub(crate) fn lock_path(&self) -> PathBuf {
        lock_path(Path::new(&self.corpus))
    }

    /// The corpus this path names: the main corpus is the file
    /// `bench/corpus.yaml` however a path reaches it (`./bench/corpus.yaml`, an
    /// absolute path, a `..` path). Any other file, `experiments/corpus.yaml`
    /// included, is named by its stem. A corpus file that cannot be read is an error, as
    /// when it is loaded.
    pub(crate) fn corpus_id(&self) -> Result<CorpusId, String> {
        id_of(&self.corpus, Path::new(DEFAULT_CORPUS))
    }
}

/// The lock beside `corpus`: `bench/corpus.yaml` is locked by
/// `bench/corpus.lock.json`.
pub(crate) fn lock_path(corpus: &Path) -> PathBuf {
    corpus.with_extension("lock.json")
}

/// The corpus file at `corpus`: the main corpus when it is the file at `main`.
fn id_of(corpus: &str, main: &Path) -> Result<CorpusId, String> {
    let path = Path::new(corpus);
    let file = path
        .canonicalize()
        .map_err(|e| format!("failed to read corpus file {corpus}: {e}"))?;
    if main.canonicalize().is_ok_and(|main| main == file) {
        return Ok(CorpusId::Main);
    }
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| CorpusId::Named(CorpusName::new(stem)))
        .ok_or_else(|| format!("corpus file {corpus} has no file name to record"))
}

/// Docker Hub credentials for authenticated pulls (avoids 10 pulls/hr anon limit).
#[derive(Clone)]
pub(crate) struct DockerHubAuth {
    pub(crate) username: String,
    pub(crate) token: String,
}

impl std::fmt::Debug for DockerHubAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerHubAuth")
            .field("username", &self.username)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// The repository name a source flattens to under the target prefix: the host
/// dropped and `/` replaced with `-`.
fn flat_repo(source: &str) -> String {
    let path = match source.find('/') {
        Some(i) => &source[i + 1..],
        None => source,
    };
    path.replace('/', "-")
}

/// Top-level corpus configuration.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Corpus {
    /// Corpus-wide settings.
    #[serde(rename = "corpus")]
    pub(crate) settings: CorpusSettings,
    /// Image entries to sync.
    #[serde(rename = "images")]
    pub(crate) images: Vec<ImageEntry>,
    /// Docker Hub auth credentials (populated at runtime, not from YAML).
    #[serde(skip)]
    pub(crate) dockerhub_auth: Option<DockerHubAuth>,
}

/// Corpus-level settings (target registry, prefix).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CorpusSettings {
    /// Target registry hostname.
    pub(crate) target_registry: String,
    /// Prefix for all target repos.
    pub(crate) target_prefix: String,
}

/// A single image entry in the corpus.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct ImageEntry {
    /// Source image reference.
    pub(crate) source: String,
    /// Tags to sync.
    pub(crate) tags: Vec<String>,
}

/// Partial sync overrides -- tag replacements applied to a base corpus.
#[derive(Debug, Deserialize)]
pub(crate) struct PartialOverrides {
    /// Image entries whose tags replace the matching base corpus entry.
    pub(crate) overrides: Vec<ImageEntry>,
}

impl Corpus {
    /// Return a subset of the corpus (first `n` images).
    pub(crate) fn limit(&self, n: usize) -> Corpus {
        Corpus {
            settings: self.settings.clone(),
            images: self.images.iter().take(n).cloned().collect(),
            dockerhub_auth: self.dockerhub_auth.clone(),
        }
    }

    /// Derive the target repository name for a source image.
    ///
    /// `cgr.dev/chainguard/static` with prefix `bench` becomes `bench/chainguard-static`.
    /// `public.ecr.aws/amazonlinux/amazonlinux` with prefix `bench` becomes `bench/amazonlinux-amazonlinux`.
    /// `docker.io/library/alpine` with prefix `bench` becomes `bench/library-alpine`.
    pub(crate) fn target_repo(&self, source: &str) -> String {
        format!("{}/{}", self.settings.target_prefix, flat_repo(source))
    }

    /// Full target reference: `{registry}/{target_repo}`.
    pub(crate) fn target_ref(&self, source: &str) -> String {
        format!(
            "{}/{}",
            self.settings.target_registry,
            self.target_repo(source)
        )
    }

    /// Return a corpus with images from the given source registries removed.
    ///
    /// Registry matching uses the first path component of the source
    /// (e.g. `docker.io` from `docker.io/nvidia/cuda`).
    pub(crate) fn skip_registries(&self, registries: &[String]) -> Corpus {
        Corpus {
            settings: self.settings.clone(),
            images: self
                .images
                .iter()
                .filter(|img| {
                    let registry = img.source.split('/').next().unwrap_or("");
                    !registries.iter().any(|r| r == registry)
                })
                .cloned()
                .collect(),
            dockerhub_auth: self.dockerhub_auth.clone(),
        }
    }

    /// Total number of (image, tag) pairs in the corpus.
    pub(crate) fn total_tags(&self) -> usize {
        self.images.iter().map(|img| img.tags.len()).sum()
    }

    /// Returns a new corpus with overrides applied.
    ///
    /// Each override replaces the tags for the image with the matching source.
    /// Overrides that don't match any image in the corpus are ignored.
    pub(crate) fn with_overrides(&self, overrides: &[ImageEntry]) -> Corpus {
        let mut result = self.clone();
        for ovr in overrides {
            if let Some(img) = result.images.iter_mut().find(|i| i.source == ovr.source) {
                img.tags.clone_from(&ovr.tags);
            }
        }
        result
    }
}

/// The `overrides` that name a source of `images`; the rest are ignored.
pub(crate) fn overrides_naming<'a>(
    images: &'a [ImageEntry],
    overrides: &'a [ImageEntry],
) -> impl Iterator<Item = &'a ImageEntry> {
    overrides
        .iter()
        .filter(|ovr| images.iter().any(|img| img.source == ovr.source))
}

/// Load and parse a corpus YAML file.
///
/// Expands `${VAR}` environment variable references in the raw YAML before
/// deserialization. Returns an error if any referenced variable is not set.
pub(crate) fn load(path: &str) -> Result<Corpus, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read corpus file {path}: {e}"))?;
    let expanded = expand_env_vars(&raw).map_err(|e| format!("corpus file {path}: {e}"))?;
    let corpus: Corpus = serde_yaml::from_str(&expanded)
        .map_err(|e| format!("failed to parse corpus file {path}: {e}"))?;

    validate(&corpus)?;
    Ok(corpus)
}

/// Load partial sync overrides from a YAML file.
///
/// Expands `${VAR}` environment variable references before deserialization.
pub(crate) fn load_overrides(path: &str) -> Result<PartialOverrides, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read overrides file {path}: {e}"))?;
    let expanded = expand_env_vars(&raw).map_err(|e| format!("overrides file {path}: {e}"))?;
    serde_yaml::from_str(&expanded)
        .map_err(|e| format!("failed to parse overrides file {path}: {e}"))
}

/// Load only the image entries of a corpus YAML file.
///
/// Skips the corpus settings, whose `${BENCH_TARGET_REGISTRY}` only a run
/// against a target needs, so a command that only reads sources works anywhere.
/// Image entries are read as written, with no variable expansion.
pub(crate) fn load_images(path: &str) -> Result<Vec<ImageEntry>, String> {
    /// The part of a corpus file that names sources.
    #[derive(Deserialize)]
    struct Images {
        images: Vec<ImageEntry>,
    }

    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read corpus file {path}: {e}"))?;
    let parsed: Images = serde_yaml::from_str(&raw)
        .map_err(|e| format!("failed to parse corpus file {path}: {e}"))?;
    validate_images(&parsed.images)?;
    Ok(parsed.images)
}

/// Validates corpus invariants.
fn validate(corpus: &Corpus) -> Result<(), String> {
    if corpus.settings.target_registry.is_empty() {
        return Err("corpus target_registry must not be empty".into());
    }
    if corpus.settings.target_prefix.is_empty() {
        return Err("corpus target_prefix must not be empty".into());
    }
    validate_images(&corpus.images)
}

/// Validates that there are images, that each names a tag, and that no two
/// sources flatten to one target repository, which would sync into, reset and
/// verify one repository as if it were two.
fn validate_images(images: &[ImageEntry]) -> Result<(), String> {
    if images.is_empty() {
        return Err("corpus has no images".into());
    }
    let mut sources_by_repo = BTreeMap::new();
    for (i, img) in images.iter().enumerate() {
        if img.tags.is_empty() {
            return Err(format!("corpus image {i} ({}) has no tags", img.source));
        }
        match sources_by_repo.entry(flat_repo(&img.source)) {
            Entry::Occupied(first) => {
                return Err(format!(
                    "corpus sources {} and {} both map to target repository {}",
                    first.get(),
                    img.source,
                    first.key()
                ));
            }
            Entry::Vacant(slot) => {
                slot.insert(&img.source);
            }
        }
    }
    Ok(())
}

/// Expands `${VAR}` patterns in `input` using environment variables.
///
/// Returns an error if a referenced variable is not set. Only braced
/// `${...}` syntax is recognized; bare `$VAR` is left as-is.
fn expand_env_vars(input: &str) -> Result<String, String> {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut found_close = false;
            for c in chars.by_ref() {
                if c == '}' {
                    found_close = true;
                    break;
                }
                var_name.push(c);
            }
            if found_close {
                let val = std::env::var(&var_name)
                    .map_err(|_| format!("environment variable {var_name} is not set"))?;
                result.push_str(&val);
            } else {
                result.push_str("${");
                result.push_str(&var_name);
            }
        } else {
            result.push(c);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_yaml() -> &'static str {
        r#"
corpus:
  target_registry: "123456789.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: "bench"

images:
  - source: "cgr.dev/chainguard/static"
    tags: ["latest"]

  - source: "cgr.dev/chainguard/python"
    tags: ["latest", "3.12"]

  - source: "public.ecr.aws/amazonlinux/amazonlinux"
    tags: ["2023", "2"]

  - source: "docker.io/library/alpine"
    tags: ["3.20", "latest"]
"#
    }

    #[test]
    fn the_lock_sits_beside_the_corpus_it_locks() {
        let cases = [
            ("bench/corpus.yaml", "bench/corpus.lock.json"),
            ("elsewhere/small.yml", "elsewhere/small.lock.json"),
        ];
        for (corpus, want) in cases {
            let paths = CorpusPaths {
                corpus: corpus.into(),
                partial_overrides: String::new(),
            };
            assert_eq!(paths.lock_path(), PathBuf::from(want), "{corpus}");
        }
    }

    #[test]
    fn only_the_main_corpus_file_is_unnamed_however_its_path_is_spelled() {
        // Catches: a shape corpus's records, baseline and refusals carrying no
        // name; the main corpus reached by `./`, `..` or an absolute path
        // gaining one; another directory's `corpus.yaml` mistaken for the main
        // corpus; and a missing file named instead of reported.
        let main = format!("{BENCH_DIR}corpus.yaml");
        let elsewhere = tempfile::tempdir().unwrap();
        let same_stem = elsewhere.path().join("corpus.yaml");
        std::fs::write(&same_stem, "").unwrap();
        let same_stem = same_stem.to_str().unwrap();
        for (corpus, want) in [
            (main.as_str(), Ok(None)),
            (&format!("{BENCH_DIR}./corpus.yaml"), Ok(None)),
            (&format!("{BENCH_DIR}../bench/corpus.yaml"), Ok(None)),
            (
                &format!("{BENCH_DIR}corpus-small.yaml"),
                Ok(Some("corpus-small")),
            ),
            (same_stem, Ok(Some("corpus"))),
        ] {
            assert_eq!(
                id_of(corpus, Path::new(&main)),
                want.map(
                    |name| name.map_or(CorpusId::Main, |n| CorpusId::Named(CorpusName::new(n)))
                ),
                "{corpus}"
            );
        }
        let missing = id_of("experiments/corpus.yaml", Path::new(&main)).unwrap_err();
        assert!(
            missing.starts_with("failed to read corpus file experiments/corpus.yaml: "),
            "{missing}"
        );
    }

    #[test]
    fn parse_corpus_yaml() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        assert_eq!(corpus.images.len(), 4);
        assert_eq!(corpus.settings.target_prefix, "bench");
        assert_eq!(corpus.images[1].tags, vec!["latest", "3.12"]);
    }

    #[test]
    fn target_repo_derivation() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        assert_eq!(
            corpus.target_repo("cgr.dev/chainguard/static"),
            "bench/chainguard-static"
        );
        assert_eq!(
            corpus.target_repo("public.ecr.aws/amazonlinux/amazonlinux"),
            "bench/amazonlinux-amazonlinux"
        );
        assert_eq!(
            corpus.target_repo("docker.io/library/alpine"),
            "bench/library-alpine"
        );
    }

    #[test]
    fn target_ref_includes_registry_and_bare_name_gets_prefix() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        assert_eq!(
            corpus.target_ref("cgr.dev/chainguard/static"),
            "123456789.dkr.ecr.us-east-1.amazonaws.com/bench/chainguard-static"
        );
        assert_eq!(corpus.target_repo("nginx"), "bench/nginx");
    }

    #[test]
    fn limit_subset() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let small = corpus.limit(2);
        assert_eq!(small.images.len(), 2);
        assert_eq!(small.images[0].source, "cgr.dev/chainguard/static");
    }

    #[test]
    fn limit_zero_returns_empty() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let empty = corpus.limit(0);
        assert!(empty.images.is_empty());
    }

    #[test]
    fn limit_beyond_length_returns_all() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let all = corpus.limit(100);
        assert_eq!(all.images.len(), 4);
    }

    #[test]
    fn total_tags_count() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        assert_eq!(corpus.total_tags(), 7);
    }

    // -- Validation tests --

    #[test]
    fn validate_rejects_each_malformed_corpus() {
        for (name, yaml, expected) in [
            (
                "empty target_registry",
                r#"
corpus:
  target_registry: ""
  target_prefix: "bench"
images:
  - source: "cgr.dev/chainguard/static"
    tags: ["latest"]
"#,
                "corpus target_registry must not be empty",
            ),
            (
                "empty target_prefix",
                r#"
corpus:
  target_registry: "123.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: ""
images:
  - source: "cgr.dev/chainguard/static"
    tags: ["latest"]
"#,
                "corpus target_prefix must not be empty",
            ),
            (
                "no images",
                r#"
corpus:
  target_registry: "123.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: "bench"
images: []
"#,
                "corpus has no images",
            ),
            (
                "image without tags",
                r#"
corpus:
  target_registry: "123.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: "bench"
images:
  - source: "cgr.dev/chainguard/static"
    tags: []
"#,
                "corpus image 0 (cgr.dev/chainguard/static) has no tags",
            ),
            (
                "two sources flatten to one target repository",
                r#"
corpus:
  target_registry: "123.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: "bench"
images:
  - source: "docker.io/nvidia/cuda"
    tags: ["12.6.2-base-ubuntu22.04"]
  - source: "nvcr.io/nvidia/cuda"
    tags: ["13.4.3-base-ubuntu24.04"]
"#,
                "corpus sources docker.io/nvidia/cuda and nvcr.io/nvidia/cuda both map to \
                 target repository nvidia-cuda",
            ),
        ] {
            let corpus: Corpus = serde_yaml::from_str(yaml).unwrap();
            assert_eq!(validate(&corpus).unwrap_err(), expected, "{name}");
        }
    }

    #[test]
    fn the_images_alone_are_refused_when_two_flatten_to_one_repository() {
        // Catches: `bench-lock`, which reads only the images, locking a corpus
        // that every run would then refuse to load.
        let image = |source: &str| ImageEntry {
            source: source.to_owned(),
            tags: vec!["1".to_owned()],
        };
        assert_eq!(
            validate_images(&[image("docker.io/nvidia/cuda"), image("nvcr.io/nvidia/cuda")]),
            Err(
                "corpus sources docker.io/nvidia/cuda and nvcr.io/nvidia/cuda both map to \
                 target repository nvidia-cuda"
                    .to_owned()
            )
        );
        assert_eq!(
            validate_images(&[
                image("docker.io/nvidia/cuda"),
                image("nvcr.io/nvidia/cudnn")
            ]),
            Ok(())
        );
    }

    // -- Env var expansion tests --

    #[test]
    fn expand_env_vars_substitutes_set_variable() {
        // SAFETY: test-only, no concurrent access to this variable.
        unsafe { std::env::set_var("OCYNC_TEST_EXPAND", "replaced") };
        let result = expand_env_vars("before-${OCYNC_TEST_EXPAND}-after").unwrap();
        assert_eq!(result, "before-replaced-after");
        unsafe { std::env::remove_var("OCYNC_TEST_EXPAND") };
    }

    #[test]
    fn expand_env_vars_errors_on_unset_variable() {
        // SAFETY: test-only, no concurrent access to this variable.
        unsafe { std::env::remove_var("OCYNC_TEST_UNSET_VAR") };
        let err = expand_env_vars("${OCYNC_TEST_UNSET_VAR}").unwrap_err();
        assert_eq!(err, "environment variable OCYNC_TEST_UNSET_VAR is not set");
    }

    #[test]
    fn expand_env_vars_leaves_non_references_alone() {
        for (input, expected) in [("$PATH stays", "$PATH stays"), ("${UNCLOSED", "${UNCLOSED")] {
            assert_eq!(expand_env_vars(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn expand_env_vars_multiple_variables() {
        // SAFETY: test-only, no concurrent access to these variables.
        unsafe {
            std::env::set_var("OCYNC_TEST_A", "alpha");
            std::env::set_var("OCYNC_TEST_B", "beta");
        }
        let result = expand_env_vars("${OCYNC_TEST_A}-${OCYNC_TEST_B}").unwrap();
        assert_eq!(result, "alpha-beta");
        unsafe {
            std::env::remove_var("OCYNC_TEST_A");
            std::env::remove_var("OCYNC_TEST_B");
        }
    }

    // -- Skip registries tests --

    #[test]
    fn skip_registries_removes_matching_sources() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let filtered = corpus.skip_registries(&["docker.io".to_string()]);
        assert_eq!(filtered.images.len(), 3);
        assert!(
            filtered
                .images
                .iter()
                .all(|img| !img.source.starts_with("docker.io"))
        );
    }

    #[test]
    fn skip_registries_multiple() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let filtered = corpus.skip_registries(&["docker.io".to_string(), "cgr.dev".to_string()]);
        assert_eq!(filtered.images.len(), 1);
        assert_eq!(
            filtered.images[0].source,
            "public.ecr.aws/amazonlinux/amazonlinux"
        );
    }

    #[test]
    fn skip_registries_empty_keeps_all() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let filtered = corpus.skip_registries(&[]);
        assert_eq!(filtered.images.len(), corpus.images.len());
    }

    #[test]
    fn skip_registries_unmatched_keeps_all() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let filtered = corpus.skip_registries(&["quay.io".to_string()]);
        assert_eq!(filtered.images.len(), corpus.images.len());
    }

    // -- Partial overrides tests --

    #[test]
    fn with_overrides_replaces_matching_tags() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let overrides = vec![ImageEntry {
            source: "cgr.dev/chainguard/python".to_string(),
            tags: vec!["latest".to_string(), "3.11".to_string()],
        }];
        let partial = corpus.with_overrides(&overrides);
        assert_eq!(partial.images[1].tags, vec!["latest", "3.11"]);
        // Other images unchanged.
        assert_eq!(partial.images[0].tags, vec!["latest"]);
    }

    #[test]
    fn with_overrides_ignores_unmatched_source() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let overrides = vec![ImageEntry {
            source: "nonexistent/image".to_string(),
            tags: vec!["v1".to_string()],
        }];
        let partial = corpus.with_overrides(&overrides);
        assert_eq!(partial.images.len(), corpus.images.len());
        assert_eq!(partial.images[0].tags, corpus.images[0].tags);
    }

    #[test]
    fn with_overrides_applies_multiple_simultaneously() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        let overrides = vec![
            ImageEntry {
                source: "cgr.dev/chainguard/static".to_string(),
                tags: vec!["v1.0".to_string()],
            },
            ImageEntry {
                source: "docker.io/library/alpine".to_string(),
                tags: vec!["edge".to_string()],
            },
        ];
        let partial = corpus.with_overrides(&overrides);
        assert_eq!(partial.images[0].tags, vec!["v1.0"]);
        assert_eq!(partial.images[3].tags, vec!["edge"]);
        // Untouched images remain unchanged.
        assert_eq!(partial.images[1].tags, vec!["latest", "3.12"]);
        assert_eq!(partial.images[2].tags, vec!["2023", "2"]);
    }

    /// The prepare corpus is only worth running if it defeats the exact-tag
    /// fast path.
    ///
    /// `TagsConfig::exact_tags` returns the tags verbatim when every pattern
    /// is a literal, and `resolve_mapping_inner` then skips `list_tags`
    /// entirely. A prepare corpus pinned to literal tags would run clean, take
    /// a plausible-looking time, and measure none of the phase it is named
    /// for. The main corpus is exactly that shape, which is why this one
    /// exists.
    #[test]
    fn prepare_corpus_tags_are_all_wildcards() {
        let corpus = tracked_corpus("corpus-prepare.yaml");

        assert!(
            corpus.images.len() >= 20,
            "the phase costs one round trip per mapping, so a narrow corpus \
             cannot show it; got {} mappings",
            corpus.images.len()
        );

        let literal: Vec<&str> = corpus
            .images
            .iter()
            .flat_map(|img| img.tags.iter())
            .filter(|tag| !tag.contains(['*', '?', '[']))
            .map(String::as_str)
            .collect();
        assert!(
            literal.is_empty(),
            "every prepare-corpus tag must be a wildcard or the tag listing is \
             never issued; these are literals: {literal:?}"
        );
    }

    /// The tracked `bench/` directory.
    const BENCH_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../bench/");

    /// The partial-overrides file, which sits among the corpora but is not one.
    const PARTIAL_OVERRIDES: &str = "corpus-partial-overrides.yaml";

    /// A tracked corpus, parsed and validated as `load` does but with its
    /// `${VAR}` references left as they are, so no variable need be set.
    fn tracked_corpus(file: &str) -> Corpus {
        let raw = std::fs::read_to_string(format!("{BENCH_DIR}{file}")).unwrap();
        let corpus: Corpus = serde_yaml::from_str(&raw).unwrap();
        validate(&corpus).unwrap();
        corpus
    }

    #[test]
    fn the_shape_corpora_are_locked_and_off_docker_hub() {
        // Catches: a shape corpus edited without re-locking, so its first run
        // is refused, and a Docker Hub source spending its pull budget.
        for name in ["corpus-small", "corpus-large"] {
            let corpus = tracked_corpus(&format!("{name}.yaml"));
            let lock =
                crate::bench::lock::Lock::load(Path::new(&format!("{BENCH_DIR}{name}.lock.json")))
                    .expect("the lock parses");

            let unlocked: Vec<String> = crate::bench::lock::refs_of(&corpus.images)
                .into_iter()
                .filter(|image| lock.entry(&image.source, &image.tag).is_err())
                .map(|image| image.to_string())
                .collect();
            assert!(unlocked.is_empty(), "{name}: not in its lock: {unlocked:?}");

            let hub: Vec<&str> = corpus
                .images
                .iter()
                .map(|img| img.source.as_str())
                .filter(|source| {
                    crate::bench::registry::split_source(source)
                        .is_some_and(|repo| repo.host == crate::bench::registry::Host::DockerHub)
                })
                .collect();
            assert!(hub.is_empty(), "{name}: Docker Hub sources: {hub:?}");
        }
    }

    #[test]
    fn no_two_tracked_corpora_share_a_target_prefix() {
        // Catches: a corpus whose reset deletes the repositories of another
        // that shares its prefix, and a tracked corpus with a repeated target
        // repository.
        let mut prefixes = BTreeMap::new();
        let mut files: Vec<String> = std::fs::read_dir(BENCH_DIR)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| {
                name.starts_with("corpus") && name.ends_with(".yaml") && name != PARTIAL_OVERRIDES
            })
            .collect();
        files.sort();
        assert!(files.len() >= 4, "corpora not found: {files:?}");
        for file in files {
            let prefix = tracked_corpus(&file).settings.target_prefix;
            if let Some(other) = prefixes.insert(prefix.clone(), file.clone()) {
                panic!("{file} and {other} share the prefix {prefix}");
            }
        }
    }

    #[test]
    fn dockerhub_auth_debug_redacts_token() {
        let auth = DockerHubAuth {
            username: "bench".to_string(),
            token: "dckr_pat_secret".to_string(),
        };
        assert_eq!(
            format!("{auth:?}"),
            r#"DockerHubAuth { username: "bench", token: "[REDACTED]" }"#
        );
    }
}
