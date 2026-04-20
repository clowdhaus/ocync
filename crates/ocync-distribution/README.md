# ocync-distribution

OCI Distribution Specification client library for Rust.

This crate is part of [ocync](https://github.com/clowdhaus/ocync), a fast OCI registry sync tool. The primary interface is the `ocync` CLI - this crate is the underlying library, exposed for embedding in other Rust applications.

## What it does

Standalone client for the [OCI Distribution Spec](https://github.com/opencontainers/distribution-spec). Handles authentication, content-addressable blob operations, manifest negotiation, tag listing, and adaptive rate limiting against any OCI-compliant registry.

## Key features

- Registry client with automatic provider detection (ECR, Docker Hub, GHCR, GAR, ACR, Chainguard, anonymous)
- Streaming blob pull and push (zero intermediate buffering)
- Cross-repo blob mounting
- Manifest pull, push, HEAD with OCI and Docker v2 content negotiation
- Tag listing with automatic pagination
- SHA-256 digest computation and verification (FIPS 140-3 via aws-lc-rs)
- [AIMD](https://clowdhaus.github.io/ocync/design/overview#adaptive-concurrency-aimd) (additive increase, multiplicative decrease) adaptive concurrency per registry action
- ECR batch blob existence checks

## Feature flags

| Flag | Default | Description |
|------|---------|-------------|
| `fips` | Yes | FIPS 140-3 validated cryptography via aws-lc-rs |
| `non-fips` | No | aws-lc-rs without FIPS mode |

## Minimum Rust version

1.94 (edition 2024)

## Example

```rust
use ocync_distribution::{RegistryClient, install_crypto_provider};
use url::Url;

// Required before any TLS connection -- registers aws-lc-rs as the
// process-wide rustls crypto provider.
install_crypto_provider();

let client = RegistryClient::builder(
        Url::parse("https://registry-1.docker.io").unwrap(),
    )
    .build()
    .unwrap();

let manifest = client
    .pull_manifest("library/nginx", "latest")
    .await
    .unwrap();

println!("digest: {}", manifest.digest);
```

## Public API surface

The primary entry points and types are listed below.

| Type | Description |
|------|-------------|
| `RegistryClient` / `RegistryClientBuilder` | Main entry point for all registry operations |
| `Reference` | OCI image reference parser (`registry/repo:tag@digest`) |
| `Digest` | Content-addressable SHA-256 digest |
| `ManifestPull` / `ManifestHead` | Manifest operation results |
| `MountResult` | Cross-repo mount outcome (mounted or upload session) |
| `BatchBlobChecker` | ECR batch blob existence checks |
| `Descriptor` | OCI content descriptor (digest, size, media type) |
| `ImageManifest` / `ImageIndex` | OCI image manifest and multi-arch index |
| `Platform` | OS/architecture platform specifier |
| `MediaType` | OCI and Docker v2 media type enum |

## Links

- [ocync CLI](https://github.com/clowdhaus/ocync) - the primary interface
- License: Apache-2.0
