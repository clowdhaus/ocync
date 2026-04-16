//! Generate equivalent config files for ocync, dregsy, and regsync from a corpus.

use std::collections::BTreeMap;

use crate::bench::corpus::{Corpus, ImageEntry};

/// Extract the registry hostname from a source image reference.
///
/// `cgr.dev/chainguard/static` → `cgr.dev`
/// `docker.io/library/alpine` → `docker.io`
fn source_registry(source: &str) -> &str {
    match source.find('/') {
        Some(i) => &source[..i],
        None => source,
    }
}

/// Extract the image path (without registry) from a source image reference.
///
/// `cgr.dev/chainguard/static` → `chainguard/static`
/// `docker.io/library/alpine` → `library/alpine`
fn source_path(source: &str) -> &str {
    match source.find('/') {
        Some(i) => &source[i + 1..],
        None => source,
    }
}

/// Sanitize a registry hostname into a valid YAML key.
///
/// `cgr.dev` → `cgr-dev`
/// `123456.dkr.ecr.us-east-1.amazonaws.com` → `ecr`
fn registry_key(hostname: &str) -> String {
    if hostname.contains(".dkr.ecr.") {
        "ecr".to_string()
    } else {
        hostname.replace('.', "-")
    }
}

/// Generate an ocync YAML config from a corpus.
///
/// Produces a `registries:` section with unique source registries (anonymous auth)
/// and the target ECR registry (ecr auth), followed by a `mappings:` section.
/// Each mapping uses `source:` and `targets:` to reference registries by name,
/// and `tags: glob:` with a list of exact tag globs.
pub(crate) fn ocync_config(corpus: &Corpus) -> String {
    let mut out = String::new();

    // Collect unique source registries in deterministic order.
    let mut source_registries: Vec<&str> = corpus
        .images
        .iter()
        .map(|img| source_registry(&img.source))
        .collect();
    source_registries.sort_unstable();
    source_registries.dedup();

    let ecr_key = registry_key(&corpus.settings.target_registry);

    out.push_str("registries:\n");
    for reg in &source_registries {
        let key = registry_key(reg);
        out.push_str(&format!("  {key}:\n"));
        out.push_str(&format!("    url: {reg}\n"));
    }
    out.push_str(&format!("  {ecr_key}:\n"));
    out.push_str(&format!("    url: {}\n", corpus.settings.target_registry));
    out.push_str("    auth_type: ecr\n");

    out.push('\n');
    out.push_str("mappings:\n");
    for img in &corpus.images {
        let src_key = registry_key(source_registry(&img.source));
        let path = source_path(&img.source);
        let target_repo = corpus.target_repo(&img.source);
        out.push_str(&format!("  - from: {path}\n"));
        out.push_str(&format!("    to: {target_repo}\n"));
        out.push_str(&format!("    source: {src_key}\n"));
        out.push_str(&format!("    targets: [{ecr_key}]\n"));
        out.push_str("    tags:\n");
        out.push_str("      glob:\n");
        for tag in &img.tags {
            out.push_str(&format!("        - \"{tag}\"\n"));
        }
    }

    out
}

/// Generate a dregsy YAML config from a corpus.
///
/// Produces `relay: skopeo` followed by `tasks:` grouped by source registry in deterministic
/// order. Each task contains source/target registry fields and a flat mappings list.
///
/// The ECR target gets `auth-refresh: 12h` so dregsy uses its internal AWS SDK refresher
/// (via `GetAuthorizationToken`) instead of falling through to skopeo's Docker credential
/// resolution. 12h matches the ECR token lifetime to minimize refresh overhead.
pub(crate) fn dregsy_config(corpus: &Corpus) -> String {
    // Group images by source registry. BTreeMap gives deterministic iteration order.
    let mut by_registry: BTreeMap<&str, Vec<&ImageEntry>> = BTreeMap::new();
    for img in &corpus.images {
        let reg = source_registry(&img.source);
        by_registry.entry(reg).or_default().push(img);
    }

    let mut out = String::new();
    out.push_str("relay: skopeo\n");
    out.push('\n');
    out.push_str("tasks:\n");

    for (registry, images) in &by_registry {
        let task_name = format!("sync-{}", registry.replace('.', "-"));
        out.push_str(&format!("  - name: {task_name}\n"));
        out.push_str("    source:\n");
        out.push_str(&format!("      registry: {registry}\n"));
        out.push_str("    target:\n");
        out.push_str(&format!(
            "      registry: {}\n",
            corpus.settings.target_registry
        ));
        // ECR: use dregsy's built-in AWS SDK auth refresher (12h matches token lifetime).
        out.push_str("      auth-refresh: 12h\n");
        out.push_str("    mappings:\n");
        for img in images {
            let from = source_path(&img.source);
            let to = corpus.target_repo(&img.source);
            out.push_str(&format!("      - from: {from}\n"));
            out.push_str(&format!("        to: {to}\n"));
            out.push_str("        tags:\n");
            for tag in &img.tags {
                out.push_str(&format!("          - \"{tag}\"\n"));
            }
        }
    }

    out
}

/// Generate a regsync YAML config from a corpus.
///
/// Produces `version: 1`, a `creds:` section with `repoAuth: true` for each source
/// registry (required for registries that issue per-repo scoped tokens like cgr.dev,
/// gcr.io, nvcr.io — see regclient issue #1060) and `credHelper: docker-credential-ecr-login`
/// for the ECR target, followed by a flat `sync:` list.
pub(crate) fn regsync_config(corpus: &Corpus) -> String {
    // Collect unique source registries in deterministic order.
    let mut source_registries: Vec<&str> = corpus
        .images
        .iter()
        .map(|img| source_registry(&img.source))
        .collect();
    source_registries.sort_unstable();
    source_registries.dedup();

    let mut out = String::new();
    out.push_str("version: 1\n");
    out.push('\n');
    out.push_str("creds:\n");
    for reg in &source_registries {
        out.push_str(&format!("  - registry: {reg}\n"));
        // Per-repo auth: required for registries that issue per-scope tokens and
        // reject multi-scope requests (cgr.dev, gcr.io, nvcr.io).
        out.push_str("    repoAuth: true\n");
    }
    out.push_str(&format!(
        "  - registry: {}\n",
        corpus.settings.target_registry
    ));
    out.push_str("    credHelper: docker-credential-ecr-login\n");

    out.push('\n');
    out.push_str("sync:\n");

    for img in &corpus.images {
        let target = corpus.target_ref(&img.source);
        out.push_str(&format!("  - source: \"{}\"\n", img.source));
        out.push_str(&format!("    target: \"{target}\"\n"));
        out.push_str("    type: image\n");
        out.push_str("    tags:\n");
        out.push_str("      allow:\n");
        for tag in &img.tags {
            out.push_str(&format!("        - \"{tag}\"\n"));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_corpus() -> Corpus {
        let yaml = r#"
corpus:
  target_registry: "111111111111.dkr.ecr.us-east-1.amazonaws.com"
  target_prefix: "bench"

images:
  - source: "cgr.dev/chainguard/static"
    tags: ["latest"]

  - source: "docker.io/library/alpine"
    tags: ["3.20", "latest"]
"#;
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn ocync_config_exact_output() {
        let config = ocync_config(&test_corpus());
        let expected = "\
registries:
  cgr-dev:
    url: cgr.dev
  docker-io:
    url: docker.io
  ecr:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr

mappings:
  - from: chainguard/static
    to: bench/chainguard-static
    source: cgr-dev
    targets: [ecr]
    tags:
      glob:
        - \"latest\"
  - from: library/alpine
    to: bench/library-alpine
    source: docker-io
    targets: [ecr]
    tags:
      glob:
        - \"3.20\"
        - \"latest\"
";
        assert_eq!(config, expected);
    }

    #[test]
    fn dregsy_config_exact_output() {
        let config = dregsy_config(&test_corpus());
        let expected = "\
relay: skopeo

tasks:
  - name: sync-cgr-dev
    source:
      registry: cgr.dev
    target:
      registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
      auth-refresh: 12h
    mappings:
      - from: chainguard/static
        to: bench/chainguard-static
        tags:
          - \"latest\"
  - name: sync-docker-io
    source:
      registry: docker.io
    target:
      registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
      auth-refresh: 12h
    mappings:
      - from: library/alpine
        to: bench/library-alpine
        tags:
          - \"3.20\"
          - \"latest\"
";
        assert_eq!(config, expected);
    }

    #[test]
    fn regsync_config_exact_output() {
        let config = regsync_config(&test_corpus());
        let expected = "\
version: 1

creds:
  - registry: cgr.dev
    repoAuth: true
  - registry: docker.io
    repoAuth: true
  - registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
    credHelper: docker-credential-ecr-login

sync:
  - source: \"cgr.dev/chainguard/static\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/chainguard-static\"
    type: image
    tags:
      allow:
        - \"latest\"
  - source: \"docker.io/library/alpine\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/library-alpine\"
    type: image
    tags:
      allow:
        - \"3.20\"
        - \"latest\"
";
        assert_eq!(config, expected);
    }

    #[test]
    fn registry_key_ecr_shortens() {
        assert_eq!(
            registry_key("111111111111.dkr.ecr.us-east-1.amazonaws.com"),
            "ecr"
        );
    }

    #[test]
    fn registry_key_non_ecr_replaces_dots() {
        assert_eq!(registry_key("cgr.dev"), "cgr-dev");
        assert_eq!(registry_key("docker.io"), "docker-io");
        assert_eq!(registry_key("public.ecr.aws"), "public-ecr-aws");
    }
}
