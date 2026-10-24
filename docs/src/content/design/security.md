---
title: Security design
description: How ocync handles cryptography, cloud authentication and supply chain, measured against a FedRAMP High bar
order: 5
---

## The bar

`ocync` targets regulated environments: FIPS, GovCloud and DoD. FedRAMP High is the bar each decision is measured against. Three principles follow from it:

- **Every cryptographic operation runs inside the FIPS module.** TLS, request signing, token signing and digests all go through `aws-lc-rs` built against the FIPS module. No pure-Rust cryptography in a FIPS build.
- **Own the code on the security path.** Cloud authentication is a small surface: a few API calls and a credential chain. Owning it keeps the dependency tree small and every cryptographic call on the FIPS module.
- **Every release carries evidence.** Signatures, provenance and a bill of materials, so an assessor can check what shipped.

## Current state

- `aws-lc-rs` is the only TLS provider. `ring`, `openssl` and `native-tls` are banned in `deny.toml`.
- Linux release binaries are built with the `fips` feature, statically linked against glibc. See [FIPS 140-3](/fips).
- Content digests use `aws-lc-rs` (`crates/ocync-distribution/src/sha256.rs`).
- Release binaries carry build provenance and a bill of materials attestation. Release images are signed with cosign and carry a bill of materials attestation. GitHub Actions are pinned to commit SHAs.
- The Docker base images (`cgr.dev/chainguard/rust:latest-dev`, `cgr.dev/chainguard/static:latest`) float by design so every build picks up Chainguard's patches. They are not pinned by digest.

## Dependency weight

The FIPS build of the `ocync` binary resolves 244 crates (normal and build dependencies). The cloud SDKs account for most of what can be removed:

| Dependency | Used for | Crates only it brings |
|---|---|---|
| `aws-config`, `aws-sdk-ecr`, `aws-sdk-ecrpublic` | `GetAuthorizationToken` (ECR, ECR Public), `BatchCheckLayerAvailability`, the credential chain | 41 |
| `google-cloud-auth` | one access token | 21 |
| Both together | | 80 |

The duplicated hash crate versions (`digest`, `block-buffer`, `crypto-common`, `cpufeatures`) all come from these SDKs, and four of the `skip` entries in `deny.toml` exist for them.

## TLS uses only FIPS-approved algorithms

The `fips` feature enables `rustls/fips`. That feature applies to every `rustls` user in the build, so `aws_lc_rs::default_provider()` drops ChaCha20-Poly1305 and every key exchange group the module does not approve, for `reqwest`, which `google-cloud-auth` also uses, and for the TLS client the AWS SDK builds itself. TLS 1.2 also requires extended master secret.

At startup a FIPS build checks `tls_fips()`, which asks `rustls` whether the installed provider is FIPS-approved, including whether the module is in approved mode. Every command except `version` refuses to start if it is not, so `version` can still report the failing state. `ocync version` prints the AWS-LC-FIPS module number a certificate names (`module: AWS-LC-FIPS 4`), the AWS-LC release (`release: AWS-LC 4.2.0`) and the result (`tls: approved`), in the shape `openssl list -providers` uses. A FIPS build decides all of this from one constant, `CRYPTO_BUILD` in `ocync-distribution`, so the CLI cannot disagree with the linked module.

`tests/fips_tls.rs` in `ocync-distribution` pins the behavior: the registry client fails the handshake against servers offering only TLS 1.3 ChaCha20-Poly1305, only TLS 1.2 ChaCha20-Poly1305, or only X25519, and succeeds against one offering AES-128-GCM.

## The shipped module's status is stated correctly

`ocync` links AWS-LC FIPS module 4, which AWS has submitted to NIST and which awaits certification. The docs name module 4 and its status, not a certificate it does not hold yet.

## Releases carry a bill of materials

Release binaries and the image's binary are built with `cargo auditable`, which embeds the dependency list in the binary, so scanners such as grype and trivy read it from the shipped artifact. `syft` turns it into CycloneDX:

- Each binary ships with `<binary>.cdx.json`, attested on GitHub next to its build provenance.
- Each platform image carries a cosign attestation of its bill of materials, which also lists the base image's packages.

The list comes from cargo's dependency graph, so it can name crates that are not linked: `ring`, which a weak feature in `rustls-webpki` mentions, and `aws-lc-sys`, which `rustls` resolves in a FIPS build. `cargo deny` is the ban check. `cargo xtask sbom-check` fails CI and the release when a bill of materials lacks `ocync`, `aws-lc-rs` or the build's AWS-LC module crate, which catches a binary built without its dependency list or a FIPS binary built without the FIPS module.

## Owned Google authentication

> **Status: Planned.**

`google-cloud-auth` signs and hashes with the pure-Rust `sha2` and `hmac` crates and draws randomness from a pure-Rust ChaCha20 generator. Replace it with an owned Application Default Credentials resolver that matches upstream behavior:

- `GOOGLE_APPLICATION_CREDENTIALS`, then the gcloud well-known file, then the GCE and GKE metadata server.
- Credential file types: service account keys (RS256 signed assertion via `aws-lc-rs`), authorized user (refresh token), and external account (Workload Identity Federation).
- Federation subject tokens from a file (such as a projected Kubernetes service account token), a URL, or AWS. The AWS source signs `GetCallerIdentity` with the owned AWS signer and credential chain, so `ocync` on EKS reaches Artifact Registry with no long-lived key.
- Service account impersonation after the federation exchange (`service_account_impersonation_url`).
- The real token lifetime from `expires_in`, replacing the fixed 10-minute assumption in `auth/gcp.rs`.

## Owned AWS request signing

> **Status: Planned.**

`aws-sigv4` computes Signature Version 4 with the pure-Rust `sha2` and `hmac` crates. Replace it with an owned signer on `aws-lc-rs` HMAC and SHA-256. The signer covers the calls `ocync` makes: ECR and ECR Public (`GetAuthorizationToken`, `BatchCheckLayerAvailability`) and STS (`AssumeRole` for role chaining).

## Owned AWS credential chain

> **Status: Planned.**

Replace `aws-config` and the AWS SDK crates with an owned credential chain that matches the documented behavior in [Amazon ECR](/registries/ecr), in AWS precedence order:

- Environment variables.
- Shared config and credentials files, including `source_profile` and `role_arn` role chaining and the per-registry `aws_profile` field.
- Web identity (IRSA) through `AssumeRoleWithWebIdentity`.
- `credential_process`.
- IAM Identity Center (`aws sso login`): read the cached token, exchange it with `GetRoleCredentials`, and refresh it with `CreateToken` within 5 minutes of expiry, at most once every 30 seconds, writing the refreshed token back to the shared cache in a form the AWS CLI still reads. Neither call is signed. The cache file name is a SHA-1 of the session name, computed with `aws-lc-rs`.
- The container credential endpoint (ECS task roles, EKS Pod Identity).
- The EC2 instance metadata service, IMDSv2 only.
- FIPS endpoints through `AWS_USE_FIPS_ENDPOINT`, and the GovCloud, China and ISO partitions.

The SDK crates are removed once the owned chain and signer reach parity.

## Alternatives considered

Lighter crates were evaluated before choosing to own the code:

| Crate | Why it does not fit |
|---|---|
| `reqsign` (Apache OpenDAL) | The core crate depends on the pure-Rust `sha1`, `sha2` and `hmac` crates, so signing stays outside the FIPS module. |
| `aws-sign-v4` | Built on `ring`, which `deny.toml` bans. |
| `aws-creds` | Credentials only, blocking HTTP, and no IAM Identity Center, `source_profile` role chaining or `credential_process`. |
| `gcp_auth` | Its cryptography runs on `aws-lc-rs`, but it has no Workload Identity Federation. Federation lets `ocync` outside Google Cloud reach Artifact Registry without a long-lived service account key. |

## Replacing a dependency

Each replacement starts from the upstream crate's tests and works backwards:

1. Copy the upstream tests and test data into the repo with Apache-2.0 attribution and the upstream commit recorded. Point them at the new module's API, so they fail.
2. Implement until they pass. The AWS signing test suite shipped in `aws-sigv4` (`aws-signing-test-suite/v4`) is the reference for the signer. `google-cloud-auth` and `aws-config` supply theirs.
3. An upstream test for behavior `ocync` deliberately does not support is listed in this document with the reason. A gap is a decision, never an omission.
4. While both implementations exist, the upstream crate stays as a dev-dependency and a differential test runs both on the same inputs: identical signatures, identical resolved credentials.
5. Before the upstream crate is removed, the real binary runs against real ECR, ECR Public and Artifact Registry, and the result is diffed against the base commit.
