//! YAML corpus config parsing with environment variable expansion.

use serde::Deserialize;

/// Top-level corpus configuration.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Corpus {
    /// Corpus-wide settings.
    #[serde(rename = "corpus")]
    pub(crate) settings: CorpusSettings,
    /// Image entries to sync.
    #[serde(rename = "images")]
    pub(crate) images: Vec<ImageEntry>,
}

/// Corpus-level settings (target registry, prefix).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CorpusSettings {
    /// ECR target registry hostname.
    pub(crate) target_registry: String,
    /// Prefix for all target repos.
    pub(crate) target_prefix: String,
}

/// A single image entry in the corpus.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ImageEntry {
    /// Source image reference.
    pub(crate) source: String,
    /// Tags to sync.
    pub(crate) tags: Vec<String>,
}

/// Partial sync overrides — tag replacements applied to a base corpus.
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
        }
    }

    /// Derive the target repository name for a source image.
    ///
    /// `cgr.dev/chainguard/static` with prefix `bench` becomes `bench/chainguard-static`.
    /// `public.ecr.aws/amazonlinux/amazonlinux` with prefix `bench` becomes `bench/amazonlinux-amazonlinux`.
    /// `docker.io/library/alpine` with prefix `bench` becomes `bench/library-alpine`.
    pub(crate) fn target_repo(&self, source: &str) -> String {
        let path = match source.find('/') {
            Some(i) => &source[i + 1..],
            None => source,
        };
        let flat = path.replace('/', "-");
        format!("{}/{flat}", self.settings.target_prefix)
    }

    /// Full target reference: `{registry}/{target_repo}`.
    pub(crate) fn target_ref(&self, source: &str) -> String {
        format!(
            "{}/{}",
            self.settings.target_registry,
            self.target_repo(source)
        )
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

/// Validates corpus invariants.
fn validate(corpus: &Corpus) -> Result<(), String> {
    if corpus.settings.target_registry.is_empty() {
        return Err("corpus target_registry must not be empty".into());
    }
    if corpus.settings.target_prefix.is_empty() {
        return Err("corpus target_prefix must not be empty".into());
    }
    if corpus.images.is_empty() {
        return Err("corpus has no images".into());
    }
    for (i, img) in corpus.images.iter().enumerate() {
        if img.tags.is_empty() {
            return Err(format!("corpus image {i} ({}) has no tags", img.source));
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
                // Unclosed brace — emit literally.
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
    fn target_ref_includes_registry() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
        assert_eq!(
            corpus.target_ref("cgr.dev/chainguard/static"),
            "123456789.dkr.ecr.us-east-1.amazonaws.com/bench/chainguard-static"
        );
    }

    #[test]
    fn target_repo_bare_name_no_slash() {
        let corpus: Corpus = serde_yaml::from_str(sample_yaml()).unwrap();
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
    fn validate_rejects_empty_target_registry() {
        let yaml = r#"
corpus:
  target_registry: ""
  target_prefix: "bench"
images:
  - source: "cgr.dev/chainguard/static"
    tags: ["latest"]
"#;
        let corpus: Corpus = serde_yaml::from_str(yaml).unwrap();
        let err = validate(&corpus).unwrap_err();
        assert_eq!(err, "corpus target_registry must not be empty");
    }

    #[test]
    fn validate_rejects_empty_target_prefix() {
        let yaml = r#"
corpus:
  target_registry: "123.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: ""
images:
  - source: "cgr.dev/chainguard/static"
    tags: ["latest"]
"#;
        let corpus: Corpus = serde_yaml::from_str(yaml).unwrap();
        let err = validate(&corpus).unwrap_err();
        assert_eq!(err, "corpus target_prefix must not be empty");
    }

    #[test]
    fn validate_rejects_empty_images() {
        let yaml = r#"
corpus:
  target_registry: "123.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: "bench"
images: []
"#;
        let corpus: Corpus = serde_yaml::from_str(yaml).unwrap();
        let err = validate(&corpus).unwrap_err();
        assert_eq!(err, "corpus has no images");
    }

    #[test]
    fn validate_rejects_image_with_no_tags() {
        let yaml = r#"
corpus:
  target_registry: "123.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: "bench"
images:
  - source: "cgr.dev/chainguard/static"
    tags: []
"#;
        let corpus: Corpus = serde_yaml::from_str(yaml).unwrap();
        let err = validate(&corpus).unwrap_err();
        assert_eq!(
            err,
            "corpus image 0 (cgr.dev/chainguard/static) has no tags"
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
    fn expand_env_vars_leaves_bare_dollar_alone() {
        let result = expand_env_vars("$PATH stays").unwrap();
        assert_eq!(result, "$PATH stays");
    }

    #[test]
    fn expand_env_vars_unclosed_brace_emitted_literally() {
        let result = expand_env_vars("${UNCLOSED").unwrap();
        assert_eq!(result, "${UNCLOSED");
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
}
