//! Generate equivalent config files for ocync, dregsy, and regsync from a corpus.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;

use ocync_distribution::Digest;

use crate::bench::RegistryKind;
use crate::bench::corpus::{Corpus, ImageEntry};
use crate::bench::lock::Lock;
use crate::bench::target::Kind;

/// What a generated config syncs.
#[derive(Clone, Copy)]
pub(crate) enum Mode<'a> {
    /// Each corpus tag from the digest `lock` pins it to, so a source tag that
    /// has moved cannot change what is copied.
    Sync(&'a Lock),
    /// Resolve wildcard tags and transfer nothing, which no lock describes.
    Prepare,
}

/// The digest `lock` pins `tag` of `source` to. `ensure_served` refuses a ref
/// the lock lacks before any config is written, as `blobs_to_warm` relies on.
fn locked_digest<'a>(lock: &'a Lock, source: &str, tag: &str) -> &'a Digest {
    &lock
        .entry(source, tag)
        .expect("ensure_served refuses a ref the lock lacks")
        .digest
}

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
    if RegistryKind::of(hostname) == RegistryKind::Ecr {
        "ecr".to_string()
    } else {
        hostname.replace('.', "-")
    }
}

/// Generate an ocync YAML config from a corpus.
///
/// Produces a `registries:` section with unique source registries (anonymous auth)
/// and the target registry (signed in to as `target` says), followed by a `mappings:` section.
/// Each mapping uses `source:` and `targets:` to reference registries by name,
/// and `tags:`. For [`Mode::Sync`], `tags:` pins each tag to its locked digest; for
/// [`Mode::Prepare`] it is a `glob:` of the corpus tags.
///
/// A `max_concurrent_transfers` writes a leading `global:` block carrying it;
/// without one the config has no `global:` block and ocync keeps its default.
pub(crate) fn ocync_config(
    corpus: &Corpus,
    target: Kind,
    max_concurrent_transfers: Option<NonZeroUsize>,
    mode: Mode<'_>,
) -> String {
    let mut out = String::new();

    if let Some(cap) = max_concurrent_transfers {
        out.push_str(&format!("global:\n  max_concurrent_transfers: {cap}\n\n"));
    }

    let mut source_registries: Vec<&str> = corpus
        .images
        .iter()
        .map(|img| source_registry(&img.source))
        .collect();
    source_registries.sort_unstable();
    source_registries.dedup();

    let target_key = registry_key(&corpus.settings.target_registry);

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
    out.push_str(&format!("  {target_key}:\n"));
    out.push_str(&format!("    url: {}\n", corpus.settings.target_registry));
    out.push_str(&format!("    auth_type: {}\n", target.auth_type()));

    out.push('\n');
    out.push_str("mappings:\n");
    for img in &corpus.images {
        let src_key = registry_key(source_registry(&img.source));
        let path = source_path(&img.source);
        let target_repo = corpus.target_repo(&img.source);
        out.push_str(&format!("  - from: \"{path}\"\n"));
        out.push_str(&format!("    to: \"{target_repo}\"\n"));
        out.push_str(&format!("    source: {src_key}\n"));
        out.push_str(&format!("    targets: [{target_key}]\n"));
        out.push_str("    tags:\n");
        match mode {
            Mode::Sync(lock) => {
                for tag in &img.tags {
                    let digest = locked_digest(lock, &img.source, tag);
                    out.push_str(&format!("      - name: \"{tag}\"\n"));
                    out.push_str(&format!("        digest: \"{digest}\"\n"));
                }
            }
            Mode::Prepare => {
                out.push_str("      glob:\n");
                for tag in &img.tags {
                    out.push_str(&format!("        - \"{tag}\"\n"));
                }
            }
        }
    }

    out
}

/// Generate a dregsy YAML config from a corpus.
///
/// Produces `relay: skopeo` followed by `tasks:` grouped by source registry in deterministic
/// order. Each task contains source/target registry fields and a flat mappings list.
/// Each tag is written `<tag>@<digest>` from `lock`: dregsy pulls the digest and creates
/// `<tag>` at the target, so a moved source tag cannot change what is copied. `skopeo` is
/// the binary dregsy runs as skopeo.
///
/// The ECR target gets `auth-refresh: 12h` so dregsy uses its internal AWS SDK refresher
/// (via `GetAuthorizationToken`) instead of falling through to skopeo's Docker credential
/// resolution. 12h matches the ECR token lifetime to minimize refresh overhead.
pub(crate) fn dregsy_config(corpus: &Corpus, lock: &Lock, skopeo: &Path) -> String {
    // Group images by source registry. BTreeMap gives deterministic iteration order.
    let mut by_registry: BTreeMap<&str, Vec<&ImageEntry>> = BTreeMap::new();
    for img in &corpus.images {
        let reg = source_registry(&img.source);
        by_registry.entry(reg).or_default().push(img);
    }

    let mut out = String::new();
    out.push_str("relay: skopeo\n");
    out.push('\n');
    out.push_str(&format!("skopeo:\n  binary: \"{}\"\n", skopeo.display()));
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
            out.push_str(&format!("      - from: \"{from}\"\n"));
            out.push_str(&format!("        to: \"{to}\"\n"));
            // Deep-copy all platforms so dregsy syncs the full manifest
            // list, matching ocync and regsync behavior. Without this,
            // skopeo copies only the native platform (single manifest PUT)
            // and the comparison is not apples-to-apples.
            out.push_str("        platform: all\n");
            out.push_str("        tags:\n");
            for tag in &img.tags {
                let digest = locked_digest(lock, &img.source, tag);
                out.push_str(&format!("          - \"{tag}@{digest}\"\n"));
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
/// for the ECR target, followed by a flat `sync:` list. Each entry copies the source at its
/// locked digest from `lock` to the corpus tag, so a moved source tag cannot change what is
/// copied.
pub(crate) fn regsync_config(corpus: &Corpus, lock: &Lock) -> String {
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

    // Emit one entry per (image, tag) pair with `type: image`, the source named by
    // digest and the target by tag. `type: repository` enumerates ALL tags (1900+
    // pages on quay.io/jupyter) and then filters, which is extremely slow and can
    // skip images due to Docker v1 manifest filtering. `type: image` with an
    // explicit reference avoids both problems.
    //
    // Explicit `mediaTypes` is required for quay.io: without it, regclient
    // receives Docker v1 manifests (quay.io's default when no Accept header
    // specifies v2) and skips the image entirely.
    for img in &corpus.images {
        for tag in &img.tags {
            let digest = locked_digest(lock, &img.source, tag);
            let source_ref = format!("{}@{digest}", img.source);
            let target_ref = format!("{}:{}", corpus.target_ref(&img.source), tag);
            out.push_str(&format!("  - source: \"{source_ref}\"\n"));
            out.push_str(&format!("    target: \"{target_ref}\"\n"));
            out.push_str("    type: image\n");
            out.push_str("    mediaTypes:\n");
            out.push_str("      - \"application/vnd.docker.distribution.manifest.v2+json\"\n");
            out.push_str("      - \"application/vnd.docker.distribution.manifest.list.v2+json\"\n");
            out.push_str("      - \"application/vnd.oci.image.manifest.v1+json\"\n");
            out.push_str("      - \"application/vnd.oci.image.index.v1+json\"\n");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::lock::fixtures::{entry, lock_of};

    /// Pins each of `test_corpus`'s tags to a digest of its own: `1`s, `2`s, `3`s.
    fn test_lock() -> Lock {
        lock_of(&[
            ("cgr.dev/chainguard/static:latest", entry('1', &[])),
            ("docker.io/library/alpine:3.20", entry('2', &[])),
            ("docker.io/library/alpine:latest", entry('3', &[])),
        ])
    }

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
    fn a_transfer_cap_prepends_a_global_block_and_no_cap_writes_none() {
        // Catches: the cap silently dropped (ocync then runs at its default
        // and the measurement is of the wrong cap), and a `global:` block
        // written when none was asked for.
        let (corpus, lock) = (test_corpus(), test_lock());
        let config = |cap| ocync_config(&corpus, Kind::Ecr, cap, Mode::Sync(&lock));
        assert_eq!(
            config(NonZeroUsize::new(20)),
            format!(
                "global:\n  max_concurrent_transfers: 20\n\n{}",
                config(None)
            )
        );
        assert!(!config(None).contains("global:"));
    }

    #[test]
    fn generated_configs_match_exactly() {
        use crate::bench::corpus::DockerHubAuth;

        let plain = test_corpus();
        let lock = test_lock();
        let mut with_auth = test_corpus();
        with_auth.dockerhub_auth = Some(DockerHubAuth {
            username: "ocync-bench".to_string(),
            token: "dkr_pat_abc123".to_string(),
        });

        let mut gar = test_corpus();
        gar.settings.target_registry = "europe-west9-docker.pkg.dev".to_string();
        gar.settings.target_prefix = "example-project/run-r1/bench".to_string();

        for (name, config, expected) in [
            (
                "ocync_config_artifact_registry",
                ocync_config(&gar, Kind::ArtifactRegistry, None, Mode::Sync(&lock)),
                "\
registries:
  cgr-dev:
    url: cgr.dev
  docker-io:
    url: docker.io
  europe-west9-docker-pkg-dev:
    url: europe-west9-docker.pkg.dev
    auth_type: gar

mappings:
  - from: \"chainguard/static\"
    to: \"example-project/run-r1/bench/chainguard-static\"
    source: cgr-dev
    targets: [europe-west9-docker-pkg-dev]
    tags:
      - name: \"latest\"
        digest: \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"
  - from: \"library/alpine\"
    to: \"example-project/run-r1/bench/library-alpine\"
    source: docker-io
    targets: [europe-west9-docker-pkg-dev]
    tags:
      - name: \"3.20\"
        digest: \"sha256:2222222222222222222222222222222222222222222222222222222222222222\"
      - name: \"latest\"
        digest: \"sha256:3333333333333333333333333333333333333333333333333333333333333333\"
",
            ),
            (
                "ocync_config_exact_output",
                ocync_config(&plain, Kind::Ecr, None, Mode::Sync(&lock)),
                "\
registries:
  cgr-dev:
    url: cgr.dev
  docker-io:
    url: docker.io
  ecr:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr

mappings:
  - from: \"chainguard/static\"
    to: \"bench/chainguard-static\"
    source: cgr-dev
    targets: [ecr]
    tags:
      - name: \"latest\"
        digest: \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"
  - from: \"library/alpine\"
    to: \"bench/library-alpine\"
    source: docker-io
    targets: [ecr]
    tags:
      - name: \"3.20\"
        digest: \"sha256:2222222222222222222222222222222222222222222222222222222222222222\"
      - name: \"latest\"
        digest: \"sha256:3333333333333333333333333333333333333333333333333333333333333333\"
",
            ),
            (
                // The prepare scenario transfers nothing and resolves wildcards, so
                // it has no lock and keeps the tag globs.
                "ocync_config_without_lock",
                ocync_config(&plain, Kind::Ecr, None, Mode::Prepare),
                "\
registries:
  cgr-dev:
    url: cgr.dev
  docker-io:
    url: docker.io
  ecr:
    url: 111111111111.dkr.ecr.us-east-1.amazonaws.com
    auth_type: ecr

mappings:
  - from: \"chainguard/static\"
    to: \"bench/chainguard-static\"
    source: cgr-dev
    targets: [ecr]
    tags:
      glob:
        - \"latest\"
  - from: \"library/alpine\"
    to: \"bench/library-alpine\"
    source: docker-io
    targets: [ecr]
    tags:
      glob:
        - \"3.20\"
        - \"latest\"
",
            ),
            (
                "dregsy_config_exact_output",
                dregsy_config(&plain, &lock, Path::new("/run/dir/skopeo")),
                "\
relay: skopeo

skopeo:
  binary: \"/run/dir/skopeo\"

tasks:
  - name: sync-cgr-dev
    source:
      registry: cgr.dev
    target:
      registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
      auth-refresh: 12h
    mappings:
      - from: \"chainguard/static\"
        to: \"bench/chainguard-static\"
        platform: all
        tags:
          - \"latest@sha256:1111111111111111111111111111111111111111111111111111111111111111\"
  - name: sync-docker-io
    source:
      registry: docker.io
    target:
      registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
      auth-refresh: 12h
    mappings:
      - from: \"library/alpine\"
        to: \"bench/library-alpine\"
        platform: all
        tags:
          - \"3.20@sha256:2222222222222222222222222222222222222222222222222222222222222222\"
          - \"latest@sha256:3333333333333333333333333333333333333333333333333333333333333333\"
",
            ),
            (
                "regsync_config_exact_output",
                regsync_config(&plain, &lock),
                "\
version: 1

creds:
  - registry: cgr.dev
    repoAuth: true
  - registry: docker.io
    repoAuth: true
  - registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
    credHelper: docker-credential-ecr-login

sync:
  - source: \"cgr.dev/chainguard/static@sha256:1111111111111111111111111111111111111111111111111111111111111111\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/chainguard-static:latest\"
    type: image
    mediaTypes:
      - \"application/vnd.docker.distribution.manifest.v2+json\"
      - \"application/vnd.docker.distribution.manifest.list.v2+json\"
      - \"application/vnd.oci.image.manifest.v1+json\"
      - \"application/vnd.oci.image.index.v1+json\"
  - source: \"docker.io/library/alpine@sha256:2222222222222222222222222222222222222222222222222222222222222222\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/library-alpine:3.20\"
    type: image
    mediaTypes:
      - \"application/vnd.docker.distribution.manifest.v2+json\"
      - \"application/vnd.docker.distribution.manifest.list.v2+json\"
      - \"application/vnd.oci.image.manifest.v1+json\"
      - \"application/vnd.oci.image.index.v1+json\"
  - source: \"docker.io/library/alpine@sha256:3333333333333333333333333333333333333333333333333333333333333333\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/library-alpine:latest\"
    type: image
    mediaTypes:
      - \"application/vnd.docker.distribution.manifest.v2+json\"
      - \"application/vnd.docker.distribution.manifest.list.v2+json\"
      - \"application/vnd.oci.image.manifest.v1+json\"
      - \"application/vnd.oci.image.index.v1+json\"
",
            ),
            (
                "ocync_config_with_dockerhub_auth",
                ocync_config(&with_auth, Kind::Ecr, None, Mode::Sync(&lock)),
                "\
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
  - from: \"chainguard/static\"
    to: \"bench/chainguard-static\"
    source: cgr-dev
    targets: [ecr]
    tags:
      - name: \"latest\"
        digest: \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"
  - from: \"library/alpine\"
    to: \"bench/library-alpine\"
    source: docker-io
    targets: [ecr]
    tags:
      - name: \"3.20\"
        digest: \"sha256:2222222222222222222222222222222222222222222222222222222222222222\"
      - name: \"latest\"
        digest: \"sha256:3333333333333333333333333333333333333333333333333333333333333333\"
",
            ),
            (
                "dregsy_config_with_dockerhub_auth",
                dregsy_config(&with_auth, &lock, Path::new("/run/dir/skopeo")),
                "\
relay: skopeo

skopeo:
  binary: \"/run/dir/skopeo\"

tasks:
  - name: sync-cgr-dev
    source:
      registry: cgr.dev
    target:
      registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
      auth-refresh: 12h
    mappings:
      - from: \"chainguard/static\"
        to: \"bench/chainguard-static\"
        platform: all
        tags:
          - \"latest@sha256:1111111111111111111111111111111111111111111111111111111111111111\"
  - name: sync-docker-io
    source:
      registry: docker.io
      auth: b2N5bmMtYmVuY2g6ZGtyX3BhdF9hYmMxMjM=
    target:
      registry: 111111111111.dkr.ecr.us-east-1.amazonaws.com
      auth-refresh: 12h
    mappings:
      - from: \"library/alpine\"
        to: \"bench/library-alpine\"
        platform: all
        tags:
          - \"3.20@sha256:2222222222222222222222222222222222222222222222222222222222222222\"
          - \"latest@sha256:3333333333333333333333333333333333333333333333333333333333333333\"
",
            ),
            (
                "regsync_config_with_dockerhub_auth",
                regsync_config(&with_auth, &lock),
                "\
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
  - source: \"cgr.dev/chainguard/static@sha256:1111111111111111111111111111111111111111111111111111111111111111\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/chainguard-static:latest\"
    type: image
    mediaTypes:
      - \"application/vnd.docker.distribution.manifest.v2+json\"
      - \"application/vnd.docker.distribution.manifest.list.v2+json\"
      - \"application/vnd.oci.image.manifest.v1+json\"
      - \"application/vnd.oci.image.index.v1+json\"
  - source: \"docker.io/library/alpine@sha256:2222222222222222222222222222222222222222222222222222222222222222\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/library-alpine:3.20\"
    type: image
    mediaTypes:
      - \"application/vnd.docker.distribution.manifest.v2+json\"
      - \"application/vnd.docker.distribution.manifest.list.v2+json\"
      - \"application/vnd.oci.image.manifest.v1+json\"
      - \"application/vnd.oci.image.index.v1+json\"
  - source: \"docker.io/library/alpine@sha256:3333333333333333333333333333333333333333333333333333333333333333\"
    target: \"111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/library-alpine:latest\"
    type: image
    mediaTypes:
      - \"application/vnd.docker.distribution.manifest.v2+json\"
      - \"application/vnd.docker.distribution.manifest.list.v2+json\"
      - \"application/vnd.oci.image.manifest.v1+json\"
      - \"application/vnd.oci.image.index.v1+json\"
",
            ),
        ] {
            assert_eq!(config, expected, "{name}");
        }
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
    fn registry_key_shortens_ecr_and_replaces_dots() {
        for (host, key) in [
            ("111111111111.dkr.ecr.us-east-1.amazonaws.com", "ecr"),
            ("cgr.dev", "cgr-dev"),
            ("docker.io", "docker-io"),
            ("public.ecr.aws", "public-ecr-aws"),
        ] {
            assert_eq!(registry_key(host), key, "{host}");
        }
    }
}
