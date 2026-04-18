//! Generate equivalent config files for ocync, dregsy, and regsync from a corpus.

use std::collections::BTreeMap;

use crate::bench::corpus::{Corpus, ImageEntry};

/// Base64-encode a byte slice using the standard alphabet with padding.
///
/// Hand-rolled to avoid adding a `base64` crate dependency for a single
/// call site (dregsy auth field).
pub(crate) fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

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
        // Docker Hub: inject basic auth to avoid 10 pulls/hr anonymous limit.
        if *reg == "docker.io" {
            if let Some(auth) = &corpus.dockerhub_auth {
                out.push_str("    auth_type: basic\n");
                out.push_str("    credentials:\n");
                out.push_str(&format!("      username: {}\n", auth.username));
                out.push_str(&format!("      password: {}\n", auth.token));
            }
        }
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
        // Docker Hub: inject base64-encoded auth to avoid 10 pulls/hr anonymous limit.
        if *registry == "docker.io" {
            if let Some(auth) = &corpus.dockerhub_auth {
                let encoded = base64_encode(&format!("{}:{}", auth.username, auth.token));
                out.push_str(&format!("      auth: {encoded}\n"));
            }
        }
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
            // Deep-copy all platforms so dregsy syncs the full manifest
            // list, matching ocync and regsync behavior. Without this,
            // skopeo copies only the native platform (single manifest PUT)
            // and the comparison is not apples-to-apples.
            out.push_str("        platform: all\n");
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
/// gcr.io, nvcr.io -- see regclient issue #1060) and `credHelper: docker-credential-ecr-login`
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
        // Docker Hub: inject user/pass to avoid 10 pulls/hr anonymous limit.
        if *reg == "docker.io" {
            if let Some(auth) = &corpus.dockerhub_auth {
                out.push_str(&format!("    user: {}\n", auth.username));
                out.push_str(&format!("    pass: {}\n", auth.token));
            }
        }
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
        platform: all
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
        platform: all
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

    fn test_corpus_with_dockerhub_auth() -> Corpus {
        use crate::bench::corpus::DockerHubAuth;
        let mut corpus = test_corpus();
        corpus.dockerhub_auth = Some(DockerHubAuth {
            username: "ocync-bench".to_string(),
            token: "dkr_pat_abc123".to_string(),
        });
        corpus
    }

    #[test]
    fn ocync_config_with_dockerhub_auth() {
        let config = ocync_config(&test_corpus_with_dockerhub_auth());
        let expected = "\
registries:
  cgr-dev:
    url: cgr.dev
  docker-io:
    url: docker.io
    auth_type: basic
    credentials:
      username: ocync-bench
      password: dkr_pat_abc123
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
    fn dregsy_config_with_dockerhub_auth() {
        let config = dregsy_config(&test_corpus_with_dockerhub_auth());
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
        platform: all
        tags:
          - \"latest\"
  - name: sync-docker-io
    source:
      registry: docker.io
      auth: b2N5bmMtYmVuY2g6ZGtyX3BhdF9hYmMxMjM=
    target:
      registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
      auth-refresh: 12h
    mappings:
      - from: library/alpine
        to: bench/library-alpine
        platform: all
        tags:
          - \"3.20\"
          - \"latest\"
";
        assert_eq!(config, expected);
    }

    #[test]
    fn regsync_config_with_dockerhub_auth() {
        let config = regsync_config(&test_corpus_with_dockerhub_auth());
        let expected = "\
version: 1

creds:
  - registry: cgr.dev
    repoAuth: true
  - registry: docker.io
    repoAuth: true
    user: ocync-bench
    pass: dkr_pat_abc123
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
    fn base64_encode_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("f"), "Zg==");
        assert_eq!(base64_encode("fo"), "Zm8=");
        assert_eq!(base64_encode("foo"), "Zm9v");
        assert_eq!(base64_encode("foob"), "Zm9vYg==");
        assert_eq!(base64_encode("fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode("foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_auth_string() {
        // Verify the exact encoding used in dregsy auth field.
        assert_eq!(
            base64_encode("ocync-bench:dkr_pat_abc123"),
            "b2N5bmMtYmVuY2g6ZGtyX3BhdF9hYmMxMjM="
        );
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
