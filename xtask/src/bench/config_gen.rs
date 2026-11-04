//! Generate equivalent config files for ocync, dregsy, and regsync from a corpus,
//! and the Docker config and per-tag command lines of the tools that copy one tag
//! per process.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;

use ocync_distribution::Digest;

use crate::bench::RegistryKind;
use crate::bench::corpus::{Corpus, DockerHubAuth, ImageEntry};
use crate::bench::lock::Lock;
use crate::bench::runner::Copier;
use crate::bench::skopeo_shim::PRESERVE_DIGESTS;
use crate::bench::source::Controlled;
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

/// The `user:token` Docker Hub credentials, base64-encoded as dregsy's `auth`
/// field and a Docker config's `auth` both take them.
fn docker_hub_basic_auth(auth: &DockerHubAuth) -> String {
    base64_encode(&format!("{}:{}", auth.username, auth.token))
}

/// Base64-encode a byte slice using the standard alphabet with padding.
///
/// Hand-rolled to avoid adding a `base64` crate dependency for two call
/// sites (the dregsy auth field and the Docker config's `auth`).
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
pub(crate) fn source_path(source: &str) -> &str {
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
        host_key(hostname)
    }
}

/// A registry hostname as a YAML key, for a source: unlike the target's, an ECR
/// source keeps its whole host, so the controlled source and the target, both
/// ECR, are two registries.
fn host_key(hostname: &str) -> String {
    hostname.replace('.', "-")
}

/// The credential helper regsync and the Docker config name for an ECR registry.
const ECR_CRED_HELPER: &str = "docker-credential-ecr-login";

/// The registries the tools read `corpus` from: their hostnames, sorted, once
/// each.
fn source_registries(corpus: &Corpus) -> Vec<String> {
    let mut registries: Vec<String> = corpus
        .images
        .iter()
        .map(|img| source_registry(&corpus.source.locate(&img.source)).to_owned())
        .collect();
    registries.sort_unstable();
    registries.dedup();
    registries
}

/// Whether a source registry is signed in to as ECR is, which only the
/// controlled source is: the public ones the corpus names are not private ECR.
fn is_ecr(registry: &str) -> bool {
    RegistryKind::of(registry) == RegistryKind::Ecr
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
    ocync_yaml(
        corpus,
        &corpus.settings.target_registry,
        target,
        max_concurrent_transfers,
        mode,
        &|source| corpus.target_repo(source),
    )
}

/// The ocync config `bench-seed` runs: every image `corpus` names, each locked
/// tag pinned to its digest in `lock`, copied from its public source to its
/// repository in `controlled`, which ocync signs in to as ECR.
pub(crate) fn seed_config(corpus: &Corpus, lock: &Lock, controlled: &Controlled) -> String {
    ocync_yaml(
        corpus,
        controlled.host(),
        Kind::Ecr,
        None,
        Mode::Sync(lock),
        &|source| controlled.repo(source),
    )
}

/// [`ocync_config`] for a target registry `target_registry`, each image going
/// to the repository `to` names for its source.
fn ocync_yaml(
    corpus: &Corpus,
    target_registry: &str,
    target: Kind,
    max_concurrent_transfers: Option<NonZeroUsize>,
    mode: Mode<'_>,
    to: &dyn Fn(&str) -> String,
) -> String {
    let mut out = String::new();

    if let Some(cap) = max_concurrent_transfers {
        out.push_str(&format!("global:\n  max_concurrent_transfers: {cap}\n\n"));
    }

    let target_key = registry_key(target_registry);

    out.push_str("registries:\n");
    for reg in source_registries(corpus) {
        let key = host_key(&reg);
        out.push_str(&format!("  {key}:\n"));
        out.push_str(&format!("    url: {reg}\n"));
        if is_ecr(&reg) {
            out.push_str(&format!("    auth_type: {}\n", Kind::Ecr.auth_type()));
        }
        // Docker Hub: inject basic auth to avoid 10 pulls/hr anonymous limit.
        if reg == "docker.io" {
            if let Some(auth) = &corpus.dockerhub_auth {
                out.push_str("    auth_type: basic\n");
                out.push_str("    credentials:\n");
                out.push_str(&format!("      username: {}\n", auth.username));
                out.push_str(&format!("      password: {}\n", auth.token));
            }
        }
    }
    out.push_str(&format!("  {target_key}:\n"));
    out.push_str(&format!("    url: {target_registry}\n"));
    out.push_str(&format!("    auth_type: {}\n", target.auth_type()));

    out.push('\n');
    out.push_str("mappings:\n");
    for img in &corpus.images {
        let read_from = corpus.source.locate(&img.source);
        let src_key = host_key(source_registry(&read_from));
        let path = source_path(&read_from);
        let target_repo = to(&img.source);
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
    // Group images by the registry they are read from. BTreeMap gives
    // deterministic iteration order.
    let mut by_registry: BTreeMap<String, Vec<(&ImageEntry, String)>> = BTreeMap::new();
    for img in &corpus.images {
        let read_from = corpus.source.locate(&img.source).into_owned();
        let reg = source_registry(&read_from).to_owned();
        by_registry.entry(reg).or_default().push((img, read_from));
    }

    let mut out = String::new();
    out.push_str("relay: skopeo\n");
    out.push('\n');
    out.push_str(&format!("skopeo:\n  binary: \"{}\"\n", skopeo.display()));
    out.push('\n');
    out.push_str("tasks:\n");

    for (registry, images) in &by_registry {
        let task_name = format!("sync-{}", host_key(registry));
        out.push_str(&format!("  - name: {task_name}\n"));
        out.push_str("    source:\n");
        out.push_str(&format!("      registry: {registry}\n"));
        // The controlled source is ECR: signed in to as the target is.
        if is_ecr(registry) {
            out.push_str("      auth-refresh: 12h\n");
        }
        // Docker Hub: inject base64-encoded auth to avoid 10 pulls/hr anonymous limit.
        if registry == "docker.io" {
            if let Some(auth) = &corpus.dockerhub_auth {
                let encoded = docker_hub_basic_auth(auth);
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
        for (img, read_from) in images {
            let from = source_path(read_from);
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
    let mut out = String::new();
    out.push_str("version: 1\n");
    out.push('\n');
    out.push_str("creds:\n");
    for reg in source_registries(corpus) {
        out.push_str(&format!("  - registry: {reg}\n"));
        if is_ecr(&reg) {
            // The controlled source is ECR: signed in to as the target is.
            out.push_str(&format!("    credHelper: {ECR_CRED_HELPER}\n"));
            continue;
        }
        // Per-repo auth: required for registries that issue per-scope tokens and
        // reject multi-scope requests (cgr.dev, gcr.io, nvcr.io).
        out.push_str("    repoAuth: true\n");
        // Docker Hub: inject user/pass to avoid 10 pulls/hr anonymous limit.
        if reg == "docker.io" {
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
    out.push_str(&format!("    credHelper: {ECR_CRED_HELPER}\n"));

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
            let source_ref = format!("{}@{digest}", corpus.source.locate(&img.source));
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

/// The file name of the Docker-format config the per-tag tools sign in with,
/// in the run's config directory.
pub(crate) const DOCKER_CONFIG_FILE: &str = "config.json";

/// The key Docker Hub credentials are stored under in a Docker config, which
/// every per-tag tool maps to `docker.io`.
const DOCKER_HUB_AUTH_KEY: &str = "https://index.docker.io/v1/";

/// The Docker-format config the per-tag tools sign in with: Docker Hub
/// credentials when the corpus has them, and the ECR credential helper for the
/// target registry and for any source that is ECR. Credentials go here, never on a command line, since the
/// stderr of a failed run is printed.
pub(crate) fn docker_config(corpus: &Corpus) -> String {
    let mut config = serde_json::Map::new();
    if let Some(auth) = &corpus.dockerhub_auth {
        let encoded = docker_hub_basic_auth(auth);
        config.insert(
            "auths".to_owned(),
            serde_json::json!({ DOCKER_HUB_AUTH_KEY: { "auth": encoded } }),
        );
    }
    let mut helpers = serde_json::Map::new();
    for registry in source_registries(corpus).into_iter().filter(|r| is_ecr(r)) {
        helpers.insert(registry, "ecr-login".into());
    }
    helpers.insert(corpus.settings.target_registry.clone(), "ecr-login".into());
    config.insert("credHelpers".to_owned(), helpers.into());
    serde_json::Value::Object(config).to_string()
}

/// The arguments after the binary of every copy a per-tag tool makes, one list
/// per corpus tag in corpus order (image, then tag). Each copies the locked
/// digest from `lock` to the corpus tag, so a moved source tag cannot change
/// what is copied. `config_dir` holds the [`DOCKER_CONFIG_FILE`] skopeo is told
/// to sign in with; the other tools find it through `DOCKER_CONFIG`.
///
/// Platform, referrer, digest-tag and fast flags are never passed: each copies
/// every platform and, as configured here, no referrers.
pub(crate) fn per_tag_copies(
    copier: Copier,
    corpus: &Corpus,
    lock: &Lock,
    config_dir: &Path,
) -> Vec<Vec<String>> {
    let authfile = config_dir.join(DOCKER_CONFIG_FILE).display().to_string();
    let mut copies = Vec::new();
    for img in &corpus.images {
        let target_ref = corpus.target_ref(&img.source);
        for tag in &img.tags {
            let source = format!(
                "{}@{}",
                corpus.source.locate(&img.source),
                locked_digest(lock, &img.source, tag)
            );
            let target = format!("{target_ref}:{tag}");
            let args = match copier {
                Copier::Skopeo => vec![
                    "--insecure-policy".to_owned(),
                    "copy".to_owned(),
                    PRESERVE_DIGESTS.to_owned(),
                    "--multi-arch".to_owned(),
                    "all".to_owned(),
                    "--authfile".to_owned(),
                    authfile.clone(),
                    format!("docker://{source}"),
                    format!("docker://{target}"),
                ],
                Copier::Crane => vec!["copy".to_owned(), source, target],
                Copier::Regctl => vec!["image".to_owned(), "copy".to_owned(), source, target],
                Copier::Oras => vec!["cp".to_owned(), source, target],
            };
            copies.push(args);
        }
    }
    copies
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
        let mut acr = test_corpus();
        acr.settings.target_registry = "ocyncbench1a2b3c.azurecr.io".to_string();
        acr.settings.target_prefix = "run-r1/bench".to_string();

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
                // Catches: ocync told to sign in to ACR the way ECR or Google
                // needs.
                "ocync_config_acr",
                ocync_config(&acr, Kind::Acr, None, Mode::Sync(&lock)),
                "\
registries:
  cgr-dev:
    url: cgr.dev
  docker-io:
    url: docker.io
  ocyncbench1a2b3c-azurecr-io:
    url: ocyncbench1a2b3c.azurecr.io
    auth_type: acr

mappings:
  - from: \"chainguard/static\"
    to: \"run-r1/bench/chainguard-static\"
    source: cgr-dev
    targets: [ocyncbench1a2b3c-azurecr-io]
    tags:
      - name: \"latest\"
        digest: \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"
  - from: \"library/alpine\"
    to: \"run-r1/bench/library-alpine\"
    source: docker-io
    targets: [ocyncbench1a2b3c-azurecr-io]
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
    fn docker_config_signs_in_to_docker_hub_when_it_has_a_token_and_to_ecr_by_helper() {
        use crate::bench::corpus::DockerHubAuth;

        // Catches: Docker Hub credentials under a key the tools do not map to
        // docker.io, a token or an `auths` key written without credentials, and
        // the helper named for another registry than the target.
        let mut with_auth = test_corpus();
        with_auth.dockerhub_auth = Some(DockerHubAuth {
            username: "ocync-bench".to_string(),
            token: "dkr_pat_abc123".to_string(),
        });
        assert_eq!(
            docker_config(&with_auth),
            r#"{"auths":{"https://index.docker.io/v1/":{"auth":"b2N5bmMtYmVuY2g6ZGtyX3BhdF9hYmMxMjM="}},"credHelpers":{"111111111111.dkr.ecr.us-east-1.amazonaws.com":"ecr-login"}}"#
        );
        assert_eq!(
            docker_config(&test_corpus()),
            r#"{"credHelpers":{"111111111111.dkr.ecr.us-east-1.amazonaws.com":"ecr-login"}}"#
        );
    }

    #[test]
    fn per_tag_copies_copy_each_locked_digest_to_its_tag_in_corpus_order() {
        // Catches: a tag-based source (a moved tag changes what is copied), a
        // target without the tag or in another repository, tags out of corpus
        // order, and a flag that narrows or widens what the other tools copy.
        let (corpus, lock) = (test_corpus(), test_lock());
        let copies = |copier| per_tag_copies(copier, &corpus, &lock, Path::new("/run/dir"));
        let (static_src, alpine_320_src, alpine_latest_src) = (
            "cgr.dev/chainguard/static@sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "docker.io/library/alpine@sha256:2222222222222222222222222222222222222222222222222222222222222222",
            "docker.io/library/alpine@sha256:3333333333333333333333333333333333333333333333333333333333333333",
        );
        let (static_tgt, alpine_320_tgt, alpine_latest_tgt) = (
            "111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/chainguard-static:latest",
            "111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/library-alpine:3.20",
            "111111111111.dkr.ecr.us-east-1.amazonaws.com/bench/library-alpine:latest",
        );
        let skopeo = |src: &str, tgt: &str| {
            vec![
                "--insecure-policy",
                "copy",
                "--preserve-digests",
                "--multi-arch",
                "all",
                "--authfile",
                "/run/dir/config.json",
                &format!("docker://{src}"),
                &format!("docker://{tgt}"),
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<String>>()
        };
        let plain = |head: &[&str], src: &str, tgt: &str| {
            head.iter()
                .copied()
                .chain([src, tgt])
                .map(str::to_owned)
                .collect::<Vec<String>>()
        };
        let pairs = [
            (static_src, static_tgt),
            (alpine_320_src, alpine_320_tgt),
            (alpine_latest_src, alpine_latest_tgt),
        ];
        assert_eq!(
            copies(Copier::Skopeo),
            pairs.map(|(src, tgt)| skopeo(src, tgt))
        );
        for (copier, head) in [
            (Copier::Crane, &["copy"][..]),
            (Copier::Regctl, &["image", "copy"][..]),
            (Copier::Oras, &["cp"][..]),
        ] {
            assert_eq!(
                copies(copier),
                pairs.map(|(src, tgt)| plain(head, src, tgt)),
                "{copier:?}"
            );
        }
    }

    /// The test corpus read from the controlled source, for a target in
    /// us-east-2: the source is the same account's us-east-1 registry.
    fn controlled_corpus() -> Corpus {
        use crate::bench::source::{Kind as SourceKind, Source};

        let mut corpus = test_corpus();
        corpus.settings.target_registry = "111111111111.dkr.ecr.us-east-2.amazonaws.com".into();
        corpus.source =
            Source::resolve(SourceKind::Controlled, &corpus.settings.target_registry).unwrap();
        corpus
    }

    const CONTROLLED_HOST: &str = "111111111111.dkr.ecr.us-east-1.amazonaws.com";

    #[test]
    fn the_controlled_source_is_every_tools_only_source_and_no_public_host_is_left() {
        // Catches: a tool still reading a public registry beside the controlled
        // one, which would spend its rate limit and make that tool's rounds
        // differ from the others'; and the source and the target, both ECR,
        // sharing one registry key so one overwrites the other.
        let (corpus, lock) = (controlled_corpus(), test_lock());
        let dir = Path::new("/run/config");
        let mut configs = vec![
            (
                "ocync",
                ocync_config(&corpus, Kind::Ecr, None, Mode::Sync(&lock)),
            ),
            ("dregsy", dregsy_config(&corpus, &lock, Path::new("skopeo"))),
            ("regsync", regsync_config(&corpus, &lock)),
            ("docker config", docker_config(&corpus)),
        ];
        for copier in [Copier::Skopeo, Copier::Crane, Copier::Regctl, Copier::Oras] {
            configs.push((
                "per-tag",
                per_tag_copies(copier, &corpus, &lock, dir)
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
            ));
        }
        for (tool, config) in &configs {
            assert!(config.contains(CONTROLLED_HOST), "{tool}: {config}");
            for public in ["docker.io", "cgr.dev", "docker-io", "cgr-dev", "ecr.aws"] {
                assert!(!config.contains(public), "{tool} names {public}: {config}");
            }
        }

        let ocync = &configs[0].1;
        assert_eq!(
            ocync.matches("    auth_type: ecr\n").count(),
            2,
            "source and target both sign in as ECR: {ocync}"
        );
        assert!(ocync.contains("  111111111111-dkr-ecr-us-east-1-amazonaws-com:\n"));
        assert!(ocync.contains("  ecr:\n    url: 111111111111.dkr.ecr.us-east-2.amazonaws.com"));
        assert!(ocync.contains(
            "  - from: \"bench-source/library/alpine\"\n    to: \"bench/library-alpine\"\n    \
             source: 111111111111-dkr-ecr-us-east-1-amazonaws-com\n    targets: [ecr]"
        ));
        // The digests are the lock's, found by the corpus source and not the copy's.
        for entry in lock.images.values() {
            assert!(ocync.contains(&entry.digest.to_string()), "{ocync}");
        }

        // The source signs in the way the target does, for each tool.
        assert!(configs[1].1.contains(&format!(
            "registry: {CONTROLLED_HOST}\n      auth-refresh: 12h"
        )));
        assert!(configs[2].1.contains(&format!(
            "- registry: {CONTROLLED_HOST}\n    credHelper: docker-credential-ecr-login"
        )));
        assert!(!configs[2].1.contains("repoAuth"));
        let docker: serde_json::Value = serde_json::from_str(&configs[3].1).unwrap();
        assert_eq!(docker["credHelpers"][CONTROLLED_HOST], "ecr-login");
        assert_eq!(
            docker["credHelpers"]["111111111111.dkr.ecr.us-east-2.amazonaws.com"],
            "ecr-login"
        );
        assert!(docker.get("auths").is_none());
    }

    #[test]
    fn the_public_source_still_reads_the_registries_the_corpus_names() {
        // Catches: the controlled source leaking into a default run.
        let (corpus, lock) = (test_corpus(), test_lock());
        for config in [
            ocync_config(&corpus, Kind::Ecr, None, Mode::Sync(&lock)),
            dregsy_config(&corpus, &lock, Path::new("skopeo")),
            regsync_config(&corpus, &lock),
        ] {
            assert!(
                config.contains("cgr.dev") && config.contains("docker.io"),
                "{config}"
            );
            assert!(!config.contains("bench-source"), "{config}");
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
