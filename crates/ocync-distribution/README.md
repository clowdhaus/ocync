# ocync-distribution

OCI Distribution Specification client library.

This crate is part of [ocync](https://github.com/clowdhaus/ocync), a fast OCI registry sync tool. The primary interface is the `ocync` CLI — this crate is the underlying library, exposed for embedding in other Rust applications.

## What it does

Standalone client for the [OCI Distribution Spec](https://github.com/opencontainers/distribution-spec). Handles authentication, content-addressable blob operations, manifest negotiation, tag listing, and adaptive rate limiting against any OCI-compliant registry.

## Capabilities

- Registry client with per-provider auth (ECR, anonymous, token exchange)
- Blob operations: streaming pull, chunked upload, cross-repo mount, existence check
- Manifest operations: pull, push, HEAD, content negotiation for OCI and Docker media types
- Tag listing with automatic pagination
- Content-addressable SHA-256 digest computation and verification
- AIMD-based adaptive concurrency per registry action
- ECR batch blob existence checks
- FIPS 140-3 cryptography by default (aws-lc-rs)

## Example

```rust
use ocync_distribution::{RegistryClient, RegistryClientBuilder};
use url::Url;

let client = RegistryClientBuilder::new(
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

## Links

- [ocync CLI](https://github.com/clowdhaus/ocync) — the primary interface
- [API documentation](https://docs.rs/ocync-distribution)
- License: Apache-2.0
